//! Multi-session runtime manager for the Web/listen control plane.
//!
//! The listener historically shared ONE live [`Application`] across every
//! Web client, so opening a second session stopped the first one's work
//! (`wait_for_idle` + in-place cutover). This manager instead owns an
//! independent [`Application`], [`RpcDispatcher`], and per-runtime state per
//! session:
//!
//! - Top-level RPC `sessionId` selects a runtime. WebSocket bootstrap may omit
//!   it exactly once to discover the primary protocol identity; later
//!   session-scoped commands fail closed when ownership is absent.
//! - `switch_session` / `new_session` / `fork` / `clone` are manager-level
//!   lifecycle operations: they build an INDEPENDENT runtime without mutating
//!   or stopping the source runtime. Legacy transports receive the target
//!   snapshot; WebSocket switches receive a lightweight subscribe handoff.
//! - `close_session` is a non-destructive resource-lifecycle operation: it
//!   rejects the primary and rejects busy secondaries; only an idle
//!   secondary is cleaned up (fan-in forwarder aborted, side-chat shut down,
//!   `Application::cleanup`, registry removal).
//! - Every projected application and extension event carries a top-level
//!   `sessionId` identifying the OWNING/source runtime (the `session_forked`
//! - Loaded sessions are capped at [`MAX_LOADED_SESSIONS`]. When the pool is
//!   full, an open transparently retires the least-recently-used IDLE
//!   non-primary runtime (never one with active commands, turns, processes,
//!   goals, todos, workflows, or other live work) and reuses its slot;
//!   fail-closed only when every loaded runtime is genuinely busy.
//!   Non-inline commands additionally take a process-wide
//!   [`MAX_CONCURRENT_SESSION_COMMANDS`] permit; abort/stop/signal/close
//!   bypass both.
//!
//! The manager is owned by [`crate::modes::listen::ListenHandle`]: listener
//! shutdown aborts every fan-in forwarder, then cleans manager-owned
//! non-primary runtimes. The primary Web listener [`Application`] remains
//! owned and cleaned by `lib.rs`.
//!
//! Terminal shutdown is explicit and single-path: it stores the closed
//! admission state FIRST (a short atomic token covers only enqueue
//! acceptance — no admission can wait on an unbounded owner reply), gives
//! in-flight enqueue acceptances a bounded drain window, then closes the
//! lifecycle intake and rejects queued
//! creates with the exact shutting-down cause, joins every in-flight
//! manager-owned creation to terminal completion for a bounded deadline
//! (then cooperatively cancels and aborts stragglers through the
//! cancellation-safe rollback guard — a never-returning prepare or finish
//! can never hang the terminal shutdown), aborts and awaits every owner
//! including the primary, waits for the in-flight command drop barrier, and
//! only then takes the final runtime snapshot, retains the primary in the
//! registry, and publishes the ONE terminal result every caller replays. A
//! successful restart checkpoint detaches alone; a failed restart checkpoint
//! runs the complete terminal cleanup for manager-owned secondaries first
//! (the primary and the host-owned Todo child service are cleaned by
//! `run_listener_mode` behind the same exact checkpoint Err).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use pi_ai::{ContentBlock, Message, Model};
use pi_coding::{Application, TodoChildService};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{
    OwnedRwLockReadGuard, OwnedRwLockWriteGuard, OwnedSemaphorePermit, RwLock, Semaphore,
    broadcast, mpsc, oneshot, watch,
};
use tokio_util::sync::CancellationToken;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
enum LifecycleResponseMode {
    FullSnapshot,
    WebSubscribeHandoff,
}

use crate::extension_ui::{ExtensionUiAdapter, ExtensionUiEvent};
use crate::modes::rpc::{
    CatalogSwitchIdentity, MAX_CONCURRENT_COMMANDS, MAX_WS_OUTBOUND_MESSAGE_BYTES,
    ResyncRequiredReason,
    FileConversationRpcState, RpcCommand,
    RpcDispatcher, RpcExtensionUiRequest, RpcResponse, RpcSessionListRow, RpcSessionState,
    RuntimeIdentity, RuntimeUnloadReason, SubscriptionCutReason, SwitchCatalogIdentityError,
    bounded_fork_messages_response, estimate_value_bytes, fork_messages_from_tree,
    project_application_event, project_extension_ui_event, public_message,
    public_transcript_rows,
};
use crate::file_conversation::FileConversationEngine;

const SESSION_DELETE_UNDO_WINDOW: Duration = Duration::from_secs(5);
const SESSION_DOWNLOAD_TTL: Duration = Duration::from_secs(60);
pub(crate) const MAX_SESSION_DOWNLOAD_BYTES: usize = 16 * 1024 * 1024;
const MAX_STAGED_SESSION_DOWNLOADS: usize = 16;
const MAX_SESSION_DOWNLOAD_FILENAME_CHARS: usize = 128;

/// Replay window and cap for idempotent `session_move` retries.
const SESSION_MOVE_REPLAY_WINDOW: Duration = Duration::from_secs(300);
const MAX_SESSION_MOVE_REPLAYS: usize = 16;

/// Maximum concurrently loaded sessions, counting the primary TUI runtime.
/// Beyond the cap, opens retire the least-recently-used idle non-primary
/// runtime instead of failing; rejection happens only when every candidate
/// is busy (see [`SessionRuntimeManager::acquire_loaded_slot`]).
pub const MAX_LOADED_SESSIONS: usize = 128;

/// Hard deadline for opening a saved session through the creation pipeline.
/// A switch must always settle with a terminal outcome: past this bound the
/// client receives the bounded switch failure instead of waiting silently.
const SWITCH_OPEN_DEADLINE: Duration = Duration::from_secs(25);

/// Bounded deadline the terminal shutdown waits for in-flight creations to
/// reach terminal completion before cooperatively cancelling them.
const CREATION_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
/// Bounded grace a cancelled creation has to observe the cooperative
/// cancellation token and settle through its rollback guard before the
/// shutdown aborts it.
const CREATION_CANCEL_GRACE: Duration = Duration::from_secs(2);
/// Bounded in-flight command drop barrier: after owner abort, the terminal
/// shutdown resolves only once every in-flight command future was observed
/// dropped (or this deadline elapses).
const COMMAND_DROP_BARRIER_TIMEOUT: Duration = Duration::from_secs(5);
/// Bounded window the terminal shutdown gives admissions that passed the
/// closed check to complete their enqueue acceptance before the intake
/// closes. Covers only the enqueue, never a protocol or owner reply: an
/// unbounded owner reply must never delay the terminal shutdown.
const ADMISSION_CLOSE_DRAIN: Duration = Duration::from_millis(250);
/// Bounded budget the terminal shutdown gives tracked detached cleanup
/// tasks (rollback fallbacks, post-close retirements) to settle before the
/// terminal result publishes. A task that misses it stays detached,
/// best-effort.
const CLEANUP_JOIN_BARRIER: Duration = Duration::from_secs(5);

/// Process-wide cap on non-inline RPC commands across ALL sessions, on top of
/// each session's own [`MAX_CONCURRENT_COMMANDS`] permit pool. Abort, stop,
/// signal, and close safety operations bypass both caps; permits release as
/// commands complete, so the overload errors recover naturally.
pub const MAX_CONCURRENT_SESSION_COMMANDS: usize = 32;

/// Fan-in event buffer: the per-runtime forwarders never block (broadcast
/// drops for lagged receivers), so this only bounds how far a slow client can
/// lag before receiving a lag-failure record.
const EVENT_FANIN_CAPACITY: usize = 1024;
const EVENT_REPLAY_CAPACITY: usize = 2048;
const EVENT_REPLAY_BYTE_BUDGET: usize = 8 * 1024 * 1024;
const COMMAND_LEDGER_BYTE_BUDGET: usize = 8 * 1024 * 1024;
const COMPLETED_SNAPSHOT_BYTE_BUDGET: usize = MAX_WS_OUTBOUND_MESSAGE_BYTES;
const MAX_CONCURRENT_RETIREMENTS: usize = 1;
const MAX_GLOBAL_SNAPSHOT_BUILDS: usize = 2;
const SUBSCRIPTION_SNAPSHOT_JOIN_TIMEOUT: Duration = Duration::from_secs(30);
const RETIREMENT_JOIN_TIMEOUT: Duration = Duration::from_secs(2);
const LIFECYCLE_WRITE_GUARD_TIMEOUT: Duration = Duration::from_secs(2);
const COMMAND_LEDGER_CAPACITY: usize = 1024;
const MAX_COMMAND_LEDGER_ID_BYTES: usize = 1024;
const PROTOCOL_MAILBOX_CAPACITY: usize = 128;
/// One absolute budget per `ProtocolMessage::Snapshot`: every queued-event
/// pull, lagged retry, epoch retry, and receive of that attempt shares it, so
/// a continuous non-Agent event stream can never monopolize the owner.
const SNAPSHOT_BARRIER_ATTEMPT_DEADLINE: Duration = Duration::from_secs(1);
#[derive(Debug)]
enum SnapshotBarrierDrain {
    Event(pi_coding::ApplicationEvent),
    Resync,
    Complete,
    /// The shared attempt deadline expired while the runtime was active.
    Deadline,
    Unavailable,
}

/// A runtime between turns with no streaming provider work can complete a
/// snapshot cut from the authoritative transcript; an active turn must
/// produce an exact cut or fail closed.
fn snapshot_is_idle(snapshot: &pi_coding::ApplicationProtocolSnapshot) -> bool {
    snapshot.lifecycle.active_turn_id.is_none() && !snapshot.state.is_streaming
}

async fn next_snapshot_barrier_event(
    application_events: &mut broadcast::Receiver<pi_coding::ApplicationEvent>,
    snapshot: &pi_coding::ApplicationProtocolSnapshot,
    attempt_deadline: tokio::time::Instant,
) -> SnapshotBarrierDrain {
    match application_events.try_recv() {
        Ok(event) => SnapshotBarrierDrain::Event(event),
        Err(broadcast::error::TryRecvError::Lagged(_)) => SnapshotBarrierDrain::Resync,
        Err(broadcast::error::TryRecvError::Closed) => {
            if snapshot_is_idle(snapshot) {
                SnapshotBarrierDrain::Complete
            } else {
                SnapshotBarrierDrain::Unavailable
            }
        }
        Err(broadcast::error::TryRecvError::Empty) => {
            match tokio::time::timeout_at(attempt_deadline, application_events.recv()).await {
                Ok(Ok(event)) => SnapshotBarrierDrain::Event(event),
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => SnapshotBarrierDrain::Resync,
                Ok(Err(broadcast::error::RecvError::Closed)) if snapshot_is_idle(snapshot) => {
                    SnapshotBarrierDrain::Complete
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => SnapshotBarrierDrain::Unavailable,
                Err(_) if snapshot_is_idle(snapshot) => SnapshotBarrierDrain::Complete,
                Err(_) => SnapshotBarrierDrain::Deadline,
            }
        }
    }
}
/// Outcome of one bounded snapshot-barrier drain pass.
#[derive(Debug)]
enum SnapshotDrainOutcome {
    /// The sequenced Agent barrier is exactly covered and the snapshot epoch
    /// has been observed; the cut may proceed.
    Settled,
    /// A lag was observed; push an authoritative resync with a fresh snapshot.
    Resync,
    /// An enveloped event changed the epoch; restart with a fresh snapshot.
    Restart,
    /// The bounded future-epoch queue overflowed; the owner must resnapshot
    /// instead of evicting identity-bearing events.
    PendingEpochOverflow,
    /// The attempt cannot produce an exact cut; fail closed.
    Unavailable(anyhow::Error),
}

/// Drain queued application events until the sequenced Agent barrier is
/// exactly covered and the snapshot epoch has been observed. Queued
/// `RuntimeChanged` events are consumed here, so a zero-Agent-barrier
/// new-epoch cut never leaves the epoch switch above the watermark. Every
/// pull and retry of the attempt shares one absolute deadline.
async fn drain_snapshot_barrier(
    runtime: &SessionRuntime,
    state: &mut ProtocolState,
    application_events: &mut broadcast::Receiver<pi_coding::ApplicationEvent>,
    events: &broadcast::Sender<Value>,
    snapshot: &pi_coding::ApplicationProtocolSnapshot,
    attempt_deadline: tokio::time::Instant,
) -> SnapshotDrainOutcome {
    // Overflow is fail-closed precedence: checked before loop entry so a
    // flagged state returns PendingEpochOverflow even when barrier/epoch are
    // already covered (per SnapshotDrainOutcome::PendingEpochOverflow doc).
    if state.pending_future_epoch_overflow {
        return SnapshotDrainOutcome::PendingEpochOverflow;
    }
    let epoch_at_entry = state.epoch;
    while state.forwarded_agent_event_barrier < snapshot.event_barrier
        || state.observed_epoch < snapshot.epoch
    {
        if state.pending_future_epoch_overflow {
            return SnapshotDrainOutcome::PendingEpochOverflow;
        }
        if tokio::time::Instant::now() >= attempt_deadline {
            // A continuous non-Agent stream must not monopolize the owner: an
            // idle runtime completes by advancing to the authoritative
            // barrier; an active turn fails closed on the finite deadline.
            if snapshot_is_idle(snapshot) {
                state.forwarded_agent_event_barrier = snapshot.event_barrier;
                return SnapshotDrainOutcome::Settled;
            }
            return SnapshotDrainOutcome::Unavailable(anyhow!(
                "session protocol snapshot event barrier did not settle before the attempt deadline"
            ));
        }
        match next_snapshot_barrier_event(application_events, snapshot, attempt_deadline).await {
            SnapshotBarrierDrain::Event(event) => {
                let epoch_before = state.epoch;
                if let Some(payload) = envelope_application_event(runtime, state, event) {
                    let _ = events.send(payload);
                }
                // Only an advanced epoch admits matching pending events; keep
                // still-future epochs queued and never re-envelope them here.
                if state.epoch > epoch_before {
                    let mut deferred = VecDeque::new();
                    while let Some(pending) = state.pending_future_epoch_events.pop_front() {
                        match &pending {
                            pi_coding::ApplicationEvent::AgentPublished { epoch, .. }
                                if *epoch > state.epoch =>
                            {
                                deferred.push_back(pending);
                            }
                            _ => {
                                if let Some(payload) =
                                    envelope_application_event(runtime, state, pending)
                                {
                                    let _ = events.send(payload);
                                }
                            }
                        }
                    }
                    state.pending_future_epoch_events = deferred;
                }
            }
            SnapshotBarrierDrain::Resync => return SnapshotDrainOutcome::Resync,
            SnapshotBarrierDrain::Complete => {
                // Only after the queue consumed the matching epoch authority:
                // adopting the snapshot barrier here cannot cover a future
                // event from the same barrier number in an older epoch.
                state.forwarded_agent_event_barrier = snapshot.event_barrier;
                // Epoch advanced during pass: outer must restart with fresh snapshot.
                if state.epoch > epoch_at_entry {
                    return SnapshotDrainOutcome::Restart;
                }
                return SnapshotDrainOutcome::Settled;
            }
            SnapshotBarrierDrain::Deadline => {
                return SnapshotDrainOutcome::Unavailable(anyhow!(
                    "session protocol snapshot event barrier did not settle before the attempt deadline"
                ));
            }
            SnapshotBarrierDrain::Unavailable => {
                return SnapshotDrainOutcome::Unavailable(anyhow!(
                    "session protocol snapshot event barrier is unavailable"
                ));
            }
        }
    }
    // Epoch advanced during pass: outer must restart with fresh snapshot.
    if state.epoch > epoch_at_entry {
        return SnapshotDrainOutcome::Restart;
    }
    SnapshotDrainOutcome::Settled
}
/// What a spawned runtime should be built from.
#[derive(Debug)]
pub enum SessionSpawnKind {
    /// Open a prepared persisted session. The retained descriptor crosses the
    /// factory boundary, so the runtime never reopens a display path.
    Open {
        prepared: pi_coding::PreparedSessionResume,
    },
    /// Start a fresh runtime in this retained operator directory.
    Fresh {
        directory: pi_coding::PreparedOperatorDirectory,
    },
    /// Fork the source session at the given entry id.
    Fork { entry_id: String },
    /// Fork a prepared saved session without opening it as a live runtime.
    ForkPrepared {
        prepared: pi_coding::PreparedSessionResume,
        entry_id: String,
    },
    /// Clone the source session's active leaf.
    Clone,
    /// Clone a prepared saved session without opening it as a live runtime.
    ClonePrepared {
        prepared: pi_coding::PreparedSessionResume,
    },
}

/// Request passed to a [`SessionSpawner`]. `source` is the runtime the
/// lifecycle command arrived on (or the primary); the spawned runtime derives
/// its model/thinking/auth options from it. A Fresh kind carries the retained
/// no-follow authority for the manager-inspected target cwd.
pub struct SessionSpawnRequest {
    pub kind: SessionSpawnKind,
    pub source: Application,
}

impl PreparedSessionSpawn {
    /// Explicit async discard of a preparation that never reaches `finish`
    /// (loaded-capacity exhaustion, shutdown cancellation). Fresh
    /// preparation settles target-root resources, trust, extensions, and
    /// tools before any journal exists; the discard tears the prepared
    /// extension runtime down through its own async shutdown instead of
    /// relying on plain drop.
    pub(crate) async fn discard(self) {
        match self {
            PreparedSessionSpawn::Request(_) => {}
            PreparedSessionSpawn::Fresh(prepared) => prepared.discard().await,
        }
    }
}

/// Output of the fallible runtime preparation phase. Production fresh-session
/// preparation returns `Ready`, so target-root resources, trust, extensions,
/// and tools are settled before loaded capacity is reserved. Test and legacy
/// spawners may retain a request for their infallible spawn phase.
pub enum PreparedSessionSpawn {
    Request(SessionSpawnRequest),
    Fresh(crate::session_run::PreparedFreshSession),
}


/// A fully initialized runtime candidate. `session_id`, `session_file`, and
/// `file_identity` come from one attached, validated append writer. The
/// manager revalidates that writer immediately before registry publication;
/// `journal_ownership` limits rollback deletion to newly-created candidates.
/// `editor_text` is the selected user prompt for Fork / SessionForkLatest
/// only; switch, new, and clone leave it unset so the lifecycle snapshot
/// omits `editorText`.
pub struct SessionSpawnResult {
    pub session_id: String,
    pub session_file: PathBuf,
    pub file_identity: pi_coding::SessionFileIdentity,
    pub application: Application,
    pub extension_ui: ExtensionUiAdapter,
    pub resume_notice: Option<String>,
    pub journal_ownership: SpawnedJournalOwnership,
    pub editor_text: Option<String>,
}

impl SessionSpawnResult {
    pub fn existing(
        session_id: String,
        session_file: PathBuf,
        file_identity: pi_coding::SessionFileIdentity,
        application: Application,
        extension_ui: ExtensionUiAdapter,
        resume_notice: Option<String>,
    ) -> Self {
        Self {
            session_id,
            session_file,
            file_identity,
            application,
            extension_ui,
            resume_notice,
            journal_ownership: SpawnedJournalOwnership::Existing,
            editor_text: None,
        }
    }

    pub fn created(
        session_id: String,
        session_file: PathBuf,
        file_identity: pi_coding::SessionFileIdentity,
        application: Application,
        extension_ui: ExtensionUiAdapter,
        resume_notice: Option<String>,
        editor_text: Option<String>,
    ) -> Self {
        Self {
            session_id,
            session_file,
            file_identity,
            application,
            extension_ui,
            resume_notice,
            journal_ownership: SpawnedJournalOwnership::Created,
            editor_text,
        }
    }
}

/// Exact selected-user bytes returned as a fork draft. Matches in-place
/// `/fork`: concatenate text blocks, including empty, and never treat the
/// selected prompt as a transcript message.
pub(crate) fn fork_selected_editor_text(message: &Message) -> Option<String> {
    let Message::User(user) = message else {
        return None;
    };
    Some(
        user.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect(),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SpawnedJournalOwnership {
    Existing,
    Created,
}

/// Factory for building manager-owned runtimes. Production wires
/// [`crate::session_run::RunSessionSpawner`] (sanitized CLI clone +
/// `session_run::build_session` policy); tests inject a faux implementation.
pub trait SessionSpawner: Send + Sync {
    fn prepare(
        &self,
        request: SessionSpawnRequest,
    ) -> Pin<Box<dyn Future<Output = Result<PreparedSessionSpawn>> + Send>> {
        Box::pin(async move { Ok(PreparedSessionSpawn::Request(request)) })
    }

    fn spawn(
        &self,
        request: SessionSpawnRequest,
    ) -> Pin<Box<dyn Future<Output = Result<SessionSpawnResult>> + Send>>;

    fn finish(
        &self,
        prepared: PreparedSessionSpawn,
    ) -> Pin<Box<dyn Future<Output = Result<SessionSpawnResult>> + Send>> {
        match prepared {
            PreparedSessionSpawn::Request(request) => self.spawn(request),
            PreparedSessionSpawn::Fresh(prepared) => {
                Box::pin(async move { prepared.finish().await })
            }
        }
    }

    /// Explicit async discard of a preparation that never reaches `finish`
    /// (loaded-capacity exhaustion, cooperative shutdown cancellation). The
    /// default tears the prepared spawn down through its own async discard;
    /// the factory owns the teardown because it built the prepared state.
    fn discard_prepared(
        &self,
        prepared: PreparedSessionSpawn,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move { prepared.discard().await })
    }

    /// Registers the terminal-shutdown join for detached rollback fallbacks.
    fn track_detached_cleanups(&self, tracker: crate::session_run::SpawnRollbackTracker) {
        let _ = tracker;
    }

    fn session_storage(&self) -> Option<crate::session_run::SessionStorage> {
        None
    }
    /// Catalog the manager uses for Web session resolution. Production
    /// spawners leave this `None` (the manager derives the catalog from
    /// [`SessionSpawner::session_storage`]); test spawners inject a hermetic
    /// catalog so foreign import/reuse can be exercised without touching the
    /// real home directory.
    fn catalog(&self) -> Option<pi_coding::SessionCatalog> {
        None
    }
    fn todo_child_service(&self) -> Option<Arc<TodoChildService>> {
        None
    }
}

/// One loaded session runtime: independent application, extension UI adapter,
/// dispatcher (per-session Settings/Workflow/SideChat state), and fan-in
/// forwarder.
struct SessionRuntime {
    protocol_id: String,
    runtime_incarnation: String,
    next_sequence: AtomicU64,
    protocol_tx: mpsc::Sender<ProtocolMessage>,
    application: Application,
    extension_ui: ExtensionUiAdapter,
    dispatcher: RpcDispatcher,
    loaded_slot: std::sync::Mutex<Option<OwnedSemaphorePermit>>,
    last_used: AtomicU64,
    owner: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    resume_notice: std::sync::Mutex<Option<String>>,
    lifecycle_lock: Arc<tokio::sync::RwLock<()>>,
    /// Set while an active-move cutover owns the runtime's identity
    /// transition. Event-driven [`SessionRuntimeManager::update_identity`]
    /// calls observe it and skip: the recording writer is transiently
    /// unvalidatable between the core move (source file renamed away) and
    /// the destination binding, and a re-key in that window would treat the
    /// runtime as dead — unregistering it and aborting the owner with the
    /// move command still in flight. The latch clears only when the
    /// cutover's authoritative re-key validates the writer at the
    /// destination.
    identity_rekey_suppressed: std::sync::atomic::AtomicBool,
    owner_stopped: AtomicBool,
    #[cfg(test)]
    cleanup_count: std::sync::atomic::AtomicUsize,
    /// One barrier per in-flight owner command task; terminal shutdown waits
    /// for it to reach zero after aborting the owner (see
    /// [`SessionRuntime::wait_in_flight_commands_dropped`]).
    in_flight_commands: Arc<AtomicUsize>,
}

struct LoadedSlotReservation {
    permit: Option<OwnedSemaphorePermit>,
    victim: Option<(Arc<SessionRuntime>, OwnedRwLockWriteGuard<()>)>,
    is_committed: bool,
}

impl LoadedSlotReservation {
    fn available(permit: OwnedSemaphorePermit) -> Self {
        Self {
            permit: Some(permit),
            victim: None,
            is_committed: false,
        }
    }

    fn replacing(
        permit: OwnedSemaphorePermit,
        victim: Arc<SessionRuntime>,
        lifecycle: OwnedRwLockWriteGuard<()>,
    ) -> Self {
        Self {
            permit: Some(permit),
            victim: Some((victim, lifecycle)),
            is_committed: false,
        }
    }

    fn take_permit(&mut self) -> OwnedSemaphorePermit {
        self.permit.take().expect("loaded slot reservation permit")
    }

    fn commit(mut self) -> Option<(Arc<SessionRuntime>, OwnedRwLockWriteGuard<()>)> {
        self.is_committed = true;
        self.victim.take()
    }
}

impl Drop for LoadedSlotReservation {
    fn drop(&mut self) {
        if self.is_committed {
            return;
        }
        let Some(permit) = self.permit.take() else {
            return;
        };
        match self.victim.as_ref() {
            Some((victim, _)) => victim.restore_loaded_slot(permit),
            None => drop(permit),
        }
    }
}

/// Rollback ownership for one manager-owned creation. The guard tracks the
/// fallible phases plain drop cannot settle: a prepared spawn holds the
/// target's extension runtime, and a finished candidate holds a live
/// application plus a created journal. Every settled path discards
/// explicitly and awaited; the `Drop` fallback exists for cancellation (a
/// terminal shutdown that gave up on the creation past its bounded join
/// deadline) and spawns the same rollback detached, so an abandoned
/// creation can never leak its prepared runtime, journal, or application.
enum CreationRollback {
    Prepared(PreparedSessionSpawn),
    Created(SessionSpawnResult),
}

struct CreationRollbackGuard {
    rollback: Option<CreationRollback>,
    /// Executor captured at construction (the manager-owned creation task
    /// always runs on one): the `Drop` fallback spawns the detached
    /// rollback through this handle — the same executor rule as
    /// `SpawnFinishRollback` — so an abandoned creation settles even when
    /// `Drop` runs outside any runtime context, and the spawned rollback
    /// is tracked for the manager's terminal-shutdown join.
    runtime: tokio::runtime::Handle,
    cleanups: Arc<DetachedCleanups>,
}

impl CreationRollbackGuard {
    fn prepared(prepared: PreparedSessionSpawn, cleanups: Arc<DetachedCleanups>) -> Self {
        Self {
            rollback: Some(CreationRollback::Prepared(prepared)),
            runtime: tokio::runtime::Handle::current(),
            cleanups,
        }
    }

    fn created(&mut self, result: SessionSpawnResult) {
        // The prepared spawn was consumed by `factory.finish` via
        // `take_prepared`; the guard holds the finished candidate from here
        // until registration settles.
        self.rollback = Some(CreationRollback::Created(result));
    }

    /// Take the prepared spawn out: either for the factory's explicit,
    /// awaited discard (slot exhaustion, failed finish before the candidate
    /// exists) or for the `factory.finish` handoff — from that point the
    /// finish future owns the prepared spawn and its internal
    /// cancellation-safe rollback, and the guard never double-owns it.
    /// `None` once consumed or disarmed.
    fn take_prepared(&mut self) -> Option<PreparedSessionSpawn> {
        match self.rollback.take() {
            Some(CreationRollback::Prepared(prepared)) => Some(prepared),
            other => {
                self.rollback = other;
                None
            }
        }
    }

    /// Borrows the finished candidate held for registration. Errors leave
    /// the candidate owned by the guard; only the settled paths disarm it.
    fn result(&self) -> Option<&SessionSpawnResult> {
        match self.rollback.as_ref() {
            Some(CreationRollback::Created(result)) => Some(result),
            _ => None,
        }
    }

    /// Explicit, awaited cleanup of a finished candidate that never
    /// registered: application cleanup plus created-journal rollback.
    /// The candidate stays owned THROUGH both awaits (no take-before-await
    /// gap); the rollback clears only after the pass returned.
    async fn discard_created(&mut self) -> Option<String> {
        let Some(CreationRollback::Created(result)) = self.rollback.as_ref() else {
            return None;
        };
        let cleanup = result.application.cleanup().await;
        let rollback = match result.journal_ownership {
            SpawnedJournalOwnership::Existing => Ok(()),
            SpawnedJournalOwnership::Created => {
                // The identity gate keeps a re-run after partial discard safe.
                if result.application.session().session_file_identity().is_some() {
                    result.application.session().discard_persisted_recording()
                } else {
                    Ok(())
                }
            }
        };
        let failure = match (cleanup, rollback) {
            (Ok(()), Ok(())) => None,
            (cleanup, rollback) => {
                let cleanup = cleanup
                    .err()
                    .map(|failure| failure.to_string())
                    .unwrap_or_else(|| "ok".to_owned());
                let rollback = rollback
                    .err()
                    .map(|failure| failure.to_string())
                    .unwrap_or_else(|| "ok".to_owned());
                Some(format!(
                    "unpublished session rollback failed: runtime={cleanup}; recorder={rollback}"
                ))
            }
        };
        self.rollback = None;
        failure
    }

    fn disarm(&mut self) {
        self.rollback = None;
    }
}

impl Drop for CreationRollbackGuard {
    fn drop(&mut self) {
        let detached: Pin<Box<dyn Future<Output = ()> + Send>> = match self.rollback.take() {
            None => return,
            Some(CreationRollback::Prepared(prepared)) => Box::pin(prepared.discard()),
            Some(CreationRollback::Created(result)) => Box::pin(async move {
                let _ = result.application.cleanup().await;
                if matches!(
                    result.journal_ownership,
                    SpawnedJournalOwnership::Created
                ) {
                    let _ = result
                        .application
                        .session()
                        .discard_persisted_recording();
                }
            }),
        };
        self.cleanups.track(self.runtime.spawn(detached));
    }
}

/// Drop barrier for one in-flight owner command task or one admission
/// token. Terminal shutdown waits for every in-flight command barrier to
/// drop before resolving: an owner abort cancels its command JoinSet, and
/// only the dropped futures release the global slot permits and lifecycle
/// guards a terminal state must not leak. The same RAII shape keeps the
/// admission counter cancellation-safe: a cancelled admission releases its
/// in-flight count even when it is dropped mid-send.
struct InFlightCommandGuard(Arc<AtomicUsize>);

impl InFlightCommandGuard {
    fn acquire(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter.clone())
    }
}

impl Drop for InFlightCommandGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Tracked detached cleanup ownership for the manager: every cleanup task
/// spawned detached (rollback fallbacks, post-close retirements) registers
/// its join handle here so the terminal shutdown can join them under a
/// bounded budget instead of racing process exit with half-settled
/// cleanup work.
#[derive(Default)]
struct DetachedCleanups(std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>);

impl DetachedCleanups {
    fn track(&self, handle: tokio::task::JoinHandle<()>) {
        let mut tasks = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tasks.retain(|handle| !handle.is_finished());
        tasks.push(handle);
    }

    /// Bounded join of every tracked cleanup task. A task that misses the
    /// budget stays detached — its cleanup is best-effort by contract and
    /// can never delay the terminal outcome.
    async fn join_bounded(&self, budget: Duration) {
        let deadline = tokio::time::Instant::now() + budget;
        let tasks: Vec<_> = std::mem::take(
            &mut *self
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for handle in tasks {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                eprintln!("session_shutdown category=cleanup_join_budget_exhausted");
                break;
            }
            if tokio::time::timeout(remaining, handle).await.is_err() {
                eprintln!("session_shutdown category=cleanup_join_timeout");
            }
        }
    }
}

enum RuntimeLifecycleCommand {
    Create {
        kind: SessionSpawnKind,
        source: Application,
        reply: oneshot::Sender<Result<(Arc<SessionRuntime>, Option<String>)>>,
    },
    Shutdown {
        is_restart: bool,
        reply: oneshot::Sender<Result<()>>,
    },
}

#[derive(Default)]
struct RuntimeRegistry {
    by_id: HashMap<String, Arc<SessionRuntime>>,
    by_file: HashMap<pi_coding::SessionFileIdentity, Arc<SessionRuntime>>,
}

impl RuntimeRegistry {
    fn contains(&self, runtime: &Arc<SessionRuntime>) -> bool {
        self.by_id
            .values()
            .any(|candidate| Arc::ptr_eq(candidate, runtime))
    }

    fn remove(&mut self, runtime: &Arc<SessionRuntime>) {
        self.by_id
            .retain(|_, candidate| !Arc::ptr_eq(candidate, runtime));
        self.by_file
            .retain(|_, candidate| !Arc::ptr_eq(candidate, runtime));
    }

    fn distinct_runtimes(&self) -> Vec<Arc<SessionRuntime>> {
        let mut seen = HashSet::new();
        self.by_id
            .values()
            .filter(|runtime| seen.insert(Arc::as_ptr(runtime) as usize))
            .cloned()
            .collect()
    }
}

/// Sequenced Agent events may arrive before their runtime epoch authority.
/// The queue is bounded and never evicts: overflow triggers one authoritative
/// resnapshot/resync instead of losing identity-bearing events.
const MAX_PENDING_FUTURE_EPOCH_EVENTS: usize = 128;

struct ProtocolState {
    epoch: u64,
    next_sequence: u64,
    replay: VecDeque<Value>,
    replay_bytes: usize,
    command_ledger_bytes: usize,
    forwarded_agent_event_barrier: u64,
    /// Sequenced Agent events whose runtime epoch is newer than the current
    /// protocol epoch. Every matching event is retained in arrival order
    /// until its exact `RuntimeChanged` authority is consumed; overflow must
    /// resync rather than drop a genuinely new event.
    pending_future_epoch_events: VecDeque<pi_coding::ApplicationEvent>,
    /// Set when the bounded future-epoch queue cannot accept another event.
    /// The protocol owner converts this into one authoritative resnapshot
    /// and never evicts queued events.
    pending_future_epoch_overflow: bool,
    /// Newest epoch admitted through the runtime broadcast queue during a
    /// snapshot attempt. This differs from `epoch` (the protocol cut epoch)
    /// only while the drain has consumed a queued `RuntimeChanged` and the
    /// outer owner restarts with the fresh authoritative snapshot.
    observed_epoch: u64,
    active_turn_id: Option<String>,
    active_turn_model: Option<Model>,
    active_assistant_message_id: Option<String>,
    application_lifecycle_revision: u64,
    next_message_ordinal: u64,
    commands: HashMap<String, CommandLedgerEntry>,
    command_order: VecDeque<String>,
    expired_commands: HashMap<String, [u8; 32]>,
    expired_command_order: VecDeque<String>,
    ordinary_queue: VecDeque<OwnedCommand>,
    ordinary_active: bool,
}

impl ProtocolState {
    fn new(epoch: u64) -> Self {
        Self {
            epoch,
            next_sequence: 0,
            replay: VecDeque::with_capacity(EVENT_REPLAY_CAPACITY),
            replay_bytes: 0,
            command_ledger_bytes: 0,
            forwarded_agent_event_barrier: 0,
            pending_future_epoch_events: VecDeque::new(),
            pending_future_epoch_overflow: false,
            observed_epoch: epoch,
            active_turn_id: None,
            active_turn_model: None,
            active_assistant_message_id: None,
            application_lifecycle_revision: 0,
            next_message_ordinal: 0,
            commands: HashMap::new(),
            command_order: VecDeque::new(),
            expired_commands: HashMap::new(),
            expired_command_order: VecDeque::new(),
            ordinary_queue: VecDeque::with_capacity(MAX_CONCURRENT_COMMANDS),
            ordinary_active: false,
        }
    }
}

struct CommandLedgerEntry {
    digest: [u8; 32],
    response: Option<Arc<RpcResponse>>,
    encoded_bytes: usize,
    outcome: watch::Sender<Option<Arc<RpcResponse>>>,
}

enum CommandReservation {
    Execute(CommandCompletion, CommandResponseWaiter),
    Replay(Arc<RpcResponse>),
    Join(watch::Receiver<Option<Arc<RpcResponse>>>),
    Conflict,
    Capacity,
    Expired,
    InvalidId,
}

struct CommandCompletion {
    id: Option<String>,
    ephemeral: Option<watch::Sender<Option<Arc<RpcResponse>>>>,
}

struct OwnedCommand {
    command: RpcCommand,
    use_per_session_slots: bool,
    completion: CommandCompletion,
    _lifecycle: OwnedRwLockReadGuard<()>,
    _global_slot: OwnedSemaphorePermit,
}

enum ProtocolMessage {
    AdmitOrdinary {
        command: RpcCommand,
        digest: [u8; 32],
        use_per_session_slots: bool,
        reply: oneshot::Sender<CommandResponseWaiter>,
        lifecycle: OwnedRwLockReadGuard<()>,
    },
    AdmitDirect {
        command: RpcCommand,
        digest: [u8; 32],
        manager_level: bool,
        use_per_session_slots: bool,
        lifecycle_response_mode: LifecycleResponseMode,
        reply: oneshot::Sender<CommandResponseWaiter>,
        lifecycle: OwnedRwLockReadGuard<()>,
    },
    Snapshot {
        cursor: u64,
        replay_from_zero: bool,
        cancel: CancellationToken,
        reply: oneshot::Sender<Result<Option<OwnedProtocolSnapshot>>>,
    },
    #[cfg(test)]
    EnvelopeTestEvent {
        event: pi_coding::ApplicationEvent,
        reply: oneshot::Sender<Option<Value>>,
    },
    #[cfg(test)]
    SetApplicationEventDrain {
        enabled: bool,
        reply: oneshot::Sender<()>,
    },
}

struct ProtocolCut {
    watermark: u64,
    has_gap: bool,
    events: Vec<Value>,
}

struct OwnedProtocolSnapshot {
    application: pi_coding::ApplicationProtocolSnapshot,
    cut: ProtocolCut,
}

pub(crate) struct CommandResponseWaiter {
    outcome: watch::Receiver<Option<Arc<RpcResponse>>>,
    id: Option<String>,
    name: String,
    response_owner: String,
}

impl CommandResponseWaiter {
    fn ready(response: RpcResponse) -> Self {
        let id = response.id.clone();
        let name = response.command.clone();
        let response_owner = response.session_id.clone().unwrap_or_default();
        let (_outcome_tx, outcome) = watch::channel(Some(Arc::new(response)));
        Self {
            outcome,
            id,
            name,
            response_owner,
        }
    }

    fn pending(
        outcome: watch::Receiver<Option<Arc<RpcResponse>>>,
        id: Option<String>,
        name: impl Into<String>,
        response_owner: String,
    ) -> Self {
        Self {
            outcome,
            id,
            name: name.into(),
            response_owner,
        }
    }

    pub(crate) async fn wait(mut self) -> RpcResponse {
        loop {
            if let Some(response) = self.outcome.borrow().clone() {
                return (*response).clone();
            }
            if self.outcome.changed().await.is_err() {
                return command_ledger_failure(
                    self.id,
                    &self.name,
                    &self.response_owner,
                    "command outcome is unavailable",
                );
            }
        }
    }
}

struct OwnerTaskCompletion {
    completion: CommandCompletion,
    response: RpcResponse,
    ordinary: bool,
    stop_owner: bool,
}

impl SessionRuntime {
    fn new(
        application: Application,
        extension_ui: ExtensionUiAdapter,
        storage: Option<crate::session_run::SessionStorage>,
        resume_notice: Option<String>,
        loaded_slot: Option<OwnedSemaphorePermit>,
    ) -> Result<(Self, mpsc::Receiver<ProtocolMessage>)> {
        let (protocol_id, _, _) = application.session().recording_writer_info()?;
        let dispatcher = RpcDispatcher::new_with_session_storage(application.clone(), storage);
        let (protocol_tx, protocol_rx) = mpsc::channel(PROTOCOL_MAILBOX_CAPACITY);
        Ok((
            Self {
                protocol_id,
                runtime_incarnation: uuid::Uuid::now_v7().to_string(),
                next_sequence: AtomicU64::new(0),
                protocol_tx,
                application,
                extension_ui,
                dispatcher,
                loaded_slot: std::sync::Mutex::new(loaded_slot),
                last_used: AtomicU64::new(0),
                owner: std::sync::Mutex::new(None),
                resume_notice: std::sync::Mutex::new(resume_notice),
                identity_rekey_suppressed: std::sync::atomic::AtomicBool::new(false),
                owner_stopped: AtomicBool::new(false),
                lifecycle_lock: Arc::new(tokio::sync::RwLock::new(())),
                #[cfg(test)]
                cleanup_count: std::sync::atomic::AtomicUsize::new(0),
                in_flight_commands: Arc::new(AtomicUsize::new(0)),
            },
            protocol_rx,
        ))
    }

    fn current_identity(&self) -> Option<(String, PathBuf)> {
        self.application.session().recorder_info()
    }

    fn current_file_identity(&self) -> Option<pi_coding::SessionFileIdentity> {
        self.application.session().session_file_identity()
    }

    fn protocol_id(&self) -> &str {
        &self.protocol_id
    }

    fn runtime_incarnation(&self) -> &str {
        &self.runtime_incarnation
    }

    fn identity(&self) -> RuntimeIdentity {
        RuntimeIdentity::new(self.protocol_id.clone(), self.runtime_incarnation.clone())
    }

    fn published_sequence(&self) -> u64 {
        self.next_sequence.load(Ordering::Acquire)
    }

    fn publish_sequence(&self, sequence: u64) {
        self.next_sequence.store(sequence, Ordering::Release);
    }

    fn mark_used(&self, sequence: u64) {
        self.last_used.store(sequence, Ordering::Relaxed);
    }

    fn last_used(&self) -> u64 {
        self.last_used.load(Ordering::Relaxed)
    }

    fn take_loaded_slot(&self) -> Option<OwnedSemaphorePermit> {
        self.loaded_slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    fn restore_loaded_slot(&self, permit: OwnedSemaphorePermit) {
        let mut slot = self
            .loaded_slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        debug_assert!(slot.is_none());
        *slot = Some(permit);
    }

    fn start_owner(
        manager: &Arc<SessionRuntimeManager>,
        runtime: &Arc<Self>,
        mut protocol_rx: mpsc::Receiver<ProtocolMessage>,
        events: broadcast::Sender<Value>,
        ui_events: broadcast::Sender<RpcExtensionUiRequest>,
        ready: oneshot::Sender<()>,
    ) {
        let mut application_events = runtime.application.subscribe();
        let mut ui_events_rx = runtime.extension_ui.subscribe();
        let manager = Arc::downgrade(manager);
        let runtime_for_task = runtime.clone();
        let initial_epoch = runtime.application.runtime_epoch();
        let handle = tokio::spawn(async move {
            let mut owner_exit = OwnerExitGuard {
                runtime: runtime_for_task.clone(),
                reason: OwnerStopReason::Aborted,
            };
            let mut state = ProtocolState::new(initial_epoch);
            let mut commands = tokio::task::JoinSet::<OwnerTaskCompletion>::new();
            let mut application_event_drain_enabled = true;
            let _ = ready.send(());
            loop {
                tokio::select! {
                    message = protocol_rx.recv() => {
                        let Some(message) = message else {
                            owner_exit.reason = OwnerStopReason::ProtocolChannelClosed;
                            break;
                        };
                        let Some(manager) = manager.upgrade() else {
                            owner_exit.reason = OwnerStopReason::ManagerDropped;
                            break;
                        };
                        match message {
                            ProtocolMessage::AdmitOrdinary { command, digest, use_per_session_slots, lifecycle, reply } => {
                                let waiter = admit_owned_command(
                                    &manager, &runtime_for_task, &mut state, &mut commands,
                                    command, digest, use_per_session_slots, lifecycle,
                                );
                                let _ = reply.send(waiter);
                            }
                            ProtocolMessage::AdmitDirect { command, digest, manager_level, use_per_session_slots, lifecycle_response_mode, lifecycle, reply } => {
                                let waiter = admit_direct_command(
                                    &manager, &runtime_for_task, &mut state, &mut commands,
                                    command, digest, manager_level, use_per_session_slots,
                                    lifecycle_response_mode, lifecycle,
                                );
                                let _ = reply.send(waiter);
                            }
                            ProtocolMessage::Snapshot { cursor, replay_from_zero, cancel, reply } => {
                                if cancel.is_cancelled() || reply.is_closed() {
                                    let _ = reply.send(Err(anyhow!("subscription snapshot cancelled")));
                                    continue;
                                }
                                // One absolute deadline for the whole attempt:
                                // queued pulls, lagged retries, epoch retries,
                                // and receives all share it.
                                let attempt_deadline = tokio::time::Instant::now()
                                    + SNAPSHOT_BARRIER_ATTEMPT_DEADLINE;
                                let cut = 'snapshot: loop {
                                    if cancel.is_cancelled() || reply.is_closed() {
                                        break 'snapshot Err(anyhow!("subscription snapshot cancelled"));
                                    }
                                    #[cfg(test)]
                                    manager.protocol_snapshot_count.fetch_add(1, Ordering::SeqCst);
                                    let snapshot = runtime_for_task.application.protocol_snapshot().await;
                                    match drain_snapshot_barrier(
                                        &runtime_for_task,
                                        &mut state,
                                        &mut application_events,
                                        &events,
                                        &snapshot,
                                        attempt_deadline,
                                    )
                                    .await
                                    {
                                        SnapshotDrainOutcome::Settled => {}
                                        SnapshotDrainOutcome::Restart => continue 'snapshot,
                                        SnapshotDrainOutcome::PendingEpochOverflow => {
                                            if cancel.is_cancelled() || reply.is_closed() {
                                                break 'snapshot Err(anyhow!("subscription snapshot cancelled"));
                                            }
                                            #[cfg(test)]
                                            manager.protocol_snapshot_count.fetch_add(1, Ordering::SeqCst);
                                            let snapshot = runtime_for_task.application.protocol_snapshot().await;
                                            state.pending_future_epoch_overflow = false;
                                            state.pending_future_epoch_events.retain(|event| match event {
                                                pi_coding::ApplicationEvent::AgentPublished { epoch, barrier, .. } => {
                                                    *epoch > snapshot.epoch
                                                        || (*epoch == snapshot.epoch
                                                            && *barrier > snapshot.event_barrier)
                                                }
                                                _ => false,
                                            });
                                            let payload = push_session_resync(
                                                &runtime_for_task, &mut state, &snapshot,
                                            );
                                            let _ = events.send(payload);
                                            continue 'snapshot;
                                        }
                                        SnapshotDrainOutcome::Resync => {
                                            if cancel.is_cancelled() || reply.is_closed() {
                                                break 'snapshot Err(anyhow!("subscription snapshot cancelled"));
                                            }
                                            #[cfg(test)]
                                            manager.protocol_snapshot_count.fetch_add(1, Ordering::SeqCst);
                                            let snapshot = runtime_for_task.application.protocol_snapshot().await;
                                            let payload = push_session_resync(
                                                &runtime_for_task, &mut state, &snapshot,
                                            );
                                            let _ = events.send(payload);
                                            continue 'snapshot;
                                        }
                                        SnapshotDrainOutcome::Unavailable(error) => {
                                            break 'snapshot Err(error);
                                        }
                                    }
                                    if cancel.is_cancelled() || reply.is_closed() {
                                        break 'snapshot Err(anyhow!("subscription snapshot cancelled"));
                                    }
                                    manager.update_identity(&runtime_for_task).await;
                                    if let Some(cut) = protocol_snapshot_cut(
                                        &runtime_for_task, &mut state, snapshot, cursor, replay_from_zero,
                                    ) {
                                        break Ok(cut);
                                    }
                                };
                                if cancel.is_cancelled() || reply.is_closed() {
                                    let _ = reply.send(Err(anyhow!("subscription snapshot cancelled")));
                                } else {
                                    let _ = reply.send(cut.map(Some));
                                }
                            }
                            #[cfg(test)]
                            ProtocolMessage::EnvelopeTestEvent { event, reply } => {
                                let payload = envelope_application_event(
                                    &runtime_for_task, &mut state, event,
                                );
                                let _ = reply.send(payload);
                            }
                            #[cfg(test)]
                            ProtocolMessage::SetApplicationEventDrain { enabled, reply } => {
                                application_event_drain_enabled = enabled;
                                let _ = reply.send(());
                            }
                        }
                    }
                    completion = commands.join_next(), if !commands.is_empty() => {
                        let Some(Ok(completion)) = completion else { continue };
                        let should_stop = completion.stop_owner && completion.response.success;
                        if completion.response.is_transient_slot_overload() {
                            remove_owned_command(&mut state, completion.completion, completion.response);
                        } else {
                            complete_owned_command(&mut state, completion.completion, completion.response);
                        }
                        if should_stop {
                            owner_exit.reason = OwnerStopReason::StopCommand;
                            break;
                        }
                        if completion.ordinary {
                            state.ordinary_active = false;
                            let Some(manager) = manager.upgrade() else {
                                owner_exit.reason = OwnerStopReason::ManagerDropped;
                                break;
                            };
                            start_next_ordinary(&manager, &runtime_for_task, &mut state, &mut commands);
                        }
                    }
                    event = application_events.recv(), if application_event_drain_enabled => {
                        let Some(manager) = manager.upgrade() else {
                            owner_exit.reason = OwnerStopReason::ManagerDropped;
                            break;
                        };
                        let mut payloads: Vec<Value> = Vec::new();
                        match event {
                            Ok(event) => {
                                manager.update_identity(&runtime_for_task).await;
                                let epoch_before = state.epoch;
                                if let Some(payload) = envelope_application_event(
                                    &runtime_for_task, &mut state, event,
                                ) {
                                    payloads.push(payload);
                                }
                                // Only an authoritative RuntimeChanged that
                                // advanced the protocol epoch may admit queued
                                // events. Never drain after a future-epoch
                                // Agent itself: that event is still waiting on
                                // its epoch and would be requeued forever.
                                if state.epoch > epoch_before {
                                    let mut deferred = VecDeque::new();
                                    while let Some(pending) = state.pending_future_epoch_events.pop_front() {
                                        match &pending {
                                            pi_coding::ApplicationEvent::AgentPublished { epoch, .. }
                                                if *epoch > state.epoch =>
                                            {
                                                deferred.push_back(pending);
                                            }
                                            _ => {
                                                if let Some(payload) = envelope_application_event(
                                                    &runtime_for_task, &mut state, pending,
                                                ) {
                                                    payloads.push(payload);
                                                }
                                            }
                                        }
                                    }
                                    state.pending_future_epoch_events = deferred;
                                }
                                if state.pending_future_epoch_overflow {
                                    #[cfg(test)]
                                    manager.protocol_snapshot_count.fetch_add(1, Ordering::SeqCst);
                                    let snapshot = runtime_for_task.application.protocol_snapshot().await;
                                    state.pending_future_epoch_overflow = false;
                                    state.pending_future_epoch_events.retain(|event| match event {
                                        pi_coding::ApplicationEvent::AgentPublished { epoch, barrier, .. } => {
                                            *epoch > snapshot.epoch
                                                || (*epoch == snapshot.epoch
                                                    && *barrier > snapshot.event_barrier)
                                        }
                                        _ => false,
                                    });
                                    payloads.push(push_session_resync(
                                        &runtime_for_task, &mut state, &snapshot,
                                    ));
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                #[cfg(test)]
                                manager.protocol_snapshot_count.fetch_add(1, Ordering::SeqCst);
                                let snapshot = runtime_for_task.application.protocol_snapshot().await;
                                payloads.push(push_session_resync(
                                    &runtime_for_task,
                                    &mut state,
                                    &snapshot,
                                ));
                            }
                            Err(broadcast::error::RecvError::Closed) => {
                                owner_exit.reason = OwnerStopReason::ApplicationEventsClosed;
                                break;
                            }
                        };
                        let mut sent = true;
                        for payload in payloads {
                            if events.send(payload).is_err() {
                                sent = false;
                                break;
                            }
                        }
                        if !sent {
                            owner_exit.reason = OwnerStopReason::EventFanoutClosed;
                            break;
                        }
                    }
                    event = ui_events_rx.recv() => {
                        let payload = match event {
                            Ok(event) if host_owned_ui_event(&event) => None,
                            Ok(event) => match project_extension_ui_event(event) {
                                Ok(Some(mut request)) => {
                                    request.session_id = Some(runtime_for_task.protocol_id().to_owned());
                                    request.runtime_incarnation = Some(runtime_for_task.runtime_incarnation().to_owned());
                                    Some(request)
                                }
                                Ok(None) => None,
                                Err(error) => Some(RpcExtensionUiRequest::error_notice_with_identity(
                                    Some(runtime_for_task.identity()),
                                    format!("failed to project extension UI event: {error}"),
                                )),
                            },
                            Err(broadcast::error::RecvError::Lagged(count)) => Some(
                                RpcExtensionUiRequest::error_notice_with_identity(
                                    Some(runtime_for_task.identity()),
                                    format!("extension UI event stream lagged by {count} records"),
                                ),
                            ),
                            Err(broadcast::error::RecvError::Closed) => {
                                owner_exit.reason = OwnerStopReason::UiFanoutClosed;
                                break;
                            }
                        };
                        if let Some(payload) = payload
                            && ui_events.send(payload).is_err()
                        {
                            owner_exit.reason = OwnerStopReason::UiFanoutClosed;
                            break;
                        }
                    }
                }
            }
            commands.abort_all();
            if let Some(manager) = manager.upgrade()
                && !Arc::ptr_eq(&runtime_for_task, &manager.primary)
            {
                let mut registry = manager.registry.write().await;
                registry.remove(&runtime_for_task);
            }
        });
        *runtime.owner.lock().expect("runtime owner lock") = Some(handle);
    }

    fn mark_owner_stopped(&self) {
        self.owner_stopped.store(true, Ordering::Release);
    }

    fn abort_owner(&self) {
        if let Some(handle) = self.owner.lock().expect("runtime owner lock").take() {
            self.mark_owner_stopped();
            handle.abort();
        }
    }

    async fn finish_owner(&self, abort_first: bool) -> Result<()> {
        let handle = self.owner.lock().expect("runtime owner lock").take();
        let Some(mut handle) = handle else {
            return Ok(());
        };
        self.mark_owner_stopped();
        if abort_first {
            handle.abort();
        }
        match tokio::time::timeout(RETIREMENT_JOIN_TIMEOUT, &mut handle).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) if error.is_cancelled() => Ok(()),
            Ok(Err(error)) => Err(anyhow!("session protocol owner join failed: {error}")),
            Err(_) => {
                handle.abort();
                let _ = tokio::time::timeout(RETIREMENT_JOIN_TIMEOUT, handle).await;
                Err(anyhow!("session protocol owner join timed out"))
            }
        }
    }

    /// Bounded in-flight command drop barrier. After the owner is aborted
    /// and joined, its in-flight command tasks unwind asynchronously; the
    /// terminal shutdown resolves only once every barrier dropped (the
    /// global slot permit and lifecycle guard were released) or this
    /// bounded deadline elapses.
    async fn wait_in_flight_commands_dropped(&self) {
        let deadline = tokio::time::Instant::now() + COMMAND_DROP_BARRIER_TIMEOUT;
        while self.in_flight_commands.load(Ordering::SeqCst) > 0 {
            if tokio::time::Instant::now() >= deadline {
                eprintln!("session_shutdown category=in_flight_command_drop_barrier_timeout");
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    async fn protocol_snapshot(
        &self,
        cursor: u64,
        replay_from_zero: bool,
        cancel: CancellationToken,
    ) -> Result<OwnedProtocolSnapshot> {
        loop {
            if cancel.is_cancelled() {
                bail!("subscription snapshot cancelled");
            }
            let (reply, response) = oneshot::channel();
            self.protocol_tx
                .send(ProtocolMessage::Snapshot {
                    cursor,
                    replay_from_zero,
                    cancel: cancel.clone(),
                    reply,
                })
                .await
                .map_err(|_| anyhow!(RuntimeUnavailable::owner_stopped()))?;
            if let Some(snapshot) = response
                .await
                .map_err(|_| anyhow!(RuntimeUnavailable::owner_stopped()))??
            {
                return Ok(snapshot);
            }
        }
    }

    #[cfg(test)]
    async fn envelope_test_event(&self, event: pi_coding::ApplicationEvent) -> Option<Value> {
        let (reply, response) = oneshot::channel();
        self.protocol_tx
            .send(ProtocolMessage::EnvelopeTestEvent { event, reply })
            .await
            .expect("session protocol owner");
        response.await.expect("protocol event response")
    }

    #[cfg(test)]
    async fn set_application_event_drain_for_test(&self, enabled: bool) {
        let (reply, response) = oneshot::channel();
        self.protocol_tx
            .send(ProtocolMessage::SetApplicationEventDrain { enabled, reply })
            .await
            .expect("session protocol owner");
        response.await.expect("application event drain gate");
    }
}
fn existing_command_reservation(
    state: &ProtocolState,
    id: &str,
    digest: [u8; 32],
) -> Option<CommandReservation> {
    if let Some(entry) = state.commands.get(id) {
        return Some(if entry.digest != digest {
            CommandReservation::Conflict
        } else if let Some(response) = entry.response.clone() {
            CommandReservation::Replay(response)
        } else {
            CommandReservation::Join(entry.outcome.subscribe())
        });
    }
    state.expired_commands.get(id).map(|expired_digest| {
        if *expired_digest == digest {
            CommandReservation::Expired
        } else {
            CommandReservation::Conflict
        }
    })
}
#[cfg(test)]
fn test_ledger_state() -> ProtocolState {
    ProtocolState::new(0)
}


fn retain_expired_command(state: &mut ProtocolState, id: String, digest: [u8; 32]) {
    while state.expired_commands.len() >= COMMAND_LEDGER_CAPACITY {
        let Some(expired) = state.expired_command_order.pop_front() else {
            break;
        };
        state.expired_commands.remove(&expired);
    }
    state.expired_command_order.push_back(id.clone());
    state.expired_commands.insert(id, digest);
}

fn evict_completed_ledger_entry(state: &mut ProtocolState) -> bool {
    let Some(position) = state.command_order.iter().position(|candidate| {
        state
            .commands
            .get(candidate)
            .is_some_and(|entry| entry.response.is_some())
    }) else {
        return false;
    };
    let expired = state.command_order.remove(position).expect("ledger position");
    if let Some(entry) = state.commands.remove(&expired) {
        state.command_ledger_bytes = state
            .command_ledger_bytes
            .saturating_sub(entry.encoded_bytes);
        retain_expired_command(state, expired, entry.digest);
    }
    true
}

fn reserve_new_command(
    state: &mut ProtocolState,
    id: Option<&str>,
    digest: [u8; 32],
    name: &'static str,
    response_owner: &str,
) -> CommandReservation {
    if id.is_some_and(|id| id.len() > MAX_COMMAND_LEDGER_ID_BYTES) {
        return CommandReservation::InvalidId;
    }
    if let Some(id) = id {
        while state.commands.len() >= COMMAND_LEDGER_CAPACITY
            || state.command_ledger_bytes >= COMMAND_LEDGER_BYTE_BUDGET
        {
            if !evict_completed_ledger_entry(state) {
                return CommandReservation::Capacity;
            }
        }
        let (outcome, receiver) = watch::channel(None);
        state.commands.insert(
            id.to_owned(),
            CommandLedgerEntry {
                digest,
                response: None,
                encoded_bytes: 0,
                outcome,
            },
        );
        state.command_order.push_back(id.to_owned());
        return CommandReservation::Execute(
            CommandCompletion {
                id: Some(id.to_owned()),
                ephemeral: None,
            },
            CommandResponseWaiter::pending(
                receiver,
                Some(id.to_owned()),
                name,
                response_owner.to_owned(),
            ),
        );
    }
    let (outcome, receiver) = watch::channel(None);
    CommandReservation::Execute(
        CommandCompletion {
            id: None,
            ephemeral: Some(outcome),
        },
        CommandResponseWaiter::pending(receiver, None, name, response_owner.to_owned()),
    )
}

fn reservation_response(
    reservation: CommandReservation,
    id: Option<String>,
    name: &'static str,
    response_owner: &str,
) -> std::result::Result<(CommandCompletion, CommandResponseWaiter), CommandResponseWaiter> {
    match reservation {
        CommandReservation::Execute(completion, waiter) => Ok((completion, waiter)),
        CommandReservation::Replay(response) => {
            Err(CommandResponseWaiter::ready((*response).clone()))
        }
        CommandReservation::Join(outcome) => Err(CommandResponseWaiter::pending(
            outcome,
            id,
            name,
            response_owner.to_owned(),
        )),
        CommandReservation::Conflict => Err(CommandResponseWaiter::ready(command_ledger_failure(
            id,
            name,
            response_owner,
            "command id was already used for a different request",
        ))),
        CommandReservation::Capacity => Err(CommandResponseWaiter::ready(command_ledger_failure(
            id,
            name,
            response_owner,
            "command ledger is full with in-flight commands",
        ))),
        CommandReservation::Expired => Err(CommandResponseWaiter::ready(command_ledger_failure(
            id,
            name,
            response_owner,
            "command result expired; issue a new command id",
        ))),
        CommandReservation::InvalidId => Err(CommandResponseWaiter::ready(command_ledger_failure(
            id,
            name,
            response_owner,
            "command id exceeds 1024-byte limit",
        ))),
    }
}

fn admit_owned_command(
    manager: &Arc<SessionRuntimeManager>,
    runtime: &Arc<SessionRuntime>,
    state: &mut ProtocolState,
    commands: &mut tokio::task::JoinSet<OwnerTaskCompletion>,
    command: RpcCommand,
    digest: [u8; 32],
    use_per_session_slots: bool,
    lifecycle: OwnedRwLockReadGuard<()>,
) -> CommandResponseWaiter {
    let id = command.id();
    let name = command.command_name();
    let response_owner = runtime.protocol_id();
    if let Some(id) = id.as_deref()
        && let Some(reservation) = existing_command_reservation(state, id, digest)
    {
        return match reservation_response(reservation, command.id(), name, response_owner) {
            Ok(_) => unreachable!("existing reservation cannot execute"),
            Err(waiter) => waiter,
        };
    }
    if state.ordinary_queue.len() + usize::from(state.ordinary_active) >= MAX_CONCURRENT_COMMANDS {
        return CommandResponseWaiter::ready(
            RpcResponse::slot_overload(
                id,
                name,
                format!("too many concurrent RPC commands (limit {MAX_CONCURRENT_COMMANDS})"),
            )
            .with_session_id(response_owner),
        );
    }
    let Ok(global_slot) = manager.global_slots.clone().try_acquire_owned() else {
        return CommandResponseWaiter::ready(
            RpcResponse::slot_overload(
                id, name,
                format!(
                    "too many concurrent RPC commands across sessions (limit {MAX_CONCURRENT_SESSION_COMMANDS})"
                ),
            )
            .with_session_id(response_owner),
        );
    };
    let reservation = reserve_new_command(state, id.as_deref(), digest, name, response_owner);
    let (completion, waiter) = match reservation_response(reservation, id, name, response_owner) {
        Ok(admission) => admission,
        Err(waiter) => return waiter,
    };
    state.ordinary_queue.push_back(OwnedCommand {
        command,
        use_per_session_slots,
        completion,
        _lifecycle: lifecycle,
        _global_slot: global_slot,
    });
    start_next_ordinary(manager, runtime, state, commands);
    waiter
}

fn start_next_ordinary(
    manager: &Arc<SessionRuntimeManager>,
    runtime: &Arc<SessionRuntime>,
    state: &mut ProtocolState,
    commands: &mut tokio::task::JoinSet<OwnerTaskCompletion>,
) {
    if state.ordinary_active {
        return;
    }
    let Some(owned) = state.ordinary_queue.pop_front() else {
        return;
    };
    state.ordinary_active = true;
    let manager = manager.clone();
    let runtime = runtime.clone();
    commands.spawn(async move {
        let _in_flight = InFlightCommandGuard::acquire(&runtime.in_flight_commands);
        let OwnedCommand {
            command,
            use_per_session_slots,
            completion,
            _lifecycle,
            _global_slot,
        } = owned;
        let response = manager
            .execute_target(&runtime, command, use_per_session_slots)
            .await;
        drop(_global_slot);
        drop(_lifecycle);
        OwnerTaskCompletion {
            completion,
            response,
            ordinary: true,
            stop_owner: false,
        }
    });
}

fn admit_direct_command(
    manager: &Arc<SessionRuntimeManager>,
    runtime: &Arc<SessionRuntime>,
    state: &mut ProtocolState,
    commands: &mut tokio::task::JoinSet<OwnerTaskCompletion>,
    command: RpcCommand,
    digest: [u8; 32],
    manager_level: bool,
    use_per_session_slots: bool,
    lifecycle_response_mode: LifecycleResponseMode,
    lifecycle: OwnedRwLockReadGuard<()>,
) -> CommandResponseWaiter {
    let id = command.id();
    let name = command.command_name();
    let response_owner = runtime.protocol_id();
    if let Some(id) = id.as_deref()
        && let Some(reservation) = existing_command_reservation(state, id, digest)
    {
        return match reservation_response(reservation, command.id(), name, response_owner) {
            Ok(_) => unreachable!("existing reservation cannot execute"),
            Err(waiter) => waiter,
        };
    }
    let bounded_control = !manager_level && !command.bypasses_command_slots();
    let global_slot = if bounded_control {
        match manager.global_slots.clone().try_acquire_owned() {
            Ok(slot) => Some(slot),
            Err(_) => {
                return CommandResponseWaiter::ready(
                    RpcResponse::slot_overload(
                        id,
                        name,
                        format!(
                            "too many concurrent RPC commands across sessions (limit {MAX_CONCURRENT_SESSION_COMMANDS})"
                        ),
                    )
                    .with_session_id(response_owner),
                );
            }
        }
    } else {
        None
    };
    let reservation = reserve_new_command(state, id.as_deref(), digest, name, response_owner);
    let (completion, waiter) = match reservation_response(reservation, id, name, response_owner) {
        Ok(admission) => admission,
        Err(waiter) => return waiter,
    };
    let stop_owner = manager_level && matches!(command, RpcCommand::CloseSession { .. });
    let manager = manager.clone();
    let runtime = runtime.clone();
    commands.spawn(async move {
        let _in_flight = InFlightCommandGuard::acquire(&runtime.in_flight_commands);
        // close_session takes the lifecycle write lock on its OWN runtime;
        // holding our admission read guard here would self-conflict. Every
        // other command keeps the shared guard through completion so LRU
        // retirement can never race it.
        let _lifecycle = (!stop_owner).then_some(lifecycle);
        let response = if manager_level {
            manager
                .execute_manager_level(&runtime, command, lifecycle_response_mode)
                .await
        } else {
            manager
                .execute_target(&runtime, command, use_per_session_slots || bounded_control)
                .await
        };
        drop(global_slot);
        OwnerTaskCompletion {
            completion,
            response,
            ordinary: false,
            stop_owner,
        }
    });
    waiter
}

fn complete_owned_command(
    state: &mut ProtocolState,
    completion: CommandCompletion,
    response: RpcResponse,
) {
    let retain_transcript = response
        .data
        .as_ref()
        .is_some_and(|data| data.get("messages").is_some());
    if retain_transcript {
        expire_owned_command(state, completion, response);
        return;
    }
    let encoded_bytes = serde_json::to_string(&response)
        .map(|encoded| encoded.len())
        .unwrap_or(0);
    if encoded_bytes > COMMAND_LEDGER_BYTE_BUDGET {
        expire_owned_command(state, completion, response);
        return;
    }
    while state.command_ledger_bytes.saturating_add(encoded_bytes) > COMMAND_LEDGER_BYTE_BUDGET {
        if !evict_completed_ledger_entry(state) {
            break;
        }
    }
    let shared = Arc::new(response);
    if let Some(id) = completion.id
        && let Some(entry) = state.commands.get_mut(&id)
    {
        state.command_ledger_bytes = state
            .command_ledger_bytes
            .saturating_add(encoded_bytes);
        entry.encoded_bytes = encoded_bytes;
        entry.response = Some(shared.clone());
        entry.outcome.send_replace(Some(shared));
    } else if let Some(outcome) = completion.ephemeral {
        outcome.send_replace(Some(shared));
    }
}

fn expire_owned_command(
    state: &mut ProtocolState,
    completion: CommandCompletion,
    response: RpcResponse,
) {
    if let Some(id) = completion.id {
        if let Some(entry) = state.commands.remove(&id) {
            state.command_ledger_bytes = state
                .command_ledger_bytes
                .saturating_sub(entry.encoded_bytes);
            let digest = entry.digest;
            entry.outcome.send_replace(Some(Arc::new(response)));
            if let Some(position) = state.command_order.iter().position(|candidate| candidate == &id) {
                state.command_order.remove(position);
            }
            retain_expired_command(state, id, digest);
        }
    } else if let Some(outcome) = completion.ephemeral {
        outcome.send_replace(Some(Arc::new(response)));
    }
}

fn remove_owned_command(
    state: &mut ProtocolState,
    completion: CommandCompletion,
    response: RpcResponse,
) {
    if let Some(id) = completion.id {
        if let Some(entry) = state.commands.remove(&id) {
            state.command_ledger_bytes = state
                .command_ledger_bytes
                .saturating_sub(entry.encoded_bytes);
            entry.outcome.send_replace(Some(Arc::new(response)));
        }
        if let Some(position) = state
            .command_order
            .iter()
            .position(|candidate| candidate == &id)
        {
            state.command_order.remove(position);
        }
    } else if let Some(outcome) = completion.ephemeral {
        outcome.send_replace(Some(Arc::new(response)));
    }
}

fn envelope_application_event(
    runtime: &SessionRuntime,
    state: &mut ProtocolState,
    event: pi_coding::ApplicationEvent,
) -> Option<Value> {
    match &event {
        // Real publication is authoritative for both the owning runtime
        // generation and the exact runtime-relative ordinal. An older
        // generation can never cross the cut; a newer generation must wait
        // for its exact RuntimeChanged authority so it is neither projected
        // early nor silently lost.
        pi_coding::ApplicationEvent::AgentPublished { epoch, barrier, .. } => {
            match (*epoch).cmp(&state.epoch) {
                std::cmp::Ordering::Less => return None,
                std::cmp::Ordering::Greater => {
                    if state.pending_future_epoch_events.len() >= MAX_PENDING_FUTURE_EPOCH_EVENTS {
                        // Overflow is authoritative-loss risk: force one
                        // resnapshot/resync rather than evicting a valid event.
                        state.pending_future_epoch_overflow = true;
                        return None;
                    }
                    state.pending_future_epoch_events.push_back(event);
                    return None;
                }
                std::cmp::Ordering::Equal => {
                    if *barrier <= state.forwarded_agent_event_barrier {
                        return None;
                    }
                    state.forwarded_agent_event_barrier = *barrier;
                }
            }
        }
        // Manually constructed events carry no ordinal and keep the
        // historical increment behavior for synthetic and test publication.
        pi_coding::ApplicationEvent::Agent(_) => {
            state.forwarded_agent_event_barrier =
                state.forwarded_agent_event_barrier.saturating_add(1);
        }
        _ => {}
    }
    if let pi_coding::ApplicationEvent::RuntimeChanged { epoch } = &event {
        // A runtime switch the protocol state already reflects (advanced by
        // an authoritative snapshot cut or a resync) must not be re-projected
        // above the watermark it was consumed beneath.
        if *epoch <= state.epoch {
            return None;
        }
        state.epoch = *epoch;
        state.observed_epoch = (*epoch).max(state.observed_epoch);
        state.active_turn_id = None;
        state.active_turn_model = None;
        state.forwarded_agent_event_barrier = 0;
        state.active_assistant_message_id = None;
        state.application_lifecycle_revision = 0;
        state.next_message_ordinal = 0;
        // Every matching event from the new epoch remains queued in arrival
        // order; only older epochs are discarded. Later events are drained
        // one-by-one after this RuntimeChanged cut so each retains its own
        // sequence and identity.
        let pending = std::mem::take(&mut state.pending_future_epoch_events);
        state.pending_future_epoch_events = pending
            .into_iter()
            .filter(|pending_event| match pending_event {
                pi_coding::ApplicationEvent::AgentPublished {
                    epoch: pending_epoch,
                    ..
                } => *pending_epoch >= *epoch,
                _ => true,
            })
            .collect();
    }
    let mut value = match project_application_event(event) {
        Ok(value) => value,
        Err(error) => serde_json::to_value(
            RpcResponse::failure(
                None,
                "events",
                format!("failed to project application event: {error}"),
            )
            .with_session_id(runtime.protocol_id()),
        )
        .unwrap_or(Value::Null),
    };
    let event_type = value.get("type").and_then(Value::as_str).map(str::to_owned);
    match event_type.as_deref() {
        Some("turn_started") => {
            let turn_id = value
                .get("turnId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            state.active_turn_id = turn_id;
            state.active_turn_model = value
                .get("model")
                .cloned()
                .and_then(|model| serde_json::from_value(model).ok());
            state.active_assistant_message_id = None;
            state.next_message_ordinal = 0;
            state.application_lifecycle_revision =
                state.application_lifecycle_revision.saturating_add(1);
        }
        Some("turn_terminal") => {
            let terminal_turn = value.get("turnId").and_then(Value::as_str);
            if terminal_turn.is_none() || terminal_turn != state.active_turn_id.as_deref() {
                return None;
            }
            state.application_lifecycle_revision =
                state.application_lifecycle_revision.saturating_add(1);
        }
        _ => {
            if value.get("turnId").is_none()
                && let Some(turn_id) = state.active_turn_id.as_deref()
            {
                value["turnId"] = json!(turn_id);
            }
        }
    }
    value["sessionId"] = json!(runtime.protocol_id());
    value["runtimeIncarnation"] = json!(runtime.runtime_incarnation());
    let turn_id = value
        .get("turnId")
        .and_then(Value::as_str)
        .unwrap_or("none");
    let protocol_id = runtime.protocol_id();
    let item_id = match event_type.as_deref() {
        Some("message_start") => {
            state.next_message_ordinal = state.next_message_ordinal.saturating_add(1);
            let item_id = format!(
                "{protocol_id}:{turn_id}:message:{}",
                state.next_message_ordinal
            );
            if value
                .get("message")
                .and_then(|message| message.get("role"))
                .and_then(Value::as_str)
                == Some("assistant")
            {
                state.active_assistant_message_id = Some(item_id.clone());
            }
            Some(item_id)
        }
        Some("message_update" | "message_end") => state.active_assistant_message_id.clone(),
        Some("tool_execution_start" | "tool_execution_update" | "tool_execution_end") => value
            .get("toolCallId")
            .and_then(Value::as_str)
            .map(|tool_call_id| format!("{protocol_id}:{turn_id}:tool:{tool_call_id}")),
        _ => None,
    };
    if let Some(item_id) = item_id {
        value["itemId"] = json!(item_id);
    }
    if matches!(event_type.as_deref(), Some("message_end"))
        && value
            .get("message")
            .and_then(|message| message.get("role"))
            .and_then(Value::as_str)
            == Some("assistant")
    {
        state.active_assistant_message_id = None;
    }
    state.next_sequence = state.next_sequence.saturating_add(1);
    value["sequence"] = json!(state.next_sequence);
    let overflowed = push_replay_event(state, value.clone());
    if overflowed {
        // The oversized payload is omitted from retained history: replace it
        // in-band with a small resync marker at the same sequence, so a
        // client behind the boundary fails closed while a client at or past
        // it keeps warm-replaying later contiguous events.
        let mut marker = json!({
            "type": "session_resync_required",
            "sessionId": runtime.protocol_id(),
            "runtimeIncarnation": runtime.runtime_incarnation(),
            "sequence": state.next_sequence,
            "reason": ResyncRequiredReason::Overflow,
        });
        if let Some(turn_id) = state.active_turn_id.as_deref() {
            marker["turnId"] = json!(turn_id);
        }
        push_replay_event(state, marker.clone());
        value = marker;
    }
    runtime.publish_sequence(state.next_sequence);
    if matches!(event_type.as_deref(), Some("turn_terminal")) {
        state.active_turn_id = None;
        state.active_turn_model = None;
        state.active_assistant_message_id = None;
    }
    Some(value)
}

fn push_session_resync(
    runtime: &SessionRuntime,
    state: &mut ProtocolState,
    snapshot: &pi_coding::ApplicationProtocolSnapshot,
) -> Value {
    state.epoch = snapshot.epoch;
    state.application_lifecycle_revision = snapshot.lifecycle.revision;
    state.forwarded_agent_event_barrier = snapshot.event_barrier;
    state.active_turn_id = snapshot.lifecycle.active_turn_id.clone();
    state.active_turn_model = snapshot.lifecycle.active_turn_model.clone();
    state.active_assistant_message_id = None;
    state.next_message_ordinal = 0;
    state.next_sequence = state.next_sequence.saturating_add(1);
    let mut event = json!({
        "type": "session_resync_required",
        "sessionId": runtime.protocol_id(),
        "runtimeIncarnation": runtime.runtime_incarnation(),
        "sequence": state.next_sequence,
        "reason": ResyncRequiredReason::Gap,
        "lifecycle": snapshot.lifecycle,
    });
    if let Some(turn_id) = state.active_turn_id.as_deref() {
        event["turnId"] = json!(turn_id);
    }
    push_replay_event(state, event.clone());
    runtime.publish_sequence(state.next_sequence);
    event
}

fn push_replay_event(state: &mut ProtocolState, value: Value) -> bool {
    let event_bytes = estimate_value_bytes(&value);
    if event_bytes > EVENT_REPLAY_BYTE_BUDGET {
        state.replay.clear();
        state.replay_bytes = 0;
        return true;
    }
    while !state.replay.is_empty()
        && (state.replay.len() >= EVENT_REPLAY_CAPACITY
            || state.replay_bytes.saturating_add(event_bytes) > EVENT_REPLAY_BYTE_BUDGET)
    {
        if let Some(oldest) = state.replay.pop_front() {
            state.replay_bytes = state
                .replay_bytes
                .saturating_sub(estimate_value_bytes(&oldest));
        }
    }
    state.replay_bytes = state.replay_bytes.saturating_add(event_bytes);
    state.replay.push_back(value);
    false
}

fn protocol_snapshot_cut(
    runtime: &SessionRuntime,
    state: &mut ProtocolState,
    snapshot: pi_coding::ApplicationProtocolSnapshot,
    cursor: u64,
    replay_from_zero: bool,
) -> Option<OwnedProtocolSnapshot> {
    if snapshot.epoch < state.epoch {
        return None;
    }
    if snapshot.epoch > state.epoch {
        state.epoch = snapshot.epoch;
        state.active_turn_id = None;
        state.active_turn_model = None;
        state.active_assistant_message_id = None;
        state.application_lifecycle_revision = 0;
        state.next_message_ordinal = 0;
    }
    if snapshot.lifecycle.revision > state.application_lifecycle_revision {
        push_session_resync(runtime, state, &snapshot);
    }
    let watermark = state.next_sequence;
    let earliest = state
        .replay
        .front()
        .and_then(|event| event.get("sequence"))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| watermark.saturating_add(1));
    let marker_after_cursor = state.replay.iter().any(|event| {
        event.get("type").and_then(Value::as_str) == Some("session_resync_required")
            && event
                .get("sequence")
                .and_then(Value::as_u64)
                .is_some_and(|sequence| sequence > cursor)
    });
    let cursor_ahead_of_epoch = cursor > watermark;
    let has_gap = marker_after_cursor
        || cursor_ahead_of_epoch
        || (cursor.saturating_add(1) < earliest && cursor < watermark);
    let events = if (cursor == 0 && !replay_from_zero) || marker_after_cursor || cursor_ahead_of_epoch {
        Vec::new()
    } else {
        state
            .replay
            .iter()
            .filter(|event| {
                event
                    .get("sequence")
                    .and_then(Value::as_u64)
                    .is_some_and(|sequence| sequence > cursor)
            })
            .cloned()
            .collect()
    };
    Some(OwnedProtocolSnapshot {
        application: snapshot,
        cut: ProtocolCut {
            watermark,
            has_gap,
            events,
        },
    })
}

fn command_digest(command: &RpcCommand, mode: LifecycleResponseMode) -> Result<[u8; 32]> {
    let bytes = serde_json::to_vec(&(command, mode))
        .context("serializing RPC command for idempotency")?;
    Ok(Sha256::digest(bytes).into())
}

fn command_ledger_failure(
    id: Option<String>,
    name: &str,
    session_id: &str,
    message: &str,
) -> RpcResponse {
    RpcResponse::failure(id, name, message).with_session_id(session_id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeUnavailableCause {
    OwnerStopped,
    ApplicationClosed,
    WriterUnavailable,
    Retrying,
}

impl RuntimeUnavailableCause {
    pub(crate) fn wire(self) -> &'static str {
        match self {
            Self::OwnerStopped => "owner_stopped",
            Self::ApplicationClosed => "application_closed",
            Self::WriterUnavailable => "writer_unavailable",
            Self::Retrying => "retrying",
        }
    }

    pub(crate) fn retryable(self) -> bool {
        matches!(self, Self::Retrying)
    }
}

#[derive(Clone, Copy)]
enum OwnerStopReason {
    Aborted,
    ProtocolChannelClosed,
    ManagerDropped,
    StopCommand,
    ApplicationEventsClosed,
    EventFanoutClosed,
    UiFanoutClosed,
}

impl OwnerStopReason {
    fn wire(self) -> &'static str {
        match self {
            Self::Aborted => "aborted",
            Self::ProtocolChannelClosed => "protocol_channel_closed",
            Self::ManagerDropped => "manager_dropped",
            Self::StopCommand => "stop_command",
            Self::ApplicationEventsClosed => "application_events_closed",
            Self::EventFanoutClosed => "event_fanout_closed",
            Self::UiFanoutClosed => "ui_fanout_closed",
        }
    }
}

struct OwnerExitGuard {
    runtime: Arc<SessionRuntime>,
    reason: OwnerStopReason,
}

impl Drop for OwnerExitGuard {
    fn drop(&mut self) {
        self.runtime.mark_owner_stopped();
        eprintln!(
            "session_owner_stop session_id={} runtime_incarnation={} reason={}",
            self.runtime.protocol_id(),
            self.runtime.runtime_incarnation(),
            self.reason.wire()
        );
    }
}

#[derive(Debug)]
pub(crate) struct RuntimeUnavailable {
    pub(crate) cause: RuntimeUnavailableCause,
}

impl RuntimeUnavailable {
    fn new(cause: RuntimeUnavailableCause) -> Self {
        Self { cause }
    }

    fn owner_stopped() -> Self {
        Self::new(RuntimeUnavailableCause::OwnerStopped)
    }
}

impl fmt::Display for RuntimeUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("session could not be opened right now; verify the selection and retry")
    }
}

impl std::error::Error for RuntimeUnavailable {}

#[derive(Debug)]
pub(crate) struct StaleRuntime;

impl fmt::Display for StaleRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("stale_runtime")
    }
}

impl std::error::Error for StaleRuntime {}

#[derive(Debug)]
pub(crate) struct SnapshotInProgress;

impl fmt::Display for SnapshotInProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("subscription snapshot already in progress")
    }
}

impl std::error::Error for SnapshotInProgress {}

#[derive(Clone)]
enum SharedSnapshotFailure {
    Cancelled,
    InProgress,
    Stale,
    TranscriptUnavailable,
    RuntimeUnavailable { cause: RuntimeUnavailableCause },
    Other(String),
}

struct SnapshotFlight {
    cursor: u64,
    exact_incarnation: bool,
    done: watch::Sender<Option<Result<Arc<Value>, SharedSnapshotFailure>>>,
}

enum SnapshotFlightClaim {
    Join {
        receiver: watch::Receiver<Option<Result<Arc<Value>, SharedSnapshotFailure>>>,
        can_share: bool,
    },
    Lead(watch::Sender<Option<Result<Arc<Value>, SharedSnapshotFailure>>>),
}

struct SnapshotFlightLead {
    flights: Arc<std::sync::Mutex<HashMap<RuntimeIdentity, SnapshotFlight>>>,
    identity: RuntimeIdentity,
    tx: watch::Sender<Option<Result<Arc<Value>, SharedSnapshotFailure>>>,
    published: bool,
}

impl Drop for SnapshotFlightLead {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        let _ = self.tx.send(Some(Err(SharedSnapshotFailure::Cancelled)));
        self.flights
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&self.identity);
    }
}

impl SnapshotFlightLead {
    fn finish(mut self, outcome: Result<Arc<Value>, SharedSnapshotFailure>) {
        let _ = self.tx.send(Some(outcome));
        self.flights
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&self.identity);
        self.published = true;
    }
}

fn snapshot_flight_can_share(
    inflight_cursor: u64,
    inflight_exact: bool,
    request_cursor: u64,
    request_exact: bool,
) -> bool {
    if request_cursor < inflight_cursor {
        return false;
    }
    if inflight_cursor == 0 {
        return true;
    }
    !request_exact || inflight_exact
}

fn classify_shared_snapshot_failure(error: &anyhow::Error) -> SharedSnapshotFailure {
    if error.downcast_ref::<StaleRuntime>().is_some() {
        SharedSnapshotFailure::Stale
    } else if error.downcast_ref::<pi_coding::TranscriptUnavailable>().is_some() {
        SharedSnapshotFailure::TranscriptUnavailable
    } else if let Some(unavailable) = error.downcast_ref::<RuntimeUnavailable>() {
        SharedSnapshotFailure::RuntimeUnavailable {
            cause: unavailable.cause,
        }
    } else if error.downcast_ref::<SnapshotInProgress>().is_some() {
        SharedSnapshotFailure::InProgress
    } else {
        SharedSnapshotFailure::Other(error.to_string())
    }
}

fn shared_snapshot_failure_into_error(failure: SharedSnapshotFailure) -> anyhow::Error {
    match failure {
        SharedSnapshotFailure::Cancelled => anyhow!("subscription snapshot cancelled"),
        SharedSnapshotFailure::InProgress => anyhow!(SnapshotInProgress),
        SharedSnapshotFailure::Stale => anyhow!(StaleRuntime),
        SharedSnapshotFailure::TranscriptUnavailable => {
            anyhow!(pi_coding::TranscriptUnavailable::from(anyhow!(
                "subscription snapshot transcript unavailable"
            )))
        }
        SharedSnapshotFailure::RuntimeUnavailable { cause } => {
            anyhow!(RuntimeUnavailable { cause })
        }
        SharedSnapshotFailure::Other(message) => anyhow!(message),
    }
}

fn require_live_runtime(
    runtime: &SessionRuntime,
) -> std::result::Result<OwnedRwLockReadGuard<()>, RuntimeUnavailable> {
    if runtime.owner_stopped.load(Ordering::Acquire) || runtime.protocol_tx.is_closed() {
        return Err(RuntimeUnavailable::new(
            RuntimeUnavailableCause::OwnerStopped,
        ));
    }
    let lifecycle = runtime
        .lifecycle_lock
        .clone()
        .try_read_owned()
        .map_err(|_| RuntimeUnavailable::new(RuntimeUnavailableCause::Retrying))?;
    if runtime.application.is_cleaned() {
        return Err(RuntimeUnavailable::new(
            RuntimeUnavailableCause::ApplicationClosed,
        ));
    }
    if runtime.application.session().recording_writer_info().is_err() {
        return Err(RuntimeUnavailable::new(
            RuntimeUnavailableCause::WriterUnavailable,
        ));
    }
    Ok(lifecycle)
}

async fn acquire_lifecycle_write_guard(
    runtime: &SessionRuntime,
) -> Option<OwnedRwLockWriteGuard<()>> {
    match tokio::time::timeout(
        LIFECYCLE_WRITE_GUARD_TIMEOUT,
        runtime.lifecycle_lock.clone().write_owned(),
    )
    .await
    {
        Ok(guard) => Some(guard),
        Err(_) => {
            eprintln!(
                "session_shutdown category=lifecycle_write_guard_timeout session_id={} runtime_incarnation={}",
                runtime.protocol_id(),
                runtime.runtime_incarnation()
            );
            None
        }
    }
}

fn runtime_unavailable_response(
    id: Option<String>,
    name: &str,
    source_id: &str,
    negotiated: bool,
    error: RuntimeUnavailable,
) -> RpcResponse {
    let mut response = RpcResponse::failure(id, name, error.to_string()).with_session_id(source_id);
    if negotiated {
        response.data = Some(json!({
            "code": "runtime_unavailable",
            "retryable": error.cause.retryable(),
            "cause": error.cause.wire(),
        }));
    }
    response
}

/// Post-open negotiated failure: the runtime opened but the lifecycle handoff
/// could not complete (lifecycle lock contended / runtime not registered).
/// Same bounded message as the legacy path, plus the machine
/// `code`/`retryable` metadata the negotiated contract carries.
fn negotiated_open_failure(
    id: Option<String>,
    name: &str,
    source_id: &str,
    negotiated: bool,
) -> RpcResponse {
    let mut response = RpcResponse::failure(
        id,
        name,
        "session could not be opened right now; verify the selection and retry",
    )
    .with_session_id(source_id);
    if negotiated {
        response.data = Some(json!({"code": "runtime_unavailable", "retryable": true}));
    }
    response
}

/// The manager. Different sessions build concurrently. Persisted opens use a
/// per-file lock so the same session can never be opened twice (two recorders
/// appending one JSONL would corrupt it), while a capacity permit counts both
/// loaded and in-flight runtimes.
pub(crate) struct SessionRuntimeManager {
    primary: Arc<SessionRuntime>,
    /// Registry keyed by session id. Stale pre-cutover ids are kept as
    /// aliases so in-flight Web frames keep routing after an in-place
    /// fork/clone; the runtime is removed wholesale on close.
    /// One atomic lookup boundary for session ids and physical file identities.
    registry: RwLock<RuntimeRegistry>,
    /// Serializes catalog imports and mutations, but never runtime construction.
    catalog_lock: tokio::sync::Mutex<()>,
    open_locks:
        std::sync::Mutex<HashMap<pi_coding::SessionFileIdentity, Weak<tokio::sync::Mutex<()>>>>,
    /// One permit per loaded or in-flight runtime, including the primary.
    usage_sequence: AtomicU64,
    loaded_slots: Arc<Semaphore>,
    /// Process-wide non-inline command permit pool.
    global_slots: Arc<Semaphore>,
    retirement_slots: Arc<Semaphore>,
    snapshot_builds: Arc<Semaphore>,
    snapshot_flights: Arc<std::sync::Mutex<HashMap<RuntimeIdentity, SnapshotFlight>>>,
    factory: Option<Arc<dyn SessionSpawner>>,
    /// D2 display codename resolved from the runner identity at listen
    /// start. Injected into every `get_state` response as `runnerCodename`
    /// (independent of the `sessionId` backfill for id-less payloads);
    /// `None` omits the key.
    runner_codename: Option<String>,
    /// Fan-in: every runtime's forwarder publishes into these; every Web
    /// connection subscribes here (multi-client fan-out).
    events: broadcast::Sender<Value>,
    ui_events: broadcast::Sender<RpcExtensionUiRequest>,
    /// Never-read receivers keeping the fan-in channels alive while the
    /// manager lives (broadcast send fails when zero receivers exist).
    _keepalive_events: broadcast::Receiver<Value>,
    _keepalive_ui: broadcast::Receiver<RpcExtensionUiRequest>,
    /// Web-only 5s undo tokens for `session_delete`. Expiry drops the token
    /// only; the archive stays on disk.
    undo_registry: RwLock<HashMap<String, (pi_coding::SessionArchiveOutcome, Instant)>>,
    /// Bounded replay window for `session_move` idempotent retries: after a
    /// successful move the original `sessionPath` no longer resolves (the
    /// file is gone), so an identical retry replays the recorded outcome
    /// instead of failing. Expired entries are dropped only.
    recent_session_moves:
        RwLock<HashMap<(String, String), (pi_coding::session_migration::SessionMoveOutcome, Instant)>>,
    session_downloads: Arc<RwLock<HashMap<String, PendingSessionDownload>>>,
    lifecycle_tx: mpsc::UnboundedSender<RuntimeLifecycleCommand>,
    /// One cloneable terminal shutdown result. The lifecycle owner publishes
    /// the FIRST terminal outcome exactly once; every caller — first,
    /// concurrent, cancelled, or late — replays the SAME stored result
    /// instead of fabricating a fresh `Ok(())` (which would mask a terminal
    /// shutdown failure) or re-entering the owner (whose command channel is
    /// closed after the first Shutdown). `None` until the owner finalizes.
    shutdown_terminal: watch::Receiver<Option<Result<(), String>>>,
    /// Cooperative cancellation for in-flight creations. Terminal shutdown
    /// waits a bounded deadline for creations to finish, then fires this
    /// token so `create_runtime` rolls its prepared/finished state back
    /// through the cancellation-safe guard before the owner aborts
    /// stragglers.
    creation_cancel: CancellationToken,
    /// Admission/shutdown state. `lifecycle_closed` is the ONE shutdown
    /// boundary: the terminal shutdown stores `true` BEFORE waiting on
    /// anything, and every admission (command admit, create dispatch)
    /// consults it as a short atomic token covering ONLY its enqueue
    /// acceptance — the closed check plus the channel send — never an owner
    /// reply, which is unbounded. `admission_inflight` counts admissions
    /// between that check and their enqueue acceptance (an RAII guard keeps
    /// the count correct even when a cancelled admission is dropped
    /// mid-send) so the shutdown can drain exactly those in a bounded
    /// window before closing the intake; anything later observes the closed
    /// state and is rejected with the exact shutting-down cause. A wedged
    /// protocol owner can therefore never delay the terminal shutdown: the
    /// shutdown closes first, then aborts and joins owners under the
    /// bounded deadlines below.
    lifecycle_closed: std::sync::atomic::AtomicBool,
    admission_inflight: Arc<AtomicUsize>,
    /// Tracked detached cleanup tasks (rollback fallbacks, retirements);
    /// the terminal shutdown joins them under [`CLEANUP_JOIN_BARRIER`].
    cleanups: Arc<DetachedCleanups>,
    #[cfg(test)]
    /// Forces the restart-shutdown checkpoint to fail so the terminal result
    /// store/replay contract (failure replayed verbatim to every caller) is
    /// testable without real checkpoint breakage.
    shutdown_checkpoint_failure: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    retirement_timeouts: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    subscription_snapshot_gate: tokio::sync::RwLock<()>,
    #[cfg(test)]
    subscription_snapshot_captured: tokio::sync::Notify,
    #[cfg(test)]
    subscription_snapshot_release: tokio::sync::Semaphore,
    #[cfg(test)]
    protocol_snapshot_count: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    /// Held (write) by tests to suspend a creation between registry
    /// publication and owner start, pinning the inserted-but-ownerless
    /// interleaving the shutdown must drain to terminal completion.
    #[cfg(test)]
    registration_gate: tokio::sync::RwLock<()>,
    #[cfg(test)]
    /// Test override for [`SWITCH_OPEN_DEADLINE`] so the bounded-open
    /// regression can exercise expiry without real-time waits.
    switch_open_deadline: std::sync::Mutex<Duration>,
}

#[derive(Clone)]
pub(crate) struct StagedSessionDownload {
    pub(crate) bytes: Arc<[u8]>,
    pub(crate) file_name: String,
}

struct PendingSessionDownload {
    download: StagedSessionDownload,
    expires_at: Instant,
}

/// Collapse every session-open failure to one bounded actionable category.
/// Detailed causes stay on stderr for diagnostics; paths, endpoints, and
/// runtime-construction labels never reach RPC clients.

/// Replay the one stored terminal shutdown result. The stored text is the
/// exact cause every caller observes; it is rebuilt as a fresh `anyhow`
/// error because `anyhow::Error` itself is not cloneable.
fn clone_terminal_shutdown(result: &Result<(), String>) -> Result<()> {
    result.clone().map_err(anyhow::Error::msg)
}

/// Wait for the lifecycle owner's one terminal shutdown result (a
/// concurrent/cancelled caller's Shutdown is already in flight) and replay it.
async fn wait_for_terminal_shutdown(
    terminal: &mut watch::Receiver<Option<Result<(), String>>>,
) -> Result<()> {
    loop {
        terminal
            .changed()
            .await
            .map_err(|_| anyhow!("session manager shutdown owner exited before reporting completion"))?;
        if let Some(result) = terminal.borrow().as_ref() {
            return clone_terminal_shutdown(result);
        }
    }
}

/// Post-open lifecycle snapshot failures (`new_session`/`fork`/`clone`/
/// `session_fork_latest` after the runtime already opened) reach Web clients
/// as one bounded, path-free cause: the internal causal chain embeds operator
/// paths and transcript detail, and the client only needs the actionable
/// category. The failure envelope stays request-source-owned (`with_session_id`
/// at the caller). Detailed causes stay on stderr for diagnostics.

/// Hard wire-text bound for a post-open lifecycle snapshot failure. Every
/// category the classifier can return is far below it; the bound pins the
/// contract so a future category can never smuggle unbounded context.
const MAX_LIFECYCLE_SNAPSHOT_FAILURE_CHARS: usize = 160;
fn bounded_lifecycle_snapshot_failure(error: &anyhow::Error) -> String {
    // Wrapped internal failures carry the marker in the ROOT cause, not the
    // outermost context ("protocol snapshot after lifecycle open"), so the
    // classification walks the whole chain.
    let text = format!("{error:#}");
    if text.contains("too many busy sessions") {
        return format!(
            "too many busy sessions (limit {MAX_LOADED_SESSIONS}); finish or stop active work and retry"
        );
    }
    if text.contains("shutting down") || text.contains("unavailable") {
        return "lifecycle target is shutting down; retry the operation".to_owned();
    }
    "lifecycle target could not be snapshotted; verify the session and retry".to_owned()
}

fn bounded_switch_failure(error: &anyhow::Error) -> String {
    if error
        .chain()
        .any(|source| source.to_string().contains("too many busy sessions"))
    {
        return format!(
            "too many busy sessions (limit {MAX_LOADED_SESSIONS}); finish or stop active work and retry"
        );
    }
    "session could not be opened right now; verify the selection and retry".to_owned()
}

/// Platform `ELOOP` raw errno: the no-follow directory-open path reports
/// blocked symlink traversal with this code.
#[cfg(target_os = "macos")]
const SYMLINK_LOOP_ERRNO: i32 = 62;
#[cfg(all(unix, not(target_os = "macos")))]
const SYMLINK_LOOP_ERRNO: i32 = 40;

/// Bounded, path-free failure text for `new_session`: one fixed literal per
/// operator-actionable directory category. Operator paths, OS error text, and
/// runtime-construction detail stay on this side of the wire.
fn bounded_new_session_failure(error: &anyhow::Error) -> String {
    if error
        .chain()
        .any(|source| source.to_string().contains("too many busy sessions"))
    {
        return format!(
            "too many busy sessions (limit {MAX_LOADED_SESSIONS}); finish or stop active work and retry"
        );
    }
    for source in error.chain() {
        let text = source.to_string();
        if text.contains("new session cwd must be absolute") {
            return "new session cwd must be an absolute directory path".to_owned();
        }
        if source
            .downcast_ref::<pi_sandbox::PolicyValidationError>()
            .is_some()
        {
            return "new session directory path is invalid".to_owned();
        }
        if source
            .downcast_ref::<pi_coding::OperatorDirectoryError>()
            .is_some()
        {
            return "new session path must not be a symbolic link".to_owned();
        }
        if let Some(error) = source.downcast_ref::<pi_coding::FsError>() {
            return match error {
                pi_coding::FsError::InvalidPath | pi_coding::FsError::OutsideCapability => {
                    "new session directory path is invalid".to_owned()
                }
                pi_coding::FsError::UnsupportedEntryType => {
                    "new session path is not a directory".to_owned()
                }
                pi_coding::FsError::Denied => {
                    "new session directory access is denied".to_owned()
                }
                pi_coding::FsError::System {
                    kind: std::io::ErrorKind::NotFound,
                    ..
                } => "new session directory does not exist".to_owned(),
                pi_coding::FsError::System {
                    kind: std::io::ErrorKind::NotADirectory,
                    ..
                } => "new session path is not a directory".to_owned(),
                // `ErrorKind::FilesystemLoop` name-matching is still
                // unstable on the pinned toolchain; `openat2(..,
                // NO_SYMLINKS)` reports blocked symlink traversal as ELOOP.
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                pi_coding::FsError::System {
                    raw_os_error: Some(SYMLINK_LOOP_ERRNO),
                    ..
                } => "new session path must not be a symbolic link".to_owned(),
                _ => "new session directory could not be opened".to_owned(),
            };
        }
        if text.contains("requires project trust") {
            return "new session project requires trust approval".to_owned();
        }
    }
    bounded_switch_failure(error)
}

/// One stable classification for `switch_session` failures: a machine `code`,
/// retryability, and a path-free internal cause class for the stderr log.
/// The full causal chain — which embeds operator paths and transcript
/// details — never reaches the wire or the log.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SwitchFailureClass {
    pub(crate) code: &'static str,
    pub(crate) retryable: bool,
    pub(crate) cause: &'static str,
}

fn switch_failure_class(code: &'static str, retryable: bool, cause: &'static str) -> SwitchFailureClass {
    SwitchFailureClass {
        code,
        retryable,
        cause,
    }
}

fn import_failure_class(error: &pi_coding::ImportSessionError) -> SwitchFailureClass {
    match error {
        pi_coding::ImportSessionError::SessionNotFound { .. }
        | pi_coding::ImportSessionError::AmbiguousSession { .. } => {
            switch_failure_class("stale_catalog_target", false, "session_not_found")
        }
        pi_coding::ImportSessionError::UnsupportedFormat(_) => {
            switch_failure_class("invalid_selection", false, "source_format_unsupported")
        }
        pi_coding::ImportSessionError::InvalidInput { .. }
        | pi_coding::ImportSessionError::InvalidNativeHeader { .. }
        | pi_coding::ImportSessionError::NoConvertibleMessages { .. }
        | pi_coding::ImportSessionError::Json { .. }
        | pi_coding::ImportSessionError::ResourceLimit { .. } => {
            switch_failure_class("invalid_selection", false, "transcript_invalid")
        }
        pi_coding::ImportSessionError::InvalidOutput { .. } => {
            switch_failure_class("invalid_selection", false, "import_destination_unavailable")
        }
        pi_coding::ImportSessionError::HomeDirectoryUnavailable => {
            switch_failure_class("runtime_unavailable", true, "catalog_unavailable")
        }
        pi_coding::ImportSessionError::Io { source, .. }
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            switch_failure_class("stale_catalog_target", false, "session_not_found")
        }
        pi_coding::ImportSessionError::Io { .. } => {
            switch_failure_class("runtime_unavailable", true, "io_error")
        }
    }
}

/// Classify any session-open failure without echoing its chain: the returned
/// fields are fixed literals only.
pub(crate) fn classify_switch_failure(error: &anyhow::Error) -> SwitchFailureClass {
    if error
        .chain()
        .any(|source| source.to_string().contains("too many busy sessions"))
    {
        return switch_failure_class("busy_capacity", true, "loaded_slot_capacity");
    }
    if let Some(unavailable) = error.downcast_ref::<RuntimeUnavailable>() {
        return switch_failure_class(
            "runtime_unavailable",
            unavailable.cause.retryable(),
            unavailable.cause.wire(),
        );
    }
    for source in error.chain() {
        if let Some(error) = source.downcast_ref::<pi_coding::CatalogError>() {
            return match error {
                pi_coding::CatalogError::SessionNotFound(_)
                | pi_coding::CatalogError::AmbiguousSession { .. } => {
                    switch_failure_class("stale_catalog_target", false, "session_not_found")
                }
                pi_coding::CatalogError::EmptySearchQuery => {
                    switch_failure_class("invalid_selection", false, "session_selection_invalid")
                }
                pi_coding::CatalogError::ReadOnlySource(_) => {
                    switch_failure_class("invalid_selection", false, "session_source_read_only")
                }
                pi_coding::CatalogError::UnsupportedSource(_) => {
                    switch_failure_class("invalid_selection", false, "session_source_unsupported")
                }
                pi_coding::CatalogError::WorkflowInternalSession => {
                    switch_failure_class("invalid_selection", false, "workflow_internal_session")
                }
                pi_coding::CatalogError::HomeDirectoryUnavailable => {
                    switch_failure_class("runtime_unavailable", true, "catalog_unavailable")
                }
                pi_coding::CatalogError::Import(error) => import_failure_class(error),
                pi_coding::CatalogError::Io { source, .. }
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    switch_failure_class("stale_catalog_target", false, "session_not_found")
                }
                pi_coding::CatalogError::Io { .. } => {
                    switch_failure_class("runtime_unavailable", true, "catalog_io_error")
                }
            };
        }
        if let Some(error) = source.downcast_ref::<pi_coding::ImportSessionError>() {
            return import_failure_class(error);
        }
        if source.downcast_ref::<serde_json::Error>().is_some() {
            return switch_failure_class("invalid_selection", false, "transcript_invalid");
        }
        if let Some(error) = source.downcast_ref::<std::io::Error>() {
            return if error.kind() == std::io::ErrorKind::NotFound {
                switch_failure_class("stale_catalog_target", false, "session_not_found")
            } else {
                switch_failure_class("runtime_unavailable", true, "io_error")
            };
        }
        let text = source.to_string();
        if text.contains("no longer matches the session catalog")
            || text.contains("no longer matches the loaded session runtime")
        {
            return switch_failure_class("stale_catalog_target", false, "catalog_row_missing");
        }
        if text.contains("already loaded from another session file") {
            return switch_failure_class("invalid_selection", false, "duplicate_session_id");
        }
    }
    switch_failure_class("runtime_unavailable", true, "runtime_unavailable")
}

/// NewSession has target-directory failures that are deterministic and must
/// never inherit switch-session retry semantics.
pub(crate) fn classify_new_session_failure(error: &anyhow::Error) -> SwitchFailureClass {
    if error
        .chain()
        .any(|source| source.to_string().contains("too many busy sessions"))
    {
        return switch_failure_class("busy_capacity", true, "loaded_slot_capacity");
    }
    for source in error.chain() {
        let text = source.to_string();
        if text.contains("new session cwd must be absolute") {
            return switch_failure_class("cwd_relative", false, "cwd_relative");
        }
        if text.contains("requires project trust") {
            return switch_failure_class("trust_denied", false, "project_trust_denied");
        }
        if source
            .downcast_ref::<pi_sandbox::PolicyValidationError>()
            .is_some()
        {
            return switch_failure_class("cwd_invalid", false, "cwd_invalid");
        }
        if source
            .downcast_ref::<pi_coding::OperatorDirectoryError>()
            .is_some()
        {
            return switch_failure_class("cwd_symlink", false, "cwd_symlink");
        }
        if let Some(error) = source.downcast_ref::<pi_coding::FsError>() {
            return match error {
                pi_coding::FsError::InvalidPath | pi_coding::FsError::OutsideCapability => {
                    switch_failure_class("cwd_invalid", false, "cwd_invalid")
                }
                pi_coding::FsError::UnsupportedEntryType => {
                    switch_failure_class("cwd_not_directory", false, "cwd_not_directory")
                }
                pi_coding::FsError::Denied => {
                    switch_failure_class("trust_denied", false, "directory_access_denied")
                }
                pi_coding::FsError::System {
                    kind: std::io::ErrorKind::NotFound,
                    ..
                } => switch_failure_class("cwd_missing", false, "cwd_missing"),
                pi_coding::FsError::System {
                    kind: std::io::ErrorKind::NotADirectory,
                    ..
                } => switch_failure_class("cwd_not_directory", false, "cwd_not_directory"),
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                pi_coding::FsError::System {
                    raw_os_error: Some(SYMLINK_LOOP_ERRNO),
                    ..
                } => switch_failure_class("cwd_symlink", false, "cwd_symlink"),
                pi_coding::FsError::IdentityChanged => {
                    switch_failure_class("cwd_changed", false, "cwd_identity_changed")
                }
                _ => switch_failure_class("runtime_unavailable", true, "filesystem_transient"),
            };
        }
    }
    switch_failure_class("runtime_unavailable", true, "runtime_unavailable")
}
impl SessionRuntimeManager {
    pub(crate) async fn new(
        primary_application: Application,
        primary_extension_ui: ExtensionUiAdapter,
        factory: Option<Arc<dyn SessionSpawner>>,
        runner_codename: Option<String>,
    ) -> Result<Arc<Self>> {
        let (events, keepalive_events) = broadcast::channel(EVENT_FANIN_CAPACITY);
        let (ui_events, keepalive_ui) = broadcast::channel(EVENT_FANIN_CAPACITY);
        let storage = factory
            .as_ref()
            .and_then(|factory| factory.session_storage());
        if let Some(factory) = factory.as_ref() {
            factory
                .todo_child_service()
                .ok_or_else(|| anyhow!("session runtime factory has no Todo child service"))?;
        }
        let cleanups = Arc::new(DetachedCleanups::default());
        if let Some(factory) = factory.as_ref() {
            factory.track_detached_cleanups({
                let cleanups = cleanups.clone();
                Arc::new(move |handle| cleanups.track(handle))
            });
        }
        let (lifecycle_tx, lifecycle_rx) = mpsc::unbounded_channel();
        let (shutdown_terminal_tx, shutdown_terminal) =
            watch::channel(None::<Result<(), String>>);
        let loaded_slots = Arc::new(Semaphore::new(MAX_LOADED_SESSIONS));
        let primary_slot = loaded_slots
            .clone()
            .try_acquire_owned()
            .expect("the primary occupies the first loaded-session slot");
        let (primary_runtime, primary_protocol_rx) = SessionRuntime::new(
            primary_application,
            primary_extension_ui,
            storage,
            None,
            Some(primary_slot),
        )?;
        let primary = Arc::new(primary_runtime);
        let manager = Arc::new(Self {
            primary: primary.clone(),
            registry: RwLock::new(RuntimeRegistry::default()),
            catalog_lock: tokio::sync::Mutex::new(()),
            open_locks:
                std::sync::Mutex::new(HashMap::new()),
            loaded_slots,
            lifecycle_closed: std::sync::atomic::AtomicBool::new(false),
            admission_inflight: Arc::new(AtomicUsize::new(0)),
            cleanups,
            usage_sequence: AtomicU64::new(0),
            global_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_SESSION_COMMANDS)),
            retirement_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_RETIREMENTS)),
            snapshot_builds: Arc::new(Semaphore::new(MAX_GLOBAL_SNAPSHOT_BUILDS)),
            snapshot_flights: Arc::new(std::sync::Mutex::new(HashMap::new())),
            creation_cancel: CancellationToken::new(),
            factory,
            runner_codename,
            events,
            ui_events,
            _keepalive_events: keepalive_events,
            _keepalive_ui: keepalive_ui,
            undo_registry: RwLock::new(HashMap::new()),
            session_downloads: Arc::new(RwLock::new(HashMap::new())),
            recent_session_moves: RwLock::new(HashMap::new()),
            lifecycle_tx,
            shutdown_terminal,
            #[cfg(test)]
            subscription_snapshot_gate: tokio::sync::RwLock::new(()),
            #[cfg(test)]
            subscription_snapshot_captured: tokio::sync::Notify::new(),
            #[cfg(test)]
            subscription_snapshot_release: tokio::sync::Semaphore::new(1),
            #[cfg(test)]
            protocol_snapshot_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            shutdown_checkpoint_failure: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            retirement_timeouts: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            registration_gate: tokio::sync::RwLock::new(()),
            #[cfg(test)]
            switch_open_deadline: std::sync::Mutex::new(SWITCH_OPEN_DEADLINE),
        });
        manager.register_runtime(&primary).await?;
        manager.mark_runtime_used(&primary);
        let (ready_tx, ready_rx) = oneshot::channel();
        SessionRuntime::start_owner(
            &manager,
            &primary,
            primary_protocol_rx,
            manager.events.clone(),
            manager.ui_events.clone(),
            ready_tx,
        );
        ready_rx
            .await
            .map_err(|_| anyhow!("primary protocol owner failed to start"))?;
        Self::start_lifecycle_owner(&manager, lifecycle_rx, shutdown_terminal_tx);
        Ok(manager)
    }

    fn start_lifecycle_owner(
        manager: &Arc<Self>,
        mut commands: mpsc::UnboundedReceiver<RuntimeLifecycleCommand>,
        shutdown_terminal: watch::Sender<Option<Result<(), String>>>,
    ) {
        let manager = Arc::downgrade(manager);
        tokio::spawn(async move {
            let mut creations = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    command = commands.recv() => {
                        let Some(command) = command else {
                            while creations.join_next().await.is_some() {}
                            break;
                        };
                        match command {
                            RuntimeLifecycleCommand::Create { kind, source, reply } => {
                                let manager = manager.clone();
                                creations.spawn(async move {
                                    let outcome = match manager.upgrade() {
                                        Some(manager) => manager.create_runtime(kind, source).await,
                                        None => Err(anyhow!("session manager is unavailable")),
                                    };
                                    let _ = reply.send(outcome);
                                });
                            }
                            RuntimeLifecycleCommand::Shutdown { is_restart, reply } => {
                                // Close FIRST: the closed store is the one
                                // admission boundary. Every admission consults
                                // it as a short atomic token covering only its
                                // enqueue acceptance — never an owner reply,
                                // which is unbounded — so the shutdown never
                                // waits on protocol or owner progress to
                                // claim the intake. Admissions that already
                                // passed the check get a bounded window to
                                // finish their enqueue; everything later
                                // observes the closed state and is rejected
                                // with the exact shutting-down cause. No
                                // check/send race, no unbounded wait.
                                let manager = manager.upgrade();
                                if let Some(manager) = manager.as_ref() {
                                    manager
                                        .lifecycle_closed
                                        .store(true, std::sync::atomic::Ordering::SeqCst);
                                    let drain_deadline =
                                        tokio::time::Instant::now() + ADMISSION_CLOSE_DRAIN;
                                    while manager.admission_inflight.load(Ordering::SeqCst) > 0 {
                                        if tokio::time::Instant::now() >= drain_deadline {
                                            eprintln!(
                                                "session_shutdown category=admission_close_drain_timeout"
                                            );
                                            break;
                                        }
                                        tokio::time::sleep(Duration::from_millis(2)).await;
                                    }
                                }
                                commands.close();
                                while let Ok(command) = commands.try_recv() {
                                    if let RuntimeLifecycleCommand::Create { reply, .. } = command {
                                        let _ = reply.send(Err(anyhow!("session manager is shutting down")));
                                    }
                                }
                                // Join every manager-owned creation to terminal
                                // completion BEFORE the final runtime
                                // snapshot/abort/retain — for a bounded
                                // deadline. Dropping this JoinSet would cancel
                                // futures past `factory.finish` and leak their
                                // created journals and half-registered
                                // runtimes, but a creation parked forever
                                // inside `factory.prepare` or `factory.finish`
                                // must never hang the terminal shutdown
                                // either: past the deadline the cooperative
                                // cancellation token fires, in-flight
                                // creations roll their prepared/finished
                                // state back through the cancellation-safe
                                // guard, stragglers are aborted, and the
                                // registry snapshot below captures whatever
                                // managed to register.
                                let creation_join = tokio::time::timeout(
                                    CREATION_JOIN_TIMEOUT,
                                    async {
                                        while creations.join_next().await.is_some() {}
                                    },
                                )
                                .await;
                                if creation_join.is_err() {
                                    if let Some(manager) = manager.as_ref() {
                                        manager.creation_cancel.cancel();
                                    }
                                    let cancelled = tokio::time::timeout(
                                        CREATION_CANCEL_GRACE,
                                        async {
                                            while creations.join_next().await.is_some() {}
                                        },
                                    )
                                    .await;
                                    if cancelled.is_err() {
                                        creations.abort_all();
                                        while creations.join_next().await.is_some() {}
                                    }
                                }
                                let result = match manager {
                                    Some(manager) => {
                                        let runtimes = manager.registry.read().await.distinct_runtimes();
                                        // A failed restart checkpoint must NOT
                                        // publish the terminal Err before the
                                        // runtime state behind it is terminal:
                                        // capture the failure and fall through
                                        // to the full termination sequence
                                        // below, publishing the exact
                                        // checkpoint error exactly once after
                                        // the cleanup it demands.
                                        let mut checkpoint_error: Option<String> = None;
                                        if is_restart {
                                            #[cfg(test)]
                                            if manager
                                                .shutdown_checkpoint_failure
                                                .load(std::sync::atomic::Ordering::SeqCst)
                                            {
                                                checkpoint_error = Some(
                                                    "forced restart checkpoint failure for tests".to_owned(),
                                                );
                                            }
                                            if checkpoint_error.is_none() {
                                                let todo_service = manager
                                                    .factory
                                                    .as_ref()
                                                    .and_then(|factory| factory.todo_child_service());
                                                let applications = runtimes
                                                    .iter()
                                                    .map(|runtime| runtime.application.clone())
                                                    .collect::<Vec<_>>();
                                                if let Err(error) = pi_coding::Application::checkpoint_process_for_restart(
                                                    &applications,
                                                    todo_service.as_deref(),
                                                ) {
                                                    checkpoint_error = Some(error.to_string());
                                                }
                                            }
                                        }
                                        // Abort AND await every runtime owner
                                        // — the primary included — before any
                                        // cleanup, detach, or terminal
                                        // publish. A joined owner can no
                                        // longer admit, publish, or hold
                                        // protocol state past the terminal
                                        // outcome.
                                        for runtime in &runtimes {
                                            if let Err(error) = runtime.finish_owner(true).await {
                                                eprintln!(
                                                    "session_shutdown category=owner_join error={error}"
                                                );
                                            }
                                        }
                                        // In-flight command drop barrier: the
                                        // joined owner aborts its command
                                        // JoinSet, and the terminal result is
                                        // published only once every in-flight
                                        // command future was dropped and
                                        // released its global permit and
                                        // lifecycle guard — the primary
                                        // included, whose lifecycle lock is
                                        // deliberately never taken here.
                                        for runtime in &runtimes {
                                            runtime.wait_in_flight_commands_dropped().await;
                                        }
                                        let mut lifecycle_guards = Vec::new();
                                        for runtime in &runtimes {
                                            if Arc::ptr_eq(runtime, &manager.primary) {
                                                continue;
                                            }
                                            if let Some(guard) =
                                                acquire_lifecycle_write_guard(runtime).await
                                            {
                                                lifecycle_guards.push(guard);
                                            }
                                        }
                                        {
                                            let mut registry = manager.registry.write().await;
                                            registry
                                                .by_id
                                                .retain(|_, runtime| Arc::ptr_eq(runtime, &manager.primary));
                                            registry
                                                .by_file
                                                .retain(|_, runtime| Arc::ptr_eq(runtime, &manager.primary));
                                        }
                                        let mut errors = Vec::new();
                                        // A successful restart checkpoint
                                        // detaches ALONE (no cleanup: the
                                        // checkpointed state survives for the
                                        // successor). A failed restart
                                        // checkpoint runs the complete terminal
                                        // cleanup for manager-owned
                                        // secondaries — the primary and the
                                        // host-owned Todo child service are
                                        // cleaned by `run_listener_mode` behind
                                        // the same exact checkpoint Err.
                                        if !is_restart || checkpoint_error.is_some() {
                                            for runtime in &runtimes {
                                                if !Arc::ptr_eq(runtime, &manager.primary)
                                                    && let Err(error) =
                                                        manager.cleanup_runtime_contents(runtime).await
                                                {
                                                    errors.push(error.to_string());
                                                }
                                            }
                                        }
                                        // Detached cleanups: join the tracked
                                        // rollback/retirement tasks under a
                                        // bounded budget so the terminal
                                        // outcome never races still-settling
                                        // cleanup work; a task that misses the
                                        // budget stays detached, best-effort.
                                        manager
                                            .cleanups
                                            .join_bounded(CLEANUP_JOIN_BARRIER)
                                            .await;
                                        drop(lifecycle_guards);
                                        if let Some(error) = checkpoint_error {
                                            // The ONE caller-observed terminal
                                            // result is the exact checkpoint
                                            // failure; secondary cleanup
                                            // failures stay on stderr.
                                            if !errors.is_empty() {
                                                eprintln!(
                                                    "session_shutdown category=checkpoint_failure_cleanup errors={}",
                                                    errors.join("; ")
                                                );
                                            }
                                            Err(anyhow!(error))
                                        } else if errors.is_empty() {
                                            Ok(())
                                        } else {
                                            Err(anyhow!("session runtime cleanup failed: {}", errors.join("; ")))
                                        }
                                    }
                                    None => Err(anyhow!("session manager is unavailable")),
                                };
                                let terminal = result.map_err(|error| error.to_string());
                                let _ = reply.send(terminal.clone().map_err(anyhow::Error::msg));
                                let _ = shutdown_terminal.send(Some(terminal));
                                break;
                            }
                        }
                    }
                    Some(_) = creations.join_next(), if !creations.is_empty() => {}
                }
            }
        });
    }

    async fn create_runtime(
        self: &Arc<Self>,
        kind: SessionSpawnKind,
        source: Application,
    ) -> Result<(Arc<SessionRuntime>, Option<String>)> {
        // Terminal shutdown owns the lifecycle intake. A create that lost the
        // shutdown race must not build a runtime that would resurrect work
        // past the terminal shutdown outcome: fail closed before any
        // prepared directory or journal exists. The queue-drain arm of the
        // owner rejects queued creates with the same terminal category.
        if self.lifecycle_tx.is_closed() {
            bail!("session manager is shutting down");
        }
        let factory = self
            .factory
            .as_ref()
            .ok_or_else(|| anyhow!("session factory is unavailable"))?
            .clone();
        let cancel = self.creation_cancel.clone();
        // Cooperative cancellation: terminal shutdown waits a bounded
        // deadline for in-flight creations, then fires this token and aborts
        // stragglers. Every manager-owned phase below is cancellation-safe:
        // the rollback guard owns the prepared spawn and the finished
        // candidate, so an abandoned creation explicitly settles its
        // prepared extension runtime, journal, and application instead of
        // leaking them. Inside `factory.prepare`/`factory.finish` the
        // factory owns fail-closed rollback of its own partial state.
        let prepared = tokio::select! {
            biased;
            () = cancel.cancelled() => bail!("session creation cancelled"),
            prepared = factory.prepare(SessionSpawnRequest { kind, source }) => prepared?,
        };
        let mut rollback = CreationRollbackGuard::prepared(prepared, self.cleanups.clone());
        let reservation = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                // Cancellation while the prepared spawn is still GUARDED:
                // the discard is explicit and awaited, never a plain drop.
                if let Some(prepared) = rollback.take_prepared() {
                    factory.discard_prepared(prepared).await;
                }
                return Err(anyhow!("session creation cancelled"));
            }
            reservation = self.acquire_loaded_slot() => match reservation {
                Ok(reservation) => reservation,
                Err(error) => {
                    // Explicit async discard on exhausted capacity: the
                    // factory tears its prepared state down (extension
                    // shutdown, candidate drop) — never silently dropped,
                    // and no journal or registry entry ever existed to
                    // leak.
                    if let Some(prepared) = rollback.take_prepared() {
                        factory.discard_prepared(prepared).await;
                    }
                    return Err(error);
                }
            },
        };
        // Cancellation between the slot reservation and the finish start: the
        // prepared spawn is discarded explicitly and awaited through the
        // factory, never plain-dropped.
        if cancel.is_cancelled() {
            if let Some(prepared) = rollback.take_prepared() {
                factory.discard_prepared(prepared).await;
            }
            return Err(anyhow!("session creation cancelled"));
        }
        // The finish future is NEVER dropped mid-flight by this task: it
        // owns the prepared spawn (and, for a production Fresh finish, the
        // partially created journal and live application behind its internal
        // cancellation-safe rollback). Cooperative cancellation is observed
        // at the phase boundary below; a finish that never returns is
        // bounded by the shutdown's cooperative-cancel deadline and abort,
        // whose detached rollback the finish owns internally. The prepared
        // spawn leaves the guard only at this handoff — the guard can no
        // longer double-own or drop it.
        let prepared = rollback
            .take_prepared()
            .expect("prepared spawn is still guarded before finish");
        let result = factory.finish(prepared).await?;
        rollback.created(result);
        if cancel.is_cancelled() {
            // The finish completed but the creation lost the shutdown race:
            // the finished candidate is discarded explicitly and awaited
            // (application cleanup plus created-journal rollback), never
            // published and never plain-dropped.
            if let Some(failure) = rollback.discard_created().await {
                eprintln!("session_open_failed category=cancelled_creation_discard error={failure}");
            }
            return Err(anyhow!("session creation cancelled"));
        }
        match self.register_spawned(
            rollback
                .result()
                .expect("creation rollback holds the finished candidate"),
            reservation,
        )
        .await {
            Ok(Ok(published)) => {
                rollback.disarm();
                Ok(published)
            }
            Ok(Err(existing)) => {
                // An identical runtime is already registered; discard this
                // duplicate candidate exactly once.
                if let Some(failure) = rollback.discard_created().await {
                    eprintln!("session_open_failed category=duplicate_discard error={failure}");
                }
                Ok((existing, None))
            }
            Err(error) => {
                if let Some(failure) = rollback.discard_created().await {
                    return Err(error.context(failure));
                }
                Err(error)
            }
        }
    }

    fn mark_runtime_used(&self, runtime: &Arc<SessionRuntime>) {
        let sequence = self.usage_sequence.fetch_add(1, Ordering::Relaxed) + 1;
        runtime.mark_used(sequence);
    }

    pub(crate) fn primary_session_id(&self) -> String {
        self.primary.protocol_id().to_owned()
    }

    pub(crate) async fn subscription_snapshot(
        &self,
        session_id: &str,
        cursor: u64,
    ) -> Result<Value> {
        self.subscription_snapshot_for_identity(
            RuntimeIdentity::new(session_id, String::new()),
            cursor,
            CancellationToken::new(),
            false,
        )
        .await
    }

    pub(crate) async fn subscription_snapshot_for_identity(
        &self,
        identity: RuntimeIdentity,
        cursor: u64,
        cancel: CancellationToken,
        exact_incarnation: bool,
    ) -> Result<Value> {
        loop {
            match self.claim_snapshot_flight(&identity, cursor, exact_incarnation) {
                SnapshotFlightClaim::Join {
                    mut receiver,
                    can_share,
                } => match self.wait_snapshot_flight(&mut receiver, &cancel).await? {
                    None => continue,
                    Some(Ok(payload)) if can_share => return Ok((*payload).clone()),
                    Some(Ok(_)) => continue,
                    Some(Err(SharedSnapshotFailure::Cancelled)) if !cancel.is_cancelled() => {
                        continue;
                    }
                    Some(Err(failure)) => {
                        return Err(shared_snapshot_failure_into_error(failure));
                    }
                },
                SnapshotFlightClaim::Lead(tx) => {
                    let lead = SnapshotFlightLead {
                        flights: self.snapshot_flights.clone(),
                        identity: identity.clone(),
                        tx,
                        published: false,
                    };
                    let built = self
                        .build_subscription_snapshot_for_identity(
                            identity.clone(),
                            cursor,
                            cancel.clone(),
                            exact_incarnation,
                        )
                        .await;
                    let shared = match &built {
                        Ok(payload) => Ok(Arc::new(payload.clone())),
                        Err(_) if cancel.is_cancelled() => Err(SharedSnapshotFailure::Cancelled),
                        Err(error) => Err(classify_shared_snapshot_failure(error)),
                    };
                    lead.finish(shared);
                    return built;
                }
            }
        }
    }

    fn claim_snapshot_flight(
        &self,
        identity: &RuntimeIdentity,
        cursor: u64,
        exact_incarnation: bool,
    ) -> SnapshotFlightClaim {
        let mut flights = self
            .snapshot_flights
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(flight) = flights.get(identity) {
            return SnapshotFlightClaim::Join {
                receiver: flight.done.subscribe(),
                can_share: snapshot_flight_can_share(
                    flight.cursor,
                    flight.exact_incarnation,
                    cursor,
                    exact_incarnation,
                ),
            };
        }
        let (tx, _) = watch::channel(None);
        flights.insert(
            identity.clone(),
            SnapshotFlight {
                cursor,
                exact_incarnation,
                done: tx.clone(),
            },
        );
        SnapshotFlightClaim::Lead(tx)
    }

    async fn wait_snapshot_flight(
        &self,
        receiver: &mut watch::Receiver<Option<Result<Arc<Value>, SharedSnapshotFailure>>>,
        cancel: &CancellationToken,
    ) -> Result<Option<Result<Arc<Value>, SharedSnapshotFailure>>> {
        if let Some(outcome) = receiver.borrow().clone() {
            return Ok(Some(outcome));
        }
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => bail!("subscription snapshot cancelled"),
                _ = tokio::time::sleep(SUBSCRIPTION_SNAPSHOT_JOIN_TIMEOUT) => {
                    return Err(anyhow!(SnapshotInProgress));
                }
                changed = receiver.changed() => {
                    if changed.is_err() {
                        return Ok(None);
                    }
                    if let Some(outcome) = receiver.borrow().clone() {
                        return Ok(Some(outcome));
                    }
                }
            }
        }
    }

    async fn build_subscription_snapshot_for_identity(
        &self,
        identity: RuntimeIdentity,
        cursor: u64,
        cancel: CancellationToken,
        exact_incarnation: bool,
    ) -> Result<Value> {
        #[cfg(test)]
        let _snapshot_gate = self.subscription_snapshot_gate.read().await;
        let runtime = self.resolve(Some(&identity.session_id)).await?;
        if exact_incarnation && runtime.runtime_incarnation() != identity.runtime_incarnation {
            return Err(anyhow!(StaleRuntime));
        }
        let lifecycle = require_live_runtime(&runtime).map_err(anyhow::Error::from)?;
        if !self.is_registered(&runtime).await {
            return Err(anyhow!("unknown session {}", identity.session_id));
        }
        drop(lifecycle);
        let _build_permit = tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("subscription snapshot cancelled"),
            permit = self.snapshot_builds.clone().acquire_owned() => {
                permit.map_err(|_| anyhow!("session protocol snapshot builds are unavailable"))?
            }
        };
        if cancel.is_cancelled() {
            bail!("subscription snapshot cancelled");
        }
        let owned = runtime
            .protocol_snapshot(cursor, exact_incarnation, cancel.clone())
            .await?;
        if cancel.is_cancelled() {
            bail!("subscription snapshot cancelled");
        }
        #[cfg(test)]
        {
            self.subscription_snapshot_captured.notify_one();
            let permit = self
                .subscription_snapshot_release
                .acquire()
                .await
                .expect("test release semaphore");
            permit.forget();
        }
        let snapshot = owned.application;
        let state = RpcSessionState::from_application(
            snapshot.state,
            snapshot.active_turn_model,
            snapshot.runtime_settings.state(),
            &snapshot.cwd,
            Some(runtime.protocol_id().to_owned()),
        );
        let reason = if owned.cut.has_gap {
            SubscriptionCutReason::Resync
        } else if exact_incarnation && cursor > 0 {
            SubscriptionCutReason::WarmContinuity
        } else {
            SubscriptionCutReason::ColdFull
        };
        let mut payload = json!({
            "type": if matches!(reason, SubscriptionCutReason::Resync) {
                "session_resync"
            } else {
                "subscription_snapshot"
            },
            "sessionId": runtime.protocol_id(),
            "runtimeIncarnation": runtime.runtime_incarnation(),
            "watermark": owned.cut.watermark,
            "reason": reason,
            "events": owned.cut.events,
            "state": state,
            "activeTurnId": snapshot.lifecycle.active_turn_id,
            "epoch": snapshot.epoch,
        });
        let rows = runtime
            .application
            .session()
            .authoritative_transcript()
            .map_err(anyhow::Error::from)?;
        payload["messages"] = json!(public_transcript_rows(rows));
        if !self.is_registered(&runtime).await {
            return Err(anyhow!("unknown session {}", identity.session_id));
        }
        let encoded = serde_json::to_string(&payload)
            .context("serializing subscription snapshot")?;
        if encoded.len() > COMPLETED_SNAPSHOT_BYTE_BUDGET {
            bail!(
                "subscription snapshot exceeds {COMPLETED_SNAPSHOT_BYTE_BUDGET}-byte budget"
            );
        }
        Ok(payload)
    }

    pub(crate) async fn current_runtime_identity(
        &self,
        session_id: &str,
    ) -> Result<RuntimeIdentity> {
        Ok(self.resolve(Some(session_id)).await?.identity())
    }

    #[cfg(test)]
    pub(crate) fn retirement_timeouts_for_test(&self) -> usize {
        self.retirement_timeouts.load(Ordering::SeqCst)
    }

    pub(crate) fn events(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    pub(crate) fn ui_events(&self) -> broadcast::Receiver<RpcExtensionUiRequest> {
        self.ui_events.subscribe()
    }
    pub(crate) fn governed_process_context(&self) -> pi_coding::GovernedProcessContext {
        self.primary.application.governed_process_context()
    }

    /// Test-only access to the fan-in broadcast sender, so
    /// `websocket_connection` tests can drive the /ws outbound path (bursts,
    /// slow readers) without a live session runtime.
    #[cfg(test)]
    pub(crate) fn events_tx(&self) -> broadcast::Sender<Value> {
        self.events.clone()
    }

    #[cfg(test)]
    pub(crate) async fn hold_subscription_snapshot_for_test(
        &self,
    ) -> tokio::sync::RwLockWriteGuard<'_, ()> {
        self.subscription_snapshot_gate.write().await
    }

    #[cfg(test)]
    pub(crate) async fn hold_subscription_snapshot_delivery_for_test(
        &self,
    ) -> tokio::sync::SemaphorePermit<'_> {
        self.subscription_snapshot_release
            .acquire()
            .await
            .expect("test snapshot delivery permit")
    }

    #[cfg(test)]
    pub(crate) fn release_subscription_snapshots_for_test(&self, count: usize) {
        self.subscription_snapshot_release.add_permits(count);
    }
    #[cfg(test)]
    pub(crate) async fn wait_for_subscription_snapshot_capture_for_test(&self) {
        self.subscription_snapshot_captured.notified().await;
    }
    #[cfg(test)]
    pub(crate) fn protocol_snapshot_count_for_test(&self) -> usize {
        self.protocol_snapshot_count.load(Ordering::SeqCst)
    }


    #[cfg(test)]
    pub(crate) async fn writer_info_for_test(
        &self,
        session_id: &str,
    ) -> Result<(String, PathBuf, pi_coding::SessionFileIdentity)> {
        self.resolve(Some(session_id))
            .await?
            .application
            .session()
            .recording_writer_info()
    }

    #[cfg(test)]
    async fn stop_owner_keep_registered_for_test(&self, session_id: &str) -> Result<()> {
        let runtime = self.resolve(Some(session_id)).await?;
        runtime.finish_owner(true).await
    }

    #[cfg(test)]
    async fn abort_owner_unmarked_keep_registered_for_test(&self, session_id: &str) -> Result<()> {
        let runtime = self.resolve(Some(session_id)).await?;
        let handle = runtime.owner.lock().expect("runtime owner lock").take();
        let Some(handle) = handle else {
            bail!("owner handle missing for {session_id}");
        };
        handle.abort();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !runtime.owner_stopped.load(Ordering::Acquire)
            || runtime.in_flight_commands.load(Ordering::Acquire) > 0
        {
            if tokio::time::Instant::now() >= deadline {
                bail!("owner terminal state was not published after abort");
            }
            tokio::task::yield_now().await;
        }
        Ok(())
    }

    #[cfg(test)]
    async fn owner_stopped_for_test(&self, session_id: &str) -> Result<bool> {
        Ok(self
            .resolve(Some(session_id))
            .await?
            .owner_stopped
            .load(Ordering::Acquire))
    }

    #[cfg(test)]
    async fn lifecycle_uncontended_for_test(&self, session_id: &str) -> Result<bool> {
        Ok(self
            .resolve(Some(session_id))
            .await?
            .lifecycle_lock
            .try_read()
            .is_ok())
    }

    #[cfg(test)]
    async fn is_registered_session_for_test(&self, session_id: &str) -> bool {
        match self.resolve(Some(session_id)).await {
            Ok(runtime) => self.is_registered(&runtime).await,
            Err(_) => false,
        }
    }

    #[cfg(test)]
    async fn inject_empty_in_memory_and_cleared_active_leaf_for_test(
        &self,
        session_id: &str,
    ) -> Result<()> {
        let runtime = self.resolve(Some(session_id)).await?;
        runtime.application.session().reset().await?;
        runtime
            .application
            .session()
            .current_recorder_for_fork()
            .ok_or_else(|| anyhow!("loaded runtime recorder is missing"))?
            .reset_leaf();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn publish_test_event(
        &self,
        session_id: &str,
        event: pi_coding::ApplicationEvent,
    ) -> Value {
        let runtime = self.resolve(Some(session_id)).await.expect("test runtime");
        let payload = runtime
            .envelope_test_event(event)
            .await
            .expect("test event envelope");
        self.events
            .send(payload.clone())
            .expect("test event receiver");
        payload
    }

    async fn register_runtime(&self, runtime: &Arc<SessionRuntime>) -> Result<()> {
        let (session_id, _, file_identity) = runtime
            .application
            .session()
            .recording_writer_info()
            .context("validating runtime recorder before registration")?;
        if session_id != runtime.protocol_id() {
            bail!("runtime protocol identity does not match its recorder");
        }
        let mut registry = self.registry.write().await;
        if registry.by_id.contains_key(&session_id) || registry.by_file.contains_key(&file_identity)
        {
            bail!("session runtime is already registered");
        }
        registry.by_id.insert(session_id, runtime.clone());
        registry.by_file.insert(file_identity, runtime.clone());
        Ok(())
    }

    async fn existing_runtime_for_id(
        &self,
        session_id: &str,
        expected_identity: Option<pi_coding::SessionFileIdentity>,
    ) -> Result<Option<Arc<SessionRuntime>>> {
        let existing = self.registry.read().await.by_id.get(session_id).cloned();
        let Some(existing) = existing else {
            return Ok(None);
        };
        if existing.current_file_identity() != expected_identity {
            bail!("session id {session_id} is already loaded from another session file");
        }
        Ok(Some(existing))
    }

    /// Add the runtime's current id alias and replace its physical-file entry.
    /// Stale ids remain routable; stale file identities never do.
    async fn update_identity(&self, runtime: &Arc<SessionRuntime>) {
        // While an active-move cutover owns the identity transition, the
        // recording writer transiently fails validation (the source file was
        // renamed away before the destination binds). Event-driven re-keys
        // MUST NOT run in that window: unregistering the runtime and
        // aborting its owner would kill the in-flight move command and
        // strand the client. The latch is cleared only by the cutover's
        // authoritative re-key once the writer validates at the destination.
        // Cutover recovery clears the latch when the writer is valid again.
        if runtime
            .identity_rekey_suppressed
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        self.rekey_identity(runtime).await;
    }

    /// Unconditional registry re-key used by the move cutover as the
    /// authoritative identity transition. Clears the suppression latch when
    /// the writer validates.
    async fn rekey_identity(&self, runtime: &Arc<SessionRuntime>) {
        let writer = runtime.application.session().recording_writer_info();
        let mut registry = self.registry.write().await;
        if !registry.contains(runtime) {
            return;
        }
        let Ok((session_id, _, file_identity)) = writer else {
            // A lost writer with NO in-flight commands cannot strand a
            // client: unregister and stop the owner as before. With commands
            // in flight the owner must survive to answer them; a later
            // authoritative re-key settles the identity.
            if runtime.in_flight_commands.load(std::sync::atomic::Ordering::Acquire) == 0 {
                registry.remove(runtime);
                runtime.abort_owner();
            }
            return;
        };
        runtime
            .identity_rekey_suppressed
            .store(false, std::sync::atomic::Ordering::Release);
        registry
            .by_id
            .entry(session_id)
            .or_insert_with(|| runtime.clone());
        registry
            .by_file
            .retain(|_, candidate| !Arc::ptr_eq(candidate, runtime));
        registry.by_file.insert(file_identity, runtime.clone());
    }
    /// Resolve the target runtime for a command. Web transports enforce the
    /// single missing-id bootstrap exception before calling the manager;
    /// internal single-runtime callers retain primary fallback.
    async fn resolve(&self, session_id: Option<&str>) -> Result<Arc<SessionRuntime>> {
        match session_id {
            None => Ok(self.primary.clone()),
            Some(id) => self
                .registry
                .read()
                .await
                .by_id
                .get(id)
                .cloned()
                .ok_or_else(|| anyhow!("unknown session {id}")),
        }
    }

    /// Reuse only a runtime whose current recorder path exactly matches the
    /// selected wire path. Session-list rows originate from these recorder
    /// paths; reparsing a live append-only JSONL before consulting the
    /// registry races the writer's current record. Aliases, symlinks, foreign
    /// sources, and every other path still go through catalog validation.
    async fn loaded_runtime_for_exact_path(
        &self,
        selected_path: &Path,
    ) -> Option<Arc<SessionRuntime>> {
        self.registry
            .read()
            .await
            .distinct_runtimes()
            .into_iter()
            .find(|runtime| {
                runtime
                    .current_identity()
                    .is_some_and(|(_, recorder_path)| recorder_path == selected_path)
            })
    }

    /// Recorder-authoritative, path-free public snapshot for a collaboration room.
    pub(crate) async fn collab_snapshot(
        &self,
        session_id: Option<&str>,
        max_entries: usize,
        max_bytes: usize,
    ) -> Result<(String, Value)> {
        let runtime = self.resolve(session_id).await?;
        let lifecycle = runtime
            .lifecycle_lock
            .clone()
            .try_read_owned()
            .map_err(|_| {
                anyhow!("session could not be opened right now; verify the selection and retry")
            })?;
        if !self.is_registered(&runtime).await {
            return Err(anyhow!("unknown session"));
        }
        let (session_id, _) = runtime
            .current_identity()
            .ok_or_else(|| anyhow!("session recording is unavailable"))?;
        let snapshot = runtime
            .application
            .collab_public_snapshot(max_entries, max_bytes)
            .map_err(|_| anyhow!("collaboration snapshot is unavailable"))?;
        drop(lifecycle);
        Ok((session_id, snapshot))
    }
    /// Resolve the source runtime for manager-level lifecycle commands.
    /// Unknown or missing ids fail closed; a stale or fabricated sessionId
    /// must not silently rebind the operation to the primary.
    async fn resolve_source(
        self: &Arc<Self>,
        session_id: Option<&str>,
    ) -> Result<Arc<SessionRuntime>> {
        self.resolve(session_id).await
    }

    /// Fast path: an unused capacity permit. Never evicts.
    fn try_reserve_loaded_slot(&self) -> Option<OwnedSemaphorePermit> {
        self.loaded_slots.clone().try_acquire_owned().ok()
    }

    fn loaded_slot_exhausted_error() -> anyhow::Error {
        anyhow!(
            "too many busy sessions (limit {MAX_LOADED_SESSIONS}); every loaded session has work in progress"
        )
    }

    /// Transactional loaded-slot acquisition with LRU retirement.
    ///
    /// Fast path takes a free permit. When the pool is exhausted, the
    /// least-recently-used IDLE non-primary runtime becomes the retirement
    /// candidate: its capacity permit is taken and its lifecycle write lock is
    /// held from selection until commit, so no new command can be admitted to
    /// the victim mid-swap. The caller either commits (retire the victim after
    /// the replacement registers) or rolls back (return the permit to the
    /// victim, leaving it exactly as it was). Fails closed only when every
    /// non-primary runtime is genuinely busy.
    async fn acquire_loaded_slot(self: &Arc<Self>) -> Result<LoadedSlotReservation> {
        if let Some(permit) = self.try_reserve_loaded_slot() {
            return Ok(LoadedSlotReservation::available(permit));
        }
        loop {
            let candidates = self.registry.read().await.distinct_runtimes();
            let mut selected: Option<(Arc<SessionRuntime>, OwnedRwLockWriteGuard<()>)> = None;
            for candidate in candidates {
                if Arc::ptr_eq(&candidate, &self.primary) {
                    continue;
                }
                let Ok(lifecycle) = candidate.lifecycle_lock.clone().try_write_owned() else {
                    continue;
                };
                if self.ensure_idle(&candidate).await.is_err() {
                    continue;
                }
                let replaces = match &selected {
                    None => true,
                    Some((current, _)) => candidate.last_used() < current.last_used(),
                };
                if replaces {
                    selected = Some((candidate, lifecycle));
                }
            }
            let (victim, victim_lifecycle) =
                selected.ok_or_else(Self::loaded_slot_exhausted_error)?;
            match victim.take_loaded_slot() {
                None => continue,
                Some(permit) => {
                    return Ok(LoadedSlotReservation::replacing(
                        permit,
                        victim,
                        victim_lifecycle,
                    ));
                }
            }
        }
    }

    fn open_lock(&self, identity: pi_coding::SessionFileIdentity) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .open_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&identity).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(identity, Arc::downgrade(&lock));
        lock
    }

    async fn is_registered(&self, runtime: &Arc<SessionRuntime>) -> bool {
        self.registry.read().await.contains(runtime)
    }

    /// Admit one command to its resident runtime before any transport-owned
    /// response waiter is spawned. Ordinary commands enter the per-runtime
    /// FIFO; safety and lifecycle commands execute outside it.
    pub(crate) async fn admit(
        self: &Arc<Self>,
        command: RpcCommand,
        session_id: Option<String>,
        use_per_session_slots: bool,
    ) -> CommandResponseWaiter {
        self.admit_with_lifecycle_response(
            command,
            session_id,
            use_per_session_slots,
            LifecycleResponseMode::FullSnapshot,
        )
        .await
    }

    pub(crate) async fn admit_web(
        self: &Arc<Self>,
        command: RpcCommand,
        session_id: Option<String>,
        use_per_session_slots: bool,
    ) -> CommandResponseWaiter {
        let mode = match &command {
            RpcCommand::SwitchSession { response_mode, .. } if response_mode.is_some() => {
                LifecycleResponseMode::WebSubscribeHandoff
            }
            _ => LifecycleResponseMode::FullSnapshot,
        };
        self.admit_with_lifecycle_response(command, session_id, use_per_session_slots, mode)
            .await
    }

    async fn admit_with_lifecycle_response(
        self: &Arc<Self>,
        command: RpcCommand,
        session_id: Option<String>,
        use_per_session_slots: bool,
        lifecycle_response_mode: LifecycleResponseMode,
    ) -> CommandResponseWaiter {
        let id = command.id();
        let name = command.command_name();
        let manager_level = is_manager_level_command(&command);
        let requires_writer = !manager_level
            || matches!(
                &command,
                RpcCommand::SetSessionName {
                    session_path: None,
                    ..
                }
            );
        let runtime = match self.resolve(session_id.as_deref()).await {
            Ok(runtime) => runtime,
            Err(_) => {
                return CommandResponseWaiter::ready(unknown_session_response(
                    id, name, session_id,
                ));
            }
        };
        // Fast reject on the closed state: the terminal shutdown stores it
        // before waiting on anything, so an admission arriving after it is
        // rejected with the exact shutting-down cause and never touches the
        // protocol owner.
        if self.lifecycle_closed.load(Ordering::SeqCst) {
            return CommandResponseWaiter::ready(command_ledger_failure(
                id,
                name,
                runtime.protocol_id(),
                "session manager is shutting down",
            ));
        }
        if IsolatedApplicationLruTouch::from_command(&command).stamps_recency() {
            self.mark_runtime_used(&runtime);
        }
        if requires_writer
            && runtime
                .application
                .session()
                .recording_writer_info()
                .is_err()
        {
            return CommandResponseWaiter::ready(
                RpcResponse::failure(
                    id,
                    name,
                    "session writer is unavailable; reopen the session and retry",
                )
                .with_session_id(runtime.protocol_id().to_owned()),
            );
        }
        let response_owner = runtime.protocol_id().to_owned();
        let digest = match command_digest(&command, lifecycle_response_mode) {
            Ok(digest) => digest,
            Err(error) => {
                return CommandResponseWaiter::ready(command_ledger_failure(
                    id,
                    name,
                    &response_owner,
                    &error.to_string(),
                ));
            }
        };
        // Hold a lifecycle read guard from admission until command completion,
        // so LRU retirement cannot race a command that was admitted but has
        // not executed yet.
        let lifecycle = match runtime.lifecycle_lock.clone().try_read_owned() {
            Ok(lifecycle) => lifecycle,
            Err(_) => {
                return CommandResponseWaiter::ready(
                    RpcResponse::failure(id, name, "session is busy: retirement in progress")
                        .with_session_id(response_owner),
                );
            }
        };
        let message = if command.bypasses_command_slots()
            || command.bypasses_ordinary_fifo()
            || manager_level
        {
            let (reply, response) = oneshot::channel();
            let message = ProtocolMessage::AdmitDirect {
                command,
                digest,
                manager_level,
                use_per_session_slots,
                lifecycle_response_mode,
                reply,
                lifecycle,
            };
            (message, response)
        } else {
            let (reply, response) = oneshot::channel();
            let message = ProtocolMessage::AdmitOrdinary {
                command,
                digest,
                use_per_session_slots,
                lifecycle,
                reply,
            };
            (message, response)
        };
        // Admission token: the enqueue acceptance is the ONLY section covered
        // against the shutdown's closed store. The owner's ledger reply below
        // is awaited OUTSIDE the token, so a wedged protocol owner can never
        // delay the terminal shutdown — the command future simply becomes one
        // of the in-flight commands the owner abort and drop barrier settles.
        // The RAII guard keeps the count cancellation-safe across the async
        // send: a cancelled admission still releases its in-flight slot.
        let admission = InFlightCommandGuard::acquire(&self.admission_inflight);
        let accepted = if self.lifecycle_closed.load(Ordering::SeqCst) {
            false
        } else {
            runtime.protocol_tx.send(message.0).await.is_ok()
        };
        drop(admission);
        if !accepted {
            return CommandResponseWaiter::ready(if self.lifecycle_closed.load(Ordering::SeqCst) {
                command_ledger_failure(
                    id,
                    name,
                    &response_owner,
                    "session manager is shutting down",
                )
            } else if self.is_registered(&runtime).await {
                command_ledger_failure(
                    id,
                    name,
                    &response_owner,
                    "session protocol owner is unavailable",
                )
            } else {
                unknown_session_response(id, name, Some(response_owner.clone()))
            });
        }
        message.1.await.unwrap_or_else(|_| {
            CommandResponseWaiter::ready(command_ledger_failure(
                id,
                name,
                &response_owner,
                "session protocol owner stopped during admission",
            ))
        })
    }

    pub(crate) async fn dispatch(
        self: &Arc<Self>,
        command: RpcCommand,
        session_id: Option<String>,
    ) -> RpcResponse {
        self.admit(command, session_id, true).await.wait().await
    }

    pub(crate) async fn dispatch_inner(
        self: &Arc<Self>,
        command: RpcCommand,
        session_id: Option<String>,
    ) -> RpcResponse {
        self.admit(command, session_id, false).await.wait().await
    }

    pub(crate) async fn dispatch_spawned(
        self: &Arc<Self>,
        command: RpcCommand,
        session_id: Option<String>,
    ) -> RpcResponse {
        self.admit(command, session_id, false).await.wait().await
    }

    async fn execute_target(
        self: &Arc<Self>,
        runtime: &Arc<SessionRuntime>,
        command: RpcCommand,
        use_per_session_slots: bool,
    ) -> RpcResponse {
        let id = command.id();
        let name = command.command_name();
        let response_owner = runtime.protocol_id().to_owned();
        if !self.is_registered(runtime).await {
            return unknown_session_response(id, name, Some(response_owner));
        }
        if let RpcCommand::FileExplorerStageDownload { path, .. } = &command {
            let response = match self.stage_workspace_download(runtime, path).await {
                Ok((download_id, file_name)) => RpcResponse::success(
                    id,
                    name,
                    Some(json!({
                        "downloadId": download_id,
                        "fileName": file_name,
                    })),
                ),
                Err(error) => RpcResponse::failure(id, name, error.to_string()),
            };
            return response.with_session_id(response_owner);
        }
        let response = if use_per_session_slots {
            runtime.dispatcher.dispatch(command).await
        } else {
            runtime.dispatcher.dispatch_inner(command).await
        };
        let mut response = self.overlay_loaded_sessions(response).await;
        if name == "get_state"
            && let Some(data) = response.data.as_mut()
            && let Some(object) = data.as_object_mut()
        {
            if object.get("sessionId").is_none_or(Value::is_null) {
                object.insert(
                    "sessionId".to_owned(),
                    Value::String(response_owner.clone()),
                );
            }
            object.insert(
                "runtimeIncarnation".to_owned(),
                Value::String(runtime.runtime_incarnation().to_owned()),
            );
            if let Some(codename) = self.runner_codename.as_deref() {
                object.insert(
                    "runnerCodename".to_owned(),
                    Value::String(codename.to_owned()),
                );
            }
        }
        response.with_session_id(response_owner)
    }

    async fn execute_manager_level(
        self: &Arc<Self>,
        source: &Arc<SessionRuntime>,
        command: RpcCommand,
        lifecycle_response_mode: LifecycleResponseMode,
    ) -> RpcResponse {
        let id = command.id();
        let name = command.command_name();
        let source_id = source.protocol_id().to_owned();
        let negotiated =
            matches!(lifecycle_response_mode, LifecycleResponseMode::WebSubscribeHandoff);
        let catalog_identity = match command.switch_catalog_identity() {
            Ok(identity) => identity,
            Err(error) => {
                let mut response =
                    RpcResponse::failure(id, name, error.message()).with_session_id(source_id);
                if negotiated {
                    response.data = Some(json!({"code": error.code(), "retryable": false}));
                }
                return response;
            }
        };
        let result = match command {
            RpcCommand::ExportHtml {
                output_path,
                session_path: Some(session_path),
                ..
            } => {
                if output_path.is_some() {
                    return RpcResponse::failure(
                        id,
                        name,
                        "outputPath is not supported with sessionPath",
                    )
                    .with_session_id(source_id);
                }
                return match self.export_saved_session(source, &session_path).await {
                    Ok((download_id, file_name)) => RpcResponse::success(
                        id,
                        name,
                        Some(json!({
                            "downloadId": download_id,
                            "fileName": file_name,
                        })),
                    ),
                    Err(error) => RpcResponse::failure(id, name, error.to_string()),
                }
                .with_session_id(source_id);
            }
            RpcCommand::SwitchSession { session_path, .. } => {
                self.open_persisted(source, &session_path, catalog_identity.as_ref())
                    .await
                    .map(|(runtime, identity)| (runtime, None, identity))
            }
            RpcCommand::NewSession { cwd, .. } => {
                return match self.create_fresh(source, cwd.as_deref()).await {
                    Ok(runtime) => {
                        self.lifecycle_result_response(
                            id,
                            name,
                            &source_id,
                            runtime,
                            None,
                            None,
                            lifecycle_response_mode,
                        )
                        .await
                    }
                    Err(error) => {
                        let class = classify_new_session_failure(&error);
                        // Fixed category + safe cause class only: the full
                        // causal chain embeds operator paths and never reaches
                        // this log.
                        eprintln!(
                            "new_session_failed mode={} code={} cause={}",
                            if negotiated {
                                "subscribe-handoff-v1"
                            } else {
                                "full-snapshot"
                            },
                            class.code,
                            class.cause,
                        );
                        let mut response = RpcResponse::failure(
                            id,
                            name,
                            bounded_new_session_failure(&error),
                        )
                        .with_session_id(source_id);
                        if negotiated {
                            response.data = Some(json!({
                                "code": class.code,
                                "retryable": class.retryable,
                            }));
                        }
                        response
                    }
                };
            }
            RpcCommand::SessionHandoff {
                custom_instructions,
                response_mode,
                ..
            } => {
                return self
                    .session_handoff_command(
                        source,
                        id,
                        name,
                        custom_instructions.as_deref(),
                        response_mode.is_some(),
                    )
                    .await;
            }
            RpcCommand::Fork {
                entry_id,
                session_path,
                ..
            } => {
                self.fork_command(source, &entry_id, session_path.as_deref())
                    .await
                    .map(|(runtime, editor_text)| (runtime, editor_text, None))
            }
            RpcCommand::Clone { session_path, .. } => self
                .clone_command(source, session_path.as_deref())
                .await
                .map(|runtime| (runtime, None, None)),
            RpcCommand::SessionForkLatest { session_path, .. } => {
                self.fork_latest_from(source, &session_path)
                    .await
                    .map(|(runtime, editor_text)| (runtime, editor_text, None))
            }
            RpcCommand::GetForkMessages {
                session_path,
                cursor,
                limit,
                ..
            } => {
                return match self
                    .fork_messages_for(source, session_path.as_deref(), cursor.as_deref(), limit)
                    .await
                {
                    Ok(data) => bounded_fork_messages_response(
                        id,
                        name,
                        Some(source_id.clone()),
                        Some(data),
                    ),
                    Err(error) => RpcResponse::failure(id, name, error.to_string())
                        .with_session_id(source_id.clone()),
                }
            }
            RpcCommand::SetSessionName {
                name: session_name,
                session_path,
                ..
            } => {
                return match self
                    .rename_saved(source, session_name.trim(), session_path.as_deref())
                    .await
                {
                    Ok(()) => RpcResponse::success(id, name, None),
                    Err(error) => RpcResponse::failure(id, name, error.to_string()),
                }
                .with_session_id(source_id);
            }
            RpcCommand::SessionDelete { session_path, .. } => {
                return self
                    .delete_session(source, id, name, &session_path)
                    .await
                    .with_session_id(source_id);
            }
            RpcCommand::SessionRestore { undo_token, .. } => {
                return self
                    .restore_session(id, name, &undo_token)
                    .await
                    .with_session_id(source_id);
            }
            RpcCommand::SessionCopyContext { session_path, .. } => {
                return match self.copy_saved_session_context(source, &session_path).await {
                    Ok(data) => RpcResponse::success(id, name, Some(data)),
                    Err(error) => RpcResponse::failure(id, name, error.to_string()),
                }
                .with_session_id(source_id);
            }
            RpcCommand::SessionMove {
                session_path,
                target_cwd,
                ..
            } => {
                return self
                    .move_saved_session(source, id, name, &session_path, &target_cwd)
                    .await
                    .with_session_id(source_id);
            }
            RpcCommand::CloseSession { .. } => {
                return self.close_session(id, name, Some(source_id.clone())).await;
            }
            RpcCommand::ExportHtml {
                session_path: None, ..
            } => {
                unreachable!("legacy export is dispatched directly to the session runtime")
            }
            _ => unreachable!("execute_manager_level only receives lifecycle commands"),
        };
        match result {
            Ok((runtime, editor_text, catalog_identity)) => {
                self.lifecycle_result_response(
                    id,
                    name,
                    &source_id,
                    runtime,
                    editor_text,
                    catalog_identity,
                    lifecycle_response_mode,
                )
                .await
            }
            Err(error) => {
                let class = classify_switch_failure(&error);
                // Fixed category + safe cause class only: the full causal
                // chain embeds operator paths and never reaches this log.
                eprintln!(
                    "switch_session_failed mode={} code={} cause={}",
                    if negotiated {
                        "subscribe-handoff-v1"
                    } else {
                        "full-snapshot"
                    },
                    class.code,
                    class.cause,
                );
                let mut response =
                    RpcResponse::failure(id, name, bounded_switch_failure(&error))
                        .with_session_id(source_id);
                if negotiated {
                    let mut data = json!({
                        "code": class.code,
                        "retryable": class.retryable,
                    });
                    if error.downcast_ref::<RuntimeUnavailable>().is_some() {
                        data["cause"] = json!(class.cause);
                    }
                    response.data = Some(data);
                }
                response
            }
        }
    }

    async fn lifecycle_result_response(
        self: &Arc<Self>,
        id: Option<String>,
        name: &str,
        source_id: &str,
        runtime: Arc<SessionRuntime>,
        editor_text: Option<String>,
        catalog_identity: Option<CatalogSwitchIdentity>,
        mode: LifecycleResponseMode,
    ) -> RpcResponse {
        // Post-open negotiated failures carry the same machine
        // `code`/`retryable` metadata as the pre-open classification: the
        // lifecycle lock is transient contention and an unregistered runtime
        // can be reopened from the catalog, so both are retryable.
        let negotiated = matches!(mode, LifecycleResponseMode::WebSubscribeHandoff);
        let lifecycle = match require_live_runtime(&runtime) {
            Ok(lifecycle) => lifecycle,
            Err(error) => {
                return runtime_unavailable_response(id, name, source_id, negotiated, error);
            }
        };
        if !self.is_registered(&runtime).await {
            return negotiated_open_failure(id, name, source_id, negotiated);
        }
        self.mark_runtime_used(&runtime);
        let response = match mode {
            LifecycleResponseMode::WebSubscribeHandoff if name == "switch_session" => {
                self.subscription_handoff_response(
                    id,
                    name,
                    source_id,
                    &runtime,
                    catalog_identity.as_ref(),
                )
                .await
            }
            LifecycleResponseMode::FullSnapshot | LifecycleResponseMode::WebSubscribeHandoff => {
                self.snapshot_response(id, name, source_id, &runtime, editor_text)
                    .await
            }
        };
        drop(lifecycle);
        response
    }

    /// Web subscribe-handoff for `switch_session`. The returned runtime
    /// identity (`sessionId`/`runtimeIncarnation`) is the ACTUAL opened
    /// runtime — for foreign imports it may differ from the clicked catalog
    /// row — while the validated catalog identity rides alongside so the
    /// frontend can bind the response to its pending target.
    async fn subscription_handoff_response(
        &self,
        id: Option<String>,
        name: &str,
        response_owner: &str,
        runtime: &Arc<SessionRuntime>,
        catalog_identity: Option<&CatalogSwitchIdentity>,
    ) -> RpcResponse {
        let cursor = runtime.published_sequence();
        let Some((session_id, session_path)) = runtime.current_identity() else {
            return negotiated_open_failure(id, name, response_owner, true);
        };
        if session_id != runtime.protocol_id() {
            return negotiated_open_failure(id, name, response_owner, true);
        }
        let notice = runtime
            .resume_notice
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let session = json!({
            "source": pi_coding::SessionSourceKind::NativePi.label(),
            "sessionId": session_id,
            "runtimeIncarnation": runtime.runtime_incarnation(),
            "cwd": runtime.application.session().cwd().to_string_lossy(),
            "path": session_path.to_string_lossy(),
            "status": "native",
            "loaded": true,
        });
        let mut data = json!({
            "sessionId": runtime.protocol_id(),
            "runtimeIncarnation": runtime.runtime_incarnation(),
            "subscriptionRequired": true,
            "lifecycleResponseVersion": 1,
            "cursor": cursor,
            "notice": notice,
            "session": session,
        });
        if let Some(identity) = catalog_identity {
            let object = data.as_object_mut().expect("handoff object");
            object.insert(
                "catalogSource".to_owned(),
                json!(identity.source().wire()),
            );
            object.insert(
                "catalogSessionId".to_owned(),
                json!(identity.session_id()),
            );
            object.insert(
                "catalogStatus".to_owned(),
                json!(identity.status().wire()),
            );
        }
        RpcResponse::success(id, name, Some(data)).with_session_id(response_owner)
    }

    fn web_session_catalog(
        &self,
        source: &SessionRuntime,
    ) -> Result<Option<pi_coding::SessionCatalog>> {
        if let Some(catalog) = self.factory.as_ref().and_then(|factory| factory.catalog()) {
            return Ok(Some(catalog));
        }
        let Some(storage) = self
            .factory
            .as_ref()
            .and_then(|factory| factory.session_storage())
        else {
            return Ok(None);
        };
        storage
            .catalog()
            .map(Some)
            .context("building Web session catalog")
    }

    /// Prepare a saved session for switch/fork/clone. Catalog-backed opens use
    /// [`SessionCatalog::prepare_native_resume`] so a canonical Pi-root path
    /// (including an external symlink alias into that tree) stays read-only
    /// and never becomes an rpi JSONL. Ad-hoc paths without a catalog still
    /// prepare in place.
    fn prepare_native_or_adhoc(
        &self,
        source: &SessionRuntime,
        session_path: &str,
    ) -> Result<pi_coding::PreparedSessionResume> {
        if let Some(catalog) = self.web_session_catalog(source)? {
            let sources = crate::resume_catalog::web_resume_sources(&source.application);
            let resolved = resolve_web_catalog_session_path(
                &catalog,
                source.application.session().cwd(),
                session_path,
                &sources,
            )?;
            return catalog
                .prepare_native_resume(&resolved)
                .map_err(anyhow::Error::new)
                .with_context(|| format!("invalid session path {session_path:?}"));
        }
        pi_coding::PreparedSessionResume::prepare_path(Path::new(session_path))
            .with_context(|| format!("invalid session path {session_path:?}"))
    }

    /// Resolve a negotiated tuple against the EXACT current live catalog row
    /// (source + id + path + status) before any reuse, echo, import, or spawn
    /// participates; returns the live catalog and resume sources so the
    /// caller continues with the SAME authoritative scan. Fails closed when
    /// the catalog is unavailable or no live row matches.
    fn negotiated_live_row(
        &self,
        source: &SessionRuntime,
        identity: &CatalogSwitchIdentity,
        session_path: &Path,
    ) -> Result<(
        pi_coding::CatalogRow,
        pi_coding::SessionCatalog,
        Vec<pi_coding::SessionSourceKind>,
    )> {
        let Some(catalog) = self.web_session_catalog(source)? else {
            bail!("negotiated session selection no longer matches the session catalog");
        };
        let sources = crate::resume_catalog::web_resume_sources(&source.application);
        let row = resolve_exact_catalog_row(&catalog, identity, session_path, &sources)?;
        Ok((row, catalog, sources))
    }
    /// spawn participates, then running the same contained native prepare /
    /// immutable foreign import the legacy flow uses.
    fn prepare_negotiated(
        &self,
        source: &SessionRuntime,
        identity: &CatalogSwitchIdentity,
        session_path: &str,
    ) -> Result<pi_coding::PreparedSessionResume> {
        let (row, catalog, sources) =
            self.negotiated_live_row(source, identity, Path::new(session_path))?;
        let selection = crate::resume_catalog::resolve_resume_selection(
            &catalog,
            &crate::resume_catalog::ResumeSelectionRequest::Target(
                crate::resume_catalog::ResumeSelectorTarget {
                    source: row.kind,
                    source_path: row.path.clone(),
                    status: row.status.clone(),
                },
            ),
            Some(source.application.session().cwd()),
            &sources,
        )
        .map_err(anyhow::Error::new)
        .context("resolving negotiated Web session selection")?;
        catalog
            .prepare_native_resume(&selection.path)
            .map_err(anyhow::Error::new)
            .with_context(|| format!("invalid session path {session_path:?}"))
    }

    /// `switch_session`: reuse an exact manager-owned recorder path before
    /// reading the live append-only file. All other selections are prepared
    /// once through the catalog, deduplicated by retained physical identity,
    /// then transferred into the spawned runtime. No canonicalize or path
    /// reopen participates after preparation.
    /// A negotiated catalog identity binds the open: EVERY negotiated tuple —
    /// including native loaded-runtime reuse — resolves the EXACT current live
    /// catalog row (source + id + path + status) before any reuse, echo,
    /// import, or spawn participates, and only the validated identity reaches
    /// the handoff. The legacy no-tuple shortcut is unchanged.
    async fn open_persisted(
        self: &Arc<Self>,
        source: &Arc<SessionRuntime>,
        session_path: &str,
        catalog_identity: Option<&CatalogSwitchIdentity>,
    ) -> Result<(Arc<SessionRuntime>, Option<CatalogSwitchIdentity>)> {
        let negotiated_native = catalog_identity.is_some_and(|identity| identity.is_native());
        if catalog_identity.is_none() || negotiated_native {
            if let Some(runtime) = self
                .loaded_runtime_for_exact_path(Path::new(session_path))
                .await
            {
                if let Some(identity) = catalog_identity {
                    // Catalog scan validates the clicked tuple before
                    // reuse; fails closed on spoofed or stale rows.
                    self.negotiated_live_row(source, identity, Path::new(session_path))?;
                    let _lifecycle = require_live_runtime(&runtime)?;
                    return match runtime.current_identity() {
                        Some((session_id, _)) if session_id == identity.session_id() => {
                            Ok((runtime, Some(identity.clone())))
                        }
                        _ => bail!(
                            "negotiated session selection no longer matches the loaded session runtime"
                        ),
                    };
                }
                let _lifecycle = require_live_runtime(&runtime)?;
                return Ok((runtime, None));
            }
        }
        let prepared = {
            let _catalog = self.catalog_lock.lock().await;
            match catalog_identity {
                Some(identity) => self.prepare_negotiated(source, identity, session_path)?,
                None => self.prepare_native_or_adhoc(source, session_path)?,
            }
        };
        let file_identity = prepared.identity();
        let open_lock = self.open_lock(file_identity);
        // Bounded open-lane acquisition: a retry that arrives while an
        // earlier creation for this identity is STILL IN FLIGHT (parked past
        // its client deadline) is REFUSED with a bounded error instead of
        // dual-opening the journal.
        let _open = tokio::time::timeout(
            self.switch_open_deadline_cfg(),
            open_lock.clone().lock_owned(),
        )
        .await
        .map_err(|_| anyhow!(bounded_switch_failure(&anyhow!("open already in progress"))))?;
        if let Some(runtime) = self
            .registry
            .read()
            .await
            .by_file
            .get(&file_identity)
            .cloned()
        {
            let _lifecycle = require_live_runtime(&runtime)?;
            return Ok((runtime, catalog_identity.cloned()));
        }
        let target_id = prepared.tree().header.id.clone();
        if let Some(existing) = self
            .existing_runtime_for_id(&target_id, Some(file_identity))
            .await?
        {
            let _lifecycle = require_live_runtime(&existing)?;
            return Ok((existing, catalog_identity.cloned()));
        }
        // Bounded open: the spawn + full protocol snapshot of a large saved
        // session can legitimately take seconds on slow disks, but a switch
        // must NEVER sit pending without a terminal outcome. Past this
        // deadline the client receives the bounded switch failure (toast-able,
        // retryable) — but the deadline does NOT cancel the creation: a
        // watcher owns the open lock (and the in-flight creation future)
        // until create_runtime reaches terminal register-or-rollback, so a
        // retry can NEVER dual-open the journal while the first creation is
        // still alive. After the watcher releases the lock, the retry
        // reuses the registered runtime through the exact-file/identity
        // guards above or re-creates it.
        let manager = self.clone();
        let source = source.clone();
        let mut creation = Box::pin(async move {
            manager.spawn_and_register(SessionSpawnKind::Open { prepared }, &source).await
        });
        match tokio::time::timeout(self.switch_open_deadline_cfg(), &mut creation).await {
            Ok(result) => result.map(|(runtime, _)| (runtime, catalog_identity.cloned())),
            Err(_elapsed) => {
                tokio::spawn(async move {
                    let _ = creation.await;
                    drop(_open); // release the identity lock only at terminal state
                });
                Err(anyhow!(bounded_switch_failure(&anyhow!(
                    "open deadline exceeded"
                ))))
            }
        }
    }

    fn switch_open_deadline_cfg(&self) -> Duration {
        #[cfg(test)]
        {
            return *self
                .switch_open_deadline
                .lock()
                .expect("switch open deadline lock");
        }
        #[allow(unreachable_code)]
        SWITCH_OPEN_DEADLINE
    }

    #[cfg(test)]
    fn set_switch_open_deadline_for_test(&self, deadline: Duration) {
        *self
            .switch_open_deadline
            .lock()
            .expect("switch open deadline lock") = deadline;
    }

    /// Fail-closed acquisition of a per-identity open lane for catalog-plane
    /// mutations (rename/delete/move). The caller holds `catalog_lock`, so
    /// ANY wait on `open_lock` — bounded or not — freezes every other
    /// catalog operation behind one parked Open(X) (the lane is held until
    /// register-or-rollback). The lane is therefore tried once: a busy lane
    /// fails immediately with a readable, path-free error and releases the
    /// catalog plane at once; the client retries later.
    fn try_acquire_open_lane(
        &self,
        open_lock: &Arc<tokio::sync::Mutex<()>>,
        past_tense: &str,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>> {
        open_lock.clone().try_lock_owned().map_err(|_| {
            anyhow!(
                "session could not be {past_tense} right now; an open of this \
                 session is still in progress; retry shortly"
            )
        })
    }

    /// Shared open path: reserve capacity (with LRU retirement when the pool
    /// is full), spawn through the factory, and register. A failed spawn rolls
    /// the reservation back so an LRU victim keeps its slot; a successful
    /// registration retires the victim.
    async fn spawn_and_register(
        self: &Arc<Self>,
        kind: SessionSpawnKind,
        source: &Arc<SessionRuntime>,
    ) -> Result<(Arc<SessionRuntime>, Option<String>)> {
        let (reply, response) = oneshot::channel();
        // Admission token: the closed check and the Create send are one short
        // atomic section covering ONLY the enqueue acceptance. The unbounded
        // lifecycle channel send never blocks, and the creation's own reply
        // is awaited OUTSIDE the token, so the shutdown can close the intake
        // without waiting for any protocol completion. A create is either
        // admitted before the intake closes (joined to terminal completion by
        // the bounded creation join) or rejected here with the exact
        // shutting-down cause.
        if self.lifecycle_closed.load(Ordering::SeqCst) {
            return Err(anyhow!("session manager is shutting down"));
        }
        // The RAII guard keeps the count cancellation-safe across the
        // unbounded channel send: a cancelled admission still releases its
        // in-flight slot. The unbounded lifecycle send never blocks, and the
        // creation's own reply is awaited OUTSIDE the token.
        let admission = InFlightCommandGuard::acquire(&self.admission_inflight);
        let sent = if self.lifecycle_closed.load(Ordering::SeqCst) {
            Err(())
        } else {
            self.lifecycle_tx
                .send(RuntimeLifecycleCommand::Create {
                    kind,
                    source: source.application.clone(),
                    reply,
                })
                .map_err(|_| ())
        };
        drop(admission);
        sent.map_err(|_| anyhow!("session manager is shutting down"))?;
        response
            .await
            .map_err(|_| anyhow!("session runtime creation task stopped"))?
    }

    /// `new_session`: validate any explicit cwd before reserving capacity or
    /// spawning, then build an independent runtime while the source remains
    /// untouched. An explicit absolute directory is an operator-selected new
    /// workspace root — any local directory, not a child of the source
    /// workspace. A missing directory is created (nested paths included)
    /// through the no-follow `prepare_or_create_operator_directory`
    /// capability path and then validated like any existing directory; it
    /// becomes the new runtime's cwd and `WorkspaceRoots` root. An empty cwd
    /// preserves source inheritance.
    async fn create_fresh(
        self: &Arc<Self>,
        source: &Arc<SessionRuntime>,
        requested_cwd: Option<&str>,
    ) -> Result<Arc<SessionRuntime>> {
        let source_session = source.application.session();
        let source_cwd = source_session.cwd();
        let directory = match requested_cwd.map(str::trim).filter(|cwd| !cwd.is_empty()) {
            None => pi_coding::prepare_operator_directory(source_cwd)
                .context("retaining inherited new session cwd")?,
            Some(cwd) => {
                let path = Path::new(cwd);
                if !path.is_absolute() {
                    bail!("new session cwd must be absolute");
                }
                pi_coding::prepare_or_create_operator_directory(path)
                    .context("validating new session cwd")?
            }
        };
        self.spawn_and_register(SessionSpawnKind::Fresh { directory }, source)
            .await
            .map(|(runtime, _)| runtime)
    }

    /// OMP-parity `session.handoff()`: summarize the CURRENT session (one
    /// bounded compaction-summarization call) into a handoff document, create
    /// a NEW session in the SAME cwd whose first record is the handoff
    /// message (`customType: "handoff"`), and answer with the same
    /// subscribe-handoff-v1 switch envelope the web switch consumes. The old
    /// session stays authoritative: any failure before the envelope leaves
    /// the current session untouched, and the response only re-points the
    /// CLIENT at the new runtime.
    async fn session_handoff_command(
        self: &Arc<Self>,
        source: &Arc<SessionRuntime>,
        id: Option<String>,
        name: &str,
        custom_instructions: Option<&str>,
        negotiated: bool,
    ) -> RpcResponse {
        let source_id = source.protocol_id().to_owned();
        // Mirror the TUI guard: a handoff summarizes the live transcript, so
        // an in-flight turn (or compaction riding on it) would fork mid-flight
        // state into the document.
        let state = source.application.state().await;
        if state.is_streaming {
            return RpcResponse::failure(
                id,
                name,
                "session is streaming a turn; wait for it to finish before handing off",
            )
            .with_session_id(source_id);
        }
        // Deterministic envelope (no provider call) plus optional operator
        // instructions.
        let handoff = match source.application.generate_handoff() {
            Ok(handoff) => handoff,
            Err(error) => {
                return RpcResponse::failure(id, name, format!("handoff failed: {error:#}"))
                    .with_session_id(source_id);
            }
        };
        let mut document = handoff.render();
        if let Some(instructions) = custom_instructions.map(str::trim).filter(|text| !text.is_empty())
        {
            document.push_str("\n\n## Operator instructions\n");
            document.push_str(instructions);
        }
        let handoff_content = format!("<handoff-context>\n{document}\n</handoff-context>");

        // NEW session in the SAME cwd (fresh, no resume): the handoff message
        // becomes its first record.
        let runtime = match self.create_fresh(source, None).await {
            Ok(runtime) => runtime,
            Err(error) => {
                // Failure path: the old session stays authoritative; the
                // bounded text never echoes operator paths.
                eprintln!("session_handoff_failed cause=creation_failed");
                return RpcResponse::failure(
                    id,
                    name,
                    bounded_switch_failure(&error),
                )
                .with_session_id(source_id);
            }
        };
        // Deterministic journal append (no run-slot gating): the handoff
        // record is the new session's first content record, visible in the
        // transcript as customType "handoff".
        let handoff_record = json!({
            "text": handoff_content,
        });
        if let Err(error) = runtime
            .application
            .session()
            .append_custom_entry("handoff", Some(handoff_record))
            .and_then(|_| runtime.application.session().persist_recording_now_durable())
        {
            // The handoff record is the whole point of the new session:
            // without it durably in the journal the runtime is an empty
            // fork. Capability-safely discard the freshly created recording
            // (the header-only journal must not linger) and stop the
            // runtime, keeping the old session authoritative. The client
            // failure is a bounded path-free literal; the raw chain only
            // reaches the local log.
            eprintln!("session_handoff_failed cause=record_append_failed");
            if let Err(cleanup) = runtime.application.session().discard_persisted_recording() {
                eprintln!("session_handoff_failed cause=discard_failed error_kind={cleanup}");
            }
            self.shutdown_runtime(&runtime).await;
            return RpcResponse::failure(id, name, bounded_switch_failure(&error))
                .with_session_id(source_id);
        }

        if !negotiated {
            return self
                .lifecycle_result_response(
                    id,
                    name,
                    &source_id,
                    runtime,
                    None,
                    None,
                    LifecycleResponseMode::FullSnapshot,
                )
                .await;
        }
        // The subscribe-handoff-v1 envelope: identical shape to the
        // switch_session handoff, with the NEW session's own native catalog
        // tuple (source `rpi`, native status, session id == runtime id).
        let lifecycle = match runtime.lifecycle_lock.clone().try_read_owned() {
            Ok(lifecycle) => lifecycle,
            Err(_) => {
                return negotiated_open_failure(id, name, &source_id, true);
            }
        };
        if !self.is_registered(&runtime).await {
            return negotiated_open_failure(id, name, &source_id, true);
        }
        self.mark_runtime_used(&runtime);
        let (session_id, _) = runtime
            .current_identity()
            .unwrap_or_else(|| (String::new(), std::path::PathBuf::new()));
        let response = RpcResponse::success(
            id,
            name,
            Some(json!({
                "sessionId": runtime.protocol_id(),
                "runtimeIncarnation": runtime.runtime_incarnation(),
                "subscriptionRequired": true,
                "lifecycleResponseVersion": 1,
                "cursor": runtime.published_sequence(),
                "notice": Option::<String>::None,
                "catalogSource": "rpi",
                "catalogSessionId": session_id,
                "catalogStatus": "native",
            })),
        )
        .with_session_id(source_id);
        drop(lifecycle);
        response
    }

    async fn fork_from(
        self: &Arc<Self>,
        source: &Arc<SessionRuntime>,
        entry_id: &str,
    ) -> Result<(Arc<SessionRuntime>, Option<String>)> {
        self.spawn_and_register(
            SessionSpawnKind::Fork {
                entry_id: entry_id.to_owned(),
            },
            source,
        )
        .await
    }

    async fn clone_from(
        self: &Arc<Self>,
        source: &Arc<SessionRuntime>,
    ) -> Result<Arc<SessionRuntime>> {
        self.spawn_and_register(SessionSpawnKind::Clone, source)
            .await
            .map(|(runtime, _)| runtime)
    }

    async fn export_saved_session(
        &self,
        source: &Arc<SessionRuntime>,
        session_path: &str,
    ) -> Result<(String, String)> {
        let prepared = {
            let _catalog = self.catalog_lock.lock().await;
            self.prepare_saved(source, session_path)
                .await
                .map_err(|error| {
                    let cause = selected_export_prepare_cause(&error);
                    error.context(format!("selected session is unavailable: {cause}"))
                })?
        };
        let html = pi_coding::export::render_saved_session_html(
            prepared.tree(),
            &pi_coding::ExportOptions::default(),
        )
        .map_err(|error| {
            error.context("selected session is unavailable: session cannot be rendered")
        })?;
        if html.len() > MAX_SESSION_DOWNLOAD_BYTES {
            bail!("selected session is unavailable: session export exceeds the byte limit");
        }
        let file_name = session_download_file_name(
            prepared.tree().session_name().as_deref(),
            &prepared.tree().header.id,
        );
        let download_id = self
            .stage_session_download(
                Arc::<[u8]>::from(html.into_bytes()),
                file_name.clone(),
                Instant::now() + SESSION_DOWNLOAD_TTL,
            )
            .await
            .map_err(|error| {
                error.context("selected session is unavailable: download staging failed")
            })?;
        Ok((download_id, file_name))
    }

    /// Render a selected saved session's full context as a bounded,
    /// sanitized, pasteable text artifact. The session is prepared read-only
    /// and never activated, resumed, imported, or recorded. Catalog-backed
    /// selections are classified WITHOUT importing: foreign/input sources
    /// (Pi, OMP, Codex, Claude, Grok, Droid) are rejected with a fixed safe
    /// error before any import could mutate the catalog, while native rpi
    /// rows and ad-hoc paths prepare in place. The response carries the
    /// source runtime's sessionId.
    async fn copy_saved_session_context(
        &self,
        source: &Arc<SessionRuntime>,
        session_path: &str,
    ) -> Result<Value> {
        let prepared = {
            let _catalog = self.catalog_lock.lock().await;
            self.prepare_saved_read_only(source, session_path)
                .await
                .map_err(|error| {
                    let cause = selected_export_prepare_cause(&error);
                    error.context(format!("selected session is unavailable: {cause}"))
                })?
        };
        let rendered = pi_coding::saved_session_context_from_tree(prepared.tree()).map_err(
            |error| error.context("selected session is unavailable: session cannot be rendered"),
        )?;
        drop(prepared);
        Ok(json!({
            "text": rendered.text,
            "format": pi_coding::SESSION_CONTEXT_FORMAT,
            "truncated": rendered.truncated,
        }))
    }

    /// Read-only preparation for copy-context: never imports. With a catalog,
    /// the selection is classified through the non-mutating
    /// [`pi_coding::SessionCatalog::resolve_any`]; a foreign/input source is
    /// rejected with a fixed error (no path text) before any import could
    /// run, and a native rpi path prepares read-only under the catalog.
    /// Without a catalog, the ad-hoc path prepares in place.
    async fn prepare_saved_read_only(
        &self,
        source: &SessionRuntime,
        session_path: &str,
    ) -> Result<pi_coding::PreparedSessionResume> {
        let Some(catalog) = self.web_session_catalog(source)? else {
            return pi_coding::PreparedSessionResume::prepare_path(Path::new(session_path))
                .with_context(|| format!("invalid session path {session_path:?}"));
        };
        let (kind, path) = catalog
            .resolve_any(session_path)
            .map_err(anyhow::Error::new)?;
        if !kind.is_native() {
            return Err(anyhow::Error::new(pi_coding::CatalogError::UnsupportedSource(
                kind.as_str().to_owned(),
            )));
        }
        catalog
            .prepare_native_resume(&path)
            .map_err(anyhow::Error::new)
            .with_context(|| format!("invalid session path {session_path:?}"))
    }

    async fn stage_workspace_download(
        &self,
        source: &Arc<SessionRuntime>,
        path: &str,
    ) -> Result<(String, String)> {
        let application = source.application.clone();
        let path = path.to_owned();
        let (bytes, file_name) = tokio::task::spawn_blocking(move || {
            crate::file_explorer::download(&application, &path, MAX_SESSION_DOWNLOAD_BYTES)
        })
        .await
        .map_err(|error| {
            if error.is_cancelled() {
                anyhow!("file download was cancelled")
            } else {
                anyhow!("file download failed")
            }
        })??;
        let file_name = download_file_name(&file_name);
        let download_id = self
            .stage_session_download(
                Arc::<[u8]>::from(bytes),
                file_name.clone(),
                Instant::now() + SESSION_DOWNLOAD_TTL,
            )
            .await
            .map_err(|_| anyhow!("download staging failed"))?;
        Ok((download_id, file_name))
    }

    pub(crate) async fn stage_session_download(
        &self,
        bytes: Arc<[u8]>,
        file_name: String,
        expires_at: Instant,
    ) -> Result<String> {
        if bytes.len() > MAX_SESSION_DOWNLOAD_BYTES {
            bail!("selected session export exceeds the byte limit");
        }
        if !valid_session_download_file_name(&file_name) {
            bail!("invalid session download filename");
        }
        let now = Instant::now();
        let mut downloads = self.session_downloads.write().await;
        downloads.retain(|_, pending| pending.expires_at > now);
        if downloads.len() >= MAX_STAGED_SESSION_DOWNLOADS {
            bail!("too many pending session downloads");
        }
        let download_id = loop {
            let candidate = uuid::Uuid::new_v4().to_string();
            if !downloads.contains_key(&candidate) {
                break candidate;
            }
        };
        downloads.insert(
            download_id.clone(),
            PendingSessionDownload {
                download: StagedSessionDownload { bytes, file_name },
                expires_at,
            },
        );
        drop(downloads);
        let registry = self.session_downloads.clone();
        let expiring_id = download_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep_until(tokio::time::Instant::from_std(expires_at)).await;
            registry.write().await.remove(&expiring_id);
        });
        Ok(download_id)
    }

    pub(crate) async fn consume_session_download(
        &self,
        download_id: &str,
    ) -> Option<StagedSessionDownload> {
        let pending = self.session_downloads.write().await.remove(download_id)?;
        (pending.expires_at > Instant::now()).then_some(pending.download)
    }

    async fn prepare_saved(
        &self,
        source: &Arc<SessionRuntime>,
        session_path: &str,
    ) -> Result<pi_coding::PreparedSessionResume> {
        self.prepare_native_or_adhoc(source, session_path)
    }

    async fn fork_command(
        self: &Arc<Self>,
        source: &Arc<SessionRuntime>,
        entry_id: &str,
        session_path: Option<&str>,
    ) -> Result<(Arc<SessionRuntime>, Option<String>)> {
        let Some(path) = session_path else {
            return self.fork_from(source, entry_id).await;
        };
        let prepared = {
            let _catalog = self.catalog_lock.lock().await;
            self.prepare_saved(source, path).await?
        };
        let selected = prepared
            .tree()
            .entries
            .iter()
            .find(|entry| entry.id == entry_id)
            .ok_or_else(|| anyhow!("invalid entry id for fork: {entry_id}"))?;
        if selected
            .message
            .as_ref()
            .and_then(fork_selected_editor_text)
            .is_none()
        {
            bail!("invalid entry id for fork: {entry_id}");
        }
        self.spawn_prepared(
            source,
            SessionSpawnKind::ForkPrepared {
                prepared,
                entry_id: entry_id.to_owned(),
            },
        )
        .await
    }

    async fn clone_command(
        self: &Arc<Self>,
        source: &Arc<SessionRuntime>,
        session_path: Option<&str>,
    ) -> Result<Arc<SessionRuntime>> {
        let Some(path) = session_path else {
            return self.clone_from(source).await;
        };
        let prepared = {
            let _catalog = self.catalog_lock.lock().await;
            self.prepare_saved(source, path).await?
        };
        if prepared.tree().active_leaf_id.is_none() {
            bail!("cannot clone session: no current entry selected");
        }
        self.spawn_prepared(source, SessionSpawnKind::ClonePrepared { prepared })
            .await
            .map(|(runtime, _)| runtime)
    }

    async fn fork_latest_from(
        self: &Arc<Self>,
        source: &Arc<SessionRuntime>,
        session_path: &str,
    ) -> Result<(Arc<SessionRuntime>, Option<String>)> {
        let prepared = {
            let _catalog = self.catalog_lock.lock().await;
            self.prepare_saved(source, session_path).await?
        };
        let entry_id = crate::modes::rpc::last_nonempty_user_entry_id(prepared.tree())?;
        self.spawn_prepared(
            source,
            SessionSpawnKind::ForkPrepared { prepared, entry_id },
        )
        .await
    }

    async fn spawn_prepared(
        self: &Arc<Self>,
        source: &Arc<SessionRuntime>,
        kind: SessionSpawnKind,
    ) -> Result<(Arc<SessionRuntime>, Option<String>)> {
        self.spawn_and_register(kind, source).await
    }

    async fn fork_messages_for(
        &self,
        source: &Arc<SessionRuntime>,
        session_path: Option<&str>,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Value> {
        let page = match session_path {
            Some(path) => {
                let prepared = self.prepare_saved(source, path).await?;
                fork_messages_from_tree(prepared.tree(), cursor, limit)?
            }
            None => source.application.fork_messages(cursor, limit)?,
        };
        Ok(serde_json::to_value(page)?)
    }

    async fn rename_saved(
        &self,
        source: &Arc<SessionRuntime>,
        name: &str,
        session_path: Option<&str>,
    ) -> Result<()> {
        if name.is_empty() {
            bail!("Session name cannot be empty");
        }
        let Some(path) = session_path else {
            source.application.set_session_name(name)?;
            return Ok(());
        };
        if let Some(runtime) = self.loaded_runtime_for_exact_path(Path::new(path)).await {
            let lifecycle = runtime
                .lifecycle_lock
                .clone()
                .try_read_owned()
                .map_err(|_| anyhow!("session is busy: retirement in progress"))?;
            if !self.is_registered(&runtime).await {
                bail!("session is no longer loaded");
            }
            let writer = match runtime.application.session().recording_writer_info() {
                Ok(writer) => writer,
                Err(_) => {
                    drop(lifecycle);
                    bail!("session writer is unavailable; reopen the session and retry");
                }
            };
            if writer.1 != Path::new(path) {
                drop(lifecycle);
                bail!("loaded session writer identity changed before rename");
            }
            runtime.application.set_session_name(name)?;
            drop(lifecycle);
            return Ok(());
        }
        let _guard = self.catalog_lock.lock().await;
        let prepared = self.prepare_saved(source, path).await?;
        let open_lock = self.open_lock(prepared.identity());
        // Catalog plane is held above: never wait on the open lane.
        let _open = self.try_acquire_open_lane(&open_lock, "renamed")?;
        if let Some(runtime) = self
            .registry
            .read()
            .await
            .by_file
            .get(&prepared.identity())
            .cloned()
        {
            let lifecycle = runtime
                .lifecycle_lock
                .clone()
                .try_read_owned()
                .map_err(|_| anyhow!("session is busy: retirement in progress"))?;
            if !self.is_registered(&runtime).await {
                bail!("session is no longer loaded");
            }
            let (_, writer_path, writer_identity) = runtime
                .application
                .session()
                .recording_writer_info()
                .context("validating loaded session writer before rename")?;
            if writer_identity != prepared.identity() || writer_path != prepared.path() {
                bail!("loaded session writer identity changed before rename");
            }
            runtime.application.set_session_name(name)?;
            drop(lifecycle);
            return Ok(());
        }
        let path = prepared.path().to_path_buf();
        drop(prepared);
        let recorder = pi_coding::resume_session(&path)?;
        recorder.record_session_name(name)?;
        recorder.close()
    }

    async fn delete_session(
        self: &Arc<Self>,
        source: &Arc<SessionRuntime>,
        id: Option<String>,
        name: &str,
        session_path: &str,
    ) -> RpcResponse {
        let _guard = self.catalog_lock.lock().await;
        let prepared = match self.prepare_saved(source, session_path).await {
            Ok(prepared) => prepared,
            Err(error) => return RpcResponse::failure(id, name, error.to_string()),
        };
        let open_lock = self.open_lock(prepared.identity());
        // Catalog plane is held above: never wait on the open lane.
        let _open = match self.try_acquire_open_lane(&open_lock, "archived") {
            Ok(guard) => guard,
            Err(error) => return RpcResponse::failure(id, name, error.to_string()),
        };
        if source.current_file_identity() == Some(prepared.identity()) {
            return RpcResponse::failure(id, name, "cannot archive the active session");
        }
        let identity = prepared.identity();
        let loaded_runtime = self.registry.read().await.by_file.get(&identity).cloned();
        let should_reopen = loaded_runtime.is_some();
        if let Some(runtime) = loaded_runtime {
            // Fail-fast on the target lifecycle lock: the catalog plane is
            // held above, so awaiting the lock here could stall every other
            // catalog operation behind an in-flight command on the target
            // (whose command task holds the shared guard while itself
            // executing catalog-plane manager commands — a cross-plane
            // deadlock). Fail fast with the exact busy reason instead.
            let Ok(_lifecycle) = runtime.lifecycle_lock.clone().try_write_owned() else {
                return RpcResponse::failure(id, name, "session is busy: a command is in progress");
            };
            if Arc::ptr_eq(&runtime, &self.primary) {
                return RpcResponse::failure(id, name, "cannot archive the primary session");
            }
            if let Err(error) = self.ensure_idle(&runtime).await {
                return RpcResponse::failure(id, name, format!("session is busy: {error}"));
            }
            if let Err(error) = pi_coding::preflight_archive_saved_session(prepared.path()) {
                return RpcResponse::failure(id, name, error.to_string());
            }
            self.shutdown_runtime(&runtime).await;
        }
        let path = prepared.path().to_path_buf();
        drop(prepared);
        let outcome = match pi_coding::archive_saved_session(&path) {
            Ok(outcome) => outcome,
            Err(error) => {
                if should_reopen {
                    let recovery = self.reopen_after_failed_archive(source, &path).await;
                    let message = match recovery {
                        Ok(()) => error.to_string(),
                        Err(recovery_error) => {
                            format!(
                                "{error}; reopening archived session after failure: {recovery_error}"
                            )
                        }
                    };
                    return RpcResponse::failure(id, name, message);
                }
                return RpcResponse::failure(id, name, error.to_string());
            }
        };
        let token = uuid::Uuid::now_v7().to_string();
        self.undo_registry.write().await.insert(
            token.clone(),
            (outcome.clone(), Instant::now() + SESSION_DELETE_UNDO_WINDOW),
        );
        RpcResponse::success(
            id,
            name,
            Some(json!({
                "archived": true,
                "sessionId": outcome.session_id(),
                "undoToken": token,
            })),
        )
    }

    async fn reopen_after_failed_archive(
        self: &Arc<Self>,
        source: &Arc<SessionRuntime>,
        path: &Path,
    ) -> Result<()> {
        let prepared = pi_coding::PreparedSessionResume::prepare_path(path)
            .context("preparing session after failed archive")?;
        self.spawn_and_register(SessionSpawnKind::Open { prepared }, source)
            .await
            .map(|(runtime, _)| runtime)
            .context("opening session after failed archive")?;
        Ok(())
    }

    async fn restore_session(&self, id: Option<String>, name: &str, token: &str) -> RpcResponse {
        let _guard = self.catalog_lock.lock().await;
        let entry = self.undo_registry.write().await.remove(token);
        let Some((outcome, deadline)) = entry else {
            return RpcResponse::failure(id, name, "session restore token is unknown or expired");
        };
        if Instant::now() > deadline {
            return RpcResponse::failure(id, name, "session restore token is unknown or expired");
        }
        match pi_coding::restore_archived_session(&outcome) {
            Ok(()) => RpcResponse::success(
                id,
                name,
                Some(json!({
                    "restored": true,
                    "sessionId": outcome.session_id(),
                })),
            ),
            Err(error) => {
                self.undo_registry
                    .write()
                    .await
                    .insert(token.to_owned(), (outcome, deadline));
                RpcResponse::failure(id, name, error.to_string())
            }
        }
    }

    /// Move one saved native session to a new working directory
    /// (`session_move`). Web control plane only.
    ///
    /// The selection resolves through the catalog authority WITHOUT
    /// importing — foreign sources never move. Only a native unloaded row,
    /// or an idle loaded runtime, may move; any busy loaded runtime is
    /// rejected before the core move. When the selection IS the idle
    /// active/primary runtime, the move performs the TUI `/move` cutover:
    /// the core move relocates the file, then the SAME primary runtime is
    /// rebound to the destination (`switch_session` + workflow rebind +
    /// registry re-key), so the session stays active and its cwd becomes
    /// the target. A loaded secondary is unloaded first, so no ghost loaded
    /// runtime survives the path change. The destination runtime is never
    /// spawned or registered here: for secondary moves the response carries
    /// the move outcome plus the destination catalog row, and the client
    /// performs an explicit `switch_session` afterwards. Catalogs are built
    /// fresh for every read, so the removed source row and the published
    /// destination row are visible immediately.
    async fn move_saved_session(
        self: &Arc<Self>,
        source: &Arc<SessionRuntime>,
        id: Option<String>,
        name: &str,
        session_path: &str,
        target_cwd: &str,
    ) -> RpcResponse {
        let _catalog_guard = self.catalog_lock.lock().await;
        if let Some(outcome) = self.replay_recent_session_move(session_path, target_cwd).await {
            return self.move_outcome_response(source, id, name, &outcome, target_cwd, true);
        }
        let Some(catalog) = self.web_session_catalog(source).ok().flatten() else {
            return RpcResponse::failure(id, name, "session catalog is unavailable");
        };
        let (kind, resolved) = match catalog.resolve_any(session_path) {
            Ok(resolved) => resolved,
            Err(error) => {
                return RpcResponse::failure(id, name, bounded_move_failure(&anyhow::Error::new(error)));
            }
        };
        if !kind.is_native() {
            return RpcResponse::failure(id, name, "only native sessions can be moved");
        }
        let prepared = match catalog.prepare_native_resume(&resolved) {
            Ok(prepared) => prepared,
            Err(error) => {
                return RpcResponse::failure(id, name, bounded_move_failure(&anyhow::Error::new(error)));
            }
        };
        let identity = prepared.identity();
        let open_lock = self.open_lock(identity);
        // Catalog plane is held above: never wait on the open lane.
        let _open = match self.try_acquire_open_lane(&open_lock, "moved") {
            Ok(guard) => guard,
            Err(error) => {
                return RpcResponse::failure(id, name, error.to_string());
            }
        };
        let active_move = source.current_file_identity() == Some(identity);
        let loaded_runtime = self.registry.read().await.by_file.get(&identity).cloned();
        // Admission fence for the active move: the file-conversation
        // collection mutex is taken BEFORE the idle check and held through
        // the disk move, cutover, and recovery, so no file conversation can
        // be opened, prompted, or snapshotted against the owner being
        // replaced. The held engine is handed to the idle check in place —
        // the tokio mutex is not reentrant, so reacquiring it here would
        // self-deadlock. Lock order observed: lifecycle read guard (command
        // admission) → catalog plane and open lane (held above) → this
        // collection → Application internals; nothing between here and the
        // cutover re-enters the collection, and an excluded
        // file-conversation command holds only its lifecycle read guard
        // while waiting, so the fence cannot cycle with admission.
        let mut file_conversation_fence = if active_move {
            Some(source.dispatcher.lock_file_conversations().await)
        } else {
            None
        };
        if active_move {
            // The busy gate for the active runtime precedes the core move
            // and is the authoritative application-state idle check: the
            // command admission already holds this runtime's lifecycle read
            // guard, so a lifecycle write probe is impossible here. The
            // file-conversation fence above stays held through the disk
            // move, cutover, and recovery below.
            if let Err(error) = self
                .ensure_idle_rejecting_queued(
                    source,
                    false,
                    file_conversation_fence.as_deref_mut(),
                )
                .await
            {
                return RpcResponse::failure(id, name, format!("session is busy: {error}"));
            }
        } else if let Some(runtime) = loaded_runtime {
            // Same fail-fast contract as delete: the catalog plane is held
            // above, so the target lifecycle lock must be tried, never
            // awaited — a busy target rejects with the exact busy reason and
            // the catalog plane is released immediately. Invariant: the
            // registry read guard is released before any await below —
            // shutdown_runtime re-enters the registry with a write on this
            // same task.
            let Ok(_lifecycle) = runtime.lifecycle_lock.clone().try_write_owned() else {
                return RpcResponse::failure(id, name, "session is busy: a command is in progress");
            };
            if Arc::ptr_eq(&runtime, &self.primary) {
                // Anomalous dual registration: two live runtimes on one file
                // identity can never be safely cut over — fail closed.
                return RpcResponse::failure(id, name, "cannot move the active session");
            }
            if let Err(error) = self.ensure_idle(&runtime).await {
                return RpcResponse::failure(id, name, format!("session is busy: {error}"));
            }
            self.shutdown_runtime(&runtime).await;
        }
        let path = prepared.path().to_path_buf();
        let session_id = prepared.tree().header.id.clone();
        drop(prepared);
        match pi_coding::session_migration::move_saved_session(&path, target_cwd) {
            Ok(outcome) => {
                if active_move
                    && let Err(cutover) = self.cutover_active_session(source, &outcome).await
                {
                    // The files are moved, so the only consistent end state is
                    // this runtime rebound to the moved destination. Finish the
                    // cutover in-process on the SAME runtime; the response may
                    // only advertise reopen when that recovery actually
                    // rebinds the live runtime — an unrecovered cutover leaves
                    // the runtime keyed on the deleted source identity, and a
                    // reopen would be rejected against it. The failure chain
                    // embeds operator paths and never reaches the client.
                    if self
                        .recover_active_move_cutover(source, &outcome)
                        .await
                        .is_err()
                    {
                        return RpcResponse::failure(
                            id,
                            name,
                            "session moved but the active cutover failed",
                        );
                    }
                }
                self.remember_session_move(session_path, target_cwd, &outcome).await;
                self.move_outcome_response(source, id, name, &outcome, target_cwd, true)
            }
            Err(error) => {
                if error
                    .chain()
                    .any(|cause| {
                        cause
                            .to_string()
                            .contains("already resides in the target session directory")
                    })
                {
                    // Idempotent retry: the session already lives in the
                    // target directory's session store (same id, target cwd).
                    let resident = pi_coding::session_migration::SessionMoveOutcome {
                        session_id,
                        from: path.clone(),
                        to: path,
                        sidecars: 0,
                        children: 0,
                        rebound: Default::default(),
                        warnings: Vec::new(),
                    };
                    return self.move_outcome_response(source, id, name, &resident, target_cwd, false);
                }
                RpcResponse::failure(id, name, bounded_move_failure(&error))
            }
        }
    }

    /// `/move` cutover for the idle active runtime: rebind the SAME primary
    /// runtime to its moved destination through the shared application
    /// switch path (the exact mechanism the TUI `/move` cutover uses), then
    /// rebind workflow storage and re-key the registry onto the moved file
    /// identity. Invariant: the runtime object, protocol id, and loaded
    /// slot survive the cutover — only the recorded session file and the
    /// workspace cwd change, so in-flight Web frames keep routing and the
    /// session stays active.
    async fn cutover_active_session(
        self: &Arc<Self>,
        source: &Arc<SessionRuntime>,
        outcome: &pi_coding::session_migration::SessionMoveOutcome,
    ) -> Result<()> {
        // The cross-directory cutover replaces the application runtime
        // generation and aborts the old one. Queue migration is owned by
        // install_session_replacement (drain-and-requeue with fresh journal
        // markers), so NO pre-drain happens here — draining twice would
        // double-enqueue every acknowledged message.
        // Latch the identity transition: until the authoritative re-key
        // below validates the writer at the destination, event-driven
        // update_identity calls must not observe the transient post-move
        // writer state (the source file was renamed away) — that would
        // unregister the runtime and abort its owner with this command
        // still in flight. A failed or cancelled cutover recovers by
        // re-keying when the writer is valid again; an unvalidatable writer
        // keeps the latch so a reopen of the moved row fails closed.
        source
            .identity_rekey_suppressed
            .store(true, std::sync::atomic::Ordering::Release);
        let result = source
            .application
            .switch_session(&outcome.to)
            .await
            .context("switching the active session to its moved destination");
        // Late drain, closing the drain→switch window: a user steer or
        // follow-up enqueued WHILE the switch was in flight lands in
        // whichever generation was live and would otherwise die with the
        // aborted generation. After the switch settles (success, failure, or
        // mapped-cancelled), drain the rebound/still-live runtime once and
        // re-enqueue what it holds into the final generation. Pre-existing
        // messages were already migrated by install_session_replacement.
        let (steering, follow_up) = source.application.drain_queued_messages().await;
        let mut result = match result {
            Ok(switch) if switch.cancelled => Err(anyhow!(
                "active session move cutover was cancelled"
            )),
            Ok(_) => {
                self.rekey_identity(source).await;
                crate::session_run::rebind_workflows_for_active_session(&source.application)
                    .await
                    .context("rebinding workflow storage after the active session move")
            }
            Err(error) => Err(error),
        };
        if result.is_err() {
            if source.application.session().recording_writer_info().is_ok() {
                self.rekey_identity(source).await;
            } else {
                eprintln!(
                    "session_move category=identity_rekey_suppressed session_id={} runtime_incarnation={}",
                    source.protocol_id(),
                    source.runtime_incarnation()
                );
            }
        }
        let reenqueue = Self::reenqueue_drained_messages(&source.application, steering, follow_up).await;
        if result.is_ok() {
            result = reenqueue;
        } else if let Err(error) = reenqueue {
            // The cutover already failed with its own error; the re-enqueue
            // failure is secondary but must never be silent.
            eprintln!("session_move category=queued_reenqueue_failed error={error:#}");
        }
        result
    }

    async fn reenqueue_drained_messages(
        application: &pi_coding::Application,
        steering: Vec<pi_ai::Message>,
        follow_up: Vec<pi_ai::Message>,
    ) -> Result<()> {
        let session = application.session();
        // The late-drained rows still carry their journal markers (re-steered
        // fresh by install_session_replacement or the old generation). Every
        // row below is re-journaled through steer/follow_up, so retire the
        // old markers first — a stale marker would requeue the delivered
        // message again on the next resume.
        session.retire_pending_steering_markers("moved").await;
        for message in steering {
            if let Err(first_error) = session.steer(message.clone()).await {
                session
                    .steer(message)
                    .await
                    .with_context(|| format!(
                        "re-enqueueing a queued steering prompt after the session move \
                         (first attempt failed: {first_error:#})"
                    ))?;
            }
        }
        for message in follow_up {
            if let Err(first_error) = session.follow_up(message.clone()).await {
                session
                    .follow_up(message)
                    .await
                    .with_context(|| format!(
                        "re-enqueueing a queued follow-up prompt after the session move \
                         (first attempt failed: {first_error:#})"
                    ))?;
            }
        }
        Ok(())
    }

    /// In-process recovery for a failed active-move cutover after the core
    /// file move already succeeded. The moved file is durable, so the only
    /// consistent end state is the SAME runtime rebound to the moved
    /// destination — never a fresh reopen against a live runtime still keyed
    /// on the deleted source identity (that reopen is rejected as dual
    /// registration and the primary cannot be closed). Two failure shapes:
    /// the switch already landed and only a post-switch step failed (the
    /// registry re-key already ran — re-key again, it is idempotent, and
    /// retry the workflow rebind), or the switch never landed (retry the
    /// whole cutover once; the runtime was idled before the core move and
    /// the command admission lifecycle guard is still held, so nothing can
    /// have made it busy in between).
    async fn recover_active_move_cutover(
        self: &Arc<Self>,
        source: &Arc<SessionRuntime>,
        outcome: &pi_coding::session_migration::SessionMoveOutcome,
    ) -> Result<()> {
        let destination = pi_coding::PreparedSessionResume::prepare_path(&outcome.to)
            .context("preparing the moved destination for cutover recovery")?
            .identity();
        if source.current_file_identity() == Some(destination) {
            self.rekey_identity(source).await;
            return crate::session_run::rebind_workflows_for_active_session(&source.application)
                .await
                .context("rebinding workflow storage after the active session move");
        }
        self.cutover_active_session(source, outcome).await
    }

    /// Replay a completed `session_move` for an identical retry. After a
    /// successful move the original selection no longer resolves (the file
    /// is gone), so the recorded outcome is the only way a same-input retry
    /// can succeed idempotently.
    async fn replay_recent_session_move(
        &self,
        session_path: &str,
        target_cwd: &str,
    ) -> Option<pi_coding::session_migration::SessionMoveOutcome> {
        let mut moves = self.recent_session_moves.write().await;
        let now = Instant::now();
        moves.retain(|_, (_, deadline)| *deadline > now);
        moves
            .get(&(session_path.to_owned(), target_cwd.to_owned()))
            .map(|(outcome, _)| outcome.clone())
    }

    async fn remember_session_move(
        &self,
        session_path: &str,
        target_cwd: &str,
        outcome: &pi_coding::session_migration::SessionMoveOutcome,
    ) {
        let mut moves = self.recent_session_moves.write().await;
        let now = Instant::now();
        moves.retain(|_, (_, deadline)| *deadline > now);
        if moves.len() >= MAX_SESSION_MOVE_REPLAYS {
            if let Some(oldest) = moves
                .iter()
                .min_by_key(|(_, (_, deadline))| *deadline)
                .map(|(key, _)| key.clone())
            {
                moves.remove(&oldest);
            }
        }
        moves.insert(
            (session_path.to_owned(), target_cwd.to_owned()),
            (outcome.clone(), now + SESSION_MOVE_REPLAY_WINDOW),
        );
    }

    /// Success payload for `session_move`: the bounded move outcome plus the
    /// destination `session_list` row scanned from the native sessions store
    /// (the store every core move publishes into).
    fn move_outcome_response(
        &self,
        source: &Arc<SessionRuntime>,
        id: Option<String>,
        name: &str,
        outcome: &pi_coding::session_migration::SessionMoveOutcome,
        target_cwd: &str,
        moved: bool,
    ) -> RpcResponse {
        let row = self.destination_session_row(source, outcome, target_cwd);
        RpcResponse::success(
            id,
            name,
            Some(json!({
                "moved": moved,
                "sessionId": outcome.session_id,
                "from": outcome.from.to_string_lossy(),
                "to": outcome.to.to_string_lossy(),
                "sidecars": outcome.sidecars,
                "children": outcome.children,
                "rebound": {
                    "memory": outcome.rebound.memory,
                    "workflows": outcome.rebound.workflows,
                    "trust": outcome.rebound.trust,
                },
                "warnings": outcome.warnings,
                "session": row,
            })),
        )
    }

    /// The `session_list`-shaped row for a destination path. Scanned from a
    /// catalog re-pointed at the native sessions store; a missing scan row
    /// (catalog roots changed between the move and the scan) degrades to the
    /// fields the outcome already proves, never an error.
    fn destination_session_row(
        &self,
        source: &Arc<SessionRuntime>,
        outcome: &pi_coding::session_migration::SessionMoveOutcome,
        target_cwd: &str,
    ) -> Value {
        let catalog = self
            .web_session_catalog(source)
            .ok()
            .flatten()
            .map(|catalog| catalog.with_native_session_root(pi_coding::native_sessions_root()));
        if let Some(catalog) = catalog.as_ref() {
            if let Some(row) = catalog
                .scan(&[pi_coding::SessionSourceKind::NativePi])
                .into_iter()
                .find(|row| row.path == outcome.to)
            {
                return session_list_row_value(row);
            }
        }
        let size = std::fs::metadata(&outcome.to)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        json!({
            "source": pi_coding::SessionSourceKind::NativePi.label(),
            "sessionId": outcome.session_id,
            "name": Value::Null,
            "cwd": target_cwd,
            "displayTime": "",
            "modifiedEpoch": 0.0,
            "summary": "",
            "path": outcome.to.to_string_lossy(),
            "size": size,
            "messageCount": Value::Null,
            "status": "native",
            "temporary": false,
        })
    }

    async fn register_spawned(
        self: &Arc<Self>,
        result: &SessionSpawnResult,
        mut reservation: LoadedSlotReservation,
    ) -> std::result::Result<
        std::result::Result<(Arc<SessionRuntime>, Option<String>), Arc<SessionRuntime>>,
        anyhow::Error,
    > {
        // Registration outcomes: full publication, or an identical runtime
        // already registered (the guard-owned duplicate candidate is
        // discarded by the caller). Errors leave `result` owned by the
        // caller's rollback guard.
        enum SpawnRegistration {
            Published,
            Duplicate(Arc<SessionRuntime>),
        }
        if let Err(error) = result.application.session().workspace_roots().validate_authorities() {
            return Err(error);
        }
        let writer = match result.application.session().recording_writer_info() {
            Ok(writer) => writer,
            Err(error) => return Err(error),
        };
        if writer.2 != result.file_identity {
            return Err(anyhow!(
                "spawned session append writer changed before registration"
            ));
        }
        if (writer.0.as_str(), writer.1.as_path())
            != (result.session_id.as_str(), result.session_file.as_path())
        {
            return Err(anyhow!(
                "spawned session recorder identity changed before registration"
            ));
        }
        let session_id = result.session_id.clone();
        let storage = self
            .factory
            .as_ref()
            .and_then(|factory| factory.session_storage());
        let extension_ui = result.extension_ui.clone();
        let resume_notice = result.resume_notice.clone();
        let editor_text = result.editor_text.clone();
        let (spawned_runtime, protocol_rx) = match SessionRuntime::new(
            result.application.clone(),
            extension_ui,
            storage,
            resume_notice,
            None,
        ) {
            Ok(runtime) => runtime,
            Err(error) => return Err(error),
        };
        let runtime = Arc::new(spawned_runtime);
        self.mark_runtime_used(&runtime);
        // ONE Result type for the whole loop: `Ok` carries a publication or
        // the duplicate discovery, `Err` the hard failure — the break arms
        // can never mix error payload types.
        let registration: std::result::Result<SpawnRegistration, anyhow::Error> = loop {
            let existing = self.registry.read().await.by_id.get(&session_id).cloned();
            let existing_identity = existing
                .as_ref()
                .and_then(|runtime| runtime.current_file_identity());
            let mut registry = self.registry.write().await;
            // The intake-shut check and the registry insert are ONE critical
            // section against the shutdown's registry retain (which takes
            // the same write lock): a create either registers while the
            // intake is still open — and then lands in the shutdown's
            // runtime snapshot and is aborted/cleaned with the rest — or it
            // fails closed here and the create_runtime failure arm cleans
            // and discards it. Either way, no registry zombie can survive a
            // terminal shutdown and no create can succeed after it.
            if self.lifecycle_tx.is_closed() {
                break Err(anyhow!("session manager is shutting down"));
            }
            match registry.by_id.get(&session_id) {
                Some(current) => {
                    let Some(observed) = existing.as_ref() else {
                        continue;
                    };
                    if !Arc::ptr_eq(current, observed) {
                        continue;
                    }
                    break if existing_identity == Some(result.file_identity.clone()) {
                        Ok(SpawnRegistration::Duplicate(observed.clone()))
                    } else {
                        Err(anyhow!(
                            "session id {session_id} is already loaded from another session file"
                        ))
                    };
                }
                None => {
                    if let Some((victim, _)) = reservation.victim.as_ref() {
                        registry.remove(victim);
                    }
                    registry.by_id.insert(session_id.clone(), runtime.clone());
                    registry
                        .by_file
                        .insert(result.file_identity.clone(), runtime.clone());
                    break Ok(SpawnRegistration::Published);
                }
            }
        };
        let duplicate = match registration {
            Ok(SpawnRegistration::Published) => {
                let permit = reservation.take_permit();
                runtime.restore_loaded_slot(permit);
                None
            }
            Ok(SpawnRegistration::Duplicate(existing)) => Some(existing),
            Err(error) => return Err(error),
        };
        if let Some(existing) = duplicate {
            return Ok(Err(existing));
        }
        let retired = reservation.commit();
        #[cfg(test)]
        let _registration_gate = self.registration_gate.read().await;
        let (ready_tx, ready_rx) = oneshot::channel();
        SessionRuntime::start_owner(
            self,
            &runtime,
            protocol_rx,
            self.events.clone(),
            self.ui_events.clone(),
            ready_tx,
        );
        if ready_rx.await.is_err() {
            runtime.abort_owner();
            let _ = runtime.application.cleanup().await;
            return Err(anyhow!("session protocol owner failed to start"));
        }
        if let Some((victim, lifecycle)) = retired {
            let _ = self.events.send(session_unloaded_event(&victim, RuntimeUnloadReason::Lru));
            let manager = self.clone();
            self.cleanups.track(tokio::spawn(async move {
                manager.retire_runtime(victim, Some(lifecycle), true).await;
            }));
        }
        Ok(Ok((runtime, editor_text)))
    }

    /// `close_session`: requires a sessionId; rejects the primary and rejects
    /// busy secondaries WITHOUT cancelling any active work; only an idle
    /// secondary is cleaned up and removed from the registry.
    async fn close_session(
        self: &Arc<Self>,
        id: Option<String>,
        name: &str,
        session_id: Option<String>,
    ) -> RpcResponse {
        let Some(sid) = session_id else {
            return RpcResponse::failure(id, name, "close_session requires a sessionId");
        };
        let failure = |message: String| {
            RpcResponse::failure(id.clone(), name, message).with_session_id(sid.clone())
        };
        let Some(runtime) = self.registry.read().await.by_id.get(&sid).cloned() else {
            return failure(format!("unknown session {sid}"));
        };
        let Ok(lifecycle) = runtime.lifecycle_lock.clone().try_write_owned() else {
            return failure("session is busy: a command is in progress".to_owned());
        };
        if Arc::ptr_eq(&runtime, &self.primary) {
            return failure("cannot close the primary TUI session".to_owned());
        }
        if let Err(error) = self.ensure_idle(&runtime).await {
            return failure(format!("session is busy: {error}"));
        }
        let incarnation = runtime.runtime_incarnation().to_owned();
        self.unregister_runtime(&runtime).await;
        drop(runtime.take_loaded_slot());
        let _ = self
            .events
            .send(session_unloaded_event(&runtime, RuntimeUnloadReason::Closed));
        let manager = self.clone();
        self.cleanups.track(tokio::spawn(async move {
            manager.retire_runtime(runtime, Some(lifecycle), false).await;
        }));
        RpcResponse::success(
            id,
            name,
            Some(json!({
                "closed": true,
                "sessionId": sid,
                "runtimeIncarnation": incarnation,
            })),
        )
        .with_session_id(sid)
    }

    /// Conservative idle check: close must never silently cancel user work.
    async fn ensure_idle(self: &Arc<Self>, runtime: &Arc<SessionRuntime>) -> Result<()> {
        self.ensure_idle_rejecting_queued(runtime, true, None).await
    }

    async fn ensure_idle_rejecting_queued(
        self: &Arc<Self>,
        runtime: &Arc<SessionRuntime>,
        reject_queued_messages: bool,
        held_file_conversations: Option<&mut FileConversationEngine>,
    ) -> Result<()> {
        let application = &runtime.application;
        if application.is_bash_running() {
            bail!("foreground bash is running");
        }
        let state = application.state().await;
        if state.is_streaming {
            bail!("a turn is in progress");
        }
        if state.is_compacting {
            bail!("compaction is in progress");
        }
        if reject_queued_messages && state.pending_message_count > 0 {
            bail!("queued messages are pending");
        }
        if application
            .process_list()
            .iter()
            .any(|process| !process.state.is_terminal())
        {
            bail!("supervised processes are running");
        }
        if application
            .goal_state()
            .current
            .is_some_and(|goal| !goal.lifecycle.is_terminal())
        {
            bail!("a goal is active or paused");
        }
        let todo = application.todo_state();
        if todo.phases.iter().any(|phase| {
            phase
                .tasks
                .iter()
                .any(|task| task.status == pi_coding::todo::TodoStatus::InProgress)
        }) {
            bail!("todo tasks are in progress");
        }
        if let Ok(manager) = application.workflow_manager()
            && manager
                .list()
                .iter()
                .any(|workflow| workflow.status.is_active())
        {
            bail!("workflows are running");
        }
        if let Some(orchestration) = application.orchestration_runtime()
            && orchestration
                .jobs(None)
                .iter()
                .any(|job| !job.status.is_settled())
        {
            bail!("orchestration jobs are running");
        }
        if let Ok(loops) = application.loop_list().await
            && !loops.is_empty()
        {
            bail!("loops are scheduled or running");
        }
        if runtime.dispatcher.side_chat_busy().await {
            bail!("a side-chat turn is in progress");
        }
        if runtime.dispatcher.code_review_busy().await {
            bail!("a code-review turn is in progress");
        }
        // File conversations ride the same idle gate: a forked side
        // controller owns live work in this runtime. The active-move fence
        // passes its already-held collection engine in place — reacquiring
        // the non-reentrant mutex here would self-deadlock — while every
        // other caller locks the collection normally. Owner-capture errors
        // propagate so the caller surfaces the real reason instead of
        // guessing busy or idle.
        let file_conversations_busy = match held_file_conversations {
            Some(engine) => FileConversationRpcState::busy_with(engine, application).await,
            None => runtime.dispatcher.file_conversation_busy().await,
        };
        match file_conversations_busy {
            Err(error) => bail!("file conversation state check failed: {}", error.message()),
            Ok(true) => bail!("a file conversation is streaming"),
            Ok(false) => {}
        }
        Ok(())
    }

    async fn unregister_runtime(&self, runtime: &Arc<SessionRuntime>) {
        self.registry.write().await.remove(runtime);
    }
    async fn cleanup_runtime_contents(&self, runtime: &Arc<SessionRuntime>) -> Result<()> {
        #[cfg(test)]
        runtime.cleanup_count.fetch_add(1, Ordering::SeqCst);
        runtime.dispatcher.shutdown_side_chat().await;
        runtime.dispatcher.shutdown_code_review().await;
        // File conversations own forked controllers with live turns and
        // private side sessions; they must be quiesced and drained BEFORE
        // the application teardown they were forked from.
        runtime.dispatcher.shutdown_file_conversations().await;
        runtime.application.cleanup().await
    }

    async fn shutdown_runtime_contents(
        self: &Arc<Self>,
        runtime: &Arc<SessionRuntime>,
    ) -> Result<()> {
        self.unregister_runtime(runtime).await;
        self.cleanup_runtime_contents(runtime).await
    }

    async fn shutdown_runtime(self: &Arc<Self>, runtime: &Arc<SessionRuntime>) -> Result<()> {
        runtime.abort_owner();
        self.shutdown_runtime_contents(runtime).await
    }
    pub(crate) async fn shutdown(&self) -> Result<()> {
        self.shutdown_with_restart(false).await
    }

    pub(crate) async fn shutdown_for_restart(&self) -> Result<()> {
        self.shutdown_with_restart(true).await
    }

    async fn shutdown_with_restart(&self, is_restart: bool) -> Result<()> {
        let mut terminal = self.shutdown_terminal.clone();
        if let Some(result) = terminal.borrow().as_ref() {
            return clone_terminal_shutdown(result);
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .lifecycle_tx
            .send(RuntimeLifecycleCommand::Shutdown {
                is_restart,
                reply: reply_tx,
            })
            .is_err()
        {
            // The owner already accepted a concurrent/cancelled caller's
            // Shutdown and closed its command channel: wait for and replay
            // the ONE terminal result it will publish.
            return wait_for_terminal_shutdown(&mut terminal).await;
        }
        let result = match reply_rx.await {
            Ok(result) => result,
            // Our Shutdown was queued behind an in-flight terminal shutdown
            // (the owner exits after the first) or the owner died before
            // replying: replay the ONE terminal result either way.
            Err(_) => return wait_for_terminal_shutdown(&mut terminal).await,
        };
        while terminal.borrow().is_none() {
            terminal
                .changed()
                .await
                .map_err(|_| anyhow!("session manager shutdown owner exited before completion"))?;
        }
        result
    }

    /// Legacy lifecycle response contract `{ sessionId, state, messages, session }`:
    /// target identity, full RPC state, the public transcript straight from
    /// the backend recorder, and the authoritative target `session` row
    /// shaped exactly like a `session_list` native loaded row so the Web
    /// sidebar can inject the target immediately (no disk rescan, no catalog
    /// delay). The top-level response envelope is TARGET-owned on success —
    /// the caller routes an immediate follow-up fork by the returned
    /// `sessionId` — while failures stay request-source-owned. WebSocket
    /// `switch_session` bypasses this path and hands transcript authority to
    /// the subsequent subscription. Fork / SessionForkLatest additionally
    /// include one-shot `editorText` (the selected user prompt). Switch, new,
    /// and clone omit that key.
    async fn snapshot_response(
        self: &Arc<Self>,
        id: Option<String>,
        name: &str,
        response_owner: &str,
        runtime: &Arc<SessionRuntime>,
        editor_text: Option<String>,
    ) -> RpcResponse {
        #[cfg(test)]
        self.protocol_snapshot_count.fetch_add(1, Ordering::SeqCst);
        let session_id = runtime.protocol_id().to_owned();
        let result: Result<Value> = async {
            let snapshot = runtime.application.protocol_snapshot().await;
            let state = RpcSessionState::from_application(
                snapshot.state,
                snapshot.active_turn_model,
                snapshot.runtime_settings.state(),
                &snapshot.cwd,
                Some(session_id.clone()),
            );
            let messages: Vec<Message> =
                snapshot.messages.into_iter().map(public_message).collect();
            let notice = runtime
                .resume_notice
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            let session_row = lifecycle_target_session_row(
                &session_id,
                runtime.runtime_incarnation(),
                &state,
            );
            let mut data = json!({
                "sessionId": session_id.clone(),
                "runtimeIncarnation": runtime.runtime_incarnation(),
                "state": state,
                "messages": messages,
                "notice": notice,
                "epoch": snapshot.epoch,
                "lifecycle": snapshot.lifecycle,
            });
            if let Some(object) = data.as_object_mut() {
                object.insert("session".to_owned(), session_row);
            }
            if let Some(editor_text) = editor_text
                && let Some(object) = data.as_object_mut()
            {
                object.insert("editorText".to_owned(), Value::String(editor_text));
            }
            Ok(data)
        }
        .await;
        match result {
            Ok(data) => RpcResponse::success(id, name, Some(data)).with_session_id(session_id),
            Err(error) => {
                RpcResponse::failure(id, name, bounded_lifecycle_snapshot_failure(&error))
                    .with_session_id(response_owner)
            }
        }
    }

    /// Mark rows of `session_list` whose session is currently loaded, so the
    /// Web sidebar can reconcile live sessions against the persisted catalog.
    async fn overlay_loaded_sessions(self: &Arc<Self>, response: RpcResponse) -> RpcResponse {
        let Some(mut data) = response.data else {
            return response;
        };
        if data.get("sessions").is_none() {
            return RpcResponse {
                data: Some(data),
                ..response
            };
        }
        let registry = self.registry.read().await;
        let loaded_ids = registry.by_id.keys().cloned().collect::<HashSet<_>>();
        let runtimes = registry.distinct_runtimes();
        drop(registry);
        let Some(sessions) = data.get_mut("sessions").and_then(Value::as_array_mut) else {
            return RpcResponse {
                data: Some(data),
                ..response
            };
        };
        let mut canonical_overlaid = HashSet::new();
        for row in sessions.iter_mut() {
            let session_id = match row.get("sessionId").and_then(Value::as_str) {
                Some(session_id) => session_id.to_owned(),
                None => continue,
            };
            let imported_native_id = row
                .get("nativeSessionId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let runtime_session_id = imported_native_id.as_deref().unwrap_or(&session_id);
            if !loaded_ids.contains(runtime_session_id) {
                continue;
            }
            let runtime = match runtimes
                .iter()
                .find(|runtime| runtime.protocol_id() == runtime_session_id)
            {
                Some(runtime) => runtime,
                None => continue,
            };
            let Some((_, canonical_path)) = runtime.current_identity() else {
                continue;
            };
            if row.get("path").and_then(Value::as_str)
                != Some(canonical_path.to_string_lossy().as_ref())
                && row.get("nativePath").and_then(Value::as_str)
                    != Some(canonical_path.to_string_lossy().as_ref())
            {
                continue;
            }
            let Some(object) = row.as_object_mut() else {
                continue;
            };
            if imported_native_id.is_none() {
                object.insert(
                    "source".to_owned(),
                    json!(pi_coding::SessionSourceKind::NativePi.label()),
                );
                object.insert("status".to_owned(), json!("native"));
            }
            object.insert("loaded".to_owned(), json!(true));
            object.insert(
                "runtimeIncarnation".to_owned(),
                json!(runtime.runtime_incarnation()),
            );
            if imported_native_id.is_none() {
                canonical_overlaid.insert(runtime_session_id.to_owned());
            }
        }
        for runtime in runtimes {
            let Some((session_id, path)) = runtime.current_identity() else {
                continue;
            };
            if canonical_overlaid.contains(&session_id) {
                continue;
            }
            let state = runtime.application.state().await;
            sessions.push(json!({
                "source": pi_coding::SessionSourceKind::NativePi.label(),
                "sessionId": session_id,
                "runtimeIncarnation": runtime.runtime_incarnation(),
                "name": state.session_name,
                "cwd": runtime.application.session().cwd().to_string_lossy(),
                "displayTime": "",
                "modifiedEpoch": 0.0,
                "summary": "",
                "path": path.to_string_lossy(),
                "size": 0,
                "messageCount": state.message_count,
                "status": "native",
                "loaded": true,
            }));
        }
        RpcResponse {
            data: Some(data),
            ..response
        }
    }

    async fn retire_runtime(
        self: &Arc<Self>,
        runtime: Arc<SessionRuntime>,
        lifecycle: Option<OwnedRwLockWriteGuard<()>>,
        abort_owner: bool,
    ) {
        let _permit = match self.retirement_slots.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => match self.retirement_slots.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => {
                    eprintln!("session_open_failed category=retirement_unavailable");
                    if abort_owner {
                        runtime.abort_owner();
                    }
                    drop(lifecycle);
                    drop(runtime);
                    return;
                }
            },
        };
        if let Err(error) = runtime.finish_owner(abort_owner).await {
            eprintln!("session_open_failed category=retirement_join error={error}");
            #[cfg(test)]
            self.retirement_timeouts.fetch_add(1, Ordering::SeqCst);
        }
        if let Err(error) = self.cleanup_runtime_contents(&runtime).await {
            eprintln!("session_open_failed category=retirement_cleanup error={error}");
            #[cfg(test)]
            self.retirement_timeouts.fetch_add(1, Ordering::SeqCst);
        }
        drop(lifecycle);
        drop(runtime);
    }
}

fn session_unloaded_event(runtime: &SessionRuntime, reason: RuntimeUnloadReason) -> Value {
    json!({
        "type": "session_unloaded",
        "sessionId": runtime.protocol_id(),
        "runtimeIncarnation": runtime.runtime_incarnation(),
        "reason": reason,
    })
}

/// Authoritative `session_list`-shaped native row for a lifecycle target,
/// built from the target runtime's live state. `cwd`/`path` are the exact
/// target state cwd and session file; `name`/`messageCount` carry the
/// bounded authoritative values; `displayTime`/`summary` stay empty rather
/// than guessed. Never scans the disk or waits on the catalog.
fn lifecycle_target_session_row(
    session_id: &str,
    runtime_incarnation: &str,
    state: &RpcSessionState,
) -> Value {
    json!({
        "source": pi_coding::SessionSourceKind::NativePi.label(),
        "sessionId": session_id,
        "runtimeIncarnation": runtime_incarnation,
        "name": state.session_name,
        "cwd": state.cwd,
        "displayTime": "",
        "modifiedEpoch": 0.0,
        "summary": "",
        "path": state.session_file,
        "size": 0,
        "messageCount": state.message_count,
        "status": "native",
        "loaded": true,
    })
}

fn tag_response(response: RpcResponse, session_id: Option<&str>) -> RpcResponse {
    match session_id {
        Some(session_id) => response.with_session_id(session_id),
        None => response,
    }
}

fn selected_export_prepare_cause(error: &anyhow::Error) -> &'static str {
    for source in error.chain() {
        if let Some(error) = source.downcast_ref::<pi_coding::CatalogError>() {
            return match error {
                pi_coding::CatalogError::UnsupportedSource(_) => "session source is unsupported",
                pi_coding::CatalogError::HomeDirectoryUnavailable => {
                    "session catalog is unavailable"
                }
                pi_coding::CatalogError::EmptySearchQuery => "session selection is invalid",
                pi_coding::CatalogError::SessionNotFound(_) => "session was not found",
                pi_coding::CatalogError::AmbiguousSession { .. } => {
                    "session selection is ambiguous"
                }
                pi_coding::CatalogError::ReadOnlySource(_) => "session source is read-only",
                pi_coding::CatalogError::Import(error) => selected_export_import_cause(error),
                pi_coding::CatalogError::WorkflowInternalSession => {
                    "workflow-internal session cannot be exported"
                }
                pi_coding::CatalogError::Io { source, .. }
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    "session was not found"
                }
                pi_coding::CatalogError::Io { .. } => "session could not be read",
            };
        }
        if let Some(error) = source.downcast_ref::<pi_coding::ImportSessionError>() {
            return selected_export_import_cause(error);
        }
        if let Some(error) = source.downcast_ref::<std::io::Error>() {
            return if error.kind() == std::io::ErrorKind::NotFound {
                "session was not found"
            } else {
                "session could not be read"
            };
        }
        if source.downcast_ref::<serde_json::Error>().is_some() {
            return "session transcript is invalid";
        }
    }
    "session could not be prepared"
}

fn selected_export_import_cause(error: &pi_coding::ImportSessionError) -> &'static str {
    match error {
        pi_coding::ImportSessionError::UnsupportedFormat(_) => "session source is unsupported",
        pi_coding::ImportSessionError::HomeDirectoryUnavailable => "session catalog is unavailable",
        pi_coding::ImportSessionError::SessionNotFound { .. } => "session was not found",
        pi_coding::ImportSessionError::AmbiguousSession { .. } => "session selection is ambiguous",
        pi_coding::ImportSessionError::InvalidInput { .. }
        | pi_coding::ImportSessionError::InvalidNativeHeader { .. }
        | pi_coding::ImportSessionError::NoConvertibleMessages { .. } => {
            "session transcript is invalid"
        }
        pi_coding::ImportSessionError::InvalidOutput { .. } => {
            "session import destination is unavailable"
        }
        pi_coding::ImportSessionError::ResourceLimit { .. } => {
            "session import exceeds the byte limit"
        }
        pi_coding::ImportSessionError::Io { source, .. }
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            "session was not found"
        }
        pi_coding::ImportSessionError::Io { .. } => "session could not be read",
        pi_coding::ImportSessionError::Json { .. } => "session transcript is invalid",
    }
}

fn resolve_web_catalog_session_path(
    catalog: &pi_coding::SessionCatalog,
    preferred_cwd: &Path,
    session_path: &str,
    sources: &[pi_coding::SessionSourceKind],
) -> Result<PathBuf> {
    crate::resume_catalog::resolve_resume_selection(
        catalog,
        &crate::resume_catalog::ResumeSelectionRequest::Input(session_path.to_owned()),
        Some(preferred_cwd),
        sources,
    )
    .map(|selection| selection.path)
    .context("resolving Web session selection")
}

/// Exact current-row resolution for a negotiated switch: the clicked tuple
/// (source, id, path, status) must still match one live catalog row before
/// any import or spawn participates. Foreign wire status matches a
/// `Foreign` or `AlreadyImported` row (both project as `"foreign"` on the
/// `session_list` wire); native wire status matches only a `Native` row.
fn resolve_exact_catalog_row(
    catalog: &pi_coding::SessionCatalog,
    identity: &CatalogSwitchIdentity,
    session_path: &Path,
    sources: &[pi_coding::SessionSourceKind],
) -> Result<pi_coding::CatalogRow> {
    let expected_kind = identity.source().session_kind();
    if !expected_kind.is_native() && !sources.contains(&expected_kind) {
        bail!("negotiated session selection no longer matches the session catalog");
    }
    let row = catalog
        .row_for_exact_path(expected_kind, session_path)
        .context("negotiated session selection no longer matches the session catalog")?;
    let matches = row.session_id == identity.session_id()
        && row.status.is_native() == identity.is_native()
        && !row.session_kind.is_workflow_internal();
    matches
        .then_some(row)
        .context("negotiated session selection no longer matches the session catalog")
}

/// Collapse every session-move failure to one bounded actionable category.
/// Core move errors and catalog errors embed absolute paths, session names,
/// and directory layouts; none of that reaches RPC clients.
fn bounded_move_failure(error: &anyhow::Error) -> String {
    for source in error.chain() {
        if let Some(error) = source.downcast_ref::<pi_coding::CatalogError>() {
            return match error {
                pi_coding::CatalogError::UnsupportedSource(_) => {
                    "only native sessions can be moved".to_owned()
                }
                pi_coding::CatalogError::HomeDirectoryUnavailable => {
                    "session catalog is unavailable".to_owned()
                }
                pi_coding::CatalogError::EmptySearchQuery => "session selection is invalid".to_owned(),
                pi_coding::CatalogError::SessionNotFound(_) => "session was not found".to_owned(),
                pi_coding::CatalogError::AmbiguousSession { .. } => {
                    "session selection is ambiguous".to_owned()
                }
                pi_coding::CatalogError::ReadOnlySource(_) => "session source is read-only".to_owned(),
                pi_coding::CatalogError::Import(_) => "session transcript is invalid".to_owned(),
                pi_coding::CatalogError::Io { source, .. }
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    "session was not found".to_owned()
                }
                pi_coding::CatalogError::Io { .. } => "session could not be read".to_owned(),
                pi_coding::CatalogError::WorkflowInternalSession => {
                    "workflow-internal session is not user-switchable".to_owned()
                }
            };
        }
        let text = source.to_string();
        if text.contains("already resides in the target session directory") {
            return "session already resides in the target session store".to_owned();
        }
        if text.contains("already exists with different content") {
            return "move destination already exists with different content".to_owned();
        }
        if text.contains("validating move target directory")
            || text.contains("invalid session directory")
            || text.contains("opening target session directory")
        {
            return "move target directory is invalid or unavailable".to_owned();
        }
        if text.contains("refusing to move a non-native session file") {
            return "only native rpi sessions can be moved".to_owned();
        }
        if text.contains("source session identity changed") {
            return "session changed while preparing the move; retry".to_owned();
        }
        if text.contains("source session id is invalid") {
            return "session transcript is invalid".to_owned();
        }
    }
    "session could not be moved right now; verify the selection and retry".to_owned()
}

/// Serialize one catalog row with the exact `session_list` wire shape
/// (reuses `RpcSessionListRow::from_resume_row`). The AllProjects visibility
/// partition decides `temporary` so the destination row behaves like the
/// same row in the sidebar listing.
fn session_list_row_value(row: pi_coding::CatalogRow) -> Value {
    let row = crate::resume_catalog::ResumeSelectorRow::from(row);
    let temporary =
        crate::resume_catalog::partition_web_noise_rows(vec![row.clone()]).1.len() == 1;
    let row = RpcSessionListRow::from_resume_row(row, temporary);
    serde_json::to_value(row).expect("session_list row serializes")
}

/// Which commands are handled by the manager instead of a session dispatcher.
fn is_manager_level_command(command: &RpcCommand) -> bool {
    matches!(
        command,
        RpcCommand::ExportHtml {
            session_path: Some(_),
            ..
        } | RpcCommand::SwitchSession { .. }
            | RpcCommand::NewSession { .. }
            | RpcCommand::Fork { .. }
            | RpcCommand::Clone { .. }
            | RpcCommand::GetForkMessages { .. }
            | RpcCommand::SetSessionName { .. }
            | RpcCommand::SessionForkLatest { .. }
            | RpcCommand::SessionDelete { .. }
            | RpcCommand::SessionRestore { .. }
            | RpcCommand::SessionCopyContext { .. }
            | RpcCommand::CloseSession { .. }
            | RpcCommand::SessionHandoff { .. }
            | RpcCommand::SessionMove { .. }
    )
}

enum IsolatedApplicationLruTouch {
    Recency,
    Housekeeping,
}

impl IsolatedApplicationLruTouch {
    fn from_command(command: &RpcCommand) -> Self {
        match command {
            RpcCommand::GetState { .. }
            | RpcCommand::JobList { .. }
            | RpcCommand::GetMessages { .. }
            | RpcCommand::GetSessionStats { .. }
            | RpcCommand::SessionList { .. } => Self::Housekeeping,
            _ => Self::Recency,
        }
    }

    fn stamps_recency(self) -> bool {
        matches!(self, Self::Recency)
    }
}

fn scrub_download_stem(label: &str, extension_len: usize, fallback: &str) -> String {
    let mut stem = String::with_capacity(MAX_SESSION_DOWNLOAD_FILENAME_CHARS);
    let mut previous_separator = false;
    for ch in label.chars() {
        let safe = if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            Some(ch)
        } else if ch.is_whitespace() {
            Some('-')
        } else {
            None
        };
        let Some(ch) = safe else { continue };
        let is_separator = matches!(ch, '-' | '_' | '.');
        if is_separator && previous_separator {
            continue;
        }
        if stem.chars().count()
            >= MAX_SESSION_DOWNLOAD_FILENAME_CHARS.saturating_sub(extension_len)
        {
            break;
        }
        stem.push(ch);
        previous_separator = is_separator;
    }
    let stem = stem.trim_matches(['-', '_', '.']);
    if stem.is_empty() {
        fallback.to_owned()
    } else {
        stem.to_owned()
    }
}

fn download_file_name(file_name: &str) -> String {
    let path = Path::new(file_name);
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 16
                && extension.chars().all(|ch| ch.is_ascii_alphanumeric())
        })
        .map(|extension| format!(".{}", extension.to_ascii_lowercase()))
        .unwrap_or_default();
    let label = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("file");
    format!(
        "{}{}",
        scrub_download_stem(label, extension.len(), "file"),
        extension
    )
}

fn session_download_file_name(session_name: Option<&str>, session_id: &str) -> String {
    let label = session_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(session_id);
    format!("{}.html", scrub_download_stem(label, ".html".len(), "session"))
}

fn valid_session_download_file_name(file_name: &str) -> bool {
    !file_name.is_empty()
        && file_name.chars().count() <= MAX_SESSION_DOWNLOAD_FILENAME_CHARS
        && !matches!(file_name, "." | "..")
        && file_name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

fn unknown_session_response(
    id: Option<String>,
    name: &str,
    session_id: Option<String>,
) -> RpcResponse {
    let error = match session_id.as_deref() {
        Some(sid) => format!("unknown session {sid}"),
        None => "session is unavailable".to_owned(),
    };
    tag_response(RpcResponse::failure(id, name, error), session_id.as_deref())
}

fn host_owned_ui_event(event: &ExtensionUiEvent) -> bool {
    matches!(
        event,
        ExtensionUiEvent::InteractionRequested { interaction }
            if interaction.context.instance.extension_id == "host"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::rpc::RpcCommand;
    use pi_agent::ThinkingLevel;
    use pi_ai::Model;
    use pi_ai::providers::{
        FauxProviderOptions, FauxProviderRegistration, FauxResponse, register_faux_provider,
    };
    use pi_coding::{
        AgentCatalog, AgentDefinition, AgentDefinitionSource, ApplicationEvent, DurableState,
        JobSnapshot, JobStatus, OrchestrationConfig, OrchestrationRuntime, ResourceDiscovery,
        ResourceManager, ResourceManagerOptions, Session, SessionOptions, TaskSpawn, ToolSelection,
    };
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::sync::{
        LazyLock,
        atomic::{AtomicUsize, Ordering},
    };
    use tempfile::TempDir;
    use tokio::sync::Barrier;
    use tokio_util::sync::CancellationToken;
    use std::io::Write;

    fn primary(manager: &SessionRuntimeManager) -> &SessionRuntime {
        &manager.primary
    }

    fn faux_model(label: &str) -> (Model, FauxProviderRegistration) {
        faux_model_sized(label, 8)
    }

    fn faux_model_sized(label: &str, chunk_size: usize) -> (Model, FauxProviderRegistration) {
        let suffix = unique(label);
        let mut model = Model::default();
        model.id = format!("{label}-model");
        model.name = format!("{label} Model");
        model.api = format!("{suffix}-api");
        model.provider = format!("{suffix}-provider");
        model.base_url = "http://localhost:0".into();
        let registration = register_faux_provider(FauxProviderOptions {
            api: model.api.clone(),
            provider: model.provider.clone(),
            models: vec![model.clone()],
            chunk_size,
        });
        (model, registration)
    }

    fn write_codex_session(catalog: &pi_coding::SessionCatalog, id: &str, cwd: &Path) -> PathBuf {
        let path = catalog
            .root_for(pi_coding::SessionSourceKind::Codex)
            .path
            .join(format!("rollout-{id}.jsonl"));
        std::fs::create_dir_all(path.parent().expect("Codex parent")).expect("Codex parent");
        let cwd = serde_json::to_string(&cwd.to_string_lossy()).expect("Codex cwd");
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":{cwd}}}}}\n{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"Codex prompt\"}}]}}}}\n{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"Codex reply\"}}]}}}}\n",
            ),
        )
        .expect("Codex session");
        path
    }

    fn write_omp_session(catalog: &pi_coding::SessionCatalog, id: &str, cwd: &Path) -> PathBuf {
        let path = catalog
            .root_for(pi_coding::SessionSourceKind::Omp)
            .path
            .join("--work--")
            .join(format!("{id}.jsonl"));
        std::fs::create_dir_all(path.parent().expect("OMP parent")).expect("OMP parent");
        let cwd = serde_json::to_string(&cwd.to_string_lossy()).expect("OMP cwd");
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":{cwd}}}\n{{\"type\":\"message\",\"id\":\"u\",\"parentId\":null,\"message\":{{\"role\":\"user\",\"content\":\"OMP prompt\"}}}}\n",
            ),
        )
        .expect("OMP session");
        path
    }

    fn web_catalog_fixture() -> (TempDir, TempDir, pi_coding::SessionCatalog) {
        let home = tempfile::tempdir().expect("catalog home");
        let native = tempfile::tempdir().expect("native root");
        let catalog =
            pi_coding::SessionCatalog::new(home.path()).with_native_session_root(native.path());
        (home, native, catalog)
    }

    fn overlapping_pi_catalog_fixture() -> (TempDir, pi_coding::SessionCatalog) {
        let home = tempfile::tempdir().expect("catalog home");
        let shared = home.path().join(".pi/agent/sessions");
        std::fs::create_dir_all(&shared).expect("shared catalog root");
        let catalog = pi_coding::SessionCatalog::new(home.path()).with_native_session_root(&shared);
        (home, catalog)
    }

    fn unique(label: &str) -> String {
        format!("{label}-{}", uuid::Uuid::now_v7().simple())
    }

    fn session_options(model: Model, cwd: &Path) -> SessionOptions {
        SessionOptions {
            model,
            cwd: cwd.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: "faux".into(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            auth_manager: None,
        }
    }
    fn isolated_agent_dir() -> &'static Path {
        static AGENT: LazyLock<TempDir> = LazyLock::new(|| {
            let dir = tempfile::Builder::new()
                .prefix("pi-cli-srm-agent-")
                .tempdir()
                .expect("shared isolated agent dir");
            std::fs::write(
                dir.path().join("settings.json"),
                r#"{"sandbox":{"mode":"danger-full-access"}}"#,
            )
            .expect("shared isolated sandbox settings");
            dir
        });
        AGENT.path()
    }

    fn isolated_auth_manager() -> Arc<pi_coding::AuthManager> {
        static AUTH: LazyLock<Arc<pi_coding::AuthManager>> = LazyLock::new(|| {
            Arc::new(
                pi_coding::AuthManager::new(isolated_agent_dir().join("auth.json"))
                    .expect("shared isolated auth manager"),
            )
        });
        AUTH.clone()
    }

    fn isolated_resources(cwd: &Path) -> ResourceManager {
        let mut options = ResourceManagerOptions::new(cwd);
        options.agent_dir = isolated_agent_dir().to_path_buf();
        options.project_trust_override = Some(false);
        options.disable_extensions = true;
        options.disable_skills = true;
        options.disable_prompt_templates = true;
        options.disable_themes = true;
        options.disable_context_files = true;
        ResourceManager::new(options).expect("isolated resources")
    }

    async fn isolated_session(model: Model, root: &Path) -> Session {
        isolated_session_with_auth_owner(model, root, false).await
    }

    async fn isolated_session_with_auth_owner(
        model: Model,
        root: &Path,
        with_auth_owner: bool,
    ) -> Session {
        isolated_session_with_stream_fn(model, root, with_auth_owner, None).await
    }

    async fn isolated_session_with_stream_fn(
        model: Model,
        root: &Path,
        with_auth_owner: bool,
        stream_fn: Option<pi_agent::StreamFn>,
    ) -> Session {
        let cwd = root.join("cwd");
        let session_dir = root.join("sessions");
        for directory in [&cwd, &session_dir] {
            std::fs::create_dir_all(directory).expect("isolated fixture directory");
        }
        let mut options = session_options(model, &cwd);
        if with_auth_owner {
            options.auth_manager = Some(isolated_auth_manager());
        }
        options.stream_fn = stream_fn;
        let session = Session::new_with_additional_tools_filtered_and_discovery(
            options,
            Vec::new(),
            ToolSelection::default(),
            ResourceDiscovery::Disabled,
        )
        .expect("session");
        session.set_session_dir(session_dir);
        session
            .attach_resources(isolated_resources(&cwd))
            .await
            .expect("attach isolated resources");
        session
    }
    async fn isolated_application(session: Session) -> Application {
        Application::from_runtime_candidate(pi_coding::ApplicationRuntimeCandidate::new(session))
            .await
            .expect("isolated application")
    }

    async fn bare_application(label: &str) -> (Application, TempDir) {
        let (model, _registration) = faux_model(label);
        let (application, _, directory) = recorded_application_with(model).await;
        (application, directory)
    }

    async fn bare_application_with(model: Model) -> (Application, TempDir) {
        let (application, _, directory) = recorded_application_with(model).await;
        (application, directory)
    }

    /// A recorded session whose session file EXISTS on disk (header persisted
    /// immediately, mirroring a production session that has started a
    /// recording), so resume-catalog scans and switch dedup see it.
    async fn recorded_application(label: &str) -> (Application, String, TempDir) {
        let (model, _registration) = faux_model(label);
        recorded_application_with(model).await
    }

    async fn recorded_application_with(model: Model) -> (Application, String, TempDir) {
        let dir = tempfile::tempdir().expect("session dir");
        let session = isolated_session(model.clone(), dir.path()).await;
        let session_dir = session.session_dir();
        let recorder = pi_coding::start_session_in(
            &session_dir,
            Some(&model),
            Some("off"),
            Some(dir.path()),
            None,
            None,
        )
        .expect("start session");
        recorder.persist_now().expect("persist header");
        session.record(recorder).expect("record");
        let application = isolated_application(session).await;
        let (id, _) = application.session().recorder_info().expect("recorder id");
        (application, id, dir)
    }

    async fn production_recorded_application(label: &str) -> (Application, String, TempDir) {
        let (model, _registration) = faux_model(label);
        let dir = tempfile::tempdir().expect("session dir");
        let session = isolated_session_with_auth_owner(model.clone(), dir.path(), true).await;
        let session_dir = session.session_dir();
        let recorder = pi_coding::start_session_in(
            &session_dir,
            Some(&model),
            Some("off"),
            Some(dir.path()),
            None,
            None,
        )
        .expect("start session");
        recorder.persist_now().expect("persist header");
        session.record(recorder).expect("record");
        let application = isolated_application(session).await;
        let (id, _) = application.session().recorder_info().expect("recorder id");
        (application, id, dir)
    }

    fn manager_task_definition() -> AgentDefinition {
        AgentDefinition {
            name: "task".to_owned(),
            description: "manager restart task".to_owned(),
            system_prompt: "complete the assignment".to_owned(),
            tools: Some(Vec::new()),
            autoload_skills: Vec::new(),
            model: None,
            thinking_level: Some(ThinkingLevel::Off),
            max_turns: None,
            max_tool_calls: None,
            timeout_secs: None,
            disallowed_tools: Vec::new(),
            capability_ceiling: None,
            source: AgentDefinitionSource::Bundled,
            path: None,
            trusted: true,
            kind: pi_coding::AgentDefinitionKind::Agent,
            personality: None,
            soft_budget: None,
            knowledge_roots: Vec::new(),
            user_proxy: false,
        }
    }

    fn manager_held_child_factory(
        cwd: PathBuf,
        started: Arc<Barrier>,
        release: CancellationToken,
    ) -> pi_coding::ChildSessionFactory {
        manager_child_factory(
            cwd,
            Some(started),
            Some(release),
            "held before restart",
            None,
        )
    }

    fn manager_recovery_child_factory(
        cwd: PathBuf,
        output: &'static str,
        release: CancellationToken,
        continuations: Arc<AtomicUsize>,
    ) -> pi_coding::ChildSessionFactory {
        manager_child_factory(cwd, None, Some(release), output, Some(continuations))
    }

    fn manager_child_factory(
        cwd: PathBuf,
        started: Option<Arc<Barrier>>,
        release: Option<CancellationToken>,
        output: &'static str,
        continuations: Option<Arc<AtomicUsize>>,
    ) -> pi_coding::ChildSessionFactory {
        Arc::new(move |request| {
            let cwd = cwd.clone();
            let started = started.clone();
            let release = release.clone();
            let continuations = continuations.clone();
            Box::pin(async move {
                let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, options| {
                    let started = started.clone();
                    let release = release.clone();
                    let continuations = continuations.clone();
                    Box::pin(async move {
                        let stream = pi_ai::new_assistant_message_event_stream();
                        let producer = stream.clone();
                        tokio::spawn(async move {
                            if let Some(started) = started {
                                started.wait().await;
                            }
                            if let Some(release) = release {
                                match options.stream.abort_signal {
                                    Some(abort) => tokio::select! {
                                        () = release.cancelled() => {}
                                        () = abort.cancelled() => {}
                                    },
                                    None => release.cancelled().await,
                                }
                            }
                            if let Some(continuations) = continuations {
                                continuations.fetch_add(1, Ordering::SeqCst);
                            }
                            let mut message = pi_ai::AssistantMessage::pending(&model);
                            message.content.push(pi_ai::ContentBlock::text(output));
                            message.stop_reason = pi_ai::StopReason::Stop;
                            producer.end(Some(message)).await;
                        });
                        stream
                    })
                });
                Session::new_with_additional_tools_filtered_and_discovery(
                    SessionOptions {
                        model: request.model,
                        cwd,
                        system_prompt: request.system_prompt,
                        thinking_level: ThinkingLevel::Off,
                        api_key: String::new(),
                        compaction: None,
                        stream_options: Default::default(),
                        tools: Some(request.orchestration_tools),
                        before_tool_call: None,
                        after_tool_call: None,
                        stream_fn: Some(stream_fn),
                        auth_resolver: None,
                        auth_manager: None,
                    },
                    Vec::new(),
                    ToolSelection::default(),
                    ResourceDiscovery::Disabled,
                )
            })
        })
    }

    async fn launch_manager_task_from_parent(
        application: &Application,
        registration: &FauxProviderRegistration,
        tool_call_id: &str,
        agent_id: &str,
    ) -> TaskSpawn {
        registration.set_responses(vec![
            FauxResponse {
                content: vec![pi_ai::ContentBlock::ToolCall(pi_ai::ToolCall {
                    id: tool_call_id.to_owned(),
                    name: "task".to_owned(),
                    arguments: json!({
                        "name": agent_id,
                        "agent": "task",
                        "task": format!("Run {agent_id} across process restart and report exactly once."),
                    }),
                    thought_signature: None,
                })],
                stop_reason: pi_ai::StopReason::ToolUse,
                error_message: None,
            },
            FauxResponse::text("parent Task launch recorded"),
        ]);
        application
            .prompt(format!("Launch {agent_id}."), Vec::new(), None)
            .await
            .expect("parent prompt")
            .expect("parent turn accepted");
        application.wait_for_idle().await;
        let runtime = application
            .orchestration_runtime()
            .expect("orchestration runtime");
        let job = runtime
            .jobs(None)
            .into_iter()
            .find(|job| job.agent_id == agent_id)
            .expect("Task-created job");
        let history = application.session().history();
        let assistant_calls = history
            .iter()
            .filter(|message| matches!(
                message,
                pi_ai::Message::Assistant(message)
                    if message.content.iter().any(|block| matches!(
                        block,
                        pi_ai::ContentBlock::ToolCall(call) if call.id == tool_call_id && call.name == "task"
                    ))
            ))
            .count();
        let results = history
            .iter()
            .filter(|message| {
                matches!(
                    message,
                    pi_ai::Message::ToolResult(result) if result.tool_call_id == tool_call_id
                )
            })
            .count();
        assert_eq!(assistant_calls, 1, "parent records one Task tool call");
        assert_eq!(results, 1, "parent records one Task launch result");
        TaskSpawn {
            index: 0,
            job_id: job.id,
            agent_id: job.agent_id,
            agent: job.agent,
            status: job.status,
        }
    }

    fn assert_parent_tool_identity_once(
        application: &Application,
        tool_call_id: &str,
        expected: &TaskSpawn,
    ) {
        let history = application.session().history();
        assert_eq!(
            history
                .iter()
                .filter(|message| matches!(
                    message,
                    pi_ai::Message::Assistant(message)
                        if message.content.iter().any(|block| matches!(
                            block,
                            pi_ai::ContentBlock::ToolCall(call) if call.id == tool_call_id && call.name == "task"
                        ))
                ))
                .count(),
            1,
            "resumed parent must retain one original Task tool call",
        );
        let results = history
            .iter()
            .filter_map(|message| match message {
                pi_ai::Message::ToolResult(result) if result.tool_call_id == tool_call_id => {
                    Some(result)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            results.len(),
            1,
            "resumed parent must retain one original Task launch result",
        );
        assert_eq!(results[0].tool_name, "task");
        let spawns = serde_json::from_value::<Vec<TaskSpawn>>(
            results[0]
                .details
                .clone()
                .expect("Task launch result details"),
        )
        .expect("decode parent Task launch result");
        assert_eq!(spawns.len(), 1, "parent Task result must bind one child");
        assert_eq!(spawns[0].index, expected.index);
        assert_eq!(spawns[0].job_id, expected.job_id);
        assert_eq!(spawns[0].agent_id, expected.agent_id);
        assert_eq!(spawns[0].agent, expected.agent);
        assert!(
            matches!(spawns[0].status, JobStatus::Queued | JobStatus::Running),
            "parent Task launch result must record a live spawn: {:#?}",
            spawns[0],
        );
    }

    async fn collect_recovered_completion_frames(
        events: &mut broadcast::Receiver<Value>,
        job_ids: [&str; 2],
    ) -> Vec<Value> {
        let mut frames = Vec::new();
        let mut completed = std::collections::BTreeSet::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            while completed.len() < job_ids.len() {
                let frame = events.recv().await.expect("resumed manager event fan-in");
                if frame.get("type").and_then(Value::as_str) == Some("job_updated")
                    && frame["job"]["status"] == "completed"
                    && let Some(job_id) = frame["job"]["id"].as_str()
                    && job_ids.contains(&job_id)
                {
                    completed.insert(job_id.to_owned());
                }
                frames.push(frame);
            }
        })
        .await
        .expect("both recovered completions reach resumed manager fan-in");
        loop {
            match events.try_recv() {
                Ok(frame) => frames.push(frame),
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(count)) => {
                    panic!("resumed manager event fan-in lagged by {count}")
                }
                Err(broadcast::error::TryRecvError::Closed) => {
                    panic!("resumed manager event fan-in closed")
                }
            }
        }
        frames
    }

    fn assert_recovered_parent_delivery_once(
        frames: &[Value],
        session_id: &str,
        group_id: &str,
        spawn: &TaskSpawn,
        expected_output: &str,
    ) {
        let delivered = frames
            .iter()
            .filter(|frame| {
                frame.get("type").and_then(Value::as_str) == Some("job_updated")
                    && frame["job"]["id"] == spawn.job_id
                    && frame["job"]["status"] == "completed"
            })
            .collect::<Vec<_>>();
        assert_eq!(
            delivered.len(),
            1,
            "recovered result must reach exactly one parent projection: {delivered:#?}",
        );
        let frame = delivered[0];
        assert_eq!(frame["sessionId"], session_id);
        assert_eq!(frame["group_id"], group_id);
        assert_eq!(frame["job"]["agentId"], spawn.agent_id);
        assert_eq!(frame["job"]["agent"], spawn.agent);
        assert_eq!(frame["job"]["parentId"], "Main");
        assert_eq!(frame["job"]["result"]["id"], spawn.agent_id);
        assert_eq!(frame["job"]["result"]["agent"], spawn.agent);
        let output = frame["job"]["result"]["output"]
            .as_str()
            .expect("recovered parent result output");
        assert!(output.starts_with(expected_output), "{frame:#}");
        assert!(
            !frame["job"]["result"]["error"]
                .as_str()
                .is_some_and(|error| error.contains("orchestration shut down")),
            "{frame:#}",
        );
    }

    fn orchestration_sidecar(application: &Application) -> PathBuf {
        let (session_id, session_path) = application
            .session()
            .recorder_info()
            .expect("recorded application");
        session_path
            .parent()
            .expect("session parent")
            .join("children")
            .join(session_id)
            .join("orchestration-state.json")
    }

    fn load_manager_sidecar(application: &Application) -> DurableState {
        serde_json::from_slice(
            &std::fs::read(orchestration_sidecar(application)).expect("read orchestration sidecar"),
        )
        .expect("decode orchestration sidecar")
    }

    fn assert_running_identity(job: &JobSnapshot, spawn: &TaskSpawn, agent_id: &str) {
        assert_eq!(job.id, spawn.job_id);
        assert_eq!(job.agent_id, agent_id);
        assert_eq!(job.agent, "task");
        assert_eq!(job.parent_id, "Main");
        assert_eq!(job.status, JobStatus::Running);
        assert!(job.result.is_none());
    }

    async fn resumed_manager_application(
        session_path: &Path,
        cwd: &Path,
        output: &'static str,
        release: CancellationToken,
        continuations: Arc<AtomicUsize>,
    ) -> (Application, OrchestrationRuntime) {
        let (model, _registration) = faux_model(output);
        let root = tempfile::tempdir().expect("resumed runtime root");
        let agent_dir = root.path().join("agent");
        std::fs::create_dir_all(&agent_dir).expect("resumed agent dir");
        let session = Session::new_with_additional_tools_filtered_and_discovery(
            session_options(model.clone(), cwd),
            Vec::new(),
            ToolSelection::default(),
            ResourceDiscovery::Disabled,
        )
        .expect("resumed session");
        session.set_session_dir(session_path.parent().expect("session parent").to_path_buf());
        session
            .attach_resources(isolated_resources(cwd))
            .await
            .expect("attach resumed resources");
        let recorder = pi_coding::resume_session(session_path).expect("resume recorder");
        let context = recorder.tree().expect("resumed tree").build_context(None);
        session
            .resume_recording(recorder, context.messages)
            .await
            .expect("attach resumed writer and history");
        session
            .recording_writer_info()
            .expect("resumed manager writer");
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![manager_task_definition()]),
            root.path().join("artifacts"),
        );
        config.parent_model = model;
        config.idle_ttl = None;
        let orchestration = OrchestrationRuntime::new(
            config,
            manager_recovery_child_factory(cwd.to_path_buf(), output, release, continuations),
        )
        .expect("resumed orchestration runtime");
        let application = Application::from_runtime_candidate(
            pi_coding::ApplicationRuntimeCandidate::new(session).with_orchestration(orchestration),
        )
        .await
        .expect("resumed orchestration application");
        let orchestration = application
            .orchestration_runtime()
            .expect("resumed orchestration runtime");
        let _kept = root.keep();
        (application, orchestration)
    }

    async fn recorded_orchestration_application(
        label: &str,
        started: Arc<Barrier>,
        release: CancellationToken,
    ) -> (
        Application,
        String,
        TempDir,
        OrchestrationRuntime,
        FauxProviderRegistration,
    ) {
        let (model, registration) = faux_model(label);
        let dir = tempfile::tempdir().expect("session dir");
        let session = isolated_session(model.clone(), dir.path()).await;
        let session_dir = session.session_dir();
        let recorder = pi_coding::start_session_in(
            &session_dir,
            Some(&model),
            Some("off"),
            Some(dir.path()),
            None,
            None,
        )
        .expect("start session");
        recorder.persist_now().expect("persist header");
        session.record(recorder).expect("record");
        let child_cwd = session.cwd().to_path_buf();
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![manager_task_definition()]),
            dir.path().join("artifacts"),
        );
        config.parent_model = model;
        config.idle_ttl = None;
        let orchestration = OrchestrationRuntime::new(
            config,
            manager_held_child_factory(child_cwd, started, release),
        )
        .expect("orchestration runtime");
        let application = Application::from_runtime_candidate(
            pi_coding::ApplicationRuntimeCandidate::new(session).with_orchestration(orchestration),
        )
        .await
        .expect("orchestration application");
        let orchestration = application
            .orchestration_runtime()
            .expect("orchestration runtime");
        let (session_id, _) = application.session().recorder_info().expect("recorder id");
        (application, session_id, dir, orchestration, registration)
    }

    async fn prepared_branch_result(
        model: Model,
        prepared: pi_coding::PreparedSessionResume,
        leaf_id: &str,
        session_root: Option<PathBuf>,
    ) -> Result<SessionSpawnResult> {
        let dir = tempfile::tempdir().expect("branch runtime cwd");
        let session_root = session_root.unwrap_or_else(|| dir.path().to_path_buf());
        let recorder = pi_coding::create_branched_session_from_prepared(
            prepared,
            leaf_id,
            Some(&session_root),
        )?;
        recorder.persist_now()?;
        let session = isolated_session(model, dir.path()).await;
        let context = recorder.tree()?.build_context(None);
        session.resume_recording(recorder, context.messages).await?;
        let application = isolated_application(session).await;
        let (session_id, session_file) = application
            .session()
            .recorder_info()
            .ok_or_else(|| anyhow!("branch has no recorder"))?;
        let _kept = dir.keep();
        Ok(SessionSpawnResult {
            session_id,
            session_file,
            file_identity: application
                .session()
                .session_file_identity()
                .ok_or_else(|| anyhow!("branch has no file identity"))?,
            application,
            extension_ui: ExtensionUiAdapter::default(),
            resume_notice: None,
            journal_ownership: SpawnedJournalOwnership::Created,
            editor_text: None,
        })
    }

    /// Deterministic faux spawner: every spawn builds a fresh recorded
    /// runtime from one shared controllable model (tests drive it through
    /// [`TestSpawner::registration`]) and counts calls so tests can assert
    /// dedup/cap behavior. An `Open` child consumes the prepared recorder,
    /// preserving the same retained file identity as production.
    #[derive(Clone)]
    struct TestSpawner {
        spawns: Arc<AtomicUsize>,
        opened: Arc<std::sync::Mutex<Vec<PathBuf>>>,
        fresh_cwds: Arc<std::sync::Mutex<Vec<PathBuf>>>,
        spawned_paths: Arc<std::sync::Mutex<Vec<PathBuf>>>,
        model: Model,
        registration: FauxProviderRegistration,
        /// Hermetic catalog injected via [`SessionSpawner::catalog`] so the
        /// manager's Web import/reuse path can be exercised without touching
        /// the real home directory.
        todo_child_service: Arc<TodoChildService>,
        catalog: Option<pi_coding::SessionCatalog>,
    }

    impl Default for TestSpawner {
        fn default() -> Self {
            // chunk_size 1: prompts stream byte-by-byte, so tests can queue
            // steer/abort while a turn remains active.
            let (model, registration) = faux_model_sized("manager", 1);
            Self {
                spawned_paths: Arc::new(std::sync::Mutex::new(Vec::new())),
                spawns: Arc::new(AtomicUsize::new(0)),
                opened: Arc::new(std::sync::Mutex::new(Vec::new())),
                fresh_cwds: Arc::new(std::sync::Mutex::new(Vec::new())),
                model,
                registration,
                todo_child_service: TodoChildService::new(),
                catalog: None,
            }
        }
    }

    impl TestSpawner {
        fn with_catalog(mut self, catalog: pi_coding::SessionCatalog) -> Self {
            self.catalog = Some(catalog);
            self
        }
    }


    #[derive(Clone)]
    struct IdentityMismatchSpawner {
        inner: TestSpawner,
        mismatches_remaining: Arc<AtomicUsize>,
    }

    impl SessionSpawner for IdentityMismatchSpawner {
        fn spawn(
            &self,
            request: SessionSpawnRequest,
        ) -> Pin<Box<dyn Future<Output = Result<SessionSpawnResult>> + Send>> {
            let inner = self.inner.clone();
            let mismatch = self
                .mismatches_remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok();
            Box::pin(async move {
                let mut result = inner.spawn(request).await?;
                if let Some((_, path)) = result.application.session().recorder_info() {
                    inner
                        .spawned_paths
                        .lock()
                        .expect("spawned paths")
                        .push(path);
                }
                if mismatch {
                    let directory = tempfile::tempdir().expect("mismatched identity directory");
                    let recorder = pi_coding::start_session_in(
                        directory.path(),
                        None,
                        None,
                        Some(directory.path()),
                        None,
                        None,
                    )?;
                    recorder.persist_now_durable()?;
                    result.file_identity = recorder
                        .file_identity()
                        .ok_or_else(|| anyhow!("mismatch recorder has no identity"))?;
                    let _kept = directory.keep();
                }
                Ok(result)
            })
        }

        fn todo_child_service(&self) -> Option<Arc<TodoChildService>> {
            self.inner.todo_child_service()
        }
    }
    impl SessionSpawner for TestSpawner {
        fn spawn(
            &self,
            request: SessionSpawnRequest,
        ) -> Pin<Box<dyn Future<Output = Result<SessionSpawnResult>> + Send>> {
            let this = self.clone();
            Box::pin(async move {
                this.spawns.fetch_add(1, Ordering::SeqCst);
                let SessionSpawnRequest { kind, source } = request;
                match kind {
                    SessionSpawnKind::Open { prepared } => {
                        let resume_path = prepared.path().to_path_buf();
                        let opened_session_id = prepared.tree().header.id.clone();
                        this.opened
                            .lock()
                            .expect("opened")
                            .push(resume_path.clone());
                        let dir = tempfile::tempdir().expect("runtime cwd");
                        let session = isolated_session(this.model.clone(), dir.path()).await;
                        session.set_session_dir(
                            resume_path.parent().expect("session parent").to_path_buf(),
                        );
                        let context = prepared.build_context();
                        session
                            .resume_recording(prepared.into_recorder()?, context.messages)
                            .await?;
                        let application = isolated_application(session).await;
                        let file_identity = application
                            .session()
                            .session_file_identity()
                            .ok_or_else(|| anyhow!("opened session has no file identity"))?;
                        let _kept = dir.keep();
                        Ok(SessionSpawnResult {
                            session_id: opened_session_id,
                            session_file: resume_path,
                            file_identity,
                            application,
                            extension_ui: ExtensionUiAdapter::default(),
                            resume_notice: None,
                            journal_ownership: SpawnedJournalOwnership::Existing,
                            editor_text: None,
                        })
                    }
                    SessionSpawnKind::ForkPrepared { prepared, entry_id } => {
                        let selected = prepared
                            .tree()
                            .entries
                            .iter()
                            .find(|entry| entry.id == entry_id)
                            .ok_or_else(|| anyhow!("Entry not found: {entry_id}"))?;
                        let editor_text = selected
                            .message
                            .as_ref()
                            .and_then(fork_selected_editor_text);
                        let parent_id = selected.parent_id.clone();
                        match parent_id {
                            Some(parent_id) => {
                                let session_root = this.catalog.as_ref().map(|catalog| {
                                    catalog
                                        .root_for(pi_coding::SessionSourceKind::NativePi)
                                        .path
                                });
                                let mut result = prepared_branch_result(
                                    this.model.clone(),
                                    prepared,
                                    &parent_id,
                                    session_root,
                                )
                                .await?;
                                result.editor_text = editor_text;
                                Ok(result)
                            }
                            None => {
                                let session_root = this.catalog.as_ref().map(|catalog| {
                                    catalog
                                        .root_for(pi_coding::SessionSourceKind::NativePi)
                                        .path
                                });
                                let pi_coding::MaterializedFork {
                                    recorder,
                                    editor_text,
                                } = pi_coding::materialize_fork_at_user(
                                    prepared.into(),
                                    &entry_id,
                                    session_root.as_deref(),
                                )?;
                                let dir = tempfile::tempdir().expect("fork runtime cwd");
                                let session = isolated_session(this.model.clone(), dir.path()).await;
                                let context = recorder.tree()?.build_context(None);
                                session.resume_recording(recorder, context.messages).await?;
                                let application = isolated_application(session).await;
                                let (session_id, session_file) = application
                                    .session()
                                    .recorder_info()
                                    .ok_or_else(|| anyhow!("fork has no recorder"))?;
                                let file_identity = application
                                    .session()
                                    .session_file_identity()
                                    .ok_or_else(|| anyhow!("fork has no file identity"))?;
                                let _kept = dir.keep();
                                Ok(SessionSpawnResult {
                                    session_id,
                                    session_file,
                                    file_identity,
                                    application,
                                    extension_ui: ExtensionUiAdapter::default(),
                                    resume_notice: None,
                                    journal_ownership: SpawnedJournalOwnership::Created,
                                    editor_text: Some(editor_text),
                                })
                            }
                        }
                    }
                    SessionSpawnKind::ClonePrepared { prepared } => {
                        let leaf_id = prepared.tree().active_leaf_id.clone().ok_or_else(|| {
                            anyhow!("cannot clone session: no current entry selected")
                        })?;
                        let session_root = this.catalog.as_ref().map(|catalog| {
                            catalog
                                .root_for(pi_coding::SessionSourceKind::NativePi)
                                .path
                        });
                        prepared_branch_result(
                            this.model.clone(),
                            prepared,
                            &leaf_id,
                            session_root,
                        )
                        .await
                    }
                    SessionSpawnKind::Fresh { directory } => {
                        directory.validate()?;
                        let cwd = directory.path().to_path_buf();
                        this.fresh_cwds
                            .lock()
                            .expect("fresh cwds")
                            .push(cwd.clone());
                        let runtime_root = tempfile::tempdir().expect("runtime root");
                        let session_dir = runtime_root.path().join("sessions");
                        std::fs::create_dir_all(&session_dir).expect("runtime sessions");
                        let session = Session::new_with_additional_tools_filtered_and_discovery(
                            session_options(this.model.clone(), &cwd),
                            Vec::new(),
                            ToolSelection::default(),
                            ResourceDiscovery::Disabled,
                        )?;
                        session.set_session_dir(session_dir.clone());
                        session
                            .attach_resources(isolated_resources(&cwd))
                            .await?;
                        let recorder = pi_coding::start_session_in(
                            &session_dir,
                            Some(&this.model),
                            Some("off"),
                            Some(&cwd),
                            None,
                            None,
                        )?;
                        recorder.persist_now()?;
                        session.record(recorder)?;
                        let application = isolated_application(session).await;
                        let (session_id, session_file) = application
                            .session()
                            .recorder_info()
                            .ok_or_else(|| anyhow!("fresh runtime has no recorder"))?;
                        let file_identity = application
                            .session()
                            .session_file_identity()
                            .ok_or_else(|| anyhow!("fresh runtime has no file identity"))?;
                        let _kept = runtime_root.keep();
                        Ok(SessionSpawnResult {
                            session_id,
                            session_file,
                            file_identity,
                            application,
                            extension_ui: ExtensionUiAdapter::default(),
                            resume_notice: None,
                            journal_ownership: SpawnedJournalOwnership::Created,
                            editor_text: None,
                        })
                    }
                    SessionSpawnKind::Fork { entry_id } => {
                        let editor_text = source
                            .session()
                            .session_entries(None)
                            .ok()
                            .and_then(|tree| {
                                tree.entries.into_iter().find(|entry| entry.id == entry_id)
                            })
                            .and_then(|entry| {
                                entry.message.as_ref().and_then(fork_selected_editor_text)
                            });
                        let (application, session_id, dir) =
                            recorded_application_with(this.model.clone()).await;
                        let _kept = dir.keep();
                        let session_file = application
                            .session()
                            .recorder_info()
                            .map(|(_, path)| path)
                            .ok_or_else(|| anyhow!("spawned runtime has no recorder"))?;
                        Ok(SessionSpawnResult {
                            session_id,
                            session_file,
                            file_identity: application
                                .session()
                                .session_file_identity()
                                .ok_or_else(|| anyhow!("spawned runtime has no file identity"))?,
                            application,
                            extension_ui: ExtensionUiAdapter::default(),
                            resume_notice: None,
                            journal_ownership: SpawnedJournalOwnership::Created,
                            editor_text,
                        })
                    }
                    _ => {
                        let (application, session_id, dir) =
                            recorded_application_with(this.model.clone()).await;
                        let _kept = dir.keep();
                        let session_file = application
                            .session()
                            .recorder_info()
                            .map(|(_, path)| path)
                            .ok_or_else(|| anyhow!("spawned runtime has no recorder"))?;
                        Ok(SessionSpawnResult {
                            session_id,
                            session_file,
                            file_identity: application
                                .session()
                                .session_file_identity()
                                .ok_or_else(|| anyhow!("spawned runtime has no file identity"))?,
                            application,
                            extension_ui: ExtensionUiAdapter::default(),
                            resume_notice: None,
                            journal_ownership: SpawnedJournalOwnership::Created,
                            editor_text: None,
                        })
                    }
                }
            })
        }
        fn catalog(&self) -> Option<pi_coding::SessionCatalog> {
            self.catalog.clone()
        }
        fn session_storage(&self) -> Option<crate::session_run::SessionStorage> {
            self.catalog.as_ref().map(|catalog| {
                crate::session_run::SessionStorage::ExplicitRoot(
                    catalog
                        .root_for(pi_coding::SessionSourceKind::NativePi)
                        .path,
                )
            })
        }
        fn todo_child_service(&self) -> Option<Arc<TodoChildService>> {
            Some(self.todo_child_service.clone())
        }
    }

    #[derive(Clone)]
    struct GatedSpawner {
        inner: TestSpawner,
        started: Arc<Semaphore>,
        release: Arc<Semaphore>,
        completed: Arc<Semaphore>,
        /// Session files (created journals) whose `factory.finish` completed,
        /// so shutdown-race tests can observe the created journal path.
        spawned_files: Arc<std::sync::Mutex<Vec<PathBuf>>>,
    }

    impl Default for GatedSpawner {
        fn default() -> Self {
            Self {
                inner: TestSpawner::default(),
                started: Arc::new(Semaphore::new(0)),
                release: Arc::new(Semaphore::new(0)),
                completed: Arc::new(Semaphore::new(0)),
                spawned_files: Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }
    }
    #[test]
    fn file_download_name_preserves_safe_extension_and_scrubs_headers() {
        assert_eq!(download_file_name("report FINAL.rs"), "report-FINAL.rs");
        let scrubbed = download_file_name("../bad\\name\r\n.txt");
        assert_eq!(scrubbed, "badname.txt");
        assert!(valid_session_download_file_name(&scrubbed));
    }

    /// Pre-feature there was no file-explorer staging command; this proves the
    /// new command uses the existing TTL/capability map and remains one-shot.
    #[tokio::test]
    async fn file_explorer_stage_download_reuses_one_shot_manager_store() {
        let (manager, _primary_registration) = manager_with(None).await;
        let cwd = manager.primary.application.session().cwd().to_path_buf();
        std::fs::write(cwd.join("download me.txt"), b"download body")
            .expect("download fixture");
        let response = manager
            .dispatch(
                RpcCommand::FileExplorerStageDownload {
                    id: Some("file-download".into()),
                    path: "download me.txt".into(),
                },
                None,
            )
            .await;
        assert!(response.success, "{response:?}");
        let data = response.data.expect("download response");
        assert_eq!(data["fileName"], "download-me.txt");
        let download_id = data["downloadId"].as_str().expect("download id");
        let staged = manager
            .consume_session_download(download_id)
            .await
            .expect("staged file");
        assert_eq!(&*staged.bytes, b"download body");
        assert_eq!(staged.file_name, "download-me.txt");
        assert!(manager.consume_session_download(download_id).await.is_none());
        manager.shutdown().await;
    }

    /// A direct ambient read would accept traversal or follow a symlinked
    /// parent. The manager surface must reject every such target without
    /// exposing the workspace or outside directory in its response.
    #[tokio::test]
    async fn file_explorer_stage_download_rejects_unsafe_targets_path_free() {
        let (manager, _primary_registration) = manager_with(None).await;
        let cwd = manager.primary.application.session().cwd().to_path_buf();
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret.txt"), b"secret").expect("secret");
        std::fs::create_dir(cwd.join("dir")).expect("directory");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), cwd.join("escape")).expect("escape");
        for path in [
            "../secret.txt".to_owned(),
            outside.path().join("secret.txt").to_string_lossy().into_owned(),
            "dir".to_owned(),
            #[cfg(unix)]
            "escape/secret.txt".to_owned(),
        ] {
            let response = manager
                .dispatch(
                    RpcCommand::FileExplorerStageDownload {
                        id: None,
                        path,
                    },
                    None,
                )
                .await;
            assert!(!response.success, "{response:?}");
            let error = response.error.expect("error");
            assert!(!error.contains(cwd.to_string_lossy().as_ref()), "{error}");
            assert!(!error.contains(outside.path().to_string_lossy().as_ref()), "{error}");
        }
        manager.shutdown().await;
    }

    impl SessionSpawner for GatedSpawner {
        fn spawn(
            &self,
            request: SessionSpawnRequest,
        ) -> Pin<Box<dyn Future<Output = Result<SessionSpawnResult>> + Send>> {
            let inner = self.inner.clone();
            let started = self.started.clone();
            let release = self.release.clone();
            let completed = self.completed.clone();
            let spawned_files = self.spawned_files.clone();
            Box::pin(async move {
                started.add_permits(1);
                release
                    .acquire()
                    .await
                    .expect("gated spawner release channel")
                    .forget();
                let outcome = inner.spawn(request).await;
                if let Ok(result) = &outcome {
                    spawned_files
                        .lock()
                        .expect("gated spawner file log")
                        .push(result.session_file.clone());
                }
                completed.add_permits(1);
                outcome
            })
        }

        fn todo_child_service(&self) -> Option<Arc<TodoChildService>> {
            self.inner.todo_child_service()
        }
    }

    /// Faux spawner that parks FOREVER inside `factory.prepare` or
    /// `factory.finish` (the trait-default finish routes a `Request` spawn
    /// straight into `spawn`), modelling a wedged creation the terminal
    /// shutdown must never hang on.
    #[derive(Clone)]
    struct NeverSpawner {
        prepare_entered: Arc<Semaphore>,
        finish_entered: Arc<Semaphore>,
        /// While set, `prepare` parks forever; cleared after the first
        /// creation parked so a second creation can reach `finish`.
        park_prepare: Arc<std::sync::atomic::AtomicBool>,
        todo_child_service: Arc<TodoChildService>,
    }

    impl Default for NeverSpawner {
        fn default() -> Self {
            Self {
                prepare_entered: Arc::new(Semaphore::new(0)),
                finish_entered: Arc::new(Semaphore::new(0)),
                park_prepare: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                todo_child_service: TodoChildService::new(),
            }
        }
    }

    impl SessionSpawner for NeverSpawner {
        fn prepare(
            &self,
            request: SessionSpawnRequest,
        ) -> Pin<Box<dyn Future<Output = Result<PreparedSessionSpawn>> + Send>> {
            let this = self.clone();
            Box::pin(async move {
                this.prepare_entered.add_permits(1);
                if this
                    .park_prepare
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    std::future::pending::<()>().await;
                }
                Ok(PreparedSessionSpawn::Request(request))
            })
        }

        fn spawn(
            &self,
            request: SessionSpawnRequest,
        ) -> Pin<Box<dyn Future<Output = Result<SessionSpawnResult>> + Send>> {
            let this = self.clone();
            Box::pin(async move {
                this.finish_entered.add_permits(1);
                std::future::pending::<()>().await;
                #[allow(unreachable_code)]
                Err(anyhow!("never spawner never finishes"))
            })
        }

        fn todo_child_service(&self) -> Option<Arc<TodoChildService>> {
            Some(self.todo_child_service.clone())
        }
    }

    /// Production-shaped spawner whose preparation carries real state
    /// between `prepare` and `finish` exactly like `RunSessionSpawner`, and
    /// whose `discard_prepared` counts the explicit, awaited teardown so
    /// the exhausted-capacity test can assert zero leaks.
    #[derive(Clone)]
    struct DiscardObservingSpawner {
        inner: TestSpawner,
        discarded_preparations: Arc<AtomicUsize>,
    }

    impl Default for DiscardObservingSpawner {
        fn default() -> Self {
            Self {
                inner: TestSpawner::default(),
                discarded_preparations: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl SessionSpawner for DiscardObservingSpawner {
        fn spawn(
            &self,
            request: SessionSpawnRequest,
        ) -> Pin<Box<dyn Future<Output = Result<SessionSpawnResult>> + Send>> {
            self.inner.spawn(request)
        }

        fn discard_prepared(
            &self,
            prepared: PreparedSessionSpawn,
        ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
            let discarded = self.discarded_preparations.clone();
            Box::pin(async move {
                discarded.fetch_add(1, Ordering::SeqCst);
                prepared.discard().await;
            })
        }

        fn todo_child_service(&self) -> Option<Arc<TodoChildService>> {
            self.inner.todo_child_service()
        }
    }

    #[derive(Clone)]
    struct FailingSpawner {
        inner: TestSpawner,
        failure: Arc<std::sync::Mutex<Option<&'static str>>>,
    }

    impl FailingSpawner {
        fn new() -> Self {
            Self {
                inner: TestSpawner::default(),
                failure: Arc::new(std::sync::Mutex::new(None)),
            }
        }

        fn fail_next(&self) {
            self.fail_next_with("injected spawn failure");
        }

        fn fail_next_with(&self, message: &'static str) {
            self.failure
                .lock()
                .expect("failure slot")
                .replace(message);
        }
    }
    impl SessionSpawner for FailingSpawner {
        fn spawn(
            &self,
            request: SessionSpawnRequest,
        ) -> Pin<Box<dyn Future<Output = Result<SessionSpawnResult>> + Send>> {
            if let Some(message) = self.failure.lock().expect("failure slot").take() {
                return Box::pin(async move { Err(anyhow!(message)) });
            }
            self.inner.spawn(request)
        }

        fn todo_child_service(&self) -> Option<Arc<TodoChildService>> {
            self.inner.todo_child_service()
        }
    }

    #[test]
    fn session_download_file_name_is_safe_bounded_html() {
        let unsafe_name = format!("../bad\\name\r\n{}.txt", "x".repeat(300));

        let file_name = session_download_file_name(Some(&unsafe_name), "fallback");
        assert!(file_name.ends_with(".html"));
        assert!(file_name.chars().count() <= MAX_SESSION_DOWNLOAD_FILENAME_CHARS);
        assert!(
            !file_name
                .chars()
                .any(|ch| matches!(ch, '/' | '\\' | '\r' | '\n' | '"'))
        );
        assert_ne!(file_name, ".html");
    }

    #[test]
    fn lifecycle_snapshot_failure_text_is_bounded_and_path_free() {
        // An internal snapshot failure whose causal chain embeds operator
        // store paths: the wire text collapses to one fixed actionable
        // category and never leaks a path, endpoint, or transcript detail.
        let internal = anyhow!(
            "failed to read /home/operator/.rpi/sessions/secret-project/session.jsonl: permission denied"
        )
        .context("protocol snapshot after lifecycle open");
        let rendered = bounded_lifecycle_snapshot_failure(&internal);
        assert_eq!(
            rendered,
            "lifecycle target could not be snapshotted; verify the session and retry"
        );
        assert!(!rendered.contains('/'), "{rendered}");
        assert!(
            rendered.chars().count() <= MAX_LIFECYCLE_SNAPSHOT_FAILURE_CHARS,
            "{rendered}"
        );
        let busy = anyhow!("too many busy sessions (limit {MAX_LOADED_SESSIONS}); open refused");
        assert_eq!(
            bounded_lifecycle_snapshot_failure(&busy),
            format!(
                "too many busy sessions (limit {MAX_LOADED_SESSIONS}); finish or stop active work and retry"
            )
        );
        let shutting_down = anyhow!("session manager is unavailable");
        assert_eq!(
            bounded_lifecycle_snapshot_failure(&shutting_down),
            "lifecycle target is shutting down; retry the operation"
        );
        let detail = anyhow!("session manager is shutting down").context("protocol snapshot");
        assert_eq!(
            bounded_lifecycle_snapshot_failure(&detail),
            "lifecycle target is shutting down; retry the operation"
        );
    }

    #[test]
    fn switch_failure_is_bounded_and_hides_internal_context() {
        let internal =
            anyhow!("provider connection failed").context("building resumed session runtime");
        assert_eq!(
            bounded_switch_failure(&internal),
            "session could not be opened right now; verify the selection and retry"
        );
        let busy = anyhow!(
            "too many busy sessions (limit {MAX_LOADED_SESSIONS}); every loaded session has work in progress"
        );
        assert_eq!(
            bounded_switch_failure(&busy),
            format!(
                "too many busy sessions (limit {MAX_LOADED_SESSIONS}); finish or stop active work and retry"
            )
        );
        // Switch failures never adopt new-session directory categories even
        // when a filesystem capability error rides the chain: a missing
        // session file is not a missing project directory.
        let missing_target = pi_coding::inspect_operator_directory(Path::new(
            "/definitely-missing-session-file.jsonl",
        ))
        .expect_err("missing path must fail")
        .context("preparing saved session");
        assert_eq!(
            bounded_switch_failure(&missing_target),
            "session could not be opened right now; verify the selection and retry"
        );
        assert_eq!(
            bounded_new_session_failure(&missing_target),
            "new session directory does not exist"
        );
        let relative_cwd = anyhow!("new session cwd must be absolute")
            .context("validating new session cwd");
        assert_eq!(
            bounded_new_session_failure(&relative_cwd),
            "new session cwd must be an absolute directory path"
        );
        let trust_denied = anyhow!(
            "explicit project extension requires project trust: {} (pass --approve to allow it)",
            "operator-manifest-path"
        );
        let trust_message = bounded_new_session_failure(&trust_denied);
        assert_eq!(
            trust_message,
            "new session project requires trust approval"
        );
        assert!(!trust_message.contains("operator-manifest-path"));
    }

    #[test]
    fn new_session_failure_is_bounded_with_directory_categories() {
        let selected = "operator-selected-private-dir";
        let fixture = tempfile::tempdir().expect("category fixture");
        let inspect_error = |path: &Path, label: &str| {
            pi_coding::inspect_operator_directory(path)
                .expect_err("invalid directory must fail")
                .context(format!("validating new session cwd for {label}"))
        };
        let missing_error = inspect_error(&fixture.path().join("missing"), selected);
        assert_eq!(
            bounded_new_session_failure(&missing_error),
            "new session directory does not exist"
        );
        let file = fixture.path().join("file.txt");
        std::fs::write(&file, "not a directory").expect("fixture file");
        let not_directory_error = inspect_error(&file, selected);
        assert_eq!(
            bounded_new_session_failure(&not_directory_error),
            "new session path is not a directory"
        );
        #[cfg(unix)]
        {
            let real = fixture.path().join("real");
            std::fs::create_dir_all(&real).expect("real directory");
            let link = fixture.path().join("link");
            std::os::unix::fs::symlink(&real, &link).expect("directory symlink");
            let symlink_error = inspect_error(&link, selected);
            assert_eq!(
                bounded_new_session_failure(&symlink_error),
                "new session path must not be a symbolic link"
            );
        }
        let malformed = anyhow::Error::new(pi_sandbox::PolicyValidationError::InvalidField {
            field: "path",
            reason: "must not contain NUL",
        })
        .context(format!("validating new session cwd for {selected}"));
        assert_eq!(
            bounded_new_session_failure(&malformed),
            "new session directory path is invalid"
        );
        let unsupported = anyhow::Error::new(pi_coding::FsError::UnsupportedEntryType)
            .context(format!("validating new session cwd for {selected}"));
        assert_eq!(
            bounded_new_session_failure(&unsupported),
            "new session path is not a directory"
        );
        let factory_failure =
            anyhow!("injected spawn failure").context("building fresh session runtime");
        assert_eq!(
            bounded_new_session_failure(&factory_failure),
            "session could not be opened right now; verify the selection and retry"
        );
        for error in [missing_error, not_directory_error, unsupported, malformed] {
            let rendered = bounded_new_session_failure(&error);
            assert!(!rendered.contains(selected), "{rendered}");
        }
    }

    #[test]
    fn new_session_failure_classification_is_stable_and_non_retryable() {
        let fixture = tempfile::tempdir().expect("classification fixture");
        let missing = pi_coding::prepare_operator_directory(&fixture.path().join("missing"))
            .expect_err("missing directory")
            .context("validating new session cwd");
        assert_eq!(classify_new_session_failure(&missing).code, "cwd_missing");
        assert!(!classify_new_session_failure(&missing).retryable);

        let file = fixture.path().join("file");
        std::fs::write(&file, "x").expect("file");
        let not_directory = pi_coding::prepare_operator_directory(&file)
            .expect_err("file is not directory")
            .context("validating new session cwd");
        assert_eq!(
            classify_new_session_failure(&not_directory).code,
            "cwd_not_directory"
        );
        assert!(!classify_new_session_failure(&not_directory).retryable);

        let relative = anyhow!("new session cwd must be absolute");
        assert_eq!(classify_new_session_failure(&relative).code, "cwd_relative");
        assert!(!classify_new_session_failure(&relative).retryable);

        let trust = anyhow!("explicit project extension requires project trust");
        assert_eq!(classify_new_session_failure(&trust).code, "trust_denied");
        assert!(!classify_new_session_failure(&trust).retryable);

        #[cfg(unix)]
        {
            let target = fixture.path().join("target");
            std::fs::create_dir(&target).expect("target directory");
            let link = fixture.path().join("link");
            std::os::unix::fs::symlink(&target, &link).expect("directory symlink");
            let symlink = pi_coding::prepare_operator_directory(&link)
                .expect_err("symlink directory")
                .context("validating new session cwd");
            let class = classify_new_session_failure(&symlink);
            assert_eq!(class.code, "cwd_symlink");
            assert!(!class.retryable);
        }

        let capacity = classify_new_session_failure(
            &SessionRuntimeManager::loaded_slot_exhausted_error(),
        );
        assert_eq!(capacity.code, "busy_capacity");
        assert!(capacity.retryable);
    }

    #[test]
    fn switch_failure_classification_hides_chain_and_user_input() {
        let selected = "operator-selected-private-path.jsonl";
        let internal =
            anyhow!("invalid session path {selected}").context("building resumed session runtime");
        let class = classify_switch_failure(&internal);
        assert_eq!(class.code, "runtime_unavailable");
        assert!(class.retryable);
        let rendered = format!("{class:?}");
        assert!(!rendered.contains(selected));
        assert!(!rendered.contains("building resumed session runtime"));
        let busy = classify_switch_failure(&SessionRuntimeManager::loaded_slot_exhausted_error());
        assert_eq!(busy.code, "busy_capacity");
        assert!(busy.retryable);
        assert_eq!(busy.cause, "loaded_slot_capacity");
    }

    #[test]
    fn switch_failure_classification_maps_cause_classes() {
        let missing = anyhow!(pi_coding::CatalogError::SessionNotFound(
            "/private/row.jsonl".to_owned()
        ))
        .context("resolving Web session selection");
        let stale = classify_switch_failure(&missing);
        assert_eq!(stale.code, "stale_catalog_target");
        assert!(!stale.retryable);
        assert_eq!(stale.cause, "session_not_found");

        let duplicate =
            anyhow!("session id native-id is already loaded from another session file");
        assert_eq!(
            classify_switch_failure(&duplicate).code,
            "invalid_selection"
        );

        let stale_row = anyhow!(anyhow!("row mismatch").context(
            "negotiated session selection no longer matches the session catalog"
        ));
        assert_eq!(
            classify_switch_failure(&stale_row).code,
            "stale_catalog_target"
        );
    }
    #[tokio::test]
    async fn selected_session_export_stages_selected_content_not_active_runtime() {
        let (manager, _primary_registration) = manager_with(None).await;

        let selected_dir = tempfile::tempdir().expect("selected directory");
        let selected_path = selected_dir.path().join("selected.jsonl");
        std::fs::write(
            &selected_path,
            concat!(
                r#"{"type":"session","version":3,"id":"selected-row","timestamp":"2026-01-01T00:00:00Z","cwd":"<workspace>"}"#,
                "\n",
                r#"{"type":"message","id":"u","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":[{"type":"text","text":"Selected row export marker"}],"timestamp":0}}"#,
                "\n"
            ),
        )
        .expect("write selected session");

        let response = manager
            .dispatch(
                RpcCommand::ExportHtml {
                    id: Some("selected-export".into()),
                    output_path: None,
                    session_path: Some(selected_path.to_string_lossy().into_owned()),
                },
                None,
            )
            .await;

        assert!(response.success, "{response:?}");
        let data = response.data.expect("export response data");
        let download_id = data["downloadId"].as_str().expect("download id").to_owned();
        assert_eq!(data["fileName"], "selected-row.html");
        assert!(data.get("path").is_none());
        let download = manager
            .consume_session_download(&download_id)
            .await
            .expect("staged selected export");
        let html = String::from_utf8(download.bytes.to_vec()).expect("HTML UTF-8");
        assert!(html.contains("selected-row"));
        assert!(html.contains("Selected row export marker"));
        assert!(!html.contains(manager.primary_session_id().as_str()));
        assert!(
            manager
                .consume_session_download(&download_id)
                .await
                .is_none()
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn selected_session_copy_context_renders_selected_not_active_runtime() {
        let (manager, _primary_registration) = manager_with(None).await;
        let primary_id = manager.primary_session_id();
        let primary_incarnation = manager
            .current_runtime_identity(&primary_id)
            .await
            .expect("primary identity")
            .runtime_incarnation;
        let loaded_before = manager.registry.read().await.distinct_runtimes().len();

        let selected_dir = tempfile::tempdir().expect("selected directory");
        let selected_path = selected_dir.path().join("selected.jsonl");
        std::fs::write(
            &selected_path,
            concat!(
                r#"{"type":"session","version":3,"id":"selected-row","timestamp":"2026-01-01T00:00:00Z","cwd":"<workspace>"}"#,
                "\n",
                r#"{"type":"message","id":"u","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":[{"type":"text","text":"Selected row context marker"}],"timestamp":0}}"#,
                "\n"
            ),
        )
        .expect("write selected session");

        let response = manager
            .dispatch(
                RpcCommand::SessionCopyContext {
                    id: Some("copy-selected".into()),
                    session_path: selected_path.to_string_lossy().into_owned(),
                },
                None,
            )
            .await;

        assert!(response.success, "{response:?}");
        assert_eq!(response.command, "session_copy_context");
        assert_eq!(
            response.session_id.as_deref(),
            Some(primary_id.as_str()),
            "response must carry the source runtime's sessionId"
        );
        let data = response.data.expect("copy-context response data");
        assert_eq!(data["format"], json!(pi_coding::SESSION_CONTEXT_FORMAT));
        assert_eq!(data["truncated"], json!(false));
        let text = data["text"].as_str().expect("context text");
        assert!(text.starts_with("# Pi session context"), "{text}");
        assert!(text.contains("Format: pi-session-context/1"), "{text}");
        assert!(text.contains("Truncated: no"), "{text}");
        assert!(text.contains("Selected row context marker"), "{text}");
        assert!(!text.contains(&primary_id), "{text}");
        assert!(
            !text.contains(selected_path.to_string_lossy().as_ref()),
            "no selected path may leak into the artifact: {text}"
        );

        assert_eq!(
            manager.registry.read().await.distinct_runtimes().len(),
            loaded_before,
            "copy-context must not load the selected session"
        );
        assert_eq!(
            manager
                .current_runtime_identity(&primary_id)
                .await
                .expect("primary identity after")
                .runtime_incarnation,
            primary_incarnation,
            "copy-context must not touch the source runtime"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn selected_session_copy_context_missing_and_corrupt_preserve_safe_cause() {
        let (_home, native, catalog) = web_catalog_fixture();
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog));
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;

        let missing = manager
            .execute_manager_level(
                &manager.primary,
                RpcCommand::SessionCopyContext {
                    id: Some("missing-copy-context".into()),
                    session_path: "missing.jsonl".into(),
                },
                LifecycleResponseMode::FullSnapshot,
            )
            .await;
        assert!(!missing.success, "{missing:?}");
        assert_eq!(
            missing.error.as_deref(),
            Some("selected session is unavailable: session was not found")
        );
        assert!(
            !missing
                .error
                .as_deref()
                .is_some_and(|error| error.contains("missing.jsonl")),
            "{missing:?}"
        );
        assert!(
            !missing
                .error
                .as_deref()
                .is_some_and(|error| error.contains(native.path().to_string_lossy().as_ref())),
            "{missing:?}"
        );

        let invalid_name = "invalid-copy-context.jsonl";
        let invalid = native.path().join("project").join(invalid_name);
        std::fs::create_dir_all(invalid.parent().expect("invalid transcript parent"))
            .expect("create invalid transcript parent");
        std::fs::write(&invalid, b"not-json\n").expect("write invalid transcript");
        let corrupt = manager
            .execute_manager_level(
                &manager.primary,
                RpcCommand::SessionCopyContext {
                    id: Some("corrupt-copy-context".into()),
                    session_path: invalid.to_string_lossy().into_owned(),
                },
                LifecycleResponseMode::FullSnapshot,
            )
            .await;
        assert!(!corrupt.success, "{corrupt:?}");
        assert_eq!(
            corrupt.error.as_deref(),
            Some("selected session is unavailable: session transcript is invalid")
        );
        assert!(
            !corrupt
                .error
                .as_deref()
                .is_some_and(|error| error.contains(invalid_name)),
            "{corrupt:?}"
        );
        assert!(
            !corrupt
                .error
                .as_deref()
                .is_some_and(|error| error.contains(native.path().to_string_lossy().as_ref())),
            "{corrupt:?}"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn selected_session_copy_context_rejects_foreign_source_without_import() {
        let (_home, native, catalog) = web_catalog_fixture();
        let cwd = tempfile::tempdir().expect("source cwd");
        let source = write_codex_session(&catalog, "codex-copy-context", cwd.path());
        let source_bytes_before = std::fs::read(&source).expect("source bytes before");
        let native_root = catalog
            .root_for(pi_coding::SessionSourceKind::NativePi)
            .path
            .clone();
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog));
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let loaded_before = manager.registry.read().await.distinct_runtimes().len();

        let response = manager
            .execute_manager_level(
                &manager.primary,
                RpcCommand::SessionCopyContext {
                    id: Some("foreign-copy-context".into()),
                    session_path: source.to_string_lossy().into_owned(),
                },
                LifecycleResponseMode::FullSnapshot,
            )
            .await;

        assert!(!response.success, "{response:?}");
        assert_eq!(
            response.error.as_deref(),
            Some("selected session is unavailable: session source is unsupported")
        );
        assert!(
            !response
                .error
                .as_deref()
                .is_some_and(|error| error.contains("codex-copy-context")),
            "no foreign path may leak into the rejection: {response:?}"
        );
        assert!(
            !response
                .error
                .as_deref()
                .is_some_and(|error| error.contains(native.path().to_string_lossy().as_ref())),
            "{response:?}"
        );

        // The foreign source is never imported: bytes invariant, no native
        // copy created, no runtime spawned or loaded.
        assert_eq!(
            std::fs::read(&source).expect("source bytes after"),
            source_bytes_before,
        );
        assert_eq!(
            std::fs::read_dir(&native_root)
                .expect("native root listing")
                .count(),
            0,
            "no native import may be created for a rejected foreign source",
        );
        assert_eq!(spawner.spawns.load(Ordering::SeqCst), 0);
        assert_eq!(
            manager.registry.read().await.distinct_runtimes().len(),
            loaded_before,
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn session_download_staging_enforces_size_count_and_expiry() {
        let (manager, _primary_registration) = manager_with(None).await;
        let oversized: Arc<[u8]> = vec![0; MAX_SESSION_DOWNLOAD_BYTES + 1].into();
        assert!(
            manager
                .stage_session_download(
                    oversized,
                    "large.html".into(),
                    Instant::now() + Duration::from_secs(1),
                )
                .await
                .is_err()
        );

        for index in 0..MAX_STAGED_SESSION_DOWNLOADS {
            manager
                .stage_session_download(
                    Arc::<[u8]>::from(vec![index as u8]),
                    format!("{index}.html"),
                    Instant::now() + Duration::from_secs(60),
                )
                .await
                .expect("stage bounded download");
        }
        assert!(
            manager
                .stage_session_download(
                    Arc::<[u8]>::from(vec![0_u8]),
                    "overflow.html".into(),
                    Instant::now() + Duration::from_secs(60),
                )
                .await
                .is_err()
        );

        manager.session_downloads.write().await.clear();
        let expired = manager
            .stage_session_download(
                Arc::<[u8]>::from(vec![1_u8]),
                "expired.html".into(),
                Instant::now() - Duration::from_millis(1),
            )
            .await
            .expect("stage expired fixture");
        assert!(manager.consume_session_download(&expired).await.is_none());
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn selected_pi_export_imports_without_mutating_foreign_source() {
        let (_home, _native, catalog) = web_catalog_fixture();
        let source = catalog
            .root_for(pi_coding::SessionSourceKind::Pi)
            .path
            .join("project/pi-export.jsonl");
        std::fs::create_dir_all(source.parent().expect("Pi parent")).expect("create Pi parent");
        std::fs::write(
            &source,
            concat!(
                r#"{"type":"session","version":3,"id":"pi-export","timestamp":"2026-01-01T00:00:00Z","cwd":"<workspace>"}"#,
                "\n",
                r#"{"type":"message","id":"u","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":[{"type":"text","text":"Immutable Pi export marker"}],"timestamp":0}}"#,
                "\n"
            ),
        )
        .expect("write Pi export source");
        let before = std::fs::read(&source).expect("source before");
        let modified_before = std::fs::metadata(&source)
            .expect("metadata before")
            .modified()
            .ok();
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog));
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;

        let response = manager
            .dispatch(
                RpcCommand::ExportHtml {
                    id: None,
                    output_path: None,
                    session_path: Some(source.to_string_lossy().into_owned()),
                },
                None,
            )
            .await;
        assert!(response.success, "{response:?}");
        let download_id = response
            .data
            .as_ref()
            .and_then(|data| data["downloadId"].as_str())
            .expect("download id")
            .to_owned();
        let download = manager
            .consume_session_download(&download_id)
            .await
            .expect("download");
        assert!(String::from_utf8_lossy(&download.bytes).contains("Immutable Pi export marker"));
        assert_eq!(std::fs::read(&source).expect("source after"), before);
        assert_eq!(
            std::fs::metadata(&source)
                .expect("metadata after")
                .modified()
                .ok(),
            modified_before,
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn selected_export_missing_catalog_preserves_safe_cause() {
        let (_home, native, catalog) = web_catalog_fixture();
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog));
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let missing_name = "does-not-exist.jsonl";
        let missing = native.path().join("project").join(missing_name);

        let response = manager
            .execute_manager_level(
                &manager.primary,
                RpcCommand::ExportHtml {
                    id: Some("missing-selected-export".into()),
                    output_path: None,
                    session_path: Some("missing.jsonl".into()),
                },
                LifecycleResponseMode::FullSnapshot,
            )
            .await;

        assert!(!response.success, "{response:?}");
        let error = response.error.expect("safe export failure");
        assert_eq!(
            error,
            "selected session is unavailable: session was not found"
        );
        assert!(!error.contains(missing_name), "{error}");
        assert!(
            !error.contains(native.path().to_string_lossy().as_ref()),
            "{error}"
        );
        assert!(
            manager
                .consume_session_download("missing-export-download")
                .await
                .is_none()
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn selected_export_invalid_transcript_preserves_safe_cause() {
        let (_home, native, catalog) = web_catalog_fixture();
        let invalid_name = "invalid-selected.jsonl";
        let invalid = native.path().join("project").join(invalid_name);
        std::fs::create_dir_all(invalid.parent().expect("invalid transcript parent"))
            .expect("create invalid transcript parent");
        std::fs::write(&invalid, b"not-json\n").expect("write invalid transcript");
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog));
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;

        let response = manager
            .execute_manager_level(
                &manager.primary,
                RpcCommand::ExportHtml {
                    id: Some("invalid-selected-export".into()),
                    output_path: None,
                    session_path: Some(invalid.to_string_lossy().into_owned()),
                },
                LifecycleResponseMode::FullSnapshot,
            )
            .await;

        assert!(!response.success, "{response:?}");
        let error = response.error.expect("safe invalid transcript failure");
        assert_eq!(
            error,
            "selected session is unavailable: session transcript is invalid"
        );
        assert!(!error.contains(invalid_name), "{error}");
        assert!(
            !error.contains(native.path().to_string_lossy().as_ref()),
            "{error}"
        );
        manager.shutdown().await;
    }
    #[tokio::test]
    async fn selected_export_overlap_import_preserves_distinct_safe_cause() {
        let (home, catalog) = overlapping_pi_catalog_fixture();
        let source = catalog
            .root_for(pi_coding::SessionSourceKind::Pi)
            .path
            .join("project/overlap-export.jsonl");
        std::fs::create_dir_all(source.parent().expect("overlap source parent"))
            .expect("create overlap source parent");
        std::fs::write(
            &source,
            concat!(
                r#"{"type":"session","version":3,"id":"overlap-export","timestamp":"2026-01-01T00:00:00Z","cwd":"<workspace>"}"#,
                "\n",
                r#"{"type":"message","id":"u","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":[{"type":"text","text":"Overlap export marker"}],"timestamp":0}}"#,
                "\n"
            ),
        )
        .expect("write overlap source");
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog));
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;

        let response = manager
            .execute_manager_level(
                &manager.primary,
                RpcCommand::ExportHtml {
                    id: Some("overlap-selected-export".into()),
                    output_path: None,
                    session_path: Some(source.to_string_lossy().into_owned()),
                },
                LifecycleResponseMode::FullSnapshot,
            )
            .await;

        assert!(!response.success, "{response:?}");
        let error = response.error.expect("safe overlap import failure");
        assert_eq!(
            error,
            "selected session is unavailable: session import destination is unavailable"
        );
        assert!(!error.contains("overlap-export.jsonl"), "{error}");
        assert!(
            !error.contains(home.path().to_string_lossy().as_ref()),
            "{error}"
        );
        manager.shutdown().await;
    }

    async fn manager_with(
        factory: Option<Arc<dyn SessionSpawner>>,
    ) -> (Arc<SessionRuntimeManager>, FauxProviderRegistration) {
        manager_with_sized(factory, 8).await
    }

    async fn manager_with_sized(
        factory: Option<Arc<dyn SessionSpawner>>,
        chunk_size: usize,
    ) -> (Arc<SessionRuntimeManager>, FauxProviderRegistration) {
        let (model, registration) = faux_model_sized("mgr-primary", chunk_size);
        let (application, cwd) = bare_application_with(model).await;
        let manager =
            SessionRuntimeManager::new(application, ExtensionUiAdapter::default(), factory, None)
                .await
                .expect("session runtime manager");
        let _kept = cwd.keep();
        (manager, registration)
    }

    /// Manager whose PRIMARY turn stream blocks on `release` (abort-aware),
    /// so busy-state assertions are deterministic: the unpaced faux stream
    /// completes an entire 200k turn within one scheduling burst, which
    /// races any busy probe issued after prompt admission resolves.
    async fn manager_with_gated_primary_stream(
        factory: Option<Arc<dyn SessionSpawner>>,
        release: CancellationToken,
        output: &'static str,
    ) -> (Arc<SessionRuntimeManager>, TempDir) {
        let (model, _registration) = faux_model("mgr-primary");
        let stream_fn: pi_agent::StreamFn = {
            let release = release.clone();
            Arc::new(move |model, _context, options| {
                let release = release.clone();
                Box::pin(async move {
                    let stream = pi_ai::new_assistant_message_event_stream();
                    let producer = stream.clone();
                    tokio::spawn(async move {
                        match options.stream.abort_signal {
                            Some(abort) => tokio::select! {
                                () = release.cancelled() => {}
                                () = abort.cancelled() => {}
                            },
                            None => release.cancelled().await,
                        }
                        let mut message = pi_ai::AssistantMessage::pending(&model);
                        message.content.push(pi_ai::ContentBlock::text(output));
                        message.stop_reason = pi_ai::StopReason::Stop;
                        producer.end(Some(message)).await;
                    });
                    stream
                })
            })
        };
        let dir = tempfile::tempdir().expect("session dir");
        let session =
            isolated_session_with_stream_fn(model.clone(), dir.path(), false, Some(stream_fn))
                .await;
        let session_dir = session.session_dir();
        let recorder = pi_coding::start_session_in(
            &session_dir,
            Some(&model),
            Some("off"),
            Some(dir.path()),
            None,
            None,
        )
        .expect("start session");
        recorder.persist_now().expect("persist header");
        session.record(recorder).expect("record");
        let application = isolated_application(session).await;
        let manager =
            SessionRuntimeManager::new(application, ExtensionUiAdapter::default(), factory, None)
                .await
                .expect("session runtime manager");
        (manager, dir)
    }

    async fn manager_with_codename(
        codename: Option<&str>,
    ) -> (Arc<SessionRuntimeManager>, FauxProviderRegistration) {
        let (model, registration) = faux_model_sized("mgr-primary", 8);
        let (application, cwd) = bare_application_with(model).await;
        let manager = SessionRuntimeManager::new(
            application,
            ExtensionUiAdapter::default(),
            None,
            codename.map(str::to_owned),
        )
        .await
        .expect("session runtime manager");
        let _kept = cwd.keep();
        (manager, registration)
    }

    async fn manager_recorded_with_codename(
        codename: Option<&str>,
    ) -> (
        Arc<SessionRuntimeManager>,
        String,
        FauxProviderRegistration,
    ) {
        let (model, registration) = faux_model_sized("mgr-recorded", 8);
        let (application, session_id, cwd) = recorded_application_with(model).await;
        let manager = SessionRuntimeManager::new(
            application,
            ExtensionUiAdapter::default(),
            None,
            codename.map(str::to_owned),
        )
        .await
        .expect("session runtime manager");
        let _kept = cwd.keep();
        (manager, session_id, registration)
    }

    #[tokio::test]
    async fn absent_session_id_routes_to_primary() {
        let (manager, _primary_registration) = manager_with(None).await;
        let primary_id = manager.primary_session_id().to_owned();
        let response = manager
            .dispatch(
                RpcCommand::GetState {
                    id: Some("r".into()),
                },
                None,
            )
            .await;
        assert!(response.success, "{}", response.error.unwrap_or_default());
        let data = response.data.expect("state data");
        assert_eq!(
            data.get("sessionId").and_then(Value::as_str),
            Some(primary_id.as_str())
        );
        assert!(data.get("cwd").is_some());
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn get_state_injects_runner_codename_only_when_configured() {
        let (named, _named_registration) = manager_with_codename(Some("alpha-one")).await;
        let response = named.dispatch(RpcCommand::GetState { id: None }, None).await;
        assert!(response.success, "{}", response.error.unwrap_or_default());
        let data = response.data.expect("state data");
        assert_eq!(
            data.get("runnerCodename").and_then(Value::as_str),
            Some("alpha-one")
        );
        named.shutdown().await;

        let (anonymous, _anonymous_registration) = manager_with_codename(None).await;
        let response = anonymous
            .dispatch(RpcCommand::GetState { id: None }, None)
            .await;
        assert!(response.success, "{}", response.error.unwrap_or_default());
        assert!(
            response
                .data
                .expect("state data")
                .get("runnerCodename")
                .is_none(),
            "runnerCodename must stay absent without a resolved codename"
        );
        anonymous.shutdown().await;
    }

    #[tokio::test]
    async fn get_state_carries_runner_codename_on_recorded_session_state() {
        // Production recorded sessions embed a real sessionId in the get_state
        // payload; the codename must be inserted independently of the
        // sessionId backfill gate, on both the bootstrap (id-less) probe and
        // the session-scoped probe.
        let (named, session_id, _registration) =
            manager_recorded_with_codename(Some("alpha-one")).await;

        let bootstrap = named
            .dispatch(RpcCommand::GetState { id: None }, None)
            .await;
        assert!(bootstrap.success, "{}", bootstrap.error.unwrap_or_default());
        let data = bootstrap.data.expect("state data");
        assert_eq!(
            data.get("sessionId").and_then(Value::as_str),
            Some(session_id.as_str())
        );
        assert_eq!(
            data.get("runnerCodename").and_then(Value::as_str),
            Some("alpha-one")
        );

        let scoped = named
            .dispatch(
                RpcCommand::GetState {
                    id: Some("r".into()),
                },
                Some(session_id.clone()),
            )
            .await;
        assert!(scoped.success, "{}", scoped.error.unwrap_or_default());
        let data = scoped.data.expect("state data");
        assert_eq!(
            data.get("sessionId").and_then(Value::as_str),
            Some(session_id.as_str()),
            "recorded payload keeps its real sessionId"
        );
        assert_eq!(
            data.get("runnerCodename").and_then(Value::as_str),
            Some("alpha-one"),
            "recorded get_state must still carry runnerCodename"
        );
        named.shutdown().await;
    }

    #[tokio::test]
    async fn anonymous_manager_omits_runner_codename_across_session_id_shapes() {
        let (anonymous, session_id, _registration) = manager_recorded_with_codename(None).await;

        let scoped = anonymous
            .dispatch(
                RpcCommand::GetState {
                    id: Some("r".into()),
                },
                Some(session_id.clone()),
            )
            .await;
        assert!(scoped.success, "{}", scoped.error.unwrap_or_default());
        let data = scoped.data.expect("state data");
        assert_eq!(
            data.get("sessionId").and_then(Value::as_str),
            Some(session_id.as_str())
        );
        assert!(
            data.get("runnerCodename").is_none(),
            "runnerCodename must stay absent without a resolved codename"
        );

        let bootstrap = anonymous
            .dispatch(RpcCommand::GetState { id: None }, None)
            .await;
        assert!(bootstrap.success, "{}", bootstrap.error.unwrap_or_default());
        let data = bootstrap.data.expect("state data");
        assert_eq!(
            data.get("sessionId").and_then(Value::as_str),
            Some(session_id.as_str())
        );
        assert!(
            data.get("runnerCodename").is_none(),
            "runnerCodename must stay absent without a resolved codename"
        );
        anonymous.shutdown().await;
    }

    #[tokio::test]
    async fn unknown_session_id_fails_closed() {
        let (manager, _primary_registration) = manager_with(None).await;
        let response = manager
            .dispatch(
                RpcCommand::GetState {
                    id: Some("r".into()),
                },
                Some("nope".into()),
            )
            .await;
        assert!(!response.success);
        assert_eq!(response.session_id.as_deref(), Some("nope"));
        let error = response.error.expect("error");
        assert!(error.contains("unknown session nope"), "{error}");
        let ok = manager
            .dispatch(RpcCommand::GetState { id: None }, None)
            .await;
        assert!(ok.success);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn persona_commands_are_session_scoped_and_fail_closed() {
        let (manager, _primary_registration) = manager_with(None).await;
        // Every persona command (reads, mutations, destructive ops with
        // confirmation) routes through the sessionId registry: an unknown
        // session is rejected BEFORE any runtime is touched, so a stale or
        // forged sessionId can never mutate another session's persona store.
        for command in [
            RpcCommand::PersonaList { id: None },
            RpcCommand::PersonaGet {
                id: None,
                name: "mentor".into(),
            },
            RpcCommand::PersonaCreate {
                id: None,
                name: "guide".into(),
                content: "---\nname: guide\ndescription: g\n---\nprompt".into(),
            },
            RpcCommand::PersonaEdit {
                id: None,
                name: "guide".into(),
                content: "---\nname: guide\ndescription: g\n---\nprompt".into(),
            },
            RpcCommand::PersonaRemove {
                id: None,
                name: "mentor".into(),
                confirm: true,
            },
            RpcCommand::PersonaPurge {
                id: None,
                name: "mentor".into(),
                confirm: true,
            },
            RpcCommand::PersonaSelect {
                id: None,
                name: "mentor".into(),
            },
            RpcCommand::PersonaClear { id: None },
            RpcCommand::PersonaCurrent { id: None },
        ] {
            let name = command.command_name();
            let response = manager.dispatch(command, Some("missing".into())).await;
            assert!(!response.success, "{name} must fail on an unknown session");
            let error = response.error.expect("error");
            assert!(error.contains("unknown session missing"), "{name}: {error}");
        }
        // Absent sessionId routes to the primary runtime. The hermetic primary
        // now owns an isolated ResourceManager, so persona support is enabled
        // with an empty fixture-local catalog rather than ambient state.
        let listed = manager
            .dispatch(RpcCommand::PersonaList { id: None }, None)
            .await;
        assert!(listed.success, "{listed:?}");
        let data = listed.data.expect("list");
        assert_eq!(data["enabled"], json!(true));
        assert_eq!(data["personas"], json!([]));
        manager.shutdown().await;
    }

    #[test]
    fn web_catalog_switch_imports_foreign_sessions_idempotently() {
        let (_home, _native, catalog) = web_catalog_fixture();
        let cwd = tempfile::tempdir().expect("source cwd");
        let source = write_codex_session(&catalog, "codex-web", cwd.path());
        let sources = [
            pi_coding::SessionSourceKind::NativePi,
            pi_coding::SessionSourceKind::Omp,
            pi_coding::SessionSourceKind::Codex,
            pi_coding::SessionSourceKind::Grok,
        ];

        let first = resolve_web_catalog_session_path(
            &catalog,
            cwd.path(),
            &source.to_string_lossy(),
            &sources,
        )
        .expect("first import");
        let second = resolve_web_catalog_session_path(
            &catalog,
            cwd.path(),
            &source.to_string_lossy(),
            &sources,
        )
        .expect("reuse import");

        assert_ne!(first, source);
        assert_eq!(second, first);
        let native_root = catalog
            .root_for(pi_coding::SessionSourceKind::NativePi)
            .path;
        assert_eq!(first.parent(), Some(native_root.as_path()));
        pi_coding::PreparedSessionResume::prepare_path(&first).expect("imported native session");
    }

    #[tokio::test]
    async fn switch_session_imports_pi_catalog_path_without_mutating_source() {
        let (_home, _native, catalog) = web_catalog_fixture();
        let source = catalog
            .root_for(pi_coding::SessionSourceKind::Pi)
            .path
            .join("project/pi-wire.jsonl");
        std::fs::create_dir_all(source.parent().expect("Pi session parent"))
            .expect("create Pi session root");
        std::fs::write(
            &source,
            concat!(
                r#"{"type":"session","version":3,"id":"pi-wire","timestamp":"2026-01-01T00:00:00Z","cwd":"<workspace>"}"#,
                "\n",
                r#"{"type":"message","id":"u","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":"pi wire prompt"}}"#,
                "\n"
            ),
        )
        .expect("write Pi session");
        let source_bytes = std::fs::read(&source).expect("Pi source bytes");
        let source_modified = std::fs::metadata(&source)
            .expect("Pi source metadata")
            .modified()
            .expect("Pi source mtime");
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog.clone()));
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;

        let response = manager
            .dispatch(
                RpcCommand::SwitchSession {
                    id: Some("pi-wire-switch".into()),
                    session_path: source.to_string_lossy().into_owned(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                None,
            )
            .await;

        assert!(response.success, "{response:?}");
        assert_eq!(spawner.spawns.load(Ordering::SeqCst), 1);
        let opened = spawner.opened.lock().expect("opened");
        assert_eq!(opened.len(), 1);
        assert_ne!(opened[0], source);
        assert!(catalog.is_safe_session_path(pi_coding::SessionSourceKind::NativePi, &opened[0]));
        drop(opened);
        assert_eq!(
            std::fs::read(&source).expect("Pi source after"),
            source_bytes
        );
        assert_eq!(
            std::fs::metadata(&source)
                .expect("Pi source metadata after")
                .modified()
                .expect("Pi source mtime after"),
            source_modified,
        );
        manager.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn switch_session_rejects_pi_catalog_symlink_alias_before_spawn() {
        let (_home, _native, catalog) = web_catalog_fixture();
        let source = catalog
            .root_for(pi_coding::SessionSourceKind::Pi)
            .path
            .join("project/pi-linked.jsonl");
        std::fs::create_dir_all(source.parent().expect("Pi session parent"))
            .expect("create Pi session root");
        std::fs::write(
            &source,
            concat!(
                r#"{"type":"session","version":3,"id":"pi-linked","timestamp":"2026-01-01T00:00:00Z","cwd":"<workspace>"}"#,
                "\n"
            ),
        )
        .expect("write Pi session");
        let source_bytes = std::fs::read(&source).expect("Pi source bytes");
        let alias_dir = tempfile::tempdir().expect("alias dir");
        let alias = alias_dir.path().join("pi-session-alias.jsonl");
        std::os::unix::fs::symlink(&source, &alias).expect("link Pi session outside catalog root");
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog.clone()));
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;

        let response = manager
            .dispatch(
                RpcCommand::SwitchSession {
                    id: Some("pi-alias-switch".into()),
                    session_path: alias.to_string_lossy().into_owned(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                None,
            )
            .await;

        assert!(!response.success, "{response:?}");
        let error = response.error.as_deref().expect("bounded switch failure");
        assert_eq!(
            error,
            "session could not be opened right now; verify the selection and retry"
        );
        assert!(!error.contains(alias.to_string_lossy().as_ref()));
        assert_eq!(spawner.spawns.load(Ordering::SeqCst), 0);
        assert_eq!(
            std::fs::read(&source).expect("Pi source after"),
            source_bytes
        );
        let native_root = catalog
            .root_for(pi_coding::SessionSourceKind::NativePi)
            .path;
        assert!(match native_root.read_dir() {
            Ok(mut entries) => entries.next().is_none(),
            Err(_) => true,
        });
        manager.shutdown().await;
    }

    #[test]
    fn web_catalog_switch_never_opens_omp_store_in_place() {
        let (_home, _native, catalog) = web_catalog_fixture();
        let cwd = tempfile::tempdir().expect("source cwd");
        let source = write_omp_session(&catalog, "omp-web", cwd.path());
        let imported = resolve_web_catalog_session_path(
            &catalog,
            cwd.path(),
            &source.to_string_lossy(),
            &[
                pi_coding::SessionSourceKind::NativePi,
                pi_coding::SessionSourceKind::Omp,
            ],
        )
        .expect("OMP import");

        assert_ne!(imported, source);
        let native_root = catalog
            .root_for(pi_coding::SessionSourceKind::NativePi)
            .path;
        assert_eq!(imported.parent(), Some(native_root.as_path()));
    }

    #[test]
    fn web_catalog_switch_rejects_disabled_foreign_sources() {
        let (_home, _native, catalog) = web_catalog_fixture();
        let cwd = tempfile::tempdir().expect("source cwd");
        let source = write_codex_session(&catalog, "codex-disabled", cwd.path());
        let error = resolve_web_catalog_session_path(
            &catalog,
            cwd.path(),
            &source.to_string_lossy(),
            &[pi_coding::SessionSourceKind::NativePi],
        )
        .expect_err("disabled Codex source");

        assert!(
            format!("{error:#}").contains("session not found"),
            "{error:#}"
        );
        let native_root = catalog
            .root_for(pi_coding::SessionSourceKind::NativePi)
            .path;
        assert!(
            native_root
                .read_dir()
                .expect("native root")
                .next()
                .is_none()
        );
    }
    #[tokio::test]
    async fn web_switch_handoff_defers_authoritative_snapshot_and_preserves_legacy_response() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let dir = tempfile::tempdir().expect("session dir");
        let (model, _registration) = faux_model("handoff-target");
        let recorder = pi_coding::start_session_in(
            dir.path(),
            Some(&model),
            Some("off"),
            Some(dir.path()),
            None,
            None,
        )
        .expect("start target session");
        recorder
            .record_message(&Message::user_text("authoritative transcript", 0))
            .expect("record target message");
        recorder.persist_now().expect("persist target");
        let path = recorder.path().to_string_lossy().into_owned();
        let source_id = manager.primary_session_id();

        let web = manager
            .admit_web(
                RpcCommand::SwitchSession {
                    id: Some("web-switch".into()),
                    session_path: path.clone(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: Some(crate::modes::rpc::SubscribeHandoffV1::SubscribeHandoffV1),
                },
                Some(source_id.clone()),
                false,
            )
            .await
            .wait()
            .await;
        assert!(web.success, "{web:?}");
        assert_eq!(web.session_id.as_deref(), Some(source_id.as_str()));
        let handoff = web.data.expect("Web handoff");
        let target_id = handoff["sessionId"].as_str().expect("target id").to_owned();
        assert_eq!(handoff["subscriptionRequired"], true);
        assert_eq!(handoff["lifecycleResponseVersion"], 1);
        let incarnation = handoff["runtimeIncarnation"]
            .as_str()
            .expect("handoff incarnation")
            .to_owned();
        assert!(!incarnation.is_empty());
        assert!(handoff["cursor"].is_u64());
        assert!(handoff.get("state").is_none());
        assert!(handoff.get("messages").is_none());
        assert_eq!(manager.protocol_snapshot_count_for_test(), 0);
        let raced = manager
            .publish_test_event(
                &target_id,
                ApplicationEvent::TurnStarted {
                    turn_id: "between-switch-and-subscribe".to_owned(),
                    model: None,
                },
            )
            .await;

        manager.release_subscription_snapshots_for_test(1);
        let snapshot = manager
            .subscription_snapshot_for_identity(
                RuntimeIdentity::new(target_id.clone(), incarnation.clone()),
                handoff["cursor"].as_u64().expect("handoff cursor"),
                CancellationToken::new(),
                true,
            )
            .await
            .expect("authoritative subscription snapshot");
        assert_eq!(snapshot["sessionId"], target_id);
        assert_eq!(snapshot["runtimeIncarnation"], incarnation);
        assert_eq!(snapshot["reason"], "cold-full");
        assert!(snapshot["messages"].is_array());
        assert!(
            snapshot["events"]
                .as_array()
                .expect("snapshot replay window")
                .iter()
                .any(|event| event["sequence"] == raced["sequence"]),
            "event published after the switch response must be included by the subscription cut",
        );
        assert_eq!(manager.protocol_snapshot_count_for_test(), 1);

        let legacy = manager
            .dispatch(
                RpcCommand::SwitchSession {
                    id: Some("legacy-switch".into()),
                    session_path: path,
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                Some(source_id.clone()),
            )
            .await;
        assert!(legacy.success, "{legacy:?}");
        assert_eq!(legacy.session_id.as_deref(), Some(target_id.as_str()));
        let legacy = legacy.data.expect("legacy snapshot");
        assert_eq!(legacy["sessionId"], target_id);
        assert!(legacy["state"].is_object());
        assert_eq!(legacy["runtimeIncarnation"], incarnation);
        assert!(legacy["messages"].is_array());
        assert_eq!(manager.protocol_snapshot_count_for_test(), 2);
        manager.shutdown().await;
    }

    fn persist_recorded_branch(
        label: &str,
        user_text: &str,
        assistant_text: &str,
    ) -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("session dir");
        let (model, _registration) = faux_model(label);
        let recorder = pi_coding::start_session_in(
            dir.path(),
            Some(&model),
            Some("off"),
            Some(dir.path()),
            None,
            None,
        )
        .expect("start session");
        recorder
            .record_message(&Message::user_text(user_text, 0))
            .expect("record user");
        let mut assistant = pi_ai::AssistantMessage::pending(&pi_ai::Model::default());
        assistant.content = vec![pi_ai::ContentBlock::text(assistant_text)];
        assistant.stop_reason = pi_ai::StopReason::Stop;
        assistant.timestamp = 1;
        recorder
            .record_message(&Message::Assistant(assistant))
            .expect("record assistant");
        recorder.persist_now().expect("persist session");
        let path = recorder.path().to_path_buf();
        (dir, path)
    }

    fn persist_recorded_messages(label: &str, count: usize, body: &str) -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("session dir");
        let (model, _registration) = faux_model(label);
        let recorder = pi_coding::start_session_in(
            dir.path(),
            Some(&model),
            Some("off"),
            Some(dir.path()),
            None,
            None,
        )
        .expect("start session");
        for index in 0..count {
            recorder
                .record_message(&Message::user_text(
                    format!("{label}-{index}:{body}"),
                    index as i64,
                ))
                .expect("record user");
        }
        recorder.persist_now().expect("persist session");
        let path = recorder.path().to_path_buf();
        (dir, path)
    }

    fn negotiated_switch(path: String, id: &str) -> RpcCommand {
        RpcCommand::SwitchSession {
            id: Some(id.into()),
            session_path: path,
            catalog_source: None,
            catalog_session_id: None,
            catalog_status: None,
            response_mode: Some(crate::modes::rpc::SubscribeHandoffV1::SubscribeHandoffV1),
        }
    }

    #[tokio::test]
    async fn switch_rejects_registered_dead_target_owner() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let (_dir, path) = persist_recorded_branch(
            "dead-owner-target",
            "keep this user",
            "keep this assistant",
        );
        let path_text = path.to_string_lossy().into_owned();
        let source_id = manager.primary_session_id();
        let opened = manager
            .admit_web(
                negotiated_switch(path_text.clone(), "open-live"),
                Some(source_id.clone()),
                false,
            )
            .await
            .wait()
            .await;
        assert!(opened.success, "{opened:?}");
        let target_id = opened.data.expect("handoff")["sessionId"]
            .as_str()
            .expect("target id")
            .to_owned();
        manager
            .stop_owner_keep_registered_for_test(&target_id)
            .await
            .expect("stop target owner");
        assert!(
            manager.is_registered_session_for_test(&target_id).await,
            "dead owner must stay registered"
        );
        let journal_before = std::fs::read(&path).expect("journal before refused switch");

        let refused = manager
            .admit_web(
                negotiated_switch(path_text.clone(), "reuse-dead"),
                Some(source_id.clone()),
                false,
            )
            .await
            .wait()
            .await;
        assert!(!refused.success, "{refused:?}");
        assert_eq!(refused.session_id.as_deref(), Some(source_id.as_str()));
        let data = refused.data.expect("typed runtime_unavailable");
        assert_eq!(data["code"], "runtime_unavailable");
        assert_eq!(data["cause"], "owner_stopped");
        assert_eq!(data["retryable"], false);

        let refused_again = manager
            .admit_web(
                negotiated_switch(path_text, "reuse-dead-again"),
                Some(source_id.clone()),
                false,
            )
            .await
            .wait()
            .await;
        assert!(!refused_again.success, "{refused_again:?}");
        let again = refused_again.data.expect("second typed refusal");
        assert_eq!(again["code"], "runtime_unavailable");
        assert_eq!(again["cause"], "owner_stopped");
        assert_eq!(again["retryable"], false);

        let source_messages = manager
            .dispatch(RpcCommand::GetMessages { id: None }, Some(source_id))
            .await;
        assert!(source_messages.success, "{source_messages:?}");
        assert!(
            manager.is_registered_session_for_test(&target_id).await,
            "registration must survive refused reuse"
        );
        let journal_after = std::fs::read(&path).expect("journal after refused switch");
        assert_eq!(journal_before, journal_after);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn switch_rejects_registered_aborted_owner_by_terminal_state() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let (_dir, path) = persist_recorded_branch(
            "aborted-owner-target",
            "keep this user",
            "keep this assistant",
        );
        let path_text = path.to_string_lossy().into_owned();
        let source_id = manager.primary_session_id();
        let opened = manager
            .admit_web(
                negotiated_switch(path_text.clone(), "open-live"),
                Some(source_id.clone()),
                false,
            )
            .await
            .wait()
            .await;
        assert!(opened.success, "{opened:?}");
        let target_id = opened.data.expect("handoff")["sessionId"]
            .as_str()
            .expect("target id")
            .to_owned();
        manager
            .abort_owner_unmarked_keep_registered_for_test(&target_id)
            .await
            .expect("abort target owner");
        assert!(
            manager.owner_stopped_for_test(&target_id).await.expect("owner terminal"),
            "health must observe published owner terminal state"
        );
        assert!(
            manager.is_registered_session_for_test(&target_id).await,
            "aborted owner must stay registered"
        );
        assert!(
            manager
                .lifecycle_uncontended_for_test(&target_id)
                .await
                .expect("lifecycle lock"),
            "membership plus a free lifecycle lock must not be treated as healthy"
        );

        let refused = manager
            .admit_web(
                negotiated_switch(path_text, "reuse-aborted"),
                Some(source_id.clone()),
                false,
            )
            .await
            .wait()
            .await;
        assert!(!refused.success, "{refused:?}");
        assert_eq!(refused.session_id.as_deref(), Some(source_id.as_str()));
        let data = refused.data.expect("typed runtime_unavailable");
        assert_eq!(data["code"], "runtime_unavailable");
        assert_eq!(data["cause"], "owner_stopped");
        assert_eq!(data["retryable"], false);
        assert!(
            manager.is_registered_session_for_test(&target_id).await,
            "refusal must not depend on unregister"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn aborted_primary_owner_publishes_terminal_state() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let primary_id = manager.primary_session_id();
        manager
            .abort_owner_unmarked_keep_registered_for_test(&primary_id)
            .await
            .expect("abort primary owner");
        assert!(
            manager
                .owner_stopped_for_test(&primary_id)
                .await
                .expect("owner terminal"),
            "primary abort must publish owner terminal state"
        );
        assert!(
            manager.is_registered_session_for_test(&primary_id).await,
            "aborted primary must stay registered"
        );
        let snapshot_error = manager
            .subscription_snapshot_for_identity(
                RuntimeIdentity::new(primary_id.clone(), String::new()),
                0,
                CancellationToken::new(),
                false,
            )
            .await
            .expect_err("aborted primary must not snapshot");
        let unavailable = snapshot_error
            .downcast_ref::<RuntimeUnavailable>()
            .expect("typed runtime unavailable");
        assert_eq!(unavailable.cause, RuntimeUnavailableCause::OwnerStopped);
        assert!(
            manager.is_registered_session_for_test(&primary_id).await,
            "refusal must not depend on unregister"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn warm_subscription_cut_attests_messages_array() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let (_dir, path) = persist_recorded_branch(
            "warm-cut-target",
            "keep this user",
            "keep this assistant",
        );
        let path_text = path.to_string_lossy().into_owned();
        let source_id = manager.primary_session_id();
        let opened = manager
            .admit_web(
                negotiated_switch(path_text, "open-for-warm"),
                Some(source_id.clone()),
                false,
            )
            .await
            .wait()
            .await;
        assert!(opened.success, "{opened:?}");
        let handoff = opened.data.expect("handoff");
        let target_id = handoff["sessionId"]
            .as_str()
            .expect("target id")
            .to_owned();
        let incarnation = handoff["runtimeIncarnation"]
            .as_str()
            .expect("incarnation")
            .to_owned();
        manager
            .publish_test_event(
                &target_id,
                ApplicationEvent::TurnStarted {
                    turn_id: "warm-cut-turn".to_owned(),
                    model: None,
                },
            )
            .await;
        manager.release_subscription_snapshots_for_test(2);
        let cold = manager
            .subscription_snapshot_for_identity(
                RuntimeIdentity::new(target_id.clone(), incarnation.clone()),
                0,
                CancellationToken::new(),
                true,
            )
            .await
            .expect("cold cut");
        assert_eq!(cold["reason"], "cold-full");
        let watermark = cold["watermark"].as_u64().expect("cold watermark");
        assert!(watermark > 0, "published event must advance watermark");
        let warm = manager
            .subscription_snapshot_for_identity(
                RuntimeIdentity::new(target_id, incarnation),
                watermark,
                CancellationToken::new(),
                true,
            )
            .await
            .expect("warm cut");
        assert_eq!(warm["reason"], "warm-continuity");
        assert!(
            warm["messages"].is_array(),
            "warm cuts must attest a messages array after retained-writer projection: {warm}"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn persisted_transcript_load_failure_never_succeeds_empty() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let (_dir, path) = persist_recorded_branch(
            "load-fail-target",
            "durable user line",
            "durable assistant line",
        );
        let path_text = path.to_string_lossy().into_owned();
        let source_id = manager.primary_session_id();
        let opened = manager
            .admit_web(
                negotiated_switch(path_text, "open-for-load"),
                Some(source_id.clone()),
                false,
            )
            .await
            .wait()
            .await;
        assert!(opened.success, "{opened:?}");
        let handoff = opened.data.expect("handoff");
        let target_id = handoff["sessionId"]
            .as_str()
            .expect("target id")
            .to_owned();
        let incarnation = handoff["runtimeIncarnation"]
            .as_str()
            .expect("incarnation")
            .to_owned();
        let original = std::fs::read(&path).expect("original journal");
        manager
            .publish_test_event(
                &target_id,
                ApplicationEvent::TurnStarted {
                    turn_id: "corrupt-warm-turn".to_owned(),
                    model: None,
                },
            )
            .await;
        manager.release_subscription_snapshots_for_test(4);
        let cold = manager
            .subscription_snapshot_for_identity(
                RuntimeIdentity::new(target_id.clone(), incarnation.clone()),
                0,
                CancellationToken::new(),
                true,
            )
            .await
            .expect("cold cut before corruption");
        let watermark = cold["watermark"].as_u64().expect("cold watermark");
        assert!(watermark > 0, "published event must advance watermark");
        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&path)
                .expect("truncate live journal");
            file.write_all(b"{not-json\n")
                .expect("corrupt live journal");
            file.sync_all().expect("sync corrupt journal");
        }

        let snapshot_error = manager
            .subscription_snapshot_for_identity(
                RuntimeIdentity::new(target_id.clone(), incarnation.clone()),
                0,
                CancellationToken::new(),
                true,
            )
            .await
            .expect_err("load failure must not snapshot");
        assert!(
            snapshot_error
                .downcast_ref::<pi_coding::TranscriptUnavailable>()
                .is_some(),
            "typed TranscriptUnavailable: {snapshot_error:#}"
        );
        let dyn_error: &(dyn std::error::Error + 'static) = &*snapshot_error;
        assert!(
            dyn_error.source().is_some(),
            "TranscriptUnavailable must retain the tree-load source: {snapshot_error:#}"
        );

        let warm_error = manager
            .subscription_snapshot_for_identity(
                RuntimeIdentity::new(target_id.clone(), incarnation.clone()),
                watermark,
                CancellationToken::new(),
                true,
            )
            .await
            .expect_err("warm cut must not succeed on a corrupt retained journal");
        assert!(
            warm_error
                .downcast_ref::<pi_coding::TranscriptUnavailable>()
                .is_some(),
            "{warm_error:#}"
        );

        let messages = manager
            .dispatch(RpcCommand::GetMessages { id: None }, Some(target_id.clone()))
            .await;
        assert!(!messages.success, "{messages:?}");
        assert_eq!(
            messages.data.as_ref().and_then(|data| data.get("code")),
            Some(&json!("transcript_unavailable"))
        );
        assert!(
            messages
                .data
                .as_ref()
                .and_then(|data| data.get("messages"))
                .is_none()
        );

        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&path)
                .expect("restore live journal");
            file.write_all(&original).expect("rewrite original journal");
            file.sync_all().expect("sync restored journal");
        }

        let restored = manager
            .dispatch(RpcCommand::GetMessages { id: None }, Some(target_id.clone()))
            .await;
        assert!(restored.success, "{restored:?}");
        let restored_texts = history_texts(&restored);
        assert!(
            restored_texts.iter().any(|text| text.contains("durable user line")),
            "{restored_texts:?}"
        );
        let restored_ids: Vec<_> = restored
            .data
            .as_ref()
            .and_then(|data| data.get("messages"))
            .and_then(Value::as_array)
            .expect("restored messages")
            .iter()
            .filter_map(|row| row.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect();
        assert!(!restored_ids.is_empty(), "{restored:?}");

        let snapshot = manager
            .subscription_snapshot_for_identity(
                RuntimeIdentity::new(target_id, incarnation),
                0,
                CancellationToken::new(),
                true,
            )
            .await
            .expect("restored snapshot");
        let snapshot_ids: Vec<_> = snapshot["messages"]
            .as_array()
            .expect("snapshot messages")
            .iter()
            .filter_map(|row| row.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect();
        assert_eq!(snapshot_ids, restored_ids);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn valid_empty_and_active_branch_negative_controls() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;

        let opened = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(opened.success, "{opened:?}");
        let empty_id = opened.data.expect("new session")["sessionId"]
            .as_str()
            .expect("new session id")
            .to_owned();
        let empty_messages = manager
            .dispatch(RpcCommand::GetMessages { id: None }, Some(empty_id))
            .await;
        assert!(empty_messages.success, "{empty_messages:?}");
        assert_eq!(
            empty_messages.data.unwrap()["messages"]
                .as_array()
                .expect("empty messages")
                .len(),
            0
        );

        let dir = tempfile::tempdir().expect("branched session dir");
        let (model, _registration) = faux_model("empty-active-branch");
        let recorder = pi_coding::start_session_in(
            dir.path(),
            Some(&model),
            Some("off"),
            Some(dir.path()),
            None,
            None,
        )
        .expect("start branched session");
        recorder
            .record_message(&Message::user_text("archived other branch", 0))
            .expect("record archived user");
        let mut assistant = pi_ai::AssistantMessage::pending(&pi_ai::Model::default());
        assistant.content = vec![pi_ai::ContentBlock::text("archived assistant")];
        assistant.stop_reason = pi_ai::StopReason::Stop;
        assistant.timestamp = 1;
        recorder
            .record_message(&Message::Assistant(assistant))
            .expect("record archived assistant");
        recorder
            .branch_with_summary(None, "empty current branch")
            .expect("empty active leaf");
        recorder.persist_now().expect("persist branched session");
        let branched_path = recorder.path().to_string_lossy().into_owned();
        let disk = pi_coding::load_session_tree(Path::new(&branched_path)).expect("disk tree");
        assert!(
            disk.entries.len() > 1,
            "catalog/other-branch records must exist: {}",
            disk.entries.len()
        );

        let source_id = manager.primary_session_id();
        let branched = manager
            .admit_web(
                negotiated_switch(branched_path, "open-empty-active"),
                Some(source_id.clone()),
                false,
            )
            .await
            .wait()
            .await;
        assert!(branched.success, "{branched:?}");
        let branched_id = branched.data.expect("handoff")["sessionId"]
            .as_str()
            .expect("branched id")
            .to_owned();
        let branched_messages = manager
            .dispatch(
                RpcCommand::GetMessages { id: None },
                Some(branched_id),
            )
            .await;
        assert!(branched_messages.success, "{branched_messages:?}");
        assert_eq!(
            branched_messages
                .data
                .as_ref()
                .and_then(|data| data.get("messages"))
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(0),
            "{branched_messages:?}"
        );

        let (_populated_dir, populated_path) = persist_recorded_branch(
            "populated-active-branch",
            "visible user",
            "visible assistant",
        );
        let populated = manager
            .admit_web(
                negotiated_switch(
                    populated_path.to_string_lossy().into_owned(),
                    "open-populated",
                ),
                Some(source_id),
                false,
            )
            .await
            .wait()
            .await;
        assert!(populated.success, "{populated:?}");
        let populated_id = populated.data.expect("handoff")["sessionId"]
            .as_str()
            .expect("populated id")
            .to_owned();
        let populated_messages = manager
            .dispatch(
                RpcCommand::GetMessages { id: None },
                Some(populated_id),
            )
            .await;
        assert!(populated_messages.success, "{populated_messages:?}");
        let texts = history_texts(&populated_messages);
        assert!(
            texts.iter().any(|text| text.contains("visible user")),
            "{texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|text| text.contains("visible assistant")),
            "{texts:?}"
        );
        let ids: Vec<_> = populated_messages
            .data
            .as_ref()
            .and_then(|data| data.get("messages"))
            .and_then(Value::as_array)
            .expect("populated messages")
            .iter()
            .filter_map(|row| row.get("id").and_then(Value::as_str))
            .collect();
        assert_eq!(ids.len(), 2, "{populated_messages:?}");
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn admit_web_get_messages_includes_handwritten_orchestration_custom() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let dir = tempfile::tempdir().expect("parity session dir");
        let path = dir.path().join("web-transcript-parity.jsonl");
        let stamp = "2026-01-01T00:00:00.000Z";
        let records = [
            json!({
                "type": "session",
                "version": 3,
                "id": "web-transcript-parity",
                "timestamp": stamp,
                "cwd": dir.path(),
            }),
            json!({
                "type": "session_info",
                "id": "si-1",
                "parentId": serde_json::Value::Null,
                "timestamp": stamp,
                "name": "web-transcript-parity",
            }),
            json!({
                "type": "message",
                "id": "u1",
                "parentId": serde_json::Value::Null,
                "timestamp": stamp,
                "message": Message::user_text("transcript parity seed", 1),
            }),
            json!({
                "type": "custom_message",
                "id": "hidden",
                "parentId": "u1",
                "timestamp": stamp,
                "customType": "pi.goal.active",
                "content": "<system-reminder>internal parity secret</system-reminder>",
                "display": false,
                "details": {},
            }),
            json!({
                "type": "custom_message",
                "id": "irc",
                "parentId": "hidden",
                "timestamp": stamp,
                "customType": "orchestration_message",
                "content": "<orchestration-message id=\"m1\" from=\"Main\">\nclean IRC parity body\nReplying to message: parent-9\n</orchestration-message>",
                "display": true,
                "details": {
                    "id": "m1",
                    "from": "Main",
                    "to": "Child",
                    "body": "clean IRC parity body",
                    "replyTo": "parent-9",
                },
            }),
            json!({
                "type": "message",
                "id": "bash",
                "parentId": "irc",
                "timestamp": stamp,
                "message": {
                    "role": "bashExecution",
                    "command": "seq 1 30",
                    "output": "bash-parity-30",
                    "cancelled": false,
                    "truncated": false,
                    "timestamp": 2
                },
            }),
        ];
        std::fs::write(
            &path,
            records
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .expect("write handwritten parity jsonl");

        let source_id = manager.primary_session_id();
        let opened = manager
            .admit_web(
                negotiated_switch(
                    path.to_string_lossy().into_owned(),
                    "open-handwritten-irc",
                ),
                Some(source_id),
                false,
            )
            .await
            .wait()
            .await;
        assert!(opened.success, "{opened:?}");
        let target_id = opened.data.expect("handoff")["sessionId"]
            .as_str()
            .expect("session id")
            .to_owned();
        let messages = manager
            .dispatch(RpcCommand::GetMessages { id: None }, Some(target_id))
            .await;
        assert!(messages.success, "{messages:?}");
        let rows = messages
            .data
            .as_ref()
            .and_then(|data| data.get("messages"))
            .and_then(Value::as_array)
            .expect("get_messages rows");
        let irc = rows.iter().find(|row| {
            row.get("message")
                .and_then(|message| message.get("role"))
                .and_then(Value::as_str)
                == Some("custom")
                && row
                    .get("message")
                    .and_then(|message| message.get("display"))
                    .and_then(Value::as_bool)
                    == Some(true)
        });
        let irc = irc.unwrap_or_else(|| panic!("display:true custom missing from {rows:?}"));
        assert_eq!(
            irc["message"]["customType"].as_str(),
            Some("orchestration_message")
        );
        assert_eq!(
            irc["message"]["details"]["body"].as_str(),
            Some("clean IRC parity body")
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn get_state_surfaces_loader_drop_counts() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let dir = tempfile::tempdir().expect("drop session dir");
        let path = dir.path().join("loader-drops.jsonl");
        let header = serde_json::json!({
            "type": "session",
            "version": 3,
            "id": "01loaderdropstate000000000000",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "cwd": dir.path().to_string_lossy(),
        });
        let lines = [
            header.to_string(),
            serde_json::json!({
                "type": "message",
                "id": "",
                "parentId": serde_json::Value::Null,
                "timestamp": "2026-01-01T00:00:01.000Z",
                "message": Message::user_text("idless skipped", 0),
            })
            .to_string(),
            serde_json::json!({
                "type": "message",
                "id": "visible-user",
                "parentId": serde_json::Value::Null,
                "timestamp": "2026-01-01T00:00:02.000Z",
                "message": Message::user_text("kept user", 1),
            })
            .to_string(),
            serde_json::json!({
                "type": "message",
                "id": "bad-payload",
                "parentId": "visible-user",
                "timestamp": "2026-01-01T00:00:03.000Z",
                "message": "not-a-message-object",
            })
            .to_string(),
            serde_json::json!({
                "type": "unknown_shape",
                "id": "force-lenient",
                "parentId": "bad-payload",
                "timestamp": "2026-01-01T00:00:04.000Z",
            })
            .to_string(),
        ];
        std::fs::write(&path, lines.join("\n")).expect("write drop session");
        let source_id = manager.primary_session_id();
        let opened = manager
            .admit_web(
                negotiated_switch(path.to_string_lossy().into_owned(), "open-loader-drops"),
                Some(source_id),
                false,
            )
            .await
            .wait()
            .await;
        assert!(opened.success, "{opened:?}");
        let target_id = opened.data.expect("handoff")["sessionId"]
            .as_str()
            .expect("target id")
            .to_owned();
        let state = manager
            .dispatch(RpcCommand::GetState { id: None }, Some(target_id))
            .await;
        assert!(state.success, "{state:?}");
        let data = state.data.expect("get_state data");
        assert_eq!(
            data.get("skippedIdless").and_then(Value::as_u64),
            Some(1),
            "{data}"
        );
        let undecodable = data
            .get("undecodableMessages")
            .and_then(Value::as_u64)
            .expect("undecodableMessages");
        assert!(undecodable >= 1, "{data}");
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn loaded_runtime_empty_history_missing_leaf_never_succeeds_empty() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let (_dir, path) = persist_recorded_branch(
            "loaded-empty-memory",
            "durable user line",
            "durable assistant line",
        );
        let source_id = manager.primary_session_id();
        let opened = manager
            .admit_web(
                negotiated_switch(
                    path.to_string_lossy().into_owned(),
                    "open-loaded-empty-memory",
                ),
                Some(source_id),
                false,
            )
            .await
            .wait()
            .await;
        assert!(opened.success, "{opened:?}");
        let target_id = opened.data.expect("handoff")["sessionId"]
            .as_str()
            .expect("target id")
            .to_owned();

        manager
            .inject_empty_in_memory_and_cleared_active_leaf_for_test(&target_id)
            .await
            .expect("inject empty in-memory history and missing active leaf");

        let state = manager
            .dispatch(RpcCommand::GetState { id: None }, Some(target_id.clone()))
            .await;
        assert!(state.success, "{state:?}");

        let messages = manager
            .dispatch(RpcCommand::GetMessages { id: None }, Some(target_id))
            .await;
        if messages.success {
            let count = messages
                .data
                .as_ref()
                .and_then(|data| data.get("messages"))
                .and_then(Value::as_array)
                .map(Vec::len);
            assert_ne!(
                count,
                Some(0),
                "catalog/runtime nonempty must not succeed with empty messages: {messages:?}"
            );
            let texts = history_texts(&messages);
            assert!(
                texts.iter().any(|text| text.contains("durable user line")),
                "{texts:?}"
            );
        } else {
            assert_eq!(
                messages.data.as_ref().and_then(|data| data.get("code")),
                Some(&json!("transcript_unavailable")),
                "{messages:?}"
            );
            assert!(
                messages
                    .data
                    .as_ref()
                    .and_then(|data| data.get("messages"))
                    .is_none()
            );
        }

        manager.shutdown().await;
    }

    #[tokio::test]
    async fn switch_to_long_session_projects_full_transcript() {
        const COUNT: usize = 301;
        let bulk = "x".repeat(90_000);
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let (_dir, path) = persist_recorded_messages("long-audit", COUNT, &bulk);
        let path_text = path.to_string_lossy().into_owned();
        let source_id = manager.primary_session_id();
        let opened = manager
            .admit_web(
                negotiated_switch(path_text, "open-long-audit"),
                Some(source_id),
                false,
            )
            .await
            .wait()
            .await;
        assert!(opened.success, "{opened:?}");
        let handoff = opened.data.expect("handoff");
        let target_id = handoff["sessionId"]
            .as_str()
            .expect("target id")
            .to_owned();
        let incarnation = handoff["runtimeIncarnation"]
            .as_str()
            .expect("incarnation")
            .to_owned();

        let messages = manager
            .dispatch(RpcCommand::GetMessages { id: None }, Some(target_id.clone()))
            .await;
        assert!(messages.success, "{messages:?}");
        let rows = messages
            .data
            .as_ref()
            .and_then(|data| data.get("messages"))
            .and_then(Value::as_array)
            .expect("get_messages messages");
        assert_eq!(rows.len(), COUNT, "get_messages must return every durable row");

        let snapshot = manager
            .subscription_snapshot_for_identity(
                RuntimeIdentity::new(target_id, incarnation),
                0,
                CancellationToken::new(),
                true,
            )
            .await
            .expect("long-session ColdFull snapshot");
        assert_eq!(snapshot["reason"], "cold-full");
        let snapshot_rows = snapshot["messages"]
            .as_array()
            .expect("snapshot messages");
        assert_eq!(
            snapshot_rows.len(),
            COUNT,
            "ColdFull snapshot must not drop or empty a 300+ message transcript"
        );
        let encoded = serde_json::to_string(&snapshot).expect("encode snapshot");
        assert!(
            encoded.len() > 24 * 1024 * 1024,
            "fixture must exceed the old 24 MiB snapshot cap: {}",
            encoded.len()
        );
        assert!(encoded.len() <= COMPLETED_SNAPSHOT_BYTE_BUDGET);
        assert_eq!(COMPLETED_SNAPSHOT_BYTE_BUDGET, MAX_WS_OUTBOUND_MESSAGE_BYTES);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn failed_negotiated_switch_keeps_source_identity_and_error() {
        let (manager, _primary_registration) =
            manager_with(Some(Arc::new(TestSpawner::default()))).await;
        let source_id = manager.primary_session_id();
        let response = manager
            .admit_web(
                RpcCommand::SwitchSession {
                    id: Some("missing-negotiated".into()),
                    session_path: "missing-session.jsonl".into(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: Some(crate::modes::rpc::SubscribeHandoffV1::SubscribeHandoffV1),
                },
                Some(source_id.clone()),
                false,
            )
            .await
            .wait()
            .await;
        assert!(!response.success);
        assert_eq!(response.session_id.as_deref(), Some(source_id.as_str()));
        assert_eq!(
            response.error.as_deref(),
            Some("session could not be opened right now; verify the selection and retry")
        );
        assert_eq!(manager.protocol_snapshot_count_for_test(), 0);
        manager.shutdown().await;
    }

    #[test]
    fn snapshot_flight_share_rejects_an_earlier_cursor() {
        assert!(snapshot_flight_can_share(0, false, 0, true));
        assert!(snapshot_flight_can_share(0, true, 12, true));
        assert!(snapshot_flight_can_share(4, true, 4, true));
        assert!(snapshot_flight_can_share(4, true, 8, false));
        assert!(!snapshot_flight_can_share(4, true, 0, true));
        assert!(!snapshot_flight_can_share(4, false, 4, true));
    }

    #[tokio::test]
    async fn concurrent_subscription_snapshots_share_one_build() {
        let (manager, _primary_registration) = manager_with(None).await;
        let delivery = manager.hold_subscription_snapshot_delivery_for_test().await;
        let session_id = manager.primary_session_id();
        let identity = manager
            .current_runtime_identity(&session_id)
            .await
            .expect("primary identity");
        let leader = manager.clone();
        let leader_identity = identity.clone();
        let first = tokio::spawn(async move {
            leader
                .subscription_snapshot_for_identity(
                    leader_identity,
                    0,
                    CancellationToken::new(),
                    true,
                )
                .await
        });
        manager
            .wait_for_subscription_snapshot_capture_for_test()
            .await;
        let builds_after_leader = manager.protocol_snapshot_count_for_test();
        assert!(
            builds_after_leader >= 1,
            "leader must start a protocol snapshot"
        );
        let joiner = manager.clone();
        let joiner_identity = identity.clone();
        let second = tokio::spawn(async move {
            joiner
                .subscription_snapshot_for_identity(
                    joiner_identity,
                    0,
                    CancellationToken::new(),
                    true,
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            manager.protocol_snapshot_count_for_test(),
            builds_after_leader
        );
        drop(delivery);
        let first_snapshot = tokio::time::timeout(Duration::from_secs(5), first)
            .await
            .expect("leader snapshot timeout")
            .expect("leader join")
            .expect("leader snapshot");
        let second_snapshot = tokio::time::timeout(Duration::from_secs(5), second)
            .await
            .expect("joiner snapshot timeout")
            .expect("joiner join")
            .expect("joiner snapshot");
        assert_eq!(first_snapshot["sessionId"], second_snapshot["sessionId"]);
        assert_eq!(
            first_snapshot["runtimeIncarnation"],
            second_snapshot["runtimeIncarnation"]
        );
        assert_eq!(first_snapshot["watermark"], second_snapshot["watermark"]);
        assert_eq!(first_snapshot["reason"], second_snapshot["reason"]);
        assert_eq!(
            manager.protocol_snapshot_count_for_test(),
            builds_after_leader
        );
        manager.shutdown().await;
    }

    /// Foreign session fixture for every Web-importable source kind. Returns
    /// (path, catalog session id).
    fn write_foreign_web_session(
        catalog: &pi_coding::SessionCatalog,
        kind: pi_coding::SessionSourceKind,
        id: &str,
        cwd: &Path,
    ) -> (PathBuf, &'static str) {
        match kind {
            pi_coding::SessionSourceKind::Pi => {
                let path = catalog
                    .root_for(pi_coding::SessionSourceKind::Pi)
                    .path
                    .join("project")
                    .join(format!("{id}.jsonl"));
                std::fs::create_dir_all(path.parent().expect("Pi parent"))
                    .expect("Pi root");
                let cwd = serde_json::to_string(&cwd.to_string_lossy()).expect("cwd json");
                std::fs::write(
                    &path,
                    format!(
                        "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":{cwd}}}\n{{\"type\":\"message\",\"id\":\"u\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01Z\",\"message\":{{\"role\":\"user\",\"content\":\"pi negotiated prompt\"}}}}\n",
                    ),
                )
                .expect("write Pi session");
                (path, "pi")
            }
            pi_coding::SessionSourceKind::Omp => {
                (write_omp_session(catalog, id, cwd), "omp")
            }
            pi_coding::SessionSourceKind::Codex => {
                (write_codex_session(catalog, id, cwd), "codex")
            }
            pi_coding::SessionSourceKind::Grok => {
                let directory = catalog
                    .root_for(pi_coding::SessionSourceKind::Grok)
                    .path
                    .join("injected-cwd")
                    .join(id);
                std::fs::create_dir_all(&directory).expect("grok directory");
                let cwd = cwd.to_string_lossy();
                let path = directory.join("summary.json");
                std::fs::write(
                    &path,
                    format!(r#"{{"info":{{"id":"{id}","cwd":"{cwd}"}},"created_at":"2026-01-01T00:00:00Z"}}"#),
                )
                .expect("grok summary");
                std::fs::write(
                    directory.join("chat_history.jsonl"),
                    r#"{"role":"user","content":"grok negotiated prompt","timestamp":"2026-01-01T00:00:01Z"}
"#,
                )
                .expect("grok chat");
                (path, "grok")
            }
            other => panic!("unhandled foreign source {other:?}"),
        }
    }

    #[tokio::test]
    async fn negotiated_foreign_switch_binds_catalog_identity_to_imported_runtime() {
        for kind in [
            pi_coding::SessionSourceKind::Pi,
            pi_coding::SessionSourceKind::Omp,
            pi_coding::SessionSourceKind::Codex,
            pi_coding::SessionSourceKind::Grok,
        ] {
            let (_home, _native, catalog) = web_catalog_fixture();
            let cwd = tempfile::tempdir().expect("source cwd");
            let (source_path, wire_source) =
                write_foreign_web_session(&catalog, kind, "negotiated-fk", cwd.path());
            let source_bytes = std::fs::read(&source_path).expect("source bytes");
            let source_modified = std::fs::metadata(&source_path)
                .expect("source metadata")
                .modified()
                .expect("source mtime");
            let native_root = catalog
                .root_for(pi_coding::SessionSourceKind::NativePi)
                .path
                .clone();
            let imported_files = |dir: &std::path::Path| {
                std::fs::read_dir(dir)
                    .map(|entries| {
                        entries
                            .filter_map(|entry| entry.ok())
                            .filter(|entry| {
                                entry.file_type().map(|t| t.is_file()).unwrap_or(false)
                                    && entry.file_name().to_string_lossy().starts_with("import_")
                                    && entry.file_name().to_string_lossy().ends_with(".jsonl")
                            })
                            .count()
                    })
                    .unwrap_or(0)
            };
            let imports_before = imported_files(&native_root);
            let spawner = Arc::new(TestSpawner::default().with_catalog(catalog.clone()));
            let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;

            let switch = |manager: &Arc<SessionRuntimeManager>,
                          source_id: &str,
                          attempt: &str| {
                let manager = manager.clone();
                let session_path = source_path.to_string_lossy().into_owned();
                let source_id = source_id.to_owned();
                let attempt = attempt.to_owned();
                async move {
                    manager
                        .admit_web(
                            RpcCommand::SwitchSession {
                                id: Some(format!("handoff-{attempt}")),
                                session_path,
                                catalog_source: Some(wire_source.to_owned()),
                                catalog_session_id: Some("negotiated-fk".into()),
                                catalog_status: Some("foreign".into()),
                                response_mode:
                                    Some(crate::modes::rpc::SubscribeHandoffV1::SubscribeHandoffV1),
                            },
                            Some(source_id),
                            false,
                        )
                        .await
                        .wait()
                        .await
                }
            };

            let first = switch(&manager, manager.primary_session_id().as_str(), "first").await;
            assert!(
                first.success,
                "{kind:?} negotiated switch must succeed: {}",
                first.error.unwrap_or_default()
            );
            let handoff = first.data.expect("handoff data");
            assert_eq!(handoff["catalogSource"], wire_source);
            assert_eq!(handoff["catalogSessionId"], "negotiated-fk");
            assert_eq!(handoff["catalogStatus"], "foreign");
            let runtime_id = handoff["sessionId"].as_str().expect("runtime id");
            assert_ne!(
                runtime_id, "negotiated-fk",
                "foreign import must open a distinct native runtime identity"
            );
            let incarnation = handoff["runtimeIncarnation"].as_str().expect("incarnation");
            assert!(!incarnation.is_empty());
            assert_eq!(handoff["subscriptionRequired"], true);
            assert_eq!(handoff["lifecycleResponseVersion"], 1);
            assert!(handoff["cursor"].is_u64());
            let session = handoff["session"].as_object().expect("native session row");
            assert_eq!(session["sessionId"], runtime_id);
            assert_eq!(session["runtimeIncarnation"], incarnation);
            assert_eq!(session["source"], "rpi");
            assert_eq!(session["status"], "native");
            assert_eq!(session["loaded"], true);
            assert_eq!(spawner.spawns.load(Ordering::SeqCst), 1);
            let opened = spawner.opened.lock().expect("opened").clone();
            assert_eq!(opened.len(), 1);
            assert_ne!(opened[0], source_path);
            assert_eq!(
                opened[0].parent(),
                Some(native_root.as_path()),
                "imported runtime must live under the native root"
            );
            assert_eq!(session["path"], opened[0].to_string_lossy().as_ref());
            let runtime = manager
                .resolve(Some(runtime_id))
                .await
                .expect("imported runtime");
            assert_eq!(
                session["cwd"],
                runtime.application.session().cwd().to_string_lossy().as_ref(),
            );
            drop(opened);
            assert_eq!(
                std::fs::read(&source_path).expect("source bytes after"),
                source_bytes,
                "foreign source must stay byte-invariant"
            );
            assert_eq!(
                std::fs::metadata(&source_path)
                    .expect("source metadata after")
                    .modified()
                    .expect("source mtime after"),
                source_modified,
                "foreign source must stay mtime-invariant"
            );

            // A repeat click on the same row reuses the one import/runtime.
            let second = switch(&manager, manager.primary_session_id().as_str(), "repeat").await;
            assert!(
                second.success,
                "repeat negotiated switch must succeed: {}",
                second.error.unwrap_or_default()
            );
            let repeat = second.data.expect("repeat handoff");
            assert_eq!(repeat["sessionId"], handoff["sessionId"]);
            assert_eq!(repeat["runtimeIncarnation"], incarnation);
            assert_eq!(repeat["catalogSessionId"], "negotiated-fk");
            assert_eq!(spawner.spawns.load(Ordering::SeqCst), 1);
            let imports_after = imported_files(&native_root);
            assert_eq!(
                imports_after, imports_before + 1,
                "exactly one native import must exist after both switches"
            );
            manager.shutdown().await;
        }
    }

    #[tokio::test]
    async fn negotiated_switch_rejects_wrong_catalog_tuple_before_import_or_spawn() {
        let (_home, _native, catalog) = web_catalog_fixture();
        let cwd = tempfile::tempdir().expect("source cwd");
        let source_path = write_codex_session(&catalog, "codex-guard", cwd.path());
        let native_root = catalog
            .root_for(pi_coding::SessionSourceKind::NativePi)
            .path
            .clone();
        let source_bytes = std::fs::read(&source_path).expect("source bytes");
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog.clone()));
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let source_id = manager.primary_session_id();

        let attempt = |case_index: usize, source: &str, id: &str, path: &str, status: &str| {
            let manager = manager.clone();
            let source_id = source_id.clone();
            let source = source.to_owned();
            let id = id.to_owned();
            let status = status.to_owned();
            let command_id = format!("guard-case-{case_index}");
            let path = if path.is_empty() {
                source_path.to_string_lossy().into_owned()
            } else {
                path.to_owned()
            };
            async move {
                manager
                    .admit_web(
                        RpcCommand::SwitchSession {
                            id: Some(command_id),
                            session_path: path,
                            catalog_source: Some(source),
                            catalog_session_id: Some(id),
                            catalog_status: Some(status),
                            response_mode:
                                Some(crate::modes::rpc::SubscribeHandoffV1::SubscribeHandoffV1),
                        },
                        Some(source_id),
                        false,
                    )
                    .await
                    .wait()
                    .await
            }
        };
        for (case_index, (label, source, id, path, status, expected_code)) in [
            ("wrong source", "omp", "codex-guard", "", "foreign", "stale_catalog_target"),
            ("wrong id", "codex", "codex-other", "", "foreign", "stale_catalog_target"),
            (
                "wrong path",
                "codex",
                "codex-guard",
                "/elsewhere/rollout-codex-guard.jsonl",
                "foreign",
                "stale_catalog_target",
            ),
            ("wrong status", "codex", "codex-guard", "", "native", "stale_catalog_target"),
            (
                "unsupported source",
                "claude",
                "codex-guard",
                "",
                "foreign",
                "invalid_request",
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let response = attempt(case_index, source, id, path, status).await;
            assert!(
                !response.success,
                "{label} tuple must fail closed: {response:?}"
            );
            let expected_text = if expected_code == "invalid_request" {
                "catalog identity is invalid"
            } else {
                "session could not be opened right now; verify the selection and retry"
            };
            assert_eq!(
                response.error.as_deref(),
                Some(expected_text),
                "{label} failure text must stay bounded"
            );
            let metadata = response.data.expect("negotiated failure metadata");
            assert_eq!(metadata["code"], expected_code, "{label}");
            assert_eq!(metadata["retryable"], false, "{label}");
        }

        // A partial tuple is a protocol error, classified invalid_request
        // before any catalog lookup participates.
        let partial = manager
            .admit_web(
                RpcCommand::SwitchSession {
                    id: Some("partial".into()),
                    session_path: source_path.to_string_lossy().into_owned(),
                    catalog_source: Some("codex".into()),
                    catalog_session_id: None,
                    catalog_status: Some("foreign".into()),
                    response_mode: Some(crate::modes::rpc::SubscribeHandoffV1::SubscribeHandoffV1),
                },
                Some(source_id.clone()),
                false,
            )
            .await
            .wait()
            .await;
        assert!(!partial.success, "partial tuple must fail closed");
        assert_eq!(
            partial.data.expect("partial tuple metadata")["code"],
            "invalid_request"
        );
        assert_eq!(spawner.spawns.load(Ordering::SeqCst), 0);
        assert!(spawner.opened.lock().expect("opened").is_empty());
        assert_eq!(
            std::fs::read(&source_path).expect("source bytes after"),
            source_bytes,
        );
        assert!(
            native_root
                .read_dir()
                .map(|entries| entries.count())
                .unwrap_or(0)
                == 0,
            "no import may be created for a rejected tuple"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn negotiated_native_switch_validates_catalog_id_on_reuse_and_rejection() {
        let home = tempfile::tempdir().expect("catalog home");
        let native = tempfile::tempdir().expect("native root");
        let catalog =
            pi_coding::SessionCatalog::new(home.path()).with_native_session_root(native.path());
        let cwd = tempfile::tempdir().expect("session cwd");
        let cwd_json =
            serde_json::to_string(&cwd.path().to_string_lossy()).expect("cwd json");
        let session_path = native.path().join("native-row.jsonl");
        std::fs::write(
            &session_path,
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"native-row\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":{cwd_json}}}\n{{\"type\":\"message\",\"id\":\"u\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01Z\",\"message\":{{\"role\":\"user\",\"content\":\"native negotiated prompt\"}}}}\n",
            ),
        )
        .expect("write native session");
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog.clone()));
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;

        let switch = |id: &str, status: &str, attempt: &str| {
            let manager = manager.clone();
            let id = id.to_owned();
            let status = status.to_owned();
            let attempt = attempt.to_owned();
            let session_path = session_path.to_string_lossy().into_owned();
            async move {
                manager
                    .admit_web(
                        RpcCommand::SwitchSession {
                            id: Some(format!("native-handoff-{attempt}")),
                            session_path,
                            catalog_source: Some("rpi".into()),
                            catalog_session_id: Some(id),
                            catalog_status: Some(status),
                            response_mode:
                                Some(crate::modes::rpc::SubscribeHandoffV1::SubscribeHandoffV1),
                        },
                        None,
                        false,
                    )
                    .await
                    .wait()
                    .await
            }
        };

        let first = switch("native-row", "native", "initial").await;
        assert!(
            first.success,
            "negotiated native switch must succeed: {}",
            first.error.unwrap_or_default()
        );
        let handoff = first.data.expect("handoff");
        assert_eq!(handoff["sessionId"], "native-row");
        assert_eq!(handoff["catalogSessionId"], "native-row");
        assert_eq!(handoff["catalogSource"], "rpi");
        assert_eq!(handoff["catalogStatus"], "native");
        let incarnation = handoff["runtimeIncarnation"].as_str().expect("incarnation");
        assert_eq!(spawner.spawns.load(Ordering::SeqCst), 1);


        // The loaded native runtime is reused with its SAME incarnation; the
        // expected catalog row id is validated on the fast path.
        let reused = switch("native-row", "native", "reuse").await;
        assert!(reused.success, "{}", reused.error.unwrap_or_default());
        let reuse = reused.data.expect("reuse handoff");
        assert_eq!(reuse["sessionId"], "native-row");
        assert_eq!(reuse["runtimeIncarnation"], incarnation);
        assert_eq!(spawner.spawns.load(Ordering::SeqCst), 1);

        // A mismatched catalog id must fail closed against the loaded
        // runtime without any respawn.
        let wrong_id = switch("another-native", "native", "wrong-id").await;
        assert!(!wrong_id.success, "mismatched native id must fail: {wrong_id:?}");
        assert_eq!(
            wrong_id.data.expect("wrong id metadata")["code"],
            "stale_catalog_target"
        );
        assert_eq!(spawner.spawns.load(Ordering::SeqCst), 1);
        // A native row may not be relabeled foreign, nor sourced elsewhere.
        for (case_index, (label, id, status, source)) in [
            ("foreign status on native row", "native-row", "foreign", "rpi"),
            ("wrong source on native row", "native-row", "native", "pi"),
        ]
        .into_iter()
        .enumerate()
        {
            let manager = manager.clone();
            let session_path = session_path.to_string_lossy().into_owned();
            let response = manager
                .admit_web(
                    RpcCommand::SwitchSession {
                        id: Some(format!("relabeled-{case_index}")),
                        session_path,
                        catalog_source: Some(source.to_owned()),
                        catalog_session_id: Some(id.to_owned()),
                        catalog_status: Some(status.to_owned()),
                        response_mode:
                            Some(crate::modes::rpc::SubscribeHandoffV1::SubscribeHandoffV1),
                    },
                    None,
                    false,
                )
                .await
                .wait()
                .await;
            assert!(!response.success, "{label} must fail closed: {response:?}");
            assert_eq!(
                response.data.expect("relabel metadata")["code"],
                "stale_catalog_target",
                "{label}"
            );
        }
        assert_eq!(spawner.spawns.load(Ordering::SeqCst), 1);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn negotiated_native_loaded_reuse_resolves_live_catalog_row_before_reuse() {
        // The loaded-runtime fast path must resolve the EXACT current live
        // catalog row (source + id + path + status) before any reuse or
        // echo: a spoofed tuple (foreign "pi" source relabeled native)
        // against the loaded rpi path/id fails stale BEFORE any reuse or
        // spawn participates, and a deleted/changed live row fails stale for
        // even the honest tuple.
        let home = tempfile::tempdir().expect("catalog home");
        let native = tempfile::tempdir().expect("native root");
        let catalog =
            pi_coding::SessionCatalog::new(home.path()).with_native_session_root(native.path());
        let cwd = tempfile::tempdir().expect("session cwd");
        let cwd_json =
            serde_json::to_string(&cwd.path().to_string_lossy()).expect("cwd json");
        let session_path = native.path().join("live-row.jsonl");
        let write_row = |id: &str| {
            std::fs::write(
                &session_path,
                format!(
                    "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":{cwd_json}}}\n{{\"type\":\"message\",\"id\":\"u\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01Z\",\"message\":{{\"role\":\"user\",\"content\":\"live row prompt\"}}}}\n",
                ),
            )
            .expect("write native session row");
        };
        write_row("live-row");
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog.clone()));
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;

        let switch =
            |source: &'static str, id: &'static str, status: &'static str, attempt: &str| {
            let manager = manager.clone();
            let session_path = session_path.to_string_lossy().into_owned();
                let attempt = attempt.to_owned();
                async move {
                manager
                    .admit_web(
                        RpcCommand::SwitchSession {
                            id: Some(format!("live-row-handoff-{attempt}")),
                            session_path,
                            catalog_source: Some(source.into()),
                            catalog_session_id: Some(id.into()),
                            catalog_status: Some(status.into()),
                            response_mode:
                                Some(crate::modes::rpc::SubscribeHandoffV1::SubscribeHandoffV1),
                        },
                        None,
                        false,
                    )
                    .await
                    .wait()
                    .await
            }
        };

        // Load the rpi native runtime once through the honest tuple.
        let first = switch("rpi", "live-row", "native", "initial").await;
        assert!(
            first.success,
            "negotiated native switch must succeed: {}",
            first.error.unwrap_or_default()
        );
        let handoff = first.data.expect("handoff");
        assert_eq!(handoff["sessionId"], "live-row");
        assert_eq!(handoff["catalogSource"], "rpi");
        assert_eq!(handoff["catalogStatus"], "native");
        assert_eq!(spawner.spawns.load(Ordering::SeqCst), 1);


        // Spoofed provenance: a foreign "pi" tuple relabeled native against
        // the loaded rpi path/id must fail stale BEFORE any reuse or spawn.
        let spoofed = switch("pi", "live-row", "native", "spoofed").await;
        assert!(
            !spoofed.success,
            "spoofed provenance must fail closed: {spoofed:?}"
        );
        let spoof = spoofed.data.expect("spoof failure metadata");
        assert_eq!(spoof["code"], "stale_catalog_target");
        assert_eq!(spoof["retryable"], false);
        assert_eq!(
            spawner.spawns.load(Ordering::SeqCst),
            1,
            "no spawn may participate in a rejected reuse"
        );

        // Deleted live row: even the honest tuple fails stale on the fast path.
        std::fs::remove_file(&session_path).expect("delete live row file");
        let deleted = switch("rpi", "live-row", "native", "deleted").await;
        assert!(
            !deleted.success,
            "deleted live row must fail closed: {deleted:?}"
        );
        assert_eq!(
            deleted.data.expect("deleted failure metadata")["code"],
            "stale_catalog_target"
        );
        assert_eq!(spawner.spawns.load(Ordering::SeqCst), 1);

        // Changed live row: a row whose id no longer matches fails stale, and
        // reuse never respawns behind the rejection.
        write_row("renamed-row");
        let changed = switch("rpi", "live-row", "native", "changed").await;
        assert!(
            !changed.success,
            "changed live row must fail closed: {changed:?}"
        );
        assert_eq!(
            changed.data.expect("changed failure metadata")["code"],
            "stale_catalog_target"
        );
        assert_eq!(spawner.spawns.load(Ordering::SeqCst), 1);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn negotiated_native_switch_reopens_lru_retired_target_with_new_incarnation() {
        // A catalog row's incarnation is a residency snapshot: once the LRU
        // cap retires the target, a negotiated switch must reopen the SAME
        // durable session id under a NEW incarnation, never fail on the
        // stale incarnation and never exceed the loaded-slot transaction.
        let home = tempfile::tempdir().expect("catalog home");
        let native = tempfile::tempdir().expect("native root");
        let catalog =
            pi_coding::SessionCatalog::new(home.path()).with_native_session_root(native.path());
        let cwd = tempfile::tempdir().expect("session cwd");
        let cwd_json =
            serde_json::to_string(&cwd.path().to_string_lossy()).expect("cwd json");
        let mut rows = Vec::new();
        let open_count = MAX_LOADED_SESSIONS + 8;
        for index in 0..open_count {
            let path = native.path().join(format!("lru-{index}.jsonl"));
            std::fs::write(
                &path,
                format!(
                    "{{\"type\":\"session\",\"version\":3,\"id\":\"lru-{index}\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":{cwd_json}}}\n{{\"type\":\"message\",\"id\":\"u\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01Z\",\"message\":{{\"role\":\"user\",\"content\":\"lru {index}\"}}}}\n",
                ),
            )
            .expect("write native session");
            rows.push((path, format!("lru-{index}")));
        }
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog));
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;


        let switch = |path: &Path, id: String, attempt: &str| {
            let manager = manager.clone();
            let attempt = attempt.to_owned();
            let session_path = path.to_string_lossy().into_owned();
            async move {
                manager
                    .admit_web(
                        RpcCommand::SwitchSession {
                            id: Some(format!("lru-handoff-{attempt}")),
                            session_path,
                            catalog_source: Some("rpi".into()),
                            catalog_session_id: Some(id),
                            catalog_status: Some("native".into()),
                            response_mode:
                                Some(crate::modes::rpc::SubscribeHandoffV1::SubscribeHandoffV1),
                        },
                        None,
                        false,
                    )
                    .await
                    .wait()
                    .await
            }
        };

        let mut incarnations = Vec::new();
        for (index, (path, id)) in rows.iter().enumerate() {
            let response = switch(path, id.clone(), &format!("open-{index}")).await;
            assert!(
                response.success,
                "negotiated open {index} must succeed: {}",
                response.error.unwrap_or_default()
            );
            let handoff = response.data.expect("handoff");
            assert_eq!(handoff["sessionId"], id.as_str());
            assert_eq!(handoff["catalogSessionId"], id.as_str());
            incarnations.push(
                handoff["runtimeIncarnation"]
                    .as_str()
                    .expect("incarnation")
                    .to_owned(),
            );
            let registry = manager.registry.read().await;
            assert!(registry.distinct_runtimes().len() <= MAX_LOADED_SESSIONS);
            drop(registry);
        }
        assert_eq!(spawner.spawns.load(Ordering::SeqCst), open_count);

        // The first target was retired by the LRU transaction; the old
        // runtime is unreachable by its durable id.
        let retired = manager
            .dispatch(
                RpcCommand::GetState { id: None },
                Some(rows[0].1.clone()),
            )
            .await;
        assert!(!retired.success, "the LRU victim must be retired");

        // Reopening the retired target through a negotiated switch returns
        // the SAME durable session id under a NEW incarnation.
        let reopened = switch(&rows[0].0, rows[0].1.clone(), "reopen-0").await;
        assert!(
            reopened.success,
            "negotiated reopen after LRU must succeed: {}",
            reopened.error.unwrap_or_default()
        );
        let reopen = reopened.data.expect("reopen handoff");
        assert_eq!(reopen["sessionId"], "lru-0");
        assert_eq!(reopen["catalogSessionId"], "lru-0");
        assert_eq!(reopen["catalogStatus"], "native");
        let reopened_incarnation =
            reopen["runtimeIncarnation"].as_str().expect("incarnation");
        assert_ne!(
            reopened_incarnation, incarnations[0],
            "a reopened runtime must carry a fresh incarnation"
        );
        assert_eq!(spawner.spawns.load(Ordering::SeqCst), open_count + 1);
        let registry = manager.registry.read().await;
        assert_eq!(registry.distinct_runtimes().len(), MAX_LOADED_SESSIONS);
        drop(registry);
        manager.shutdown().await;
    }



    #[test]
    fn web_catalog_foreign_import_leaves_source_bytes_invariant() {
        let (_home, _native, catalog) = web_catalog_fixture();
        let cwd = tempfile::tempdir().expect("source cwd");
        let source = write_codex_session(&catalog, "codex-bytes", cwd.path());
        let before = std::fs::read(&source).expect("source bytes before");
        let mtime_before = std::fs::metadata(&source)
            .expect("source metadata before")
            .modified()
            .ok();
        let sources = [
            pi_coding::SessionSourceKind::NativePi,
            pi_coding::SessionSourceKind::Omp,
            pi_coding::SessionSourceKind::Codex,
            pi_coding::SessionSourceKind::Grok,
        ];

        let first = resolve_web_catalog_session_path(
            &catalog,
            cwd.path(),
            &source.to_string_lossy(),
            &sources,
        )
        .expect("first import");
        // A second selection reuses the existing native import; both must
        // leave the foreign source untouched (read-only foreign JSONL).
        let second = resolve_web_catalog_session_path(
            &catalog,
            cwd.path(),
            &source.to_string_lossy(),
            &sources,
        )
        .expect("reuse import");
        assert_eq!(second, first);

        assert_eq!(
            std::fs::read(&source).expect("source bytes after"),
            before,
            "foreign source bytes must not change on import or reuse",
        );
        let mtime_after = std::fs::metadata(&source)
            .expect("source metadata after")
            .modified()
            .ok();
        assert_eq!(
            mtime_after, mtime_before,
            "foreign source mtime must not change on import or reuse",
        );
    }

    #[tokio::test]
    async fn switch_imports_foreign_session_before_spawn_and_reuses_lineage() {
        let (_home, _native, catalog) = web_catalog_fixture();
        let cwd = tempfile::tempdir().expect("source cwd");
        let source_path = write_codex_session(&catalog, "codex-mgr", cwd.path());
        let source_bytes_before = std::fs::read(&source_path).expect("source bytes before");
        let native_root = catalog
            .root_for(pi_coding::SessionSourceKind::NativePi)
            .path
            .clone();
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog.clone()));
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;

        let first = manager
            .dispatch(
                RpcCommand::SwitchSession {
                    id: None,
                    session_path: source_path.to_string_lossy().into_owned(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                None,
            )
            .await;
        assert!(first.success, "{}", first.error.unwrap_or_default());

        // The manager imported the foreign source through the catalog BEFORE
        // spawning, and spawned the NATIVE copy — never the foreign file.
        assert_eq!(spawner.spawns.load(Ordering::SeqCst), 1);
        let opened = spawner.opened.lock().expect("opened").clone();
        assert_eq!(opened.len(), 1, "exactly one spawn");
        assert_ne!(
            opened[0], source_path,
            "must not open the foreign source in place",
        );
        assert_eq!(
            opened[0].parent(),
            Some(native_root.as_path()),
            "spawned the native import under the native root: {:?}",
            opened[0],
        );

        // The foreign source file is never mutated by import or spawn.
        assert_eq!(
            std::fs::read(&source_path).expect("source bytes after"),
            source_bytes_before,
        );

        // Repeated selection reuses the one native import/runtime: the
        // already-loaded retained file identity deduplicates before spawning.
        let second = manager
            .dispatch(
                RpcCommand::SwitchSession {
                    id: None,
                    session_path: source_path.to_string_lossy().into_owned(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                None,
            )
            .await;
        assert!(second.success, "{}", second.error.unwrap_or_default());
        assert_eq!(
            spawner.spawns.load(Ordering::SeqCst),
            1,
            "reuse must not spawn a second runtime",
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn switch_nested_native_catalog_sessions_a_b_a_preserves_identity_and_messages() {
        let home = tempfile::tempdir().expect("catalog home");
        let agent = tempfile::tempdir().expect("native agent");
        let catalog =
            pi_coding::SessionCatalog::new(home.path()).with_native_agent_dir(agent.path());
        let native_root = catalog
            .root_for(pi_coding::SessionSourceKind::NativePi)
            .path;
        let cwd = tempfile::tempdir().expect("session cwd");
        let session_dir = native_root.join("--workspace-nested-switch--");
        std::fs::create_dir_all(&session_dir).expect("nested native session directory");
        let write_session = |file: &str, id: &str, prompt: &str| {
            let path = session_dir.join(file);
            let cwd_json =
                serde_json::to_string(&cwd.path().to_string_lossy()).expect("session cwd json");
            std::fs::write(
                &path,
                format!(
                    "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":{cwd_json}}}\n{{\"type\":\"message\",\"id\":\"{id}-user\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01Z\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"{prompt}\"}}],\"timestamp\":0}}}}\n",
                ),
            )
            .expect("nested native session");
            path
        };
        let session_a = write_session("a.jsonl", "nested-a", "message-a");
        let session_b = write_session("b.jsonl", "nested-b", "message-b");
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog));
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;

        let switch = |path: &Path, source_id: String| {
            manager.dispatch(
                RpcCommand::SwitchSession {
                    id: None,
                    session_path: path.to_string_lossy().into_owned(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                Some(source_id),
            )
        };
        let assert_snapshot = |response: &RpcResponse,
                               expected_target_id: &str,
                               expected_text: &str| {
            assert_eq!(response.session_id.as_deref(), Some(expected_target_id));
            assert_eq!(
                response.data.as_ref().unwrap()["sessionId"],
                expected_target_id,
            );
            let messages: Vec<Message> =
                serde_json::from_value(response.data.as_ref().unwrap()["messages"].clone())
                    .expect("typed snapshot messages");
            let Message::User(user) = messages.first().expect("snapshot user message") else {
                panic!("snapshot first message is not user-owned");
            };
            let pi_ai::ContentBlock::Text { text, .. } =
                user.content.first().expect("snapshot user text block")
            else {
                panic!("snapshot user message has no text block");
            };
            assert_eq!(text, expected_text);
        };
        let primary_id = manager.primary_session_id();
        let opened_a = switch(&session_a, primary_id.clone()).await;
        assert!(opened_a.success, "{}", opened_a.error.unwrap_or_default());
        assert_snapshot(&opened_a, "nested-a", "message-a");

        let opened_b = switch(&session_b, "nested-a".to_owned()).await;
        assert!(opened_b.success, "{}", opened_b.error.unwrap_or_default());
        assert_snapshot(&opened_b, "nested-b", "message-b");

        let returned_a = switch(&session_a, "nested-b".to_owned()).await;
        assert!(
            returned_a.success,
            "{}",
            returned_a.error.unwrap_or_default()
        );
        assert_snapshot(&returned_a, "nested-a", "message-a");
        assert_eq!(spawner.spawns.load(Ordering::SeqCst), 2);
        assert_eq!(
            spawner.opened.lock().expect("opened").as_slice(),
            [session_a, session_b],
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn switch_rejects_duplicate_session_id_from_different_path() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let dir = tempfile::tempdir().expect("sessions");
        let (model, _registration) = faux_model("duplicate-id");
        let first = pi_coding::start_session_in(
            dir.path(),
            Some(&model),
            Some("off"),
            Some(dir.path()),
            None,
            None,
        )
        .expect("first session");
        first.persist_now().expect("first persisted");
        let first_path = first.path();
        let second_path = dir.path().join("duplicate.jsonl");
        std::fs::copy(&first_path, &second_path).expect("copy duplicate session");

        let first_open = manager
            .dispatch(
                RpcCommand::SwitchSession {
                    id: None,
                    session_path: first_path.to_string_lossy().into_owned(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                None,
            )
            .await;
        assert!(
            first_open.success,
            "{}",
            first_open.error.unwrap_or_default()
        );
        let duplicate = manager
            .dispatch(
                RpcCommand::SwitchSession {
                    id: None,
                    session_path: second_path.to_string_lossy().into_owned(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                None,
            )
            .await;

        assert!(!duplicate.success);
        let error = duplicate.error.as_deref().expect("bounded switch failure");
        assert_eq!(
            error,
            "session could not be opened right now; verify the selection and retry"
        );
        assert!(!error.contains(second_path.to_string_lossy().as_ref()));
        assert_eq!(spawner.spawns.load(Ordering::SeqCst), 1);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn new_session_returns_snapshot_and_leaves_source_intact() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let before = manager
            .dispatch(RpcCommand::GetState { id: None }, None)
            .await;
        let before_messages: Vec<Message> = manager
            .primary
            .application
            .messages()
            .into_iter()
            .map(public_message)
            .collect();

        let response = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: Some("n1".into()),
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(response.success, "{}", response.error.unwrap_or_default());
        let data = response.data.expect("snapshot");
        let child_id = data["sessionId"]
            .as_str()
            .expect("child sessionId")
            .to_owned();
        assert!(data["state"]["sessionId"].as_str() == Some(child_id.as_str()));
        assert!(data["messages"].is_array());
        let child_runtime = manager
            .resolve(Some(&child_id))
            .await
            .expect("child runtime");
        assert_ne!(
            manager.primary.application.session().process_owner_id(),
            child_runtime.application.session().process_owner_id(),
            "manager-owned replacement must have a fresh Session process owner",
        );
        assert_eq!(spawner.spawns.load(Ordering::SeqCst), 1);
        assert_eq!(
            spawner.fresh_cwds.lock().expect("fresh cwds").as_slice(),
            [manager.primary.application.session().cwd().to_path_buf()]
        );

        let after = manager
            .dispatch(RpcCommand::GetState { id: None }, None)
            .await;
        assert!(after.success);
        assert_eq!(
            after.data.as_ref().unwrap().get("sessionId"),
            before.data.as_ref().unwrap().get("sessionId")
        );
        let after_messages: Vec<Message> = manager.primary.application.messages();
        assert_eq!(after_messages.len(), before_messages.len());

        let child_state = manager
            .dispatch(RpcCommand::GetState { id: None }, Some(child_id.clone()))
            .await;
        assert!(
            child_state.success,
            "{}",
            child_state.error.unwrap_or_default()
        );
        assert_eq!(
            child_state.data.as_ref().unwrap()["sessionId"].as_str(),
            Some(child_id.as_str())
        );
        let unknown = manager
            .dispatch(
                RpcCommand::GetState { id: None },
                Some(format!("{child_id}x")),
            )
            .await;
        assert!(!unknown.success);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn explicit_new_session_cwd_is_validated_before_spawn_and_preserves_source() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let source_id = manager.primary_session_id();
        let source_cwd = manager.primary.application.session().cwd().to_path_buf();
        let source_messages = manager.primary.application.messages();
        let source_state = manager.primary.application.state().await;
        let target_parent = tempfile::tempdir().expect("independent workspace");
        let target = target_parent.path().join("target");
        std::fs::create_dir_all(&target).expect("target directory");

        let opened = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: Some("explicit-cwd".into()),
                    parent_session: None,
                    cwd: Some(target.to_string_lossy().into_owned()),
                },
                Some(source_id.clone()),
            )
            .await;
        assert!(opened.success, "{}", opened.error.unwrap_or_default());
        assert_eq!(
            spawner.fresh_cwds.lock().expect("fresh cwds").as_slice(),
            [target.clone()]
        );
        assert_eq!(
            opened.data.as_ref().unwrap()["state"]["cwd"].as_str(),
            target.to_str()
        );
        let target_session_id = opened.data.as_ref().unwrap()["sessionId"]
            .as_str()
            .expect("target session id");
        let target_runtime = manager
            .resolve(Some(target_session_id))
            .await
            .expect("target runtime");
        assert_eq!(
            target_runtime
                .application
                .session()
                .resource_manager()
                .expect("target resources")
                .snapshot()
                .cwd,
            target.as_path()
        );
        assert_eq!(
            manager.primary.application.session().cwd(),
            source_cwd.as_path()
        );
        assert_eq!(manager.primary.application.messages(), source_messages);
        let source_state_after = manager.primary.application.state().await;
        assert_eq!(source_state_after.session_id, source_state.session_id);
        assert_eq!(source_state_after.session_file, source_state.session_file);

        // Explicitly re-selecting the current cwd is an operator choice and
        // succeeds like any other existing directory.
        let current = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: Some("current-cwd".into()),
                    parent_session: None,
                    cwd: Some(source_cwd.to_string_lossy().into_owned()),
                },
                Some(source_id.clone()),
            )
            .await;
        assert!(current.success, "{}", current.error.unwrap_or_default());
        assert_eq!(
            spawner.fresh_cwds.lock().expect("fresh cwds").last(),
            Some(&source_cwd)
        );

        let inherited = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: Some("empty-cwd".into()),
                    parent_session: None,
                    cwd: Some("   ".into()),
                },
                Some(source_id),
            )
            .await;
        assert!(inherited.success, "{}", inherited.error.unwrap_or_default());
        assert_eq!(
            spawner.fresh_cwds.lock().expect("fresh cwds").last(),
            Some(&source_cwd)
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn invalid_new_session_cwds_fail_before_reservation_or_spawn() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let source_id = manager.primary_session_id();
        let source_cwd = manager.primary.application.session().cwd().to_path_buf();
        let source_messages = manager.primary.application.messages();
        let fixture = tempfile::tempdir().expect("invalid cwd fixture");
        let file = fixture.path().join("file.txt");
        std::fs::write(&file, "not a directory").expect("fixture file");
        let relative = PathBuf::from("relative-cwd");
        let mut rejected = vec![relative, file];
        #[cfg(unix)]
        {
            let real = fixture.path().join("real");
            let nested = real.join("nested");
            let link = fixture.path().join("link");
            std::fs::create_dir_all(&nested).expect("nested real directory");
            std::os::unix::fs::symlink(&real, &link).expect("directory symlink");
            rejected.push(link.clone());
            rejected.push(link.join("nested"));
        }

        let available_before = manager.loaded_slots.available_permits();
        for (index, cwd) in rejected.into_iter().enumerate() {
            let response = manager
                .dispatch(
                    RpcCommand::NewSession {
                        id: Some(format!("reject-{index}")),
                        parent_session: None,
                        cwd: Some(cwd.to_string_lossy().into_owned()),
                    },
                    Some(source_id.clone()),
                )
                .await;
            // Lifecycle FAILURES stay request-source-owned: the envelope
            // sessionId is the source, never a guessed target, so the Web
            // caller's ordinary source-ownership check accepts the rejection.
            assert_eq!(
                response.session_id.as_deref(),
                Some(source_id.as_str()),
                "new_session failure must keep the source-owned envelope"
            );
            assert_eq!(
                manager.primary.application.session().cwd(),
                source_cwd.as_path()
            );
            assert_eq!(manager.primary.application.messages(), source_messages);
        }

        // An embedded NUL is malformed input: rejected by the policy-path
        // validation before any reservation or spawn.
        let nul_response = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: Some("reject-nul".into()),
                    parent_session: None,
                    cwd: Some(format!("{}\0bad", fixture.path().display())),
                },
                Some(source_id.clone()),
            )
            .await;
        assert!(!nul_response.success, "NUL cwd opened");
        assert_eq!(
            nul_response.error.as_deref(),
            Some("new session directory path is invalid")
        );
        assert_eq!(spawner.spawns.load(Ordering::SeqCst), 0);
        assert_eq!(manager.loaded_slots.available_permits(), available_before);

        // NUL in an INTERMEDIATE component takes the ancestor-walk branch and
        // must lose no category precision either.
        let intermediate_nul = format!("{}\0parent", fixture.path().display());
        let intermediate_nul_cwd = format!("{intermediate_nul}/child");
        let intermediate_response = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: Some("reject-nul-intermediate".into()),
                    parent_session: None,
                    cwd: Some(intermediate_nul_cwd),
                },
                Some(source_id.clone()),
            )
            .await;
        assert!(
            !intermediate_response.success,
            "intermediate NUL cwd opened"
        );
        assert_eq!(
            intermediate_response.error.as_deref(),
            Some("new session directory path is invalid")
        );

        // A missing directory auto-creates (nested paths included) and opens.
        let missing = fixture.path().join("projects").join("auto-created");
        let missing_response = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: Some("create-missing".into()),
                    parent_session: None,
                    cwd: Some(missing.to_string_lossy().into_owned()),
                },
                Some(source_id.clone()),
            )
            .await;
        assert!(missing_response.success, "{}", missing_response.error.unwrap_or_default());
        assert!(missing.is_dir(), "missing cwd must be auto-created");
        assert_eq!(
            spawner.fresh_cwds.lock().expect("fresh cwds").last(),
            Some(&missing),
        );
        let permits_after_create = manager.loaded_slots.available_permits();
        #[cfg(unix)]
        {
            let link = fixture.path().join("link");
            let link_response = manager
                .dispatch(
                    RpcCommand::NewSession {
                        id: Some("reject-link".into()),
                        parent_session: None,
                        cwd: Some(link.to_string_lossy().into_owned()),
                    },
                    Some(source_id.clone()),
                )
                .await;
            assert!(!link_response.success, "symlink cwd opened");
            assert_eq!(
                link_response.error.as_deref(),
                Some("new session path must not be a symbolic link")
            );
            assert_eq!(spawner.spawns.load(Ordering::SeqCst), 1);
            assert_eq!(
                manager.loaded_slots.available_permits(),
                permits_after_create
            );
        }
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn explicit_new_session_cwd_accepts_independent_directory_outside_source_workspace() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let source_id = manager.primary_session_id();
        let source_cwd = manager.primary.application.session().cwd().to_path_buf();
        let source_messages = manager.primary.application.messages();
        let sibling = tempfile::tempdir().expect("independent workspace");
        let target = sibling.path().join("independent-project");
        std::fs::create_dir_all(&target).expect("target directory");
        assert!(
            !target.starts_with(&source_cwd),
            "fixture must live outside the source workspace"
        );

        let opened = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: Some("independent-cwd".into()),
                    parent_session: None,
                    cwd: Some(target.to_string_lossy().into_owned()),
                },
                Some(source_id.clone()),
            )
            .await;
        assert!(opened.success, "{}", opened.error.unwrap_or_default());
        assert_eq!(
            spawner.fresh_cwds.lock().expect("fresh cwds").as_slice(),
            [target.clone()]
        );
        assert_eq!(
            opened.data.as_ref().unwrap()["state"]["cwd"].as_str(),
            target.to_str()
        );
        let target_session_id = opened.data.as_ref().unwrap()["sessionId"]
            .as_str()
            .expect("target session id")
            .to_owned();
        let target_runtime = manager
            .resolve(Some(&target_session_id))
            .await
            .expect("target runtime");
        assert_eq!(
            target_runtime
                .application
                .session()
                .resource_manager()
                .expect("target resources")
                .snapshot()
                .cwd,
            target.as_path()
        );
        assert_eq!(
            manager.primary.application.session().cwd(),
            source_cwd.as_path()
        );
        assert_eq!(manager.primary.application.messages(), source_messages);

        // Switch/fork never adopt the explicit new-session directory: a
        // directory path stays a failed switch target, and fork spawns do not
        // route through the Fresh cwd path.
        let switch = manager
            .dispatch(
                RpcCommand::SwitchSession {
                    id: None,
                    session_path: target.to_string_lossy().into_owned(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                Some(source_id.clone()),
            )
            .await;
        assert!(
            !switch.success,
            "a directory path must not become a switch target"
        );
        assert_eq!(
            switch.error.as_deref(),
            Some("session could not be opened right now; verify the selection and retry")
        );
        let fork = manager
            .dispatch(
                RpcCommand::Fork {
                    id: Some("fork-after-new".into()),
                    entry_id: "any-entry".into(),
                    session_path: None,
                },
                Some(source_id.clone()),
            )
            .await;
        assert!(fork.success, "{}", fork.error.unwrap_or_default());
        assert_eq!(
            spawner.fresh_cwds.lock().expect("fresh cwds").len(),
            1,
            "fork must not route through the Fresh cwd path"
        );
        assert_eq!(
            manager.primary.application.session().cwd(),
            source_cwd.as_path()
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn trust_denied_new_session_factory_failure_leaves_source_unchanged() {
        let spawner = Arc::new(FailingSpawner::new());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let source_id = manager.primary_session_id();
        let source_cwd = manager.primary.application.session().cwd().to_path_buf();
        let source_messages = manager.primary.application.messages();
        let sibling = tempfile::tempdir().expect("independent workspace");
        let target = sibling.path().join("untrusted-project");
        std::fs::create_dir_all(&target).expect("target directory");

        spawner.fail_next_with(
            "explicit project extension requires project trust: (pass --approve to allow it)",
        );
        let response = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: Some("trust-denied".into()),
                    parent_session: None,
                    cwd: Some(target.to_string_lossy().into_owned()),
                },
                Some(source_id),
            )
            .await;
        assert!(!response.success, "trust-denied spawn must fail");
        assert_eq!(
            response.error.as_deref(),
            Some("new session project requires trust approval")
        );
        assert_eq!(
            manager.primary.application.session().cwd(),
            source_cwd.as_path()
        );
        assert_eq!(manager.primary.application.messages(), source_messages);
        assert_eq!(
            manager.loaded_slots.available_permits(),
            MAX_LOADED_SESSIONS - 1
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn different_sessions_build_concurrently() {
        let spawner = Arc::new(GatedSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;

        let first = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .dispatch(
                        RpcCommand::NewSession {
                            id: Some("first".into()),
                            parent_session: None,
                            cwd: None,
                        },
                        None,
                    )
                    .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), spawner.started.acquire())
            .await
            .expect("first spawn must reach the factory")
            .expect("first spawn starts")
            .forget();

        let second = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .dispatch(
                        RpcCommand::NewSession {
                            id: Some("second".into()),
                            parent_session: None,
                            cwd: None,
                        },
                        None,
                    )
                    .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), spawner.started.acquire())
            .await
            .expect("second independent spawn must start before the first finishes")
            .expect("second spawn starts")
            .forget();

        spawner.release.add_permits(2);
        let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            (
                first.await.expect("first spawn task"),
                second.await.expect("second spawn task"),
            )
        })
        .await
        .expect("independent spawns must finish after release");
        assert!(
            first.success,
            "first spawn: {}",
            first.error.unwrap_or_default()
        );
        assert!(
            second.success,
            "second spawn: {}",
            second.error.unwrap_or_default()
        );
        assert_ne!(
            first
                .data
                .as_ref()
                .and_then(|data| data["sessionId"].as_str()),
            second
                .data
                .as_ref()
                .and_then(|data| data["sessionId"].as_str()),
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn switch_session_dedups_by_canonical_path_and_returns_same_runtime() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let dir = tempfile::tempdir().expect("session dir");
        let (model, _registration) = faux_model("dedup");
        let recorder = pi_coding::start_session_in(
            dir.path(),
            Some(&model),
            Some("off"),
            Some(dir.path()),
            None,
            None,
        )
        .expect("start session");
        recorder.persist_now().expect("persist");
        let path = recorder.path();

        let first = manager
            .dispatch(
                RpcCommand::SwitchSession {
                    id: None,
                    session_path: path.to_string_lossy().into_owned(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                None,
            )
            .await;
        assert!(first.success, "{}", first.error.unwrap_or_default());
        let first_id = first.data.as_ref().unwrap()["sessionId"]
            .as_str()
            .expect("first id")
            .to_owned();

        let second = manager
            .dispatch(
                RpcCommand::SwitchSession {
                    id: None,
                    session_path: path.to_string_lossy().into_owned(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                None,
            )
            .await;
        assert!(second.success, "{}", second.error.unwrap_or_default());
        let second_id = second.data.as_ref().unwrap()["sessionId"]
            .as_str()
            .expect("second id")
            .to_owned();
        assert_eq!(first_id, second_id, "dedup must return the same runtime");
        assert_eq!(spawner.spawns.load(Ordering::SeqCst), 1, "no second open");
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn switch_loaded_runtime_does_not_reparse_its_live_recorder() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let opened = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: Some("open-live-target".into()),
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(opened.success, "{}", opened.error.unwrap_or_default());
        let target_id = opened.data.as_ref().unwrap()["sessionId"]
            .as_str()
            .expect("live target id")
            .to_owned();
        let target = manager
            .resolve(Some(&target_id))
            .await
            .expect("live target runtime");
        let target_path = target
            .application
            .session()
            .recorder_info()
            .expect("live target recorder")
            .1;
        let original = std::fs::read(&target_path).expect("live target bytes");
        std::fs::write(&target_path, b"not a complete session record\n")
            .expect("replace live recorder bytes");

        let response = manager
            .dispatch(
                RpcCommand::SwitchSession {
                    id: Some("reuse-live-target".into()),
                    session_path: target_path.to_string_lossy().into_owned(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                None,
            )
            .await;

        std::fs::write(&target_path, original).expect("restore live target recorder");
        assert!(response.success, "{}", response.error.unwrap_or_default());
        assert_eq!(response.session_id.as_deref(), Some(target_id.as_str()));
        assert_eq!(response.data.as_ref().unwrap()["sessionId"], target_id);
        assert_eq!(
            spawner.spawns.load(Ordering::SeqCst),
            1,
            "loaded target must not respawn"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn concurrent_switches_to_the_same_file_spawn_once() {
        let spawner = Arc::new(GatedSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let dir = tempfile::tempdir().expect("session dir");
        let (model, _registration) = faux_model("concurrent-dedup");
        let recorder = pi_coding::start_session_in(
            dir.path(),
            Some(&model),
            Some("off"),
            Some(dir.path()),
            None,
            None,
        )
        .expect("start session");
        recorder.persist_now().expect("persist");
        let path = recorder.path().to_string_lossy().into_owned();

        let first = tokio::spawn({
            let manager = manager.clone();
            let path = path.clone();
            async move {
                manager
                    .dispatch(
                        RpcCommand::SwitchSession {
                            id: Some("first".into()),
                            session_path: path,
                            catalog_source: None,
                            catalog_session_id: None,
                            catalog_status: None,
                            response_mode: None,
                        },
                        None,
                    )
                    .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), spawner.started.acquire())
            .await
            .expect("first open must reach the factory")
            .expect("first open starts")
            .forget();
        let second = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .dispatch(
                        RpcCommand::SwitchSession {
                            id: Some("second".into()),
                            session_path: path,
                            catalog_source: None,
                            catalog_session_id: None,
                            catalog_status: None,
                            response_mode: None,
                        },
                        None,
                    )
                    .await
            }
        });
        spawner.release.add_permits(1);
        let responses = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            (
                first.await.expect("first open task"),
                second.await.expect("second open task"),
            )
        })
        .await;
        if responses.is_err() {
            spawner.release.add_permits(2);
        }
        let (first, second) =
            responses.expect("duplicate opens must finish from one released spawn");
        assert!(
            first.success,
            "first open: {}",
            first.error.unwrap_or_default()
        );
        assert!(
            second.success,
            "second open: {}",
            second.error.unwrap_or_default()
        );
        assert_eq!(
            first
                .data
                .as_ref()
                .and_then(|data| data["sessionId"].as_str()),
            second
                .data
                .as_ref()
                .and_then(|data| data["sessionId"].as_str()),
        );
        assert_eq!(spawner.inner.spawns.load(Ordering::SeqCst), 1);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn slow_prompt_on_a_does_not_block_b_command() {
        let spawner = Arc::new(TestSpawner::default());
        // The primary's model streams one byte per chunk: a 200k-char reply
        // keeps A busy for well past B's timeout window.
        let (manager, primary_registration) = manager_with_sized(Some(spawner.clone()), 1).await;
        let opened = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        let b_id = opened.data.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();

        primary_registration.set_responses(vec![FauxResponse::text("x".repeat(200_000))]);

        let prompt = RpcCommand::Prompt {
            id: Some("p".into()),
            message: "slow".into(),
            images: Vec::new(),
            streaming_behavior: None,
        };
        let manager_task = manager.clone();
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let wait_started = started.notified();
        let signal_started = started.clone();
        let mut app_events = manager.primary.application.subscribe();
        tokio::spawn(async move {
            loop {
                match app_events.recv().await {
                    Ok(ApplicationEvent::TurnStarted { .. }) => {
                        signal_started.notify_one();
                        return;
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        });
        let a_task = tokio::spawn(async move { manager_task.dispatch(prompt, None).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), wait_started)
            .await
            .expect("A prompt never started streaming");

        let b_response = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            manager.dispatch(RpcCommand::GetState { id: None }, Some(b_id.clone())),
        )
        .await
        .expect("B get_state must complete while A is prompting");
        assert!(
            b_response.success,
            "{}",
            b_response.error.unwrap_or_default()
        );
        assert_eq!(
            b_response.data.unwrap()["sessionId"].as_str(),
            Some(b_id.as_str())
        );

        let a_result = a_task.await.expect("A prompt task joins");
        assert!(
            a_result.success,
            "A prompt completed: {}",
            a_result.error.unwrap_or_default()
        );
        manager.shutdown().await;
    }
    #[tokio::test]
    async fn expired_ledger_handoff_completes_admitted_waiter_and_releases_entry() {
        let mut state = test_ledger_state();
        let digest = [7; 32];
        let reservation = reserve_new_command(&mut state, Some("large"), digest, "get_state", "s");
        let (completion, waiter) = reservation_response(
            reservation,
            Some("large".into()),
            "get_state",
            "s",
        )
        .unwrap_or_else(|_| panic!("new command"));
        let response = RpcResponse::success(Some("large".into()), "get_state", None)
            .with_session_id("s");

        expire_owned_command(&mut state, completion, response.clone());

        assert_eq!(waiter.wait().await.id, response.id);
        assert!(!state.commands.contains_key("large"));
        assert!(!state.command_order.iter().any(|id| id == "large"));
        assert!(matches!(
            existing_command_reservation(&state, "large", digest),
            Some(CommandReservation::Expired)
        ));
    }

    #[test]
    fn ledger_eviction_physically_releases_capacity_and_keeps_bounded_tombstone() {
        let mut state = test_ledger_state();
        for index in 0..COMMAND_LEDGER_CAPACITY {
            let id = format!("id-{index}");
            let digest = [index as u8; 32];
            let (outcome, _receiver) = watch::channel(Some(Arc::new(RpcResponse::success(
                Some(id.clone()),
                "get_state",
                None,
            ))));
            let response = outcome.borrow().clone();
            state.commands.insert(
                id.clone(),
                CommandLedgerEntry {
                    digest,
                    response,
                    encoded_bytes: 1,
                    outcome,
                },
            );
            state.command_order.push_back(id);
            state.command_ledger_bytes += 1;
        }
        assert!(evict_completed_ledger_entry(&mut state));
        assert_eq!(state.commands.len(), COMMAND_LEDGER_CAPACITY - 1);
        assert_eq!(state.command_order.len(), COMMAND_LEDGER_CAPACITY - 1);
        assert_eq!(state.expired_commands.len(), 1);
    }

    #[test]
    fn oversized_command_id_is_not_retained_by_ledger() {
        let mut state = test_ledger_state();
        let id = "x".repeat(MAX_COMMAND_LEDGER_ID_BYTES + 1);
        assert!(matches!(
            reserve_new_command(&mut state, Some(&id), [1; 32], "get_state", "s"),
            CommandReservation::InvalidId
        ));
        assert!(state.commands.is_empty());
        assert!(state.command_order.is_empty());
        assert!(state.expired_commands.is_empty());
    }

    /// The ledger capacity branch with COMMAND_LEDGER_CAPACITY IN-FLIGHT
    /// (never-completed) entries: admission fails closed with the exact
    /// response and ZERO state mutation — no eviction, no tombstone, no
    /// byte-budget drift.
    #[tokio::test]
    async fn command_ledger_capacity_rejects_in_flight_admission_without_mutation() {
        let mut state = test_ledger_state();
        for index in 0..COMMAND_LEDGER_CAPACITY {
            let id = format!("in-flight-{index}");
            assert!(
                matches!(
                    reserve_new_command(&mut state, Some(&id), [index as u8; 32], "get_state", "s"),
                    CommandReservation::Execute(..)
                ),
                "reservation {index} must be admitted"
            );
        }
        let commands_before = state.commands.len();
        let order_before = state.command_order.len();
        let bytes_before = state.command_ledger_bytes;
        let expired_before = state.expired_commands.len();

        let overflow = format!("in-flight-{COMMAND_LEDGER_CAPACITY}");
        let reservation = reserve_new_command(
            &mut state,
            Some(&overflow),
            [0; 32],
            "get_state",
            "s",
        );
        assert!(
            matches!(reservation, CommandReservation::Capacity),
            "saturated ledger with in-flight entries must return Capacity"
        );
        let rejection = match reservation_response(
            reservation,
            Some(overflow.clone()),
            "get_state",
            "s",
        ) {
            Err(rejection) => rejection,
            Ok(_) => panic!("capacity reservation must fail closed"),
        };
        let response = rejection.wait().await;
        assert!(!response.success);
        assert_eq!(
            response.error.as_deref(),
            Some("command ledger is full with in-flight commands"),
            "the capacity branch returns the exact bounded response"
        );
        assert_eq!(response.session_id.as_deref(), Some("s"));
        assert_eq!(response.id.as_deref(), Some(overflow.as_str()));

        assert_eq!(state.commands.len(), commands_before, "zero mutation");
        assert_eq!(state.command_order.len(), order_before, "zero mutation");
        assert_eq!(state.command_ledger_bytes, bytes_before, "zero mutation");
        assert_eq!(state.expired_commands.len(), expired_before, "zero mutation");
        assert_eq!(state.expired_command_order.len(), 0, "no tombstone leaked");
    }

    /// A transcript-bearing identified response expires out of the replay
    /// ledger; re-admitting the IDENTICAL command digest in FullSnapshot
    /// mode resolves to exactly `Expired` — never `Conflict`.
    #[test]
    fn transcript_ledger_re_admit_identical_digest_requires_expired() {
        let mut state = test_ledger_state();
        let digest = [9; 32];
        let (completion, _waiter) = match reservation_response(
            reserve_new_command(&mut state, Some("transcript"), digest, "get_messages", "s"),
            Some("transcript".into()),
            "get_messages",
            "s",
        ) {
            Ok(value) => value,
            Err(_) => panic!("new command must be admitted"),
        };
        expire_owned_command(
            &mut state,
            completion,
            RpcResponse::success(
                Some("transcript".into()),
                "get_messages",
                Some(json!({"messages": []})),
            )
            .with_session_id("s"),
        );
        assert!(
            !state.commands.contains_key("transcript"),
            "transcript-bearing entries are not replayable"
        );
        assert!(
            matches!(
                existing_command_reservation(&state, "transcript", digest),
                Some(CommandReservation::Expired)
            ),
            "identical digest re-admission resolves to exactly Expired"
        );
        assert!(
            matches!(
                existing_command_reservation(&state, "transcript", [8; 32]),
                Some(CommandReservation::Conflict)
            ),
            "a different digest still conflicts"
        );
    }

    /// End-to-end: re-dispatching the SAME transcript command (identical
    /// id, digest, and FullSnapshot response mode) after its transcript
    /// ledger expiry fails with the exact `Expired` cause — never accepted,
    /// never replayed, never conflicted.
    #[tokio::test]
    async fn transcript_ledger_re_admit_identical_command_digest_is_expired_not_conflict() {
        let (manager, _registration) = manager_with(None).await;
        let session_id = manager.primary_session_id();
        let command = || RpcCommand::GetMessages {
            id: Some("transcript-once".into()),
        };
        let first = manager
            .dispatch(command(), Some(session_id.clone()))
            .await;
        assert!(first.success, "{}", first.error.unwrap_or_default());
        let replay = manager.dispatch(command(), Some(session_id)).await;
        assert!(!replay.success, "the transcript entry must not be replayed");
        let error = replay.error.unwrap();
        assert!(error.contains("expired"), "{error}");
        assert!(
            !error.contains("different request"),
            "an identical digest is never a conflict: {error}"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn overloaded_control_read_does_not_reserve_command_id() {
        let (manager, _primary_registration) = manager_with(None).await;
        let session_id = manager.primary_session_id();
        let held_slots = (0..MAX_CONCURRENT_SESSION_COMMANDS)
            .map(|_| {
                manager
                    .global_slots
                    .clone()
                    .try_acquire_owned()
                    .expect("global slot")
            })
            .collect::<Vec<_>>();

        let rejected = manager
            .dispatch(
                RpcCommand::GetState {
                    id: Some("control-retry".into()),
                },
                Some(session_id.clone()),
            )
            .await;
        assert!(!rejected.success);
        assert!(
            rejected
                .error
                .as_deref()
                .is_some_and(|error| error.contains("across sessions")),
            "unexpected overload response: {}",
            rejected.error.unwrap_or_default(),
        );
        assert_eq!(
            rejected.data.as_ref().and_then(|data| data.get("code")),
            Some(&json!("busy_capacity")),
        );

        drop(held_slots);
        let retried = manager
            .dispatch(
                RpcCommand::GetState {
                    id: Some("control-retry".into()),
                },
                Some(session_id),
            )
            .await;
        assert!(retried.success, "{}", retried.error.unwrap_or_default());
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn per_session_overload_does_not_reserve_command_id() {
        let (manager, _primary_registration) = manager_with(None).await;
        let session_id = manager.primary_session_id();
        let runtime = manager.resolve(Some(&session_id)).await.expect("runtime");
        let held_slots = runtime.dispatcher.acquire_all_command_slots_for_test();

        let rejected = manager
            .dispatch(
                RpcCommand::GetState {
                    id: Some("session-control-retry".into()),
                },
                Some(session_id.clone()),
            )
            .await;
        assert!(!rejected.success);
        assert!(
            rejected.error.as_deref().is_some_and(|error| {
                error.starts_with("too many concurrent RPC commands (limit ")
            }),
            "unexpected overload response: {}",
            rejected.error.unwrap_or_default(),
        );
        assert_eq!(
            rejected.data.as_ref().and_then(|data| data.get("code")),
            Some(&json!("busy_capacity")),
        );

        drop(held_slots);
        let retried = manager
            .dispatch(
                RpcCommand::GetState {
                    id: Some("session-control-retry".into()),
                },
                Some(session_id),
            )
            .await;
        assert!(retried.success, "{}", retried.error.unwrap_or_default());
        manager.shutdown().await;
    }
    #[tokio::test]
    async fn session_panel_reads_and_new_session_bypass_active_turn() {
        let spawner = Arc::new(TestSpawner::default());
        let (model, _registration) = faux_model("stalled-control-lane");
        let root = tempfile::tempdir().expect("stalled root");
        let cwd = root.path().join("cwd");
        std::fs::create_dir_all(&cwd).expect("stalled cwd");

        let mut options = session_options(model, &cwd);
        options.stream_fn = Some(Arc::new(|_model, _context, _options| {
            Box::pin(async move { pi_ai::new_assistant_message_event_stream() })
        }));
        let session = Session::new_with_additional_tools_filtered_and_discovery(
            options,
            Vec::new(),
            ToolSelection::default(),
            ResourceDiscovery::Disabled,
        )
        .expect("stalled session");
        session
            .attach_resources(isolated_resources(&cwd))
            .await
            .expect("stalled resources");
        session.start_new_recording().expect("stalled recorder");
        let application = isolated_application(session).await;
        let manager =
            SessionRuntimeManager::new(application, ExtensionUiAdapter::default(), Some(spawner), None)
                .await
                .expect("session runtime manager");
        let session_id = manager.primary_session_id();

        let stalled = manager
            .admit(
                RpcCommand::Prompt {
                    id: Some("stalled".into()),
                    message: "hold ordinary FIFO".into(),
                    images: Vec::new(),
                    streaming_behavior: None,
                },
                Some(session_id.clone()),
                true,
            )
            .await
            .wait()
            .await;
        assert!(stalled.success, "{}", stalled.error.unwrap_or_default());

        for command in [
            RpcCommand::GetState {
                id: Some("state".into()),
            },
            RpcCommand::GetSessionStats {
                id: Some("stats".into()),
            },
            RpcCommand::SessionList {
                id: Some("list".into()),
                scope: crate::modes::rpc::RpcSessionListScope::Current,
            },
            RpcCommand::GetForkMessages {
                id: Some("forks".into()),
                session_path: None,
                cursor: None,
                limit: None,
            },
        ] {
            let name = command.command_name();
            let response = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                manager.dispatch(command, Some(session_id.clone())),
            )
            .await
            .unwrap_or_else(|_| panic!("{name} blocked behind the ordinary FIFO"));
            if name != "get_fork_messages" {
                assert!(
                    response.success,
                    "{name}: {}",
                    response.error.unwrap_or_default()
                );
            }
        }

        let opened = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            manager.dispatch(
                RpcCommand::NewSession {
                    id: Some("new".into()),
                    parent_session: None,
                    cwd: None,
                },
                Some(session_id.clone()),
            ),
        )
        .await
        .expect("new_session blocked behind the ordinary FIFO");
        assert!(opened.success, "{}", opened.error.unwrap_or_default());

        let aborted = manager
            .dispatch(
                RpcCommand::Abort {
                    id: Some("abort".into()),
                },
                Some(session_id),
            )
            .await;
        assert!(aborted.success, "{}", aborted.error.unwrap_or_default());
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn commands_and_abort_route_by_session_id() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        // The RPC prompt response is acceptance: the turn runs asynchronously
        // and completes with a message_end event on the owning session.
        let mut events = manager.events();
        let opened = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        let b_id = opened.data.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();

        let b_messages = manager
            .dispatch(RpcCommand::GetMessages { id: None }, Some(b_id.clone()))
            .await;
        assert!(b_messages.success);
        assert_eq!(
            b_messages.data.unwrap()["messages"]
                .as_array()
                .unwrap()
                .len(),
            0
        );

        spawner
            .registration
            .set_responses(vec![FauxResponse::text("b-reply")]);
        manager
            .dispatch(
                RpcCommand::Prompt {
                    id: None,
                    message: "hello b".into(),
                    images: Vec::new(),
                    streaming_behavior: None,
                },
                Some(b_id.clone()),
            )
            .await
            .success_or("prompt on B");
        wait_message_end(&mut events, &b_id).await;

        let b_history = manager
            .dispatch(RpcCommand::GetMessages { id: None }, Some(b_id.clone()))
            .await;
        let b_texts = history_texts(&b_history);
        assert!(
            b_texts.iter().any(|text| text.contains("hello b")),
            "{b_texts:?}"
        );
        assert!(
            b_texts.iter().any(|text| text.contains("b-reply")),
            "{b_texts:?}"
        );

        let a_history = manager
            .dispatch(RpcCommand::GetMessages { id: None }, None)
            .await;
        let a_texts = history_texts(&a_history);
        assert!(
            !a_texts.iter().any(|text| text.contains("hello b")),
            "{a_texts:?}"
        );

        let abort_ok = manager
            .dispatch(RpcCommand::Abort { id: None }, Some(b_id.clone()))
            .await;
        assert!(abort_ok.success);
        let abort_unknown = manager
            .dispatch(RpcCommand::Abort { id: None }, Some("missing".into()))
            .await;
        assert!(!abort_unknown.success);
        assert!(
            abort_unknown
                .error
                .unwrap()
                .contains("unknown session missing")
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn events_carry_owning_session_id_on_fan_in() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let mut events = manager.events();
        let opened = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        let b_id = opened.data.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();

        spawner
            .registration
            .set_responses(vec![FauxResponse::text("event-reply")]);
        manager
            .dispatch(
                RpcCommand::Prompt {
                    id: None,
                    message: "ping".into(),
                    images: Vec::new(),
                    streaming_behavior: None,
                },
                Some(b_id.clone()),
            )
            .await
            .success_or("prompt on B");

        let mut saw_b_tagged = false;
        loop {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .expect("fan-in frame")
                .expect("fan-in open");
            if frame.get("sessionId").and_then(Value::as_str) == Some(b_id.as_str()) {
                saw_b_tagged = true;
                if frame.get("type").and_then(Value::as_str) == Some("message_end") {
                    break;
                }
            }
        }
        assert!(saw_b_tagged);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn recorded_primary_emits_attached_writer_session_id() {
        let (manager, primary_registration) = manager_with(None).await;
        let primary_id = manager.primary_session_id().to_owned();
        let mut events = manager.events();
        primary_registration.set_responses(vec![FauxResponse::text("a-reply")]);
        manager
            .dispatch(
                RpcCommand::Prompt {
                    id: None,
                    message: "ping".into(),
                    images: Vec::new(),
                    streaming_behavior: None,
                },
                None,
            )
            .await
            .success_or("prompt on primary");
        loop {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .expect("fan-in frame")
                .expect("fan-in open");
            if frame.get("type").and_then(Value::as_str) == Some("message_end") {
                assert_eq!(
                    frame.get("sessionId").and_then(Value::as_str),
                    Some(primary_id.as_str()),
                    "recorded primary uses its attached writer session id: {frame}"
                );
                break;
            }
        }
        manager.shutdown().await;
    }
    #[tokio::test]

    async fn loaded_session_cap_retires_lru_idle_runtime() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let mut opened_ids = Vec::new();
        for _ in 0..(MAX_LOADED_SESSIONS - 1) {
            let response = manager
                .dispatch(
                    RpcCommand::NewSession {
                        id: None,
                        parent_session: None,
                        cwd: None,
                    },
                    None,
                )
                .await;
            assert!(
                response.success,
                "open within cap: {}",
                response.error.unwrap_or_default()
            );
            opened_ids.push(
                response.data.unwrap()["sessionId"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
        }

        // The oldest opened session is the least-recently-used: opening one
        // more must retire exactly it and succeed without user action.
        let overflow = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(
            overflow.success,
            "LRU retirement must satisfy the open: {}",
            overflow.error.unwrap_or_default()
        );
        let retired = &opened_ids[0];
        let gone = manager
            .dispatch(RpcCommand::GetState { id: None }, Some(retired.clone()))
            .await;
        assert!(!gone.success, "the LRU victim must be retired");
        assert!(gone.error.unwrap().contains("unknown session"));

        // Every other runtime stays loaded and actionable.
        for id in opened_ids.iter().skip(1) {
            let state = manager
                .dispatch(RpcCommand::GetState { id: None }, Some(id.clone()))
                .await;
            assert!(state.success, "{} must survive LRU retirement", id);
        }
        assert_eq!(spawner.spawns.load(Ordering::SeqCst), MAX_LOADED_SESSIONS);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn lru_reuse_touch_refreshes_recency() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let mut opened_ids = Vec::new();
        for _ in 0..(MAX_LOADED_SESSIONS - 1) {
            let response = manager
                .dispatch(
                    RpcCommand::NewSession {
                        id: None,
                        parent_session: None,
                        cwd: None,
                    },
                    None,
                )
                .await;
            assert!(response.success);
            opened_ids.push(
                response.data.unwrap()["sessionId"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
        }

        // GetState is IsolatedApplication housekeeping and must not restamp
        // LRU recency (`reconnect_job_list_does_not_refresh_lru_recency`).
        // A recency-classified command is the user-facing touch.
        let touched = manager
            .dispatch(
                RpcCommand::SetThinkingLevel {
                    id: None,
                    level: ThinkingLevel::Off,
                },
                Some(opened_ids[0].clone()),
            )
            .await;
        assert!(touched.success, "{}", touched.error.unwrap_or_default());

        let overflow = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(overflow.success, "{}", overflow.error.unwrap_or_default());
        let survivor = manager
            .dispatch(
                RpcCommand::GetState { id: None },
                Some(opened_ids[0].clone()),
            )
            .await;
        assert!(
            survivor.success,
            "a recently used runtime must never be the LRU victim"
        );
        let retired = manager
            .dispatch(
                RpcCommand::GetState { id: None },
                Some(opened_ids[1].clone()),
            )
            .await;
        assert!(
            !retired.success,
            "the untouched next-oldest runtime must be retired"
        );
        manager.shutdown().await;
    }

    /// /web regression: a thinking level chosen on a loaded session must
    /// survive the runtime being unloaded (closed) and reopened through the
    /// production switch path. Mirrors the browser flow: click a saved
    /// session, adjust the level, the runtime later unloads, the sidebar
    /// reopens the same file — the select must not fall back to the default.
    #[tokio::test]
    async fn web_thinking_level_change_survives_unload_and_reopen() {
        let (native, project, _service, spawner) = production_manager_spawner();
        let suffix = uuid::Uuid::now_v7().simple();
        let mut model = Model::default();
        model.id = "web-thinking-persist-model".to_owned();
        model.name = "Web Thinking Persist".to_owned();
        model.api = format!("web-thinking-persist-{suffix}-api");
        model.provider = format!("web-thinking-persist-{suffix}-provider");
        model.base_url = "http://localhost:0".to_owned();
        model.reasoning = true;
        let _registration = register_faux_provider(FauxProviderOptions {
            api: model.api.clone(),
            provider: model.provider.clone(),
            models: vec![model.clone()],
            chunk_size: 8,
        });
        let recorder = pi_coding::start_session_in(
            project.path(),
            Some(&model),
            Some("off"),
            Some(native.path()),
            None,
            None,
        )
        .expect("saved session");
        recorder
            .record_message(&Message::user_text("web thinking flow", 0))
            .expect("record user message");
        recorder.persist_now().expect("persist saved session");
        let session_path = recorder.path().to_string_lossy().into_owned();
        drop(recorder);

        let dir = tempfile::tempdir().expect("primary dir");
        let primary_session = isolated_session_with_auth_owner(model.clone(), dir.path(), true).await;
        let primary_dir = primary_session.session_dir();
        let primary_recorder = pi_coding::start_session_in(
            &primary_dir,
            Some(&model),
            Some("off"),
            Some(dir.path()),
            None,
            None,
        )
        .expect("primary recording");
        primary_recorder.persist_now().expect("persist primary header");
        primary_session
            .record(primary_recorder)
            .expect("record primary");
        let primary_application = isolated_application(primary_session).await;
        let manager = SessionRuntimeManager::new(
            primary_application,
            ExtensionUiAdapter::default(),
            Some(Arc::new(spawner)),
            None,
        )
        .await
        .expect("production manager");

        let switch = manager
            .dispatch(
                RpcCommand::SwitchSession {
                    id: Some("open".into()),
                    session_path: session_path.clone(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                None,
            )
            .await;
        assert!(switch.success, "{}", switch.error.unwrap_or_default());
        let sid = switch
            .data
            .as_ref()
            .and_then(|data| data["sessionId"].as_str())
            .unwrap_or_else(|| panic!("switch response must name the session: {switch:?}"))
            .to_owned();

        let set = manager
            .dispatch(
                RpcCommand::SetThinkingLevel {
                    id: Some("set".into()),
                    level: ThinkingLevel::High,
                },
                Some(sid.clone()),
            )
            .await;
        assert!(set.success, "{}", set.error.unwrap_or_default());
        assert_eq!(set.data.as_ref().unwrap()["level"], "high");

        let state = manager
            .dispatch(RpcCommand::GetState { id: Some("state".into()) }, Some(sid.clone()))
            .await;
        assert!(state.success, "{}", state.error.unwrap_or_default());
        assert_eq!(state.data.as_ref().unwrap()["thinkingLevel"], "high");

        let closed = manager
            .dispatch(
                RpcCommand::CloseSession { id: Some("close".into()) },
                Some(sid.clone()),
            )
            .await;
        let reopen = manager
            .dispatch(
                RpcCommand::SwitchSession {
                    id: Some("reopen".into()),
                    session_path,
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                None,
            )
            .await;
        assert!(reopen.success, "{}", reopen.error.unwrap_or_default());

        let reopened = manager
            .dispatch(RpcCommand::GetState { id: Some("reopened".into()) }, Some(sid))
            .await;
        assert!(reopened.success, "{}", reopened.error.unwrap_or_default());
        assert_eq!(
            reopened.data.as_ref().unwrap()["thinkingLevel"],
            "high",
            "thinking level set on a /web session must survive unload→reload"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn sequential_switch_beyond_cap_evicts_lru_and_reuses_recent() {
        // Unlimited-session regression at the raised cap: opening/switching
        // through more distinct saved sessions than [`MAX_LOADED_SESSIONS`]
        // must succeed on every open with no manual close, keep the registry
        // bounded at the cap, retire exactly the least-recently-used idle
        // runtimes in open order, and reuse the most recently used runtime
        // without a respawn.
        let dir = tempfile::tempdir().expect("saved sessions dir");
        let (model, _registration) = faux_model("beyond-cap");
        let mut session_paths = Vec::new();
        let open_count = MAX_LOADED_SESSIONS + 8;
        for index in 0..open_count {
            let recorder = pi_coding::start_session_in(
                dir.path(),
                Some(&model),
                Some("off"),
                Some(dir.path()),
                Some(format!("beyond-cap-{index}").as_str()),
                None,
            )
            .expect("saved session");
            recorder
                .persist_now_durable()
                .expect("persist saved session");
            session_paths.push(recorder.path());
            drop(recorder);
        }

        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let switch = |path: &Path| {
            let manager = manager.clone();
            let session_path = path.to_string_lossy().into_owned();
            async move {
                manager
                    .dispatch(
                        RpcCommand::SwitchSession {
                            id: None,
                            session_path,
                            catalog_source: None,
                            catalog_session_id: None,
                            catalog_status: None,
                            response_mode: None,
                        },
                        None,
                    )
                    .await
            }
        };

        let mut opened_ids = Vec::new();
        for (index, path) in session_paths.iter().enumerate() {
            let response = switch(path).await;
            assert!(
                response.success,
                "open {index} beyond cap must succeed: {}",
                response.error.unwrap_or_default()
            );
            opened_ids.push(
                response.data.unwrap()["sessionId"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
            let registry = manager.registry.read().await;
            assert!(
                registry.distinct_runtimes().len() <= MAX_LOADED_SESSIONS,
                "registry must stay capped at {MAX_LOADED_SESSIONS} after open {index}"
            );
            drop(registry);
        }
        assert_eq!(spawner.spawns.load(Ordering::SeqCst), open_count);

        // Secondaries fit beside the primary; the remaining opens retired
        // exactly the least-recently-used runtimes in open order.
        let survivor_count = MAX_LOADED_SESSIONS - 1;
        let retired_count = open_count - survivor_count;
        for id in opened_ids.iter().take(retired_count) {
            let gone = manager
                .dispatch(RpcCommand::GetState { id: None }, Some(id.clone()))
                .await;
            assert!(!gone.success, "the LRU victim {id} must be retired");
            assert!(gone.error.unwrap().contains("unknown session"));
        }
        for id in opened_ids.iter().skip(retired_count) {
            let state = manager
                .dispatch(RpcCommand::GetState { id: None }, Some(id.clone()))
                .await;
            assert!(state.success, "recent runtime {id} must stay loaded");
        }
        let registry = manager.registry.read().await;
        assert_eq!(registry.distinct_runtimes().len(), MAX_LOADED_SESSIONS);
        drop(registry);
        assert_eq!(manager.loaded_slots.available_permits(), 0);

        // The most recently opened target is still loaded: switching back to
        // it must reuse its runtime instead of spawning a replacement.
        let last = open_count - 1;
        let reused = switch(&session_paths[last]).await;
        assert!(
            reused.success,
            "recent runtime must be reused: {}",
            reused.error.unwrap_or_default()
        );
        assert_eq!(
            reused.data.unwrap()["sessionId"],
            opened_ids[last].as_str(),
            "reuse must return the already-loaded runtime"
        );
        assert_eq!(
            spawner.spawns.load(Ordering::SeqCst),
            open_count,
            "a loaded target must never spawn a second runtime"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn switch_reuses_already_loaded_target_without_spawn() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let first = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(first.success);
        let b_id = first.data.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();

        // Re-selecting an already-loaded runtime (via its state round-trip)
        // neither spawns nor evicts anything.
        let before = spawner.spawns.load(Ordering::SeqCst);
        let again = manager
            .dispatch(RpcCommand::GetState { id: None }, Some(b_id.clone()))
            .await;
        assert!(again.success);
        assert_eq!(spawner.spawns.load(Ordering::SeqCst), before);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn active_goal_runtime_is_protected_from_lru_retirement() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let mut opened_ids = Vec::new();
        for index in 0..(MAX_LOADED_SESSIONS - 1) {
            let response = manager
                .dispatch(
                    RpcCommand::NewSession {
                        id: Some(format!("active-goal-{index}")),
                        parent_session: None,
                        cwd: None,
                    },
                    None,
                )
                .await;
            assert!(response.success, "{}", response.error.unwrap_or_default());
            opened_ids.push(
                response.data.unwrap()["sessionId"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
        }
        let protected = manager
            .resolve(Some(&opened_ids[0]))
            .await
            .expect("protected runtime");
        protected
            .application
            .goal_create("active goal", None)
            .expect("create active goal");

        let overflow = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(overflow.success, "{}", overflow.error.unwrap_or_default());
        assert!(
            manager.resolve(Some(&opened_ids[0])).await.is_ok(),
            "active goal survives"
        );
        assert!(
            manager.resolve(Some(&opened_ids[1])).await.is_err(),
            "oldest idle peer retires"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn completed_goal_runtimes_remain_lru_eligible() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let mut opened_ids = Vec::new();
        for index in 0..(MAX_LOADED_SESSIONS - 1) {
            let response = manager
                .dispatch(
                    RpcCommand::NewSession {
                        id: Some(format!("completed-goal-{index}")),
                        parent_session: None,
                        cwd: None,
                    },
                    None,
                )
                .await;
            assert!(response.success, "{}", response.error.unwrap_or_default());
            let session_id = response.data.unwrap()["sessionId"]
                .as_str()
                .unwrap()
                .to_owned();
            let runtime = manager
                .resolve(Some(&session_id))
                .await
                .expect("completed runtime");
            runtime
                .application
                .goal_create("completed goal", None)
                .expect("create goal");
            runtime.application.goal_complete().expect("complete goal");
            opened_ids.push(session_id);
        }

        let overflow = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(
            overflow.success,
            "terminal goals must not exhaust capacity: {}",
            overflow.error.unwrap_or_default()
        );
        assert!(
            manager.resolve(Some(&opened_ids[0])).await.is_err(),
            "completed LRU retires"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn subscription_snapshot_does_not_refresh_lru_recency() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let mut opened_ids = Vec::new();
        for index in 0..(MAX_LOADED_SESSIONS - 1) {
            let response = manager
                .dispatch(
                    RpcCommand::NewSession {
                        id: Some(format!("snapshot-lru-{index}")),
                        parent_session: None,
                        cwd: None,
                    },
                    None,
                )
                .await;
            assert!(response.success, "{}", response.error.unwrap_or_default());
            opened_ids.push(
                response.data.unwrap()["sessionId"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
        }
        let oldest = opened_ids[0].clone();
        let newest = opened_ids[opened_ids.len() - 1].clone();
        let newest_runtime = manager
            .resolve(Some(&newest))
            .await
            .expect("newest runtime");
        manager.mark_runtime_used(&newest_runtime);
        let oldest_runtime = manager
            .resolve(Some(&oldest))
            .await
            .expect("oldest runtime");
        let oldest_usage = oldest_runtime.last_used();
        manager
            .subscription_snapshot(&oldest, 0)
            .await
            .expect("reconnect snapshot");
        assert_eq!(
            oldest_runtime.last_used(),
            oldest_usage,
            "subscribe snapshots must not restamp IsolatedApplication LRU recency"
        );

        let overflow = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(overflow.success, "{}", overflow.error.unwrap_or_default());
        assert!(
            manager.resolve(Some(&oldest)).await.is_err(),
            "oldest idle still retires after a reconnect snapshot"
        );
        assert!(
            manager.resolve(Some(&newest)).await.is_ok(),
            "recently used idle survives IsolatedApplication LRU"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn reconnect_job_list_does_not_refresh_lru_recency() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let mut opened_ids = Vec::new();
        for index in 0..(MAX_LOADED_SESSIONS - 1) {
            let response = manager
                .dispatch(
                    RpcCommand::NewSession {
                        id: Some(format!("job-list-lru-{index}")),
                        parent_session: None,
                        cwd: None,
                    },
                    None,
                )
                .await;
            assert!(response.success, "{}", response.error.unwrap_or_default());
            opened_ids.push(
                response.data.unwrap()["sessionId"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
        }
        let oldest = opened_ids[0].clone();
        let newest = opened_ids[opened_ids.len() - 1].clone();
        let newest_runtime = manager
            .resolve(Some(&newest))
            .await
            .expect("newest runtime");
        manager.mark_runtime_used(&newest_runtime);
        let oldest_runtime = manager
            .resolve(Some(&oldest))
            .await
            .expect("oldest runtime");
        let state_before = oldest_runtime.last_used();
        assert!(
            manager
                .dispatch(RpcCommand::GetState { id: None }, Some(oldest.clone()))
                .await
                .success
        );
        assert!(
            manager
                .dispatch(RpcCommand::JobList { id: None }, Some(oldest.clone()))
                .await
                .success
        );
        assert_eq!(
            oldest_runtime.last_used(),
            state_before,
            "reconnect job_list and get_state must not restamp IsolatedApplication LRU recency"
        );

        let overflow = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(overflow.success, "{}", overflow.error.unwrap_or_default());
        assert!(
            manager.resolve(Some(&oldest)).await.is_err(),
            "oldest idle still retires after reconnect job_list"
        );
        assert!(
            manager.resolve(Some(&newest)).await.is_ok(),
            "recently used idle survives IsolatedApplication LRU"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn dropped_goal_runtimes_remain_lru_eligible() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let mut opened_ids = Vec::new();
        for index in 0..(MAX_LOADED_SESSIONS - 1) {
            let response = manager
                .dispatch(
                    RpcCommand::NewSession {
                        id: Some(format!("dropped-goal-{index}")),
                        parent_session: None,
                        cwd: None,
                    },
                    None,
                )
                .await;
            assert!(response.success, "{}", response.error.unwrap_or_default());
            let session_id = response.data.unwrap()["sessionId"]
                .as_str()
                .unwrap()
                .to_owned();
            let runtime = manager
                .resolve(Some(&session_id))
                .await
                .expect("dropped runtime");
            runtime
                .application
                .goal_create("dropped goal", None)
                .expect("create goal");
            runtime.application.goal_drop().expect("drop goal");
            opened_ids.push(session_id);
        }

        let overflow = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(
            overflow.success,
            "terminal goals must not exhaust capacity: {}",
            overflow.error.unwrap_or_default()
        );
        assert!(
            manager.resolve(Some(&opened_ids[0])).await.is_err(),
            "dropped LRU retires"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn in_progress_todo_protects_runtime_from_lru_retirement() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let mut opened_ids = Vec::new();
        for index in 0..(MAX_LOADED_SESSIONS - 1) {
            let response = manager
                .dispatch(
                    RpcCommand::NewSession {
                        id: Some(format!("todo-protect-{index}")),
                        parent_session: None,
                        cwd: None,
                    },
                    None,
                )
                .await;
            assert!(response.success, "{}", response.error.unwrap_or_default());
            opened_ids.push(
                response.data.unwrap()["sessionId"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
        }
        let protected_id = opened_ids[0].clone();
        let protected = manager
            .resolve(Some(&protected_id))
            .await
            .expect("protected runtime");
        protected
            .application
            .set_todos(vec![pi_coding::TodoPhase {
                name: "Work".to_owned(),
                tasks: vec![pi_coding::TodoItem {
                    id: "task-1".to_owned(),
                    content: "active task".to_owned(),
                    status: pi_coding::TodoStatus::InProgress,
                    depends_on: Vec::new(),
                    ready: true,
                    waiting_on: Vec::new(),
                    agent: None,
                }],
            }])
            .expect("set in-progress todo");

        let overflow = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(overflow.success, "{}", overflow.error.unwrap_or_default());
        assert!(
            manager.resolve(Some(&protected_id)).await.is_ok(),
            "in-progress todo survives"
        );
        assert!(
            manager.resolve(Some(&opened_ids[1])).await.is_err(),
            "oldest idle peer retires"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn busy_runtimes_are_never_retired_and_all_busy_fails_closed() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let mut opened_ids = Vec::new();
        for _ in 0..(MAX_LOADED_SESSIONS - 1) {
            let response = manager
                .dispatch(
                    RpcCommand::NewSession {
                        id: None,
                        parent_session: None,
                        cwd: None,
                    },
                    None,
                )
                .await;
            assert!(response.success);
            opened_ids.push(
                response.data.unwrap()["sessionId"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
        }

        // Streaming every secondary at once cannot saturate the 128-slot
        // pool: ordinary commands share [`MAX_CONCURRENT_SESSION_COMMANDS`]
        // (32) permits, so the 16-slot streaming harness is stale. In-progress
        // todos are the same `ensure_idle` busy signal used by
        // `in_progress_todo_protects_runtime_from_lru_retirement`.
        for id in &opened_ids {
            let runtime = manager
                .resolve(Some(id))
                .await
                .expect("loaded runtime");
            runtime
                .application
                .set_todos(vec![pi_coding::TodoPhase {
                    name: "Work".to_owned(),
                    tasks: vec![pi_coding::TodoItem {
                        id: "task-1".to_owned(),
                        content: "active task".to_owned(),
                        status: pi_coding::TodoStatus::InProgress,
                        depends_on: Vec::new(),
                        ready: true,
                        waiting_on: Vec::new(),
                        agent: None,
                    }],
                }])
                .expect("set in-progress todo");
        }

        let rejected = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(!rejected.success, "all-busy pools must fail closed");
        let error = rejected.error.unwrap();
        assert!(error.contains("too many busy sessions"), "{error}");
        assert!(error.contains(&MAX_LOADED_SESSIONS.to_string()), "{error}");

        for id in &opened_ids {
            assert!(
                manager.resolve(Some(id)).await.is_ok(),
                "a busy runtime must never be the LRU victim: {id}"
            );
        }
        manager.shutdown().await;
    }
    #[tokio::test]
    async fn concurrent_switches_do_not_over_evict() {
        let spawner = Arc::new(GatedSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let mut opened_ids = Vec::new();
        for index in 0..(MAX_LOADED_SESSIONS - 1) {
            let open = tokio::spawn({
                let manager = manager.clone();
                async move {
                    manager
                        .dispatch(
                            RpcCommand::NewSession {
                                id: Some(format!("seed-{index}")),
                                parent_session: None,
                                cwd: None,
                            },
                            None,
                        )
                        .await
                }
            });
            tokio::time::timeout(Duration::from_secs(1), spawner.started.acquire())
                .await
                .expect("seed spawn reaches factory")
                .expect("seed spawn signal")
                .forget();
            spawner.release.add_permits(1);
            let response = open.await.expect("seed open task");
            assert!(response.success, "{}", response.error.unwrap_or_default());
            opened_ids.push(
                response.data.unwrap()["sessionId"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
        }
        let mut original_runtimes = Vec::new();
        for session_id in &opened_ids {
            original_runtimes.push(
                manager
                    .resolve(Some(session_id))
                    .await
                    .expect("seed runtime"),
            );
        }
        let recent_id = opened_ids[0].clone();
        assert!(
            manager
                .dispatch(RpcCommand::GetState { id: None }, Some(recent_id.clone()))
                .await
                .success
        );
        manager.mark_runtime_used(&original_runtimes[0]);
        let recent_usage = original_runtimes[0].last_used();
        assert!(
            original_runtimes
                .iter()
                .skip(1)
                .all(|runtime| runtime.last_used() < recent_usage),
            "recent runtime has the newest usage sequence"
        );
        let recent_lifecycle = original_runtimes[0]
            .lifecycle_lock
            .clone()
            .read_owned()
            .await;

        let mut tasks = Vec::new();
        for index in 0..4 {
            tasks.push(tokio::spawn({
                let manager = manager.clone();
                async move {
                    manager
                        .dispatch(
                            RpcCommand::NewSession {
                                id: Some(format!("burst-{index}")),
                                parent_session: None,
                                cwd: None,
                            },
                            None,
                        )
                        .await
                }
            }));
        }
        for _ in 0..4 {
            tokio::time::timeout(Duration::from_secs(1), spawner.started.acquire())
                .await
                .expect("burst spawn reaches factory")
                .expect("burst spawn signal")
                .forget();
        }
        spawner.release.add_permits(4);

        let mut replacement_ids = HashSet::new();
        for task in tasks {
            let response = tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .expect("burst task")
                .expect("burst join");
            assert!(response.success, "{}", response.error.unwrap_or_default());
            replacement_ids.insert(
                response.data.unwrap()["sessionId"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
        }
        assert_eq!(
            replacement_ids.len(),
            4,
            "every burst open owns a distinct runtime"
        );
        assert!(
            manager.resolve(Some(&recent_id)).await.is_ok(),
            "recent runtime survives"
        );
        assert!(
            manager
                .resolve(Some(manager.primary.protocol_id()))
                .await
                .is_ok()
        );
        let registry = manager.registry.read().await;
        let retired_runtimes = original_runtimes
            .iter()
            .filter(|runtime| !registry.contains(runtime))
            .map(Arc::as_ptr)
            .collect::<HashSet<_>>();
        assert_eq!(
            retired_runtimes.len(),
            4,
            "four distinct eligible runtimes retire"
        );
        assert!(
            registry.contains(&original_runtimes[0]),
            "recent runtime survives"
        );
        assert_eq!(registry.distinct_runtimes().len(), MAX_LOADED_SESSIONS);
        drop(registry);
        assert_eq!(manager.loaded_slots.available_permits(), 0);
        drop(recent_lifecycle);
        manager.shutdown().await;
    }
    #[tokio::test]
    async fn replacement_registry_cut_never_exposes_retired_runtime() {
        let spawner = Arc::new(GatedSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let mut opened_ids = Vec::new();
        for index in 0..(MAX_LOADED_SESSIONS - 1) {
            let open = tokio::spawn({
                let manager = manager.clone();
                async move {
                    manager
                        .dispatch(
                            RpcCommand::NewSession {
                                id: Some(format!("atomic-seed-{index}")),
                                parent_session: None,
                                cwd: None,
                            },
                            None,
                        )
                        .await
                }
            });
            spawner
                .started
                .acquire()
                .await
                .expect("seed start")
                .forget();
            spawner.release.add_permits(1);
            let response = open.await.expect("seed task");
            assert!(response.success, "{}", response.error.unwrap_or_default());
            opened_ids.push(
                response.data.unwrap()["sessionId"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
            spawner
                .completed
                .acquire()
                .await
                .expect("seed completion")
                .forget();
        }
        let victim = manager.resolve(Some(&opened_ids[0])).await.expect("victim");
        let registry_before = manager.registry.read().await;
        assert!(registry_before.contains(&victim));

        let replacement = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .dispatch(
                        RpcCommand::NewSession {
                            id: Some("atomic-replacement".into()),
                            parent_session: None,
                            cwd: None,
                        },
                        None,
                    )
                    .await
            }
        });
        spawner
            .started
            .acquire()
            .await
            .expect("replacement start")
            .forget();
        spawner.release.add_permits(1);
        spawner
            .completed
            .acquire()
            .await
            .expect("replacement built")
            .forget();

        tokio::time::timeout(Duration::from_secs(1), async {
            while manager.registry.try_read().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("replacement reaches the registry cut");
        assert!(
            registry_before.contains(&victim),
            "reader sees the complete pre-cut registry"
        );
        assert!(!registry_before.by_id.contains_key("atomic-replacement"));
        drop(registry_before);

        let response = replacement.await.expect("replacement task");
        assert!(response.success, "{}", response.error.unwrap_or_default());
        let replacement_id = response.data.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        let registry_after = manager.registry.read().await;
        assert!(
            !registry_after.contains(&victim),
            "retired runtime is absent after the cut"
        );
        assert!(registry_after.by_id.contains_key(&replacement_id));
        assert_eq!(
            registry_after.distinct_runtimes().len(),
            MAX_LOADED_SESSIONS
        );
        drop(registry_after);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn aborted_lru_open_finishes_replacement_without_losing_capacity() {
        let spawner = Arc::new(GatedSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let mut opened_ids = Vec::new();
        for index in 0..(MAX_LOADED_SESSIONS - 1) {
            let open = tokio::spawn({
                let manager = manager.clone();
                async move {
                    manager
                        .dispatch(
                            RpcCommand::NewSession {
                                id: Some(format!("seed-{index}")),
                                parent_session: None,
                                cwd: None,
                            },
                            None,
                        )
                        .await
                }
            });
            tokio::time::timeout(Duration::from_secs(1), spawner.started.acquire())
                .await
                .expect("seed spawn reaches factory")
                .expect("seed spawn signal")
                .forget();
            spawner.release.add_permits(1);
            let response = open.await.expect("seed open task");
            assert!(response.success, "{}", response.error.unwrap_or_default());
            opened_ids.push(
                response.data.unwrap()["sessionId"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
        }

        let victim = manager
            .resolve(Some(&opened_ids[0]))
            .await
            .expect("LRU victim before cancelled open");
        let cancelled = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .execute_manager_level(
                        &manager.primary,
                        RpcCommand::NewSession {
                            id: Some("cancelled-open".into()),
                            parent_session: None,
                            cwd: None,
                        },
                        LifecycleResponseMode::FullSnapshot,
                    )
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(1), spawner.started.acquire())
            .await
            .expect("replacement reaches factory")
            .expect("replacement spawn signal")
            .forget();
        cancelled.abort();
        cancelled
            .await
            .expect_err("lifecycle command task is cancelled");
        spawner.release.add_permits(1);

        tokio::time::timeout(Duration::from_secs(5), async {
            while manager.is_registered(&victim).await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("manager-owned replacement commits after command cancellation");
        let retired = manager
            .dispatch(
                RpcCommand::GetState { id: None },
                Some(opened_ids[0].clone()),
            )
            .await;
        assert!(
            !retired.success,
            "the cancelled command cannot strand or restore its victim"
        );
        assert_eq!(manager.loaded_slots.available_permits(), 0);
        manager.shutdown().await;
    }
    #[tokio::test]
    async fn shutdown_waits_for_manager_owned_creation_without_zombie_runtime() {
        let spawner = Arc::new(GatedSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let open = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .dispatch(
                        RpcCommand::NewSession {
                            id: Some("shutdown-race".into()),
                            parent_session: None,
                            cwd: None,
                        },
                        None,
                    )
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(1), spawner.started.acquire())
            .await
            .expect("creation reaches factory")
            .expect("creation start signal")
            .forget();
        let shutdown = tokio::spawn({
            let manager = manager.clone();
            async move { manager.shutdown().await }
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while !manager.lifecycle_tx.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("lifecycle owner processes shutdown");
        let late = tokio::time::timeout(
            Duration::from_secs(1),
            manager.dispatch(
                RpcCommand::NewSession {
                    id: Some("late-shutdown-open".into()),
                    parent_session: None,
                    cwd: None,
                },
                None,
            ),
        )
        .await
        .expect("late creation receives a bounded response");
        assert!(!late.success, "shutdown must reject queued creation");
        assert_eq!(spawner.inner.spawns.load(Ordering::SeqCst), 0);
        spawner.release.add_permits(1);
        tokio::time::timeout(Duration::from_secs(5), shutdown)
            .await
            .expect("shutdown completes")
            .expect("shutdown task");
        let open = tokio::time::timeout(Duration::from_secs(5), open)
            .await
            .expect("raced creation settles")
            .expect("raced creation task");
        assert!(
            !open.success,
            "a creation parked in the factory when shutdown closed the intake must fail closed: {open:?}"
        );

        let registry = manager.registry.read().await;
        let distinct = registry
            .by_id
            .values()
            .map(Arc::as_ptr)
            .collect::<HashSet<_>>()
            .len();
        assert_eq!(distinct, 1, "shutdown must leave only the primary runtime");
        assert!(
            registry
                .by_id
                .values()
                .all(|runtime| Arc::ptr_eq(runtime, &manager.primary))
        );
        drop(registry);
        assert!(manager.lifecycle_tx.is_closed());
    }

    /// Multi-threaded shutdown race: a gated `factory.finish` resumes WHILE
    /// the shutdown's cleanup is in flight. The registration's intake-shut
    /// check and the shutdown's registry retain share one write-lock critical
    /// section, so under ANY interleaving the raced create either registered
    /// while the intake was open (and is aborted/cleaned with the shutdown's
    /// snapshot) or fails closed and the create_runtime failure arm cleans
    /// and discards it. Either way: no registry zombie, and every shutdown
    /// caller replays the SAME terminal outcome.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn finish_resuming_during_shutdown_cleanup_registers_no_zombie() {
        let spawner = Arc::new(GatedSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        // Three concurrent creations park inside factory.finish; the
        // shutdown closes the intake while they are suspended there, and the
        // registrations then race the shutdown's cleanup on the
        // multi-thread runtime.
        let opens: Vec<_> = (0..3)
            .map(|index| {
                let manager = manager.clone();
                tokio::spawn(async move {
                    manager
                        .dispatch(
                            RpcCommand::NewSession {
                                id: Some(format!("finish-resume-race-{index}")),
                                parent_session: None,
                                cwd: None,
                            },
                            None,
                        )
                        .await
                })
            })
            .collect();
        for _ in 0..3 {
            tokio::time::timeout(Duration::from_secs(1), spawner.started.acquire())
                .await
                .expect("creation reaches the gated factory")
                .expect("start permit")
                .forget();
        }
        let shutdown = tokio::spawn({
            let manager = manager.clone();
            async move { manager.shutdown().await }
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while !manager.lifecycle_tx.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("lifecycle owner shuts the intake");
        // Deterministic race orchestration: park the shutdown's registry
        // snapshot AND every resumed registration behind one test-held write
        // gate, then resume `finish`. All three creations complete `finish`
        // and park at their registration while the gate is held — proving
        // `finish` resumed DURING the shutdown body — and every registration
        // then observes the already-shut intake in its write-lock critical
        // section, fail closed.
        let registry_gate = manager.registry.write().await;
        spawner.release.add_permits(3);
        tokio::time::timeout(Duration::from_secs(5), async {
            while spawner.inner.spawns.load(Ordering::SeqCst) < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("all three gated finishes resume and complete under the gate");
        drop(registry_gate);
        tokio::time::timeout(Duration::from_secs(10), shutdown)
            .await
            .expect("shutdown completes")
            .expect("shutdown task")
            .expect("shutdown succeeds");
        for open in opens {
            let response = tokio::time::timeout(Duration::from_secs(5), open)
                .await
                .expect("every raced creation settles");
            match response {
                Ok(response) => {
                    assert!(
                        !response.success,
                        "a creation that lost the shutdown race must fail closed: {response:?}"
                    );
                }
                Err(_) => panic!("raced creation task failed unexpectedly: {response:?}"),
            }
        }
        assert_eq!(
            spawner.inner.spawns.load(Ordering::SeqCst),
            3,
            "finish genuinely resumed during cleanup (not a no-op mirror)"
        );
        // No registry zombie: every entry is the primary, by id AND file.
        let registry = manager.registry.read().await;
        assert!(
            registry
                .by_id
                .values()
                .all(|runtime| Arc::ptr_eq(runtime, &manager.primary)),
            "terminal shutdown leaves no registry zombie"
        );
        assert!(
            registry
                .by_file
                .values()
                .all(|runtime| Arc::ptr_eq(runtime, &manager.primary))
        );
        drop(registry);
        // The one terminal outcome is stored and replayed verbatim by every
        // late caller — never fabricated per caller.
        assert!(manager.shutdown_terminal.borrow().is_some());
        assert!(
            manager.shutdown().await.is_ok(),
            "late caller replays the same terminal outcome"
        );
        assert!(
            manager.shutdown_for_restart().await.is_ok(),
            "replay stays terminal and identical"
        );
    }
    #[tokio::test]
    async fn cancelled_first_shutdown_and_late_caller_share_terminal_completion() {
        let spawner = Arc::new(GatedSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let open = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .dispatch(
                        RpcCommand::NewSession {
                            id: Some("shutdown-cancellation".into()),
                            parent_session: None,
                            cwd: None,
                        },
                        None,
                    )
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(1), spawner.started.acquire())
            .await
            .expect("creation reaches factory")
            .expect("creation start signal")
            .forget();

        let first = tokio::spawn({
            let manager = manager.clone();
            async move { manager.shutdown().await }
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while !manager.lifecycle_tx.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("lifecycle owner closes intake");
        first.abort();
        first.await.expect_err("first shutdown caller is cancelled");

        let late = tokio::spawn({
            let manager = manager.clone();
            async move { manager.shutdown().await }
        });
        assert!(
            !late.is_finished(),
            "late shutdown waits for owner finalization"
        );
        spawner.release.add_permits(1);
        tokio::time::timeout(Duration::from_secs(5), late)
            .await
            .expect("late shutdown completes")
            .expect("late shutdown task");
        let open = tokio::time::timeout(Duration::from_secs(5), open)
            .await
            .expect("raced creation settles")
            .expect("raced creation task");
        assert!(
            !open.success,
            "a creation parked in the factory when shutdown closed the intake must fail closed: {open:?}"
        );

        assert!(
            manager.shutdown_terminal.borrow().is_some(),
            "terminal completion published"
        );
        let registry = manager.registry.read().await;
        assert!(
            registry
                .by_id
                .values()
                .all(|runtime| Arc::ptr_eq(runtime, &manager.primary))
        );
    }

    /// Shutdown must drain/join every manager-owned creation to terminal
    /// completion BEFORE the final runtime snapshot/abort/retain. Here the
    /// creation has ALREADY published its registry entry when the shutdown
    /// begins, but its owner has not started. The shutdown waits for it: the
    /// owner reaches readiness before the terminal snapshot, the created
    /// runtime is captured by that snapshot and cleaned exactly once, and no
    /// owner can start after the terminal outcome. Pre-fix, dropping the
    /// creation JoinSet cancelled the parked creation instead — the caller
    /// observed a fabricated failure for a runtime the snapshot then cleaned.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_joins_inserted_creation_to_owner_ready_and_cleans_once() {
        let spawner = Arc::new(GatedSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let open = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .dispatch(
                        RpcCommand::NewSession {
                            id: Some("inserted-before-shutdown".into()),
                            parent_session: None,
                            cwd: None,
                        },
                        None,
                    )
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(5), spawner.started.acquire())
            .await
            .expect("creation reaches factory")
            .expect("creation start signal")
            .forget();
        // Park the creation between registry publication and owner start.
        let gate = manager.registration_gate.write().await;
        spawner.release.add_permits(1);
        let inserted = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let inserted = manager
                    .registry
                    .read()
                    .await
                    .by_id
                    .values()
                    .find(|runtime| !Arc::ptr_eq(runtime, &manager.primary))
                    .cloned();
                if let Some(inserted) = inserted {
                    break inserted;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("creation inserted its registry entry");
        assert!(
            inserted.owner.lock().expect("owner lock").is_none(),
            "the inserted creation has not started its owner yet"
        );
        let shutdown = tokio::spawn({
            let manager = manager.clone();
            async move { manager.shutdown().await }
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while !manager.lifecycle_tx.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("lifecycle owner closes the intake");
        // While the creation is parked ownerless, the shutdown must be
        // WAITING on it — the pre-fix owner dropped the creation JoinSet and
        // finalized without it.
        let probe = tokio::time::timeout(Duration::from_millis(50), async {
            while !shutdown.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            probe.is_err(),
            "shutdown must wait for the inserted creation instead of dropping it"
        );
        drop(gate);
        let response = tokio::time::timeout(Duration::from_secs(10), open)
            .await
            .expect("creation settles")
            .expect("creation task");
        // The creation reaches its OWN terminal outcome — never a cancelled
        // future's dropped reply. It either published (owner ready before the
        // terminal snapshot) or lost the post-open handoff to the shutdown
        // with a bounded failure; cancellation is never an outcome.
        assert_ne!(
            response.error.as_deref(),
            Some("session runtime creation task stopped"),
            "the inserted creation must not be cancelled by the shutdown: {response:?}"
        );
        tokio::time::timeout(Duration::from_secs(5), shutdown)
            .await
            .expect("shutdown completes")
            .expect("shutdown task")
            .expect("shutdown succeeds");
        // The created runtime was captured by the terminal snapshot and
        // cleaned exactly once; its owner was aborted and can never start
        // after the terminal outcome.
        assert_eq!(
            inserted.cleanup_count.load(Ordering::SeqCst),
            1,
            "shutdown cleans the created runtime exactly once"
        );
        assert!(
            inserted.owner.lock().expect("owner lock").is_none(),
            "the created runtime's owner was aborted by the terminal shutdown"
        );
        let registry = manager.registry.read().await;
        assert!(
            registry
                .by_id
                .values()
                .all(|runtime| Arc::ptr_eq(runtime, &manager.primary)),
            "registry holds only the primary after terminal shutdown"
        );
        assert!(
            registry
                .by_file
                .values()
                .all(|runtime| Arc::ptr_eq(runtime, &manager.primary))
        );
        drop(registry);
        assert!(manager.shutdown_terminal.borrow().is_some());
        assert!(
            manager.shutdown().await.is_ok(),
            "late caller replays the same terminal outcome"
        );
    }

    /// A creation racing the shutdown AT REGISTRATION loses: the intake is
    /// already shut, the registration fails closed inside its write-lock
    /// critical section, and the create failure arm runs the explicit
    /// cleanup and REMOVES the created journal. Pre-fix, the shutdown's
    /// snapshot/retain raced ahead and dropped the creation JoinSet,
    /// cancelling the registration mid-flight and leaking the created
    /// journal on disk.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_raced_failed_registration_removes_created_journal() {
        let spawner = Arc::new(GatedSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let open = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .dispatch(
                        RpcCommand::NewSession {
                            id: Some("journal-race".into()),
                            parent_session: None,
                            cwd: None,
                        },
                        None,
                    )
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(5), spawner.started.acquire())
            .await
            .expect("creation reaches factory")
            .expect("creation start signal")
            .forget();
        // Hold the registry write gate, then close the intake via the
        // shutdown BEFORE resuming `factory.finish`: the gate serializes the
        // creation's registration read behind the shutdown's pre-fix snapshot
        // read, so under ANY interleaving the raced registration finds the
        // intake already shut and fails closed in its write-lock critical
        // section.
        let registry_gate = manager.registry.write().await;
        let shutdown = tokio::spawn({
            let manager = manager.clone();
            async move { manager.shutdown().await }
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while !manager.lifecycle_tx.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("lifecycle owner closes the intake");
        // Resume `factory.finish`: the created journal now EXISTS while the
        // shutdown is in flight, and the registration parks on the held gate
        // — the shutdown must drain this creation to its fail-closed terminal
        // cleanup instead of dropping it.
        spawner.release.add_permits(1);
        let journal = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(path) = spawner
                    .spawned_files
                    .lock()
                    .expect("gated spawner file log")
                    .last()
                    .cloned()
                {
                    break path;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("factory.finish created the journal");
        assert!(journal.is_file(), "created journal exists: {}", journal.display());
        drop(registry_gate);
        let response = tokio::time::timeout(Duration::from_secs(10), open)
            .await
            .expect("raced creation settles")
            .expect("raced creation task");
        assert!(
            !response.success,
            "a creation that lost the shutdown race at registration must fail closed: {response:?}"
        );
        tokio::time::timeout(Duration::from_secs(10), shutdown)
            .await
            .expect("shutdown completes")
            .expect("shutdown task")
            .expect("shutdown succeeds");
        assert!(
            !journal.exists(),
            "the shutdown-raced failed registration removed the created journal: {}",
            journal.display()
        );
        let registry = manager.registry.read().await;
        assert!(
            registry
                .by_id
                .values()
                .all(|runtime| Arc::ptr_eq(runtime, &manager.primary)),
            "registry holds only the primary after terminal shutdown"
        );
        assert!(
            registry
                .by_file
                .values()
                .all(|runtime| Arc::ptr_eq(runtime, &manager.primary))
        );
        drop(registry);
        assert!(manager.shutdown_terminal.borrow().is_some());
        assert!(
            manager.shutdown().await.is_ok(),
            "late caller replays the same terminal outcome"
        );
        assert!(
            manager.shutdown_for_restart().await.is_ok(),
            "replay stays terminal and identical"
        );
    }

    #[tokio::test]
    async fn terminal_shutdown_failure_is_stored_and_replayed_verbatim() {
        let (manager, _primary_registration) =
            manager_with(Some(Arc::new(TestSpawner::default()))).await;
        let opened = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(opened.success, "{}", opened.error.unwrap_or_default());
        let session_id = opened.data.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        let secondary = manager
            .resolve(Some(&session_id))
            .await
            .expect("secondary runtime");
        manager
            .shutdown_checkpoint_failure
            .store(true, Ordering::SeqCst);
        let first = tokio::spawn({
            let manager = manager.clone();
            async move { manager.shutdown_for_restart().await }
        });
        // A concurrent caller joins while the owner is finalizing the first
        // Shutdown: its own reply channel is dropped or its send hits the
        // closed command channel — either way it must block on and replay the
        // ONE stored terminal result, never fabricate its own outcome.
        let concurrent = tokio::spawn({
            let manager = manager.clone();
            async move { manager.shutdown_for_restart().await }
        });
        let first_failure = tokio::time::timeout(Duration::from_secs(5), first)
            .await
            .expect("first shutdown completes")
            .expect("first shutdown task")
            .expect_err("forced checkpoint failure is terminal");
        let replayed = tokio::time::timeout(Duration::from_secs(5), concurrent)
            .await
            .expect("concurrent shutdown completes")
            .expect("concurrent shutdown task")
            .expect_err("replay carries the same terminal failure");
        assert_eq!(
            first_failure.to_string(),
            replayed.to_string(),
            "concurrent callers replay the stored terminal result"
        );
        // The failure is terminal AND the runtime state behind it is terminal
        // — no split terminal state: the intake is closed, every owner was
        // aborted, and no runtime can revive work behind the Err.
        assert!(manager.lifecycle_tx.is_closed());
        assert!(
            manager
                .primary
                .owner
                .lock()
                .expect("primary owner lock")
                .is_none(),
            "the primary owner was aborted by the terminal shutdown"
        );
        assert!(
            secondary.owner.lock().expect("secondary owner lock").is_none(),
            "the secondary owner was aborted by the terminal shutdown"
        );
        // A failed restart checkpoint is TERMINAL cleanup for the
        // manager-owned secondaries (checkpoint success detaches alone):
        // the exact checkpoint Err still publishes only after the complete
        // termination sequence, and the primary itself is never cleaned
        // here — `run_listener_mode` owns the primary/Todo cleanup behind
        // the same exact checkpoint Err.
        assert_eq!(
            secondary.cleanup_count.load(Ordering::SeqCst),
            1,
            "checkpoint-failure restart must run the complete terminal cleanup for secondaries"
        );
        assert_eq!(
            manager.primary.cleanup_count.load(Ordering::SeqCst),
            0,
            "the primary application stays owned by lib.rs; the manager never cleans it"
        );
        let registry = manager.registry.read().await;
        assert!(
            registry
                .by_id
                .values()
                .all(|runtime| Arc::ptr_eq(runtime, &manager.primary)),
            "registry holds only the primary after the terminal failure"
        );
        assert!(
            registry
                .by_file
                .values()
                .all(|runtime| Arc::ptr_eq(runtime, &manager.primary)),
            "registry file index holds only the primary after the terminal failure"
        );
        drop(registry);
        // Ordinary admission rejects once the intake is closed: the retained
        // primary target fails closed deterministically...
        let rejected = manager
            .dispatch(
                RpcCommand::GetState { id: None },
                Some(manager.primary_session_id()),
            )
            .await;
        assert!(
            !rejected.success,
            "ordinary commands must reject once shutdown is terminal"
        );
        assert_eq!(
            rejected.error.as_deref(),
            Some("session manager is shutting down"),
            "ordinary rejection is deterministic at the admit gate"
        );
        // ...and the detached secondary is unknown, never executable.
        let gone = manager
            .dispatch(RpcCommand::GetState { id: None }, Some(session_id.clone()))
            .await;
        assert!(!gone.success, "the secondary was detached from the registry");
        assert!(gone.error.unwrap().contains("unknown session"));
        // A late caller after the terminal publication replays the SAME
        // failure verbatim — the pre-fix bool fast path returned a fabricated
        // Ok(()) that masked the terminal failure, and an early publication
        // would have left runtime state non-terminal behind the same Err.
        let late_failure = manager
            .shutdown_for_restart()
            .await
            .expect_err("late caller replays the stored failure");
        assert_eq!(
            first_failure.to_string(),
            late_failure.to_string(),
            "late callers replay the stored terminal result"
        );
        let stored = manager.shutdown_terminal.borrow().clone();
        assert_eq!(
            stored.expect("terminal result stored").expect_err("failure stored").as_str(),
            first_failure.to_string(),
            "the stored terminal result is the failure text"
        );
    }

    #[tokio::test]
    async fn restart_shutdown_detaches_secondary_without_terminal_cleanup() {
        let (manager, _primary_registration) =
            manager_with(Some(Arc::new(TestSpawner::default()))).await;
        let opened = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(opened.success, "{}", opened.error.unwrap_or_default());
        let session_id = opened.data.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        let secondary = manager
            .resolve(Some(&session_id))
            .await
            .expect("secondary runtime");

        let primary_application = manager.primary.application.clone();
        let secondary_application = secondary.application.clone();
        manager
            .shutdown_for_restart()
            .await
            .expect("restart shutdown checkpoints and detaches runtimes");

        assert_eq!(
            secondary.cleanup_count.load(Ordering::SeqCst),
            0,
            "restart detach must not invoke terminal Application cleanup",
        );
        assert!(manager.lifecycle_tx.is_closed());
        let registry = manager.registry.read().await;
        assert!(
            !primary_application
                .orchestration_runtime()
                .is_some_and(|runtime| runtime.is_shutdown()),
            "restart checkpoint must not terminally shut down the primary runtime",
        );
        assert!(
            !secondary_application
                .orchestration_runtime()
                .is_some_and(|runtime| runtime.is_shutdown()),
            "restart checkpoint must not terminally shut down the secondary runtime",
        );
        assert!(
            registry
                .by_id
                .values()
                .all(|runtime| Arc::ptr_eq(runtime, &manager.primary))
        );
    }

    /// A creation parked FOREVER inside `factory.prepare` and another parked
    /// forever inside `factory.finish` must never hang the terminal
    /// shutdown: the owner joins creations for a bounded deadline, fires the
    /// cooperative cancellation token, and aborts stragglers. Both wedged
    /// creations fail closed, nothing registered, and the terminal result
    /// publishes — no hang, no journal, no registry leak.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_bounds_never_returning_prepare_and_finish() {
        let spawner = Arc::new(NeverSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;

        let first = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .dispatch(
                        RpcCommand::NewSession {
                            id: Some("never-prepare".into()),
                            parent_session: None,
                            cwd: None,
                        },
                        None,
                    )
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(5), spawner.prepare_entered.acquire())
            .await
            .expect("first creation reaches prepare")
            .expect("prepare signal")
            .forget();
        // The first creation is parked inside prepare; let it settle there
        // before the second creation may pass prepare into finish.
        tokio::time::sleep(Duration::from_millis(100)).await;
        spawner
            .park_prepare
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let second = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .dispatch(
                        RpcCommand::NewSession {
                            id: Some("never-finish".into()),
                            parent_session: None,
                            cwd: None,
                        },
                        None,
                    )
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(5), spawner.finish_entered.acquire())
            .await
            .expect("second creation reaches finish")
            .expect("finish signal")
            .forget();

        let shutdown = tokio::spawn({
            let manager = manager.clone();
            async move { manager.shutdown().await }
        });
        let result = tokio::time::timeout(Duration::from_secs(30), shutdown)
            .await
            .expect("terminal shutdown is bounded by the creation join deadline")
            .expect("shutdown task")
            .expect("terminal shutdown succeeds");
        let _ = result;

        // Both wedged creations failed closed through the cooperative
        // cancellation — never admitted past the shutdown, never published.
        let first = tokio::time::timeout(Duration::from_secs(10), first)
            .await
            .expect("parked prepare creation settles")
            .expect("first creation task");
        assert!(!first.success, "{first:?}");
        let second = tokio::time::timeout(Duration::from_secs(10), second)
            .await
            .expect("parked finish creation settles")
            .expect("second creation task");
        assert!(!second.success, "{second:?}");

        assert!(manager.lifecycle_tx.is_closed());
        assert!(manager.shutdown_terminal.borrow().is_some());
        let registry = manager.registry.read().await;
        assert!(
            registry
                .by_id
                .values()
                .all(|runtime| Arc::ptr_eq(runtime, &manager.primary)),
            "no wedged creation leaked a registry entry"
        );
        assert!(
            registry
                .by_file
                .values()
                .all(|runtime| Arc::ptr_eq(runtime, &manager.primary)),
            "no wedged creation leaked a file index entry"
        );
    }


    /// A PRODUCTION Fresh creation parked inside `PreparedFreshSession::finish`
    /// — after the created journal is durably on disk and the application is
    /// live — must never hang the terminal shutdown, and the finish's
    /// internal cancellation-safe rollback must settle the partial Fresh
    /// state explicitly: the created journal is REMOVED and the prepared
    /// extension runtime is SHUT DOWN through the finish-owned rollback.
    /// No plain Drop gap, no leaked journal, no registry zombie.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_cancels_parked_production_fresh_finish_with_explicit_rollback() {
        let (_native, _project, _service, production) = production_manager_spawner();
        let gate = crate::session_run::FreshFinishParkGate::new();
        struct ParkedFreshFinish {
            inner: crate::session_run::RunSessionSpawner,
            gate: crate::session_run::FreshFinishParkGate,
        }
        impl SessionSpawner for ParkedFreshFinish {
            fn prepare(
                &self,
                request: SessionSpawnRequest,
            ) -> Pin<Box<dyn Future<Output = Result<PreparedSessionSpawn>> + Send>> {
                let inner = self.inner.clone();
                let gate = self.gate.clone();
                Box::pin(async move {
                    match SessionSpawner::prepare(&inner, request).await? {
                        PreparedSessionSpawn::Fresh(prepared) => Ok(
                            PreparedSessionSpawn::Fresh(
                                prepared.park_finish_after_journal_for_test(gate),
                            ),
                        ),
                        other => Ok(other),
                    }
                })
            }

            fn spawn(
                &self,
                request: SessionSpawnRequest,
            ) -> Pin<Box<dyn Future<Output = Result<SessionSpawnResult>> + Send>> {
                self.inner.spawn(request)
            }

            fn session_storage(&self) -> Option<crate::session_run::SessionStorage> {
                self.inner.session_storage()
            }

            fn track_detached_cleanups(&self, tracker: crate::session_run::SpawnRollbackTracker) {
                SessionSpawner::track_detached_cleanups(&self.inner, tracker);
            }

            fn catalog(&self) -> Option<pi_coding::SessionCatalog> {
                self.inner.catalog()
            }

            fn todo_child_service(&self) -> Option<Arc<TodoChildService>> {
                self.inner.todo_child_service()
            }
        }
        let spawner = Arc::new(ParkedFreshFinish {
            inner: production,
            gate: gate.clone(),
        });
        let (primary, _primary_id, _primary_dir) =
            production_recorded_application("parked-fresh").await;
        let manager = SessionRuntimeManager::new(
            primary,
            ExtensionUiAdapter::default(),
            Some(spawner),
            None,
        )
        .await
        .expect("production manager");
        let target = tempfile::tempdir().expect("fresh target cwd");
        let open = tokio::spawn({
            let manager = manager.clone();
            let cwd = target.path().to_string_lossy().into_owned();
            async move {
                manager
                    .dispatch(
                        RpcCommand::NewSession {
                            id: Some("parked-fresh".into()),
                            parent_session: None,
                            cwd: Some(cwd),
                        },
                        None,
                    )
                    .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(30), gate.wait_entered())
            .await
            .expect("production Fresh finish parked after the journal became durable");
        let journal = gate.journal_path().expect("created journal path recorded");
        assert!(
            journal.exists(),
            "the created journal is durably on disk while the finish is parked"
        );

        let shutdown = tokio::spawn({
            let manager = manager.clone();
            async move { manager.shutdown().await }
        });
        tokio::time::timeout(Duration::from_secs(30), shutdown)
            .await
            .expect("terminal shutdown is bounded past the parked production finish")
            .expect("shutdown task")
            .expect("terminal shutdown succeeds");
        let open = tokio::time::timeout(Duration::from_secs(10), open)
            .await
            .expect("parked finish creation settles")
            .expect("creation task");
        assert!(!open.success, "the aborted creation must fail closed: {open:?}");
        assert!(
            !journal.exists(),
            "the created journal was rolled back before the terminal shutdown returned"
        );
        assert!(
            gate.rollback_settled(),
            "the extension runtime shutdown completed before the terminal shutdown returned"
        );
        assert!(manager.lifecycle_tx.is_closed());
        assert!(manager.shutdown_terminal.borrow().is_some());
        let registry = manager.registry.read().await;
        assert!(
            registry
                .by_id
                .values()
                .all(|runtime| Arc::ptr_eq(runtime, &manager.primary)),
            "the aborted production creation left no registry zombie"
        );
        assert!(
            registry
                .by_file
                .values()
                .all(|runtime| Arc::ptr_eq(runtime, &manager.primary))
        );
        drop(registry);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_aborts_production_fresh_settle_pass_with_tracked_rollback() {
        let (_native, _project, _service, production) = production_manager_spawner();
        let gate = crate::session_run::FreshFinishParkGate::new();
        gate.fail_on_release();
        struct SettleParkedFreshFinish {
            inner: crate::session_run::RunSessionSpawner,
            gate: crate::session_run::FreshFinishParkGate,
        }
        impl SessionSpawner for SettleParkedFreshFinish {
            fn prepare(
                &self,
                request: SessionSpawnRequest,
            ) -> Pin<Box<dyn Future<Output = Result<PreparedSessionSpawn>> + Send>> {
                let inner = self.inner.clone();
                let gate = self.gate.clone();
                Box::pin(async move {
                    match SessionSpawner::prepare(&inner, request).await? {
                        PreparedSessionSpawn::Fresh(prepared) => Ok(
                            PreparedSessionSpawn::Fresh(
                                prepared.park_finish_after_journal_for_test(gate),
                            ),
                        ),
                        other => Ok(other),
                    }
                })
            }

            fn spawn(
                &self,
                request: SessionSpawnRequest,
            ) -> Pin<Box<dyn Future<Output = Result<SessionSpawnResult>> + Send>> {
                self.inner.spawn(request)
            }

            fn session_storage(&self) -> Option<crate::session_run::SessionStorage> {
                self.inner.session_storage()
            }

            fn track_detached_cleanups(&self, tracker: crate::session_run::SpawnRollbackTracker) {
                SessionSpawner::track_detached_cleanups(&self.inner, tracker);
            }

            fn catalog(&self) -> Option<pi_coding::SessionCatalog> {
                self.inner.catalog()
            }

            fn todo_child_service(&self) -> Option<Arc<TodoChildService>> {
                self.inner.todo_child_service()
            }
        }
        let spawner = Arc::new(SettleParkedFreshFinish {
            inner: production,
            gate: gate.clone(),
        });
        let (primary, _primary_id, _primary_dir) =
            production_recorded_application("settle-parked-fresh").await;
        let manager = SessionRuntimeManager::new(
            primary,
            ExtensionUiAdapter::default(),
            Some(spawner),
            None,
        )
        .await
        .expect("production manager");
        let target = tempfile::tempdir().expect("fresh target cwd");
        let open = tokio::spawn({
            let manager = manager.clone();
            let cwd = target.path().to_string_lossy().into_owned();
            async move {
                manager
                    .dispatch(
                        RpcCommand::NewSession {
                            id: Some("settle-parked-fresh".into()),
                            parent_session: None,
                            cwd: Some(cwd),
                        },
                        None,
                    )
                    .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(30), gate.wait_entered())
            .await
            .expect("production Fresh finish parked after the journal became durable");
        let journal = gate.journal_path().expect("created journal path recorded");
        assert!(
            journal.exists(),
            "the created journal is durably on disk while the finish is parked"
        );

        gate.release();
        tokio::time::timeout(std::time::Duration::from_secs(30), gate.wait_rollback_entered())
            .await
            .expect("the explicit settle pass parked mid-rollback");
        assert!(
            journal.exists(),
            "the created journal is still on disk while the settle pass is held"
        );

        let shutdown = tokio::spawn({
            let manager = manager.clone();
            async move { manager.shutdown().await }
        });
        tokio::time::timeout(Duration::from_secs(30), shutdown)
            .await
            .expect("terminal shutdown is bounded past the parked settle pass")
            .expect("shutdown task")
            .expect("terminal shutdown succeeds");
        let open = tokio::time::timeout(Duration::from_secs(10), open)
            .await
            .expect("parked finish creation settles")
            .expect("creation task");
        assert!(!open.success, "the aborted creation must fail closed: {open:?}");
        assert!(
            !journal.exists(),
            "the created journal was rolled back before the terminal shutdown returned"
        );
        assert!(
            gate.rollback_settled(),
            "the aborted settle pass completed through the tracked fallback before the terminal shutdown returned"
        );
        assert!(manager.lifecycle_tx.is_closed());
        assert!(manager.shutdown_terminal.borrow().is_some());
        let registry = manager.registry.read().await;
        assert!(
            registry
                .by_id
                .values()
                .all(|runtime| Arc::ptr_eq(runtime, &manager.primary)),
            "the aborted production creation left no registry zombie"
        );
        assert!(
            registry
                .by_file
                .values()
                .all(|runtime| Arc::ptr_eq(runtime, &manager.primary))
        );
        drop(registry);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reused_spawner_rollback_fallbacks_track_current_manager() {
        let (_native, _project, _service, spawner) = production_manager_spawner();
        let spawner = Arc::new(spawner);
        {
            let (manager1, _dir1) =
                production_manager((*spawner).clone(), "reused-tracker-first").await;
            manager1
                .shutdown()
                .await
                .expect("first manager terminal shutdown");
        }
        let (manager2, _dir2) =
            production_manager((*spawner).clone(), "reused-tracker-second").await;
        let tracker = spawner
            .detached_tracker_for_test()
            .expect("second manager registered its tracker on the reused spawner");
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        tracker(tokio::spawn(async move {
            let _ = release_rx.await;
        }));
        let mut shutdown = tokio::spawn({
            let manager2 = manager2.clone();
            async move { manager2.shutdown().await }
        });
        tokio::time::timeout(Duration::from_millis(300), &mut shutdown)
            .await
            .expect_err("second manager joins its tracked probe before publishing");
        release_tx
            .send(())
            .expect("probe still tracked by the second manager");
        tokio::time::timeout(Duration::from_secs(10), shutdown)
            .await
            .expect("shutdown completes after the tracked probe settled")
            .expect("shutdown task")
            .expect("second manager terminal shutdown succeeds");
    }

    /// Production-shaped all-busy saturation: when every loaded slot is
    /// genuinely busy, a fresh creation that already PREPARED (real
    /// target-root resources under the prepared spawn) fails closed at
    /// capacity with the exact bounded cause, and its preparation is
    /// discarded explicitly and awaited through the factory's
    /// `discard_prepared` (extension/session teardown). Zero journal,
    /// registry, or capacity mutation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn prepared_discards_explicitly_when_every_loaded_slot_is_busy() {
        let spawner = Arc::new(DiscardObservingSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let mut opened_ids = Vec::new();
        for _ in 0..(MAX_LOADED_SESSIONS - 1) {
            let response = manager
                .dispatch(
                    RpcCommand::NewSession {
                        id: None,
                        parent_session: None,
                        cwd: None,
                    },
                    None,
                )
                .await;
            assert!(response.success, "{}", response.error.unwrap_or_default());
            opened_ids.push(
                response.data.unwrap()["sessionId"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
        }

        // Streaming every secondary at once cannot saturate the 128-slot
        // pool: ordinary commands share [`MAX_CONCURRENT_SESSION_COMMANDS`]
        // (32) permits, so the 16-slot streaming harness is stale. In-progress
        // todos are the same `ensure_idle` busy signal used by
        // `in_progress_todo_protects_runtime_from_lru_retirement`.
        for id in &opened_ids {
            let runtime = manager
                .resolve(Some(id))
                .await
                .expect("loaded runtime");
            runtime
                .application
                .set_todos(vec![pi_coding::TodoPhase {
                    name: "Work".to_owned(),
                    tasks: vec![pi_coding::TodoItem {
                        id: "task-1".to_owned(),
                        content: "active task".to_owned(),
                        status: pi_coding::TodoStatus::InProgress,
                        depends_on: Vec::new(),
                        ready: true,
                        waiting_on: Vec::new(),
                        agent: None,
                    }],
                }])
                .expect("set in-progress todo");
        }

        let journals_before = spawner.inner.spawned_paths.lock().expect("spawn log").len();
        let registry_before = manager.registry.read().await.distinct_runtimes().len();
        assert_eq!(manager.loaded_slots.available_permits(), 0);

        // Overflow open: the prepared state exists (the spawner's prepare
        // built one), loaded capacity is exhausted, and the preparation must
        // be discarded explicitly and awaited.
        let rejected = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: Some("overflow-busy".into()),
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(!rejected.success, "all-busy pools must fail closed");
        let error = rejected.error.unwrap();
        assert!(error.contains("too many busy sessions"), "{error}");
        assert!(error.contains(&MAX_LOADED_SESSIONS.to_string()), "{error}");
        assert_eq!(
            spawner.discarded_preparations.load(Ordering::SeqCst),
            1,
            "the exhausted create discarded its preparation explicitly and awaited it"
        );
        assert_eq!(
            manager.registry.read().await.distinct_runtimes().len(),
            registry_before,
            "zero registry mutation"
        );
        assert_eq!(manager.loaded_slots.available_permits(), 0);
        assert_eq!(
            spawner.inner.spawned_paths.lock().expect("spawn log").len(),
            journals_before,
            "zero journal mutation"
        );

        for id in &opened_ids {
            assert!(
                manager.resolve(Some(id)).await.is_ok(),
                "a busy runtime must never be the LRU victim: {id}"
            );
        }
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn restart_shutdown_recovers_dual_running_manager_runtimes_exactly_once() {
        let primary_started = Arc::new(Barrier::new(2));
        let secondary_started = Arc::new(Barrier::new(2));
        let primary_release = CancellationToken::new();
        let secondary_release = CancellationToken::new();
        let (
            primary_application,
            primary_session_id,
            primary_dir,
            primary_orchestration,
            primary_registration,
        ) = recorded_orchestration_application(
            "manager-primary-running",
            primary_started.clone(),
            primary_release.clone(),
        )
        .await;
        let (
            secondary_application,
            secondary_session_id,
            secondary_dir,
            secondary_orchestration,
            secondary_registration,
        ) = recorded_orchestration_application(
            "manager-secondary-running",
            secondary_started.clone(),
            secondary_release.clone(),
        )
        .await;
        let primary_tool_call_id = "manager-primary-task-call";
        let secondary_tool_call_id = "manager-secondary-task-call";
        let primary_spawn = launch_manager_task_from_parent(
            &primary_application,
            &primary_registration,
            primary_tool_call_id,
            "PrimaryRunning",
        )
        .await;
        let secondary_spawn = launch_manager_task_from_parent(
            &secondary_application,
            &secondary_registration,
            secondary_tool_call_id,
            "SecondaryRunning",
        )
        .await;
        tokio::time::timeout(Duration::from_secs(3), primary_started.wait())
            .await
            .expect("primary child reached Running");
        tokio::time::timeout(Duration::from_secs(3), secondary_started.wait())
            .await
            .expect("secondary child reached Running");
        let primary_job = primary_orchestration
            .jobs(Some(std::slice::from_ref(&primary_spawn.job_id)))
            .remove(0);
        let secondary_job = secondary_orchestration
            .jobs(Some(std::slice::from_ref(&secondary_spawn.job_id)))
            .remove(0);
        assert_running_identity(&primary_job, &primary_spawn, "PrimaryRunning");
        assert_running_identity(&secondary_job, &secondary_spawn, "SecondaryRunning");
        let primary_group_id = primary_orchestration.group_id().to_owned();
        let secondary_group_id = secondary_orchestration.group_id().to_owned();

        let manager = SessionRuntimeManager::new(
            primary_application.clone(),
            ExtensionUiAdapter::default(),
            None,
            None,
        )
        .await
        .expect("manager with durable primary");
        let secondary_slot = manager.acquire_loaded_slot().await.expect("secondary slot");
        let secondary_runtime = match manager
            .register_spawned(
                &SessionSpawnResult {
                    session_id: secondary_session_id.clone(),
                    session_file: secondary_application
                        .session()
                        .recorder_info()
                        .map(|(_, path)| path)
                        .expect("durable secondary recorder"),
                    file_identity: secondary_application
                        .session()
                        .session_file_identity()
                        .expect("durable secondary file identity"),
                    application: secondary_application.clone(),
                    extension_ui: ExtensionUiAdapter::default(),
                    resume_notice: None,
                    journal_ownership: SpawnedJournalOwnership::Existing,
                    editor_text: None,
                },
                secondary_slot,
            )
            .await
        {
            Ok(Ok((runtime, _))) => runtime,
            _ => panic!("register durable secondary failed"),
        };
        assert_eq!(manager.primary_session_id(), primary_session_id);
        assert_eq!(secondary_runtime.protocol_id(), secondary_session_id);

        manager
            .shutdown_for_restart()
            .await
            .expect("checkpoint both Running manager runtimes");
        let primary_sidecar = load_manager_sidecar(&primary_application);
        let secondary_sidecar = load_manager_sidecar(&secondary_application);
        assert_eq!(primary_sidecar.group_id, primary_group_id);
        assert_eq!(secondary_sidecar.group_id, secondary_group_id);
        assert_running_identity(
            primary_sidecar
                .jobs
                .iter()
                .find(|job| job.id == primary_spawn.job_id)
                .expect("primary durable job"),
            &primary_spawn,
            "PrimaryRunning",
        );
        assert_running_identity(
            secondary_sidecar
                .jobs
                .iter()
                .find(|job| job.id == secondary_spawn.job_id)
                .expect("secondary durable job"),
            &secondary_spawn,
            "SecondaryRunning",
        );

        let primary_path = primary_application
            .session()
            .recorder_info()
            .expect("primary recorder")
            .1;
        let secondary_path = secondary_application
            .session()
            .recorder_info()
            .expect("secondary recorder")
            .1;
        let primary_cwd = primary_application.session().cwd().to_path_buf();
        let secondary_cwd = secondary_application.session().cwd().to_path_buf();
        drop(secondary_runtime);
        drop(manager);
        drop(primary_orchestration);
        drop(secondary_orchestration);
        drop(primary_application);
        drop(secondary_application);
        drop(primary_registration);
        drop(secondary_registration);
        tokio::task::yield_now().await;
        let primary_continuations = Arc::new(AtomicUsize::new(0));
        let secondary_continuations = Arc::new(AtomicUsize::new(0));
        let primary_recovery_release = CancellationToken::new();
        let secondary_recovery_release = CancellationToken::new();
        let (resumed_primary, resumed_primary_orchestration) = resumed_manager_application(
            &primary_path,
            &primary_cwd,
            "primary recovered once",
            primary_recovery_release.clone(),
            primary_continuations.clone(),
        )
        .await;
        let (resumed_secondary, resumed_secondary_orchestration) = resumed_manager_application(
            &secondary_path,
            &secondary_cwd,
            "secondary recovered once",
            secondary_recovery_release.clone(),
            secondary_continuations.clone(),
        )
        .await;
        assert_eq!(
            resumed_primary
                .session()
                .recorder_info()
                .expect("primary recorder")
                .0,
            primary_session_id,
        );
        assert_eq!(
            resumed_secondary
                .session()
                .recorder_info()
                .expect("secondary recorder")
                .0,
            secondary_session_id,
        );
        assert_eq!(resumed_primary_orchestration.group_id(), primary_group_id);
        assert_eq!(
            resumed_secondary_orchestration.group_id(),
            secondary_group_id
        );
        assert_parent_tool_identity_once(&resumed_primary, primary_tool_call_id, &primary_spawn);
        assert_parent_tool_identity_once(
            &resumed_secondary,
            secondary_tool_call_id,
            &secondary_spawn,
        );
        assert_running_identity(
            &resumed_primary_orchestration
                .jobs(Some(std::slice::from_ref(&primary_spawn.job_id)))
                .remove(0),
            &primary_spawn,
            "PrimaryRunning",
        );
        assert_running_identity(
            &resumed_secondary_orchestration
                .jobs(Some(std::slice::from_ref(&secondary_spawn.job_id)))
                .remove(0),
            &secondary_spawn,
            "SecondaryRunning",
        );
        let resumed_manager = SessionRuntimeManager::new(
            resumed_primary.clone(),
            ExtensionUiAdapter::default(),
            None,
            None,
        )
        .await
        .expect("manager with resumed durable primary");
        let resumed_secondary_slot = resumed_manager
            .acquire_loaded_slot()
            .await
            .expect("resumed secondary slot");
        let resumed_secondary_runtime = match resumed_manager
            .register_spawned(
                &SessionSpawnResult {
                    session_id: secondary_session_id.clone(),
                    session_file: resumed_secondary
                        .session()
                        .recorder_info()
                        .map(|(_, path)| path)
                        .expect("resumed secondary recorder"),
                    file_identity: resumed_secondary
                        .session()
                        .session_file_identity()
                        .expect("resumed secondary file identity"),
                    application: resumed_secondary.clone(),
                    extension_ui: ExtensionUiAdapter::default(),
                    resume_notice: None,
                    journal_ownership: SpawnedJournalOwnership::Existing,
                    editor_text: None,
                },
                resumed_secondary_slot,
            )
            .await
        {
            Ok(Ok((runtime, _))) => runtime,
            _ => panic!("register resumed durable secondary failed"),
        };
        assert_eq!(resumed_manager.primary_session_id(), primary_session_id);
        assert_eq!(
            resumed_secondary_runtime.protocol_id(),
            secondary_session_id
        );
        let mut resumed_events = resumed_manager.events();
        primary_recovery_release.cancel();
        secondary_recovery_release.cancel();

        let primary_settled = resumed_primary_orchestration
            .wait_jobs(
                std::slice::from_ref(&primary_spawn.job_id),
                Some(Duration::from_secs(5)),
                None,
            )
            .await
            .expect("primary recovery result");
        let secondary_settled = resumed_secondary_orchestration
            .wait_jobs(
                std::slice::from_ref(&secondary_spawn.job_id),
                Some(Duration::from_secs(5)),
                None,
            )
            .await
            .expect("secondary recovery result");
        assert_eq!(primary_settled.len(), 1);
        assert_eq!(secondary_settled.len(), 1);
        resumed_manager.release_subscription_snapshots_for_test(2);
        assert_eq!(
            primary_settled[0].status,
            JobStatus::Completed,
            "{:#?}",
            primary_settled[0]
        );
        assert_eq!(
            secondary_settled[0].status,
            JobStatus::Completed,
            "{:#?}",
            secondary_settled[0]
        );
        assert_eq!(primary_continuations.load(Ordering::SeqCst), 1);
        assert_eq!(secondary_continuations.load(Ordering::SeqCst), 1);
        resumed_manager
            .subscription_snapshot(&primary_session_id, 0)
            .await
            .expect("primary completion event barrier");
        resumed_manager
            .subscription_snapshot(&secondary_session_id, 0)
            .await
            .expect("secondary completion event barrier");
        let recovered_frames = collect_recovered_completion_frames(
            &mut resumed_events,
            [&primary_spawn.job_id, &secondary_spawn.job_id],
        )
        .await;
        assert_recovered_parent_delivery_once(
            &recovered_frames,
            &primary_session_id,
            &primary_group_id,
            &primary_spawn,
            "primary recovered once",
        );
        assert_recovered_parent_delivery_once(
            &recovered_frames,
            &secondary_session_id,
            &secondary_group_id,
            &secondary_spawn,
            "secondary recovered once",
        );
        assert!(
            primary_settled[0]
                .result
                .as_ref()
                .is_some_and(|result| result.output.starts_with("primary recovered once")),
            "{:#?}",
            primary_settled[0],
        );
        assert!(
            secondary_settled[0]
                .result
                .as_ref()
                .is_some_and(|result| result.output.starts_with("secondary recovered once")),
            "{:#?}",
            secondary_settled[0],
        );
        for job in primary_settled.iter().chain(&secondary_settled) {
            assert!(
                !job.result
                    .as_ref()
                    .and_then(|result| result.error.as_deref())
                    .is_some_and(|error| error.contains("orchestration shut down")),
                "restart recovery must not project terminal shutdown: {job:?}",
            );
        }
        assert_eq!(
            resumed_primary_orchestration
                .jobs(Some(std::slice::from_ref(&primary_spawn.job_id)))
                .len(),
            1,
        );
        assert_eq!(
            resumed_secondary_orchestration
                .jobs(Some(std::slice::from_ref(&secondary_spawn.job_id)))
                .len(),
            1,
        );

        resumed_manager
            .shutdown()
            .await
            .expect("cleanup resumed manager");
        primary_release.cancel();
        secondary_release.cancel();
        drop(primary_dir);
        drop(secondary_dir);
    }

    #[tokio::test]
    async fn concurrent_shutdown_calls_cleanup_secondary_once() {
        let (manager, _primary_registration) =
            manager_with(Some(Arc::new(TestSpawner::default()))).await;
        let opened = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(opened.success, "{}", opened.error.unwrap_or_default());
        let session_id = opened.data.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        let runtime = manager
            .resolve(Some(&session_id))
            .await
            .expect("secondary runtime");

        let first = tokio::spawn({
            let manager = manager.clone();
            async move { manager.shutdown().await }
        });
        let second = tokio::spawn({
            let manager = manager.clone();
            async move { manager.shutdown().await }
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            first.await.expect("first shutdown");
            second.await.expect("second shutdown");
        })
        .await
        .expect("concurrent shutdown callers complete");
        assert_eq!(
            runtime.cleanup_count.load(Ordering::SeqCst),
            1,
            "owner cleans once"
        );
        assert!(manager.lifecycle_tx.is_closed());
        let registry = manager.registry.read().await;
        assert!(
            registry
                .by_id
                .values()
                .all(|runtime| Arc::ptr_eq(runtime, &manager.primary))
        );
    }

    #[tokio::test]
    async fn lifecycle_result_fails_closed_while_target_is_retiring() {
        let (manager, _primary_registration) =
            manager_with(Some(Arc::new(TestSpawner::default()))).await;
        let opened = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(opened.success, "{}", opened.error.unwrap_or_default());
        let target_id = opened.data.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        let target = manager
            .resolve(Some(&target_id))
            .await
            .expect("target runtime");
        let retiring = target.lifecycle_lock.clone().write_owned().await;
        let response = manager
            .lifecycle_result_response(
                Some("guard-target".into()),
                "switch_session",
                manager.primary.protocol_id(),
                target,
                None,
                None,
                LifecycleResponseMode::FullSnapshot,
            )
            .await;
        assert!(
            !response.success,
            "a lifecycle result cannot publish without its target guard"
        );
        assert_eq!(
            response.error.as_deref(),
            Some("session could not be opened right now; verify the selection and retry")
        );
        drop(retiring);
        manager.shutdown().await;
    }
    #[tokio::test]
    async fn failed_spawn_rolls_back_lru_victim_and_permit() {
        let spawner = Arc::new(FailingSpawner::new());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let mut opened_ids = Vec::new();
        for _ in 0..(MAX_LOADED_SESSIONS - 1) {
            let response = manager
                .dispatch(
                    RpcCommand::NewSession {
                        id: None,
                        parent_session: None,
                        cwd: None,
                    },
                    None,
                )
                .await;
            assert!(response.success, "{}", response.error.unwrap_or_default());
            opened_ids.push(
                response.data.unwrap()["sessionId"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
        }
        spawner.fail_next();
        let failed = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(!failed.success, "injected spawn must fail");
        for id in &opened_ids {
            let state = manager
                .dispatch(RpcCommand::GetState { id: None }, Some(id.clone()))
                .await;
            assert!(state.success, "rollback must preserve victim {id}");
        }

        let recovered = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(recovered.success, "{}", recovered.error.unwrap_or_default());
        let retired = manager
            .dispatch(
                RpcCommand::GetState { id: None },
                Some(opened_ids[0].clone()),
            )
            .await;
        assert!(
            !retired.success,
            "the restored LRU remains eligible after retry"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn loaded_session_cap_counts_in_flight_spawns() {
        let spawner = Arc::new(GatedSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let mut spawns = Vec::new();
        for index in 0..(MAX_LOADED_SESSIONS - 1) {
            spawns.push(tokio::spawn({
                let manager = manager.clone();
                async move {
                    manager
                        .dispatch(
                            RpcCommand::NewSession {
                                id: Some(format!("spawn-{index}")),
                                parent_session: None,
                                cwd: None,
                            },
                            None,
                        )
                        .await
                }
            }));
            tokio::time::timeout(std::time::Duration::from_secs(1), spawner.started.acquire())
                .await
                .expect("spawn must reach the factory")
                .expect("spawn start channel")
                .forget();
        }

        let rejected = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(!rejected.success);
        assert!(
            rejected
                .error
                .as_deref()
                .is_some_and(|error| error.contains("too many busy sessions")),
            "in-flight spawns must consume loaded-session capacity",
        );

        spawner.release.add_permits(MAX_LOADED_SESSIONS - 1);
        for spawn in spawns {
            let response = tokio::time::timeout(std::time::Duration::from_secs(5), spawn)
                .await
                .expect("spawn must finish after release")
                .expect("spawn task");
            assert!(response.success, "{}", response.error.unwrap_or_default());
        }
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn close_session_rejects_primary_unknown_and_busy_but_closes_idle() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let opened = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        let b_id = opened.data.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();

        let no_sid = manager
            .dispatch(RpcCommand::CloseSession { id: None }, None)
            .await;
        assert!(!no_sid.success);
        assert!(no_sid.error.unwrap().contains("primary"));

        let unknown = manager
            .dispatch(RpcCommand::CloseSession { id: None }, Some("ghost".into()))
            .await;
        assert!(!unknown.success);
        assert!(unknown.error.unwrap().contains("unknown session ghost"));

        let primary_id = manager.primary_session_id().to_owned();
        let primary_close = manager
            .dispatch(RpcCommand::CloseSession { id: None }, Some(primary_id))
            .await;
        assert!(!primary_close.success);
        assert!(primary_close.error.unwrap().contains("primary"));

        // Queue a 200k-char reply that B streams byte-by-byte (the spawner
        // child model uses chunk_size 1), so the turn is verifiably in flight
        // when close_session is attempted.
        spawner
            .registration
            .set_responses(vec![FauxResponse::text("x".repeat(200_000))]);
        let prompt = RpcCommand::Prompt {
            id: None,
            message: "busy".into(),
            images: Vec::new(),
            streaming_behavior: None,
        };
        let manager_task = manager.clone();
        let busy_b_id = b_id.clone();
        let b_task =
            tokio::spawn(async move { manager_task.dispatch(prompt, Some(busy_b_id)).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let busy_close = manager
            .dispatch(RpcCommand::CloseSession { id: None }, Some(b_id.clone()))
            .await;
        assert!(!busy_close.success, "busy close must be rejected");
        assert!(
            busy_close.error.unwrap().contains("busy"),
            "error should name the busy reason"
        );
        let prompt_result = b_task.await.expect("prompt task");
        assert!(prompt_result.success, "turn survived the rejected close");

        // No cancellation: the turn is STILL streaming after the rejection.
        let streaming = manager
            .dispatch(RpcCommand::GetState { id: None }, Some(b_id.clone()))
            .await;
        assert!(streaming.success, "{}", streaming.error.unwrap_or_default());
        assert_eq!(
            streaming.data.as_ref().unwrap()["isStreaming"],
            json!(true),
            "rejected close must not cancel the turn"
        );

        // Explicit abort, then poll until the runtime reaches idle, then close.
        let aborted = manager
            .dispatch(RpcCommand::Abort { id: None }, Some(b_id.clone()))
            .await;
        assert!(aborted.success, "{}", aborted.error.unwrap_or_default());
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            let state = manager
                .dispatch(RpcCommand::GetState { id: None }, Some(b_id.clone()))
                .await;
            if state.data.as_ref().and_then(|data| data.get("isStreaming")) == Some(&json!(false)) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let closed = manager
            .dispatch(RpcCommand::CloseSession { id: None }, Some(b_id.clone()))
            .await;
        assert!(closed.success, "{}", closed.error.unwrap_or_default());
        assert_eq!(closed.data.unwrap()["closed"], json!(true));
        let gone = manager
            .dispatch(RpcCommand::GetState { id: None }, Some(b_id.clone()))
            .await;
        assert!(!gone.success);
        assert!(
            gone.error
                .as_deref()
                .is_some_and(|error| error.contains("unknown session")),
            "unexpected closed-session response: {gone:?}"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_cleans_manager_owned_runtimes_only() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let opened = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        let b_id = opened.data.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            manager
                .dispatch(RpcCommand::GetState { id: None }, Some(b_id.clone()))
                .await
                .success
        );

        manager.shutdown().await;

        let gone = manager
            .dispatch(RpcCommand::GetState { id: None }, Some(b_id.clone()))
            .await;
        assert!(!gone.success, "child must be cleaned by shutdown");
        assert!(gone.error.unwrap().contains("unknown session"));
        let primary = manager
            .dispatch(RpcCommand::GetState { id: None }, None)
            .await;
        assert!(
            !primary.success,
            "ordinary admission must reject once the intake is closed"
        );
        assert_eq!(
            primary.error.as_deref(),
            Some("session manager is shutting down"),
            "rejection is deterministic at the admit gate, not an owner-abort artifact"
        );
        manager.primary.application.cleanup().await;
    }

    #[tokio::test]
    async fn path_targeted_fork_is_catalog_visible_and_reopens_once_after_recreation() {
        let (_home, native, catalog) = web_catalog_fixture();
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog.clone()));
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let project = tempfile::tempdir().expect("source project");
        let (model, _registration) = faux_model("durable-root-fork");
        let source = pi_coding::start_session_in(
            project.path(),
            Some(&model),
            Some("off"),
            Some(native.path()),
            Some("durable-root-source"),
            None,
        )
        .expect("source recorder");
        let selected = source
            .record_message(&Message::user_text("fork me", 0))
            .expect("selected user");
        let mut assistant = pi_ai::AssistantMessage::pending(&pi_ai::Model::default());
        assistant.content = vec![pi_ai::ContentBlock::text("source reply")];
        assistant.stop_reason = pi_ai::StopReason::Stop;
        assistant.timestamp = 1;
        source
            .record_message(&Message::Assistant(assistant))
            .expect("source assistant");
        source.persist_now_durable().expect("persist source");
        let source_path = source.path();
        source.close().expect("close source");

        let response = manager
            .dispatch(
                RpcCommand::Fork {
                    id: Some("durable-fork".into()),
                    entry_id: selected,
                    session_path: Some(source_path.to_string_lossy().into_owned()),
                },
                None,
            )
            .await;
        assert!(response.success, "{}", response.error.unwrap_or_default());
        let data = response.data.expect("fork response");
        let fork_id = data["sessionId"].as_str().expect("fork id").to_owned();
        let fork_runtime = manager.resolve(Some(&fork_id)).await.expect("fork runtime");
        let fork_path = fork_runtime
            .application
            .session()
            .recorder_info()
            .expect("fork recorder")
            .1;
        assert!(fork_path.is_file(), "success must name a durable native file");
        assert_ne!(fork_path, source_path);
        assert_eq!(
            pi_coding::load_session_tree(&source_path)
                .expect("source remains readable")
                .header
                .id,
            "durable-root-source",
            "fork publication must not replace the source identity"
        );
        assert!(manager.resolve(Some("durable-root-source")).await.is_err());
        assert!(
            catalog
                .scan(&[pi_coding::SessionSourceKind::NativePi])
                .iter()
                .any(|row| row.session_id == fork_id && row.path == fork_path),
            "the durable fork must be visible without the live overlay"
        );
        assert_eq!(
            pi_coding::load_session_tree(&fork_path)
                .expect("reload fork")
                .header
                .parent_session
                .as_deref(),
            Some(source_path.to_string_lossy().as_ref())
        );
        drop(fork_runtime);
        manager.shutdown().await.expect("shutdown first manager");

        let recreated_spawner = Arc::new(TestSpawner::default().with_catalog(catalog.clone()));
        let (recreated, _registration) = manager_with(Some(recreated_spawner.clone())).await;
        let listed = recreated
            .dispatch(
                RpcCommand::SessionList {
                    id: Some("list-forks-after-recreation".into()),
                    scope: crate::modes::rpc::RpcSessionListScope::AllProjects,
                },
                None,
            )
            .await;
        assert!(listed.success, "{}", listed.error.unwrap_or_default());
        let occurrences = listed
            .data
            .as_ref()
            .and_then(|data| data["sessions"].as_array())
            .into_iter()
            .flatten()
            .filter(|row| row["sessionId"] == fork_id)
            .count();
        assert_eq!(occurrences, 1, "header-only fork must survive Web list recreation");
        let reopened = recreated
            .dispatch(
                RpcCommand::SwitchSession {
                    id: Some("reopen-fork".into()),
                    session_path: fork_path.to_string_lossy().into_owned(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                None,
            )
            .await;
        assert!(reopened.success, "{}", reopened.error.unwrap_or_default());
        assert_eq!(reopened.data.as_ref().unwrap()["sessionId"], fork_id);
        let selected_again = recreated
            .dispatch(
                RpcCommand::SwitchSession {
                    id: Some("reopen-fork-again".into()),
                    session_path: fork_path.to_string_lossy().into_owned(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                None,
            )
            .await;
        assert!(selected_again.success);
        assert_eq!(selected_again.data.as_ref().unwrap()["sessionId"], fork_id);
        assert_eq!(
            recreated_spawner.spawns.load(Ordering::SeqCst),
            1,
            "recreation must load one runtime for one durable identity"
        );
        recreated.shutdown().await.expect("shutdown recreated manager");
    }

    #[tokio::test]
    async fn fork_publication_identity_failure_rolls_back_file_runtime_permit_and_retry() {
        let inner = TestSpawner::default();
        let spawner = Arc::new(IdentityMismatchSpawner {
            inner: inner.clone(),
            mismatches_remaining: Arc::new(AtomicUsize::new(1)),
        });
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let permits = manager.loaded_slots.available_permits();
        let failed = manager
            .dispatch(
                RpcCommand::Fork {
                    id: Some("mismatched-fork".into()),
                    entry_id: "ignored-by-test-spawner".into(),
                    session_path: None,
                },
                None,
            )
            .await;
        // Lifecycle FAILURES stay request-source-owned: the envelope
        // sessionId is the source (the primary here), never a guessed target,
        // so the Web caller's ordinary source-ownership check accepts the
        // rejection and the caller can surface exactly one failure toast.
        assert_eq!(
            failed.session_id.as_deref(),
            Some(manager.primary_session_id().as_str()),
            "fork failure must keep the source-owned envelope"
        );
        assert_eq!(
            manager.registry.read().await.distinct_runtimes().len(),
            1,
            "failed publication must not leave a runtime"
        );
        let failed_paths = inner
            .spawned_paths
            .lock()
            .expect("spawned paths")
            .clone();
        assert_eq!(failed_paths.len(), 1);
        assert!(!failed_paths[0].exists());

        let retry = manager
            .dispatch(
                RpcCommand::Fork {
                    id: Some("mismatched-fork-retry".into()),
                    entry_id: "ignored-by-test-spawner".into(),
                    session_path: None,
                },
                None,
            )
            .await;
        assert!(retry.success, "retry after rollback must recover");
        let retry_id = retry.data.as_ref().unwrap()["sessionId"]
            .as_str()
            .expect("retry session id")
            .to_owned();

        assert_eq!(manager.loaded_slots.available_permits(), permits - 1);
        assert_eq!(manager.registry.read().await.distinct_runtimes().len(), 2);
        assert!(manager.resolve(Some(&retry_id)).await.is_ok());
        let retried_paths = inner
            .spawned_paths
            .lock()
            .expect("spawned paths")
            .clone();
        assert_eq!(retried_paths.len(), 2);
        assert!(!retried_paths[0].exists());
        assert!(retried_paths[1].exists());
        manager.shutdown().await.expect("shutdown manager");
    }

    #[tokio::test]
    async fn production_manager_recreation_reopens_appends_renames_and_reloads() {
        let native = tempfile::tempdir().expect("native root");
        let project = tempfile::tempdir().expect("project");
        let (primary, _, _primary_dir) =
            production_recorded_application("production-primary").await;
        let cli = <crate::args::Cli as clap::Parser>::try_parse_from([
            "rpi",
            "--api-key",
            "test-key",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
        ])
        .expect("cli");
        let service = TodoChildService::new();
        let spawner = crate::session_run::RunSessionSpawner::from_startup_with_service(
            &cli,
            crate::session_run::SessionStorage::ExplicitRoot(native.path().to_path_buf()),
            service.clone(),
        );
        let source = pi_coding::start_session_in(
            project.path(),
            None,
            None,
            Some(native.path()),
            Some("production-reopen"),
            None,
        )
        .expect("source recorder");
        let selected = source
            .record_message(&Message::user_text("before restart", 0))
            .expect("source message");
        source.persist_now_durable().expect("persist source");
        let path = source.path();
        drop(source);

        let first = SessionRuntimeManager::new(
            primary,
            ExtensionUiAdapter::default(),
            Some(Arc::new(spawner.clone())),
            None,
        )
        .await
        .expect("first manager");
        let first_open = first
            .dispatch(
                RpcCommand::SwitchSession {
                    id: Some("first-open".into()),
                    session_path: path.to_string_lossy().into_owned(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                None,
            )
            .await;
        assert!(first_open.success, "{}", first_open.error.unwrap_or_default());
        first.shutdown_for_restart().await.expect("restart first manager");
        drop(first);

        let (recreated_primary, _, _recreated_dir) =
            production_recorded_application("recreated-primary").await;
        let recreated = SessionRuntimeManager::new(
            recreated_primary,
            ExtensionUiAdapter::default(),
            Some(Arc::new(spawner.clone())),
            None,
        )
        .await
        .expect("recreated manager");
        let opened = recreated
            .dispatch(
                RpcCommand::SwitchSession {
                    id: Some("reopen".into()),
                    session_path: path.to_string_lossy().into_owned(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                None,
            )
            .await;
        assert!(opened.success, "{}", opened.error.unwrap_or_default());
        let session_id = opened.data.as_ref().unwrap()["sessionId"]
            .as_str()
            .expect("session id")
            .to_owned();
        assert_eq!(
            recreated
                .writer_info_for_test(&session_id)
                .await
                .expect("recreated writer")
                .1,
            path
        );
        assert!(
            recreated
                .dispatch(
                    RpcCommand::SetThinkingLevel {
                        id: Some("append".into()),
                        level: ThinkingLevel::Medium,
                    },
                    Some(session_id.clone()),
                )
                .await
                .success
        );
        assert!(
            recreated
                .dispatch(
                    RpcCommand::SetSessionName {
                        id: Some("rename".into()),
                        name: "after restart".into(),
                        session_path: None,
                    },
                    Some(session_id.clone()),
                )
                .await
                .success
        );
        let forked = recreated
            .dispatch(
                RpcCommand::Fork {
                    id: Some("fork-after-restart".into()),
                    entry_id: selected,
                    session_path: None,
                },
                Some(session_id),
            )
            .await;
        assert!(forked.success, "{}", forked.error.unwrap_or_default());
        let fork_id = forked.data.as_ref().unwrap()["sessionId"]
            .as_str()
            .expect("fork id")
            .to_owned();
        let fork_path = recreated
            .writer_info_for_test(&fork_id)
            .await
            .expect("fork writer")
            .1;
        recreated.shutdown_for_restart().await.expect("restart recreated manager");

        drop(recreated);

        let (third_primary, _, _third_dir) =
            production_recorded_application("third-primary").await;
        let third = SessionRuntimeManager::new(
            third_primary,
            ExtensionUiAdapter::default(),
            Some(Arc::new(spawner)),
            None,
        )
        .await
        .expect("third manager");
        let reopened_fork = third
            .dispatch(
                RpcCommand::SwitchSession {
                    id: Some("reopen-fork".into()),
                    session_path: fork_path.to_string_lossy().into_owned(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                None,
            )
            .await;
        assert!(reopened_fork.success, "{}", reopened_fork.error.unwrap_or_default());
        assert_eq!(reopened_fork.data.as_ref().unwrap()["sessionId"], fork_id);
        third.shutdown().await.expect("shutdown third manager");

        let tree = pi_coding::load_session_tree(&path).expect("reload source tree");
        assert_eq!(tree.session_name().as_deref(), Some("after restart"));
        assert!(tree.has_thinking_entry());
        let fork_tree = pi_coding::load_session_tree(fork_path).expect("reload fork tree");
        assert!(
            fork_tree
                .entries
                .iter()
                .all(|entry| entry.message.is_none()),
            "root fork must exclude the selected prompt: {:?}",
            tree_user_texts(&fork_tree)
        );
        assert_eq!(
            fork_tree.header.parent_session.as_deref(),
            Some(path.to_string_lossy().as_ref())
        );
        service.shutdown().await.expect("shutdown Todo service");
    }

    fn production_manager_spawner() -> (
        TempDir,
        TempDir,
        Arc<TodoChildService>,
        crate::session_run::RunSessionSpawner,
    ) {
        let native = tempfile::tempdir().expect("native root");
        let project = tempfile::tempdir().expect("project");
        let cli = <crate::args::Cli as clap::Parser>::try_parse_from([
            "rpi",
            "--api-key",
            "test-key",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
        ])
        .expect("cli");
        let service = TodoChildService::new();
        let spawner = crate::session_run::RunSessionSpawner::from_startup_with_service(
            &cli,
            crate::session_run::SessionStorage::ExplicitRoot(native.path().to_path_buf()),
            service.clone(),
        );
        (native, project, service, spawner)
    }

    async fn production_manager(
        spawner: crate::session_run::RunSessionSpawner,
        label: &str,
    ) -> (Arc<SessionRuntimeManager>, TempDir) {
        let (primary, _, directory) = production_recorded_application(label).await;
        let manager = SessionRuntimeManager::new(
            primary,
            ExtensionUiAdapter::default(),
            Some(Arc::new(spawner)),
            None,
        )
        .await
        .expect("production manager");
        (manager, directory)
    }

    fn persist_production_source(
        project: &Path,
        native: &Path,
        session_id: &str,
        texts: &[&str],
    ) -> (PathBuf, Vec<String>) {
        let source = pi_coding::start_session_in(
            project,
            None,
            None,
            Some(native),
            Some(session_id),
            None,
        )
        .expect("source recorder");
        let mut entry_ids = Vec::new();
        for (index, text) in texts.iter().enumerate() {
            entry_ids.push(
                source
                    .record_message(&Message::user_text(*text, index as i64))
                    .expect("source message"),
            );
        }
        source.persist_now_durable().expect("persist source");
        let path = source.path();
        drop(source);
        (path, entry_ids)
    }

    fn user_message_texts<'a>(messages: impl IntoIterator<Item = &'a Message>) -> Vec<String> {
        messages
            .into_iter()
            .filter_map(|message| match message {
                Message::User(user) => user.content.iter().find_map(|block| match block {
                    pi_ai::ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                }),
                _ => None,
            })
            .collect()
    }

    fn tree_user_texts(tree: &pi_coding::SessionTree) -> Vec<String> {
        user_message_texts(tree.entries.iter().filter_map(|entry| entry.message.as_ref()))
    }

    fn snapshot_user_texts(data: &Value) -> Vec<String> {
        let messages: Vec<Message> =
            serde_json::from_value(data["messages"].clone()).expect("typed snapshot messages");
        user_message_texts(&messages)
    }

    async fn switch_production_session(
        manager: &Arc<SessionRuntimeManager>,
        request_id: &str,
        path: &Path,
    ) -> RpcResponse {
        let opened = manager
            .dispatch(
                RpcCommand::SwitchSession {
                    id: Some(request_id.into()),
                    session_path: path.to_string_lossy().into_owned(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                None,
            )
            .await;
        assert!(opened.success, "{}", opened.error.unwrap_or_default());
        opened
    }

    #[tokio::test]
    async fn loaded_path_rename_persists_through_exact_session_path_with_live_writer() {
        let (native, project, service, spawner) = production_manager_spawner();
        let (path, _) = persist_production_source(
            project.path(),
            native.path(),
            "exact-path-rename",
            &["keep after rename"],
        );

        let (first, _first_dir) = production_manager(spawner.clone(), "exact-path-first").await;
        let first_open = switch_production_session(&first, "first-open", &path).await;
        assert_eq!(
            first_open.data.as_ref().unwrap()["sessionId"].as_str(),
            Some("exact-path-rename")
        );
        first.shutdown_for_restart().await.expect("restart first manager");
        drop(first);

        let (recreated, _recreated_dir) =
            production_manager(spawner, "exact-path-recreated").await;
        let primary_id = recreated.primary_session_id();
        let primary_path = recreated
            .writer_info_for_test(&primary_id)
            .await
            .expect("primary writer")
            .1;
        assert_ne!(primary_path, path);
        let primary_name_before = recreated
            .resolve(Some(&primary_id))
            .await
            .expect("primary runtime")
            .application
            .session()
            .session_name();
        let opened = switch_production_session(&recreated, "reopen", &path).await;
        let session_id = opened.data.as_ref().unwrap()["sessionId"]
            .as_str()
            .expect("session id")
            .to_owned();
        assert_eq!(session_id, "exact-path-rename");
        assert_eq!(
            recreated
                .writer_info_for_test(&session_id)
                .await
                .expect("recreated writer")
                .1,
            path
        );
        let renamed = recreated
            .dispatch(
                RpcCommand::SetSessionName {
                    id: Some("rename-exact-path".into()),
                    name: "after exact path".into(),
                    session_path: Some(path.to_string_lossy().into_owned()),
                },
                Some(primary_id.clone()),
            )
            .await;
        assert!(renamed.success, "{}", renamed.error.unwrap_or_default());
        assert_eq!(
            recreated
                .resolve(Some(&primary_id))
                .await
                .expect("primary runtime")
                .application
                .session()
                .session_name(),
            primary_name_before,
            "routed source must stay unnamed when sessionPath targets another file"
        );
        assert!(
            pi_coding::load_session_tree(&primary_path)
                .expect("reload primary tree")
                .session_name()
                .is_none()
        );
        recreated.shutdown().await.expect("shutdown recreated manager");

        assert_eq!(
            pi_coding::load_session_tree(&path)
                .expect("reload renamed tree")
                .session_name()
                .as_deref(),
            Some("after exact path")
        );
        service.shutdown().await.expect("shutdown Todo service");
    }

    #[tokio::test]
    async fn production_manager_recreation_nested_fork_retains_history_and_reloads() {
        let (native, project, service, spawner) = production_manager_spawner();
        let (path, entry_ids) = persist_production_source(
            project.path(),
            native.path(),
            "nested-fork-source",
            &["first question", "second question"],
        );
        let first_id = entry_ids[0].clone();
        let selected = entry_ids[1].clone();

        let (first, _first_dir) = production_manager(spawner.clone(), "nested-first").await;
        let first_open = switch_production_session(&first, "first-open", &path).await;
        assert_eq!(
            first_open.data.as_ref().unwrap()["sessionId"].as_str(),
            Some("nested-fork-source")
        );
        first.shutdown_for_restart().await.expect("restart first manager");
        drop(first);

        let (recreated, _recreated_dir) = production_manager(spawner.clone(), "nested-recreated").await;
        let opened = switch_production_session(&recreated, "reopen", &path).await;
        let session_id = opened.data.as_ref().unwrap()["sessionId"]
            .as_str()
            .expect("session id")
            .to_owned();
        assert_eq!(session_id, "nested-fork-source");
        assert_eq!(
            recreated
                .writer_info_for_test(&session_id)
                .await
                .expect("recreated writer")
                .1,
            path
        );
        let forked = recreated
            .dispatch(
                RpcCommand::Fork {
                    id: Some("nested-fork-after-restart".into()),
                    entry_id: selected,
                    session_path: None,
                },
                Some(session_id),
            )
            .await;
        assert!(forked.success, "{}", forked.error.unwrap_or_default());
        let fork_id = forked.data.as_ref().unwrap()["sessionId"]
            .as_str()
            .expect("fork id")
            .to_owned();
        let fork_path = recreated
            .writer_info_for_test(&fork_id)
            .await
            .expect("fork writer")
            .1;
        let fork_tree = pi_coding::load_session_tree(&fork_path).expect("reload nested fork tree");
        assert_eq!(tree_user_texts(&fork_tree), vec!["first question".to_owned()]);
        assert_eq!(fork_tree.active_leaf_id.as_deref(), Some(first_id.as_str()));
        assert_eq!(
            fork_tree.header.parent_session.as_deref(),
            Some(path.to_string_lossy().as_ref())
        );
        recreated.shutdown_for_restart().await.expect("restart recreated manager");
        drop(recreated);

        let (third, _third_dir) = production_manager(spawner, "nested-third").await;
        let reopened_fork = switch_production_session(&third, "reopen-fork", &fork_path).await;
        assert_eq!(reopened_fork.data.as_ref().unwrap()["sessionId"], fork_id);
        assert_eq!(
            snapshot_user_texts(reopened_fork.data.as_ref().unwrap()),
            vec!["first question".to_owned()]
        );
        third.shutdown().await.expect("shutdown third manager");

        let reloaded = pi_coding::load_session_tree(&fork_path).expect("reload nested fork after third");
        assert_eq!(tree_user_texts(&reloaded), vec!["first question".to_owned()]);
        service.shutdown().await.expect("shutdown Todo service");
    }

    #[tokio::test]
    async fn production_header_only_parent_fork_survives_session_list_after_recreation() {
        let (native, project, service, spawner) = production_manager_spawner();
        let (path, entry_ids) = persist_production_source(
            project.path(),
            native.path(),
            "header-only-source",
            &["fork me"],
        );

        let (first, _first_dir) = production_manager(spawner.clone(), "header-only-first").await;
        let opened = switch_production_session(&first, "first-open", &path).await;
        let session_id = opened.data.as_ref().unwrap()["sessionId"]
            .as_str()
            .expect("session id")
            .to_owned();
        assert_eq!(session_id, "header-only-source");
        let forked = first
            .dispatch(
                RpcCommand::Fork {
                    id: Some("header-only-fork".into()),
                    entry_id: entry_ids[0].clone(),
                    session_path: None,
                },
                Some(session_id),
            )
            .await;
        assert!(forked.success, "{}", forked.error.unwrap_or_default());
        let fork_id = forked.data.as_ref().unwrap()["sessionId"]
            .as_str()
            .expect("fork id")
            .to_owned();
        let fork_path = first
            .writer_info_for_test(&fork_id)
            .await
            .expect("fork writer")
            .1;
        let fork_tree = pi_coding::load_session_tree(&fork_path).expect("reload header-only fork");
        assert!(
            tree_user_texts(&fork_tree).is_empty(),
            "root fork must exclude the selected prompt: {:?}",
            tree_user_texts(&fork_tree)
        );
        assert!(
            fork_tree
                .entries
                .iter()
                .filter_map(|entry| entry.message.as_ref())
                .all(|message| !matches!(message, Message::Assistant(_))),
            "root fork must not retain assistant messages"
        );
        assert_eq!(
            fork_tree.header.parent_session.as_deref(),
            Some(path.to_string_lossy().as_ref())
        );
        first.shutdown().await.expect("shutdown first manager");
        drop(first);

        let (recreated, _recreated_dir) =
            production_manager(spawner, "header-only-recreated").await;
        let listed = recreated
            .dispatch(
                RpcCommand::SessionList {
                    id: Some("list-header-only-after-recreation".into()),
                    scope: crate::modes::rpc::RpcSessionListScope::AllProjects,
                },
                None,
            )
            .await;
        assert!(listed.success, "{}", listed.error.unwrap_or_default());
        let occurrences = listed
            .data
            .as_ref()
            .and_then(|data| data["sessions"].as_array())
            .into_iter()
            .flatten()
            .filter(|row| row["sessionId"] == fork_id)
            .count();
        assert_eq!(
            occurrences, 1,
            "header-only parent fork must appear once in AllProjects after production recreation"
        );
        let reopened = switch_production_session(&recreated, "reopen-fork", &fork_path).await;
        assert_eq!(reopened.data.as_ref().unwrap()["sessionId"], fork_id);
        recreated.shutdown().await.expect("shutdown recreated manager");
        service.shutdown().await.expect("shutdown Todo service");
    }

    #[tokio::test]
    async fn first_user_fork_snapshot_is_empty_messages_with_exact_editor_text() {
        let (native, project, service, spawner) = production_manager_spawner();
        let (path, entry_ids) = persist_production_source(
            project.path(),
            native.path(),
            "first-user-fork-draft",
            &["fork me exactly"],
        );

        let (manager, _dir) = production_manager(spawner, "first-user-fork").await;
        let opened = switch_production_session(&manager, "open-source", &path).await;
        let session_id = opened.data.as_ref().unwrap()["sessionId"]
            .as_str()
            .expect("session id")
            .to_owned();
        assert!(
            opened.data.as_ref().unwrap().get("editorText").is_none(),
            "switch must omit editorText"
        );

        let forked = manager
            .dispatch(
                RpcCommand::Fork {
                    id: Some("first-user-fork".into()),
                    entry_id: entry_ids[0].clone(),
                    session_path: None,
                },
                Some(session_id),
            )
            .await;
        assert!(forked.success, "{}", forked.error.unwrap_or_default());
        let data = forked.data.as_ref().expect("fork snapshot");
        assert_eq!(snapshot_user_texts(data), Vec::<String>::new());
        assert_eq!(
            data.get("editorText").and_then(Value::as_str),
            Some("fork me exactly")
        );
        let fork_id = data["sessionId"].as_str().expect("fork id").to_owned();
        let fork_path = manager
            .writer_info_for_test(&fork_id)
            .await
            .expect("fork writer")
            .1;
        let fork_tree = pi_coding::load_session_tree(&fork_path).expect("fork tree");
        assert!(tree_user_texts(&fork_tree).is_empty());
        assert_eq!(
            fork_tree.header.parent_session.as_deref(),
            Some(path.to_string_lossy().as_ref()),
            "parentSession remains lineage only"
        );
        manager.shutdown().await.expect("shutdown manager");
        service.shutdown().await.expect("shutdown Todo service");
    }

    #[tokio::test]
    async fn nested_fork_snapshot_retains_prior_transcript_and_selected_editor_text() {
        let (native, project, service, spawner) = production_manager_spawner();
        let (path, entry_ids) = persist_production_source(
            project.path(),
            native.path(),
            "nested-fork-draft",
            &["first question", "second question"],
        );

        let (manager, _dir) = production_manager(spawner, "nested-fork-draft").await;
        let opened = switch_production_session(&manager, "open-source", &path).await;
        let session_id = opened.data.as_ref().unwrap()["sessionId"]
            .as_str()
            .expect("session id")
            .to_owned();

        let forked = manager
            .dispatch(
                RpcCommand::Fork {
                    id: Some("nested-fork-draft".into()),
                    entry_id: entry_ids[1].clone(),
                    session_path: None,
                },
                Some(session_id),
            )
            .await;
        assert!(forked.success, "{}", forked.error.unwrap_or_default());
        let data = forked.data.as_ref().expect("fork snapshot");
        assert_eq!(
            snapshot_user_texts(data),
            vec!["first question".to_owned()]
        );
        assert_eq!(
            data.get("editorText").and_then(Value::as_str),
            Some("second question")
        );
        manager.shutdown().await.expect("shutdown manager");
        service.shutdown().await.expect("shutdown Todo service");
    }

    #[tokio::test]
    async fn clone_and_switch_snapshots_omit_editor_text() {
        let (native, project, service, spawner) = production_manager_spawner();
        let (path, _) = persist_production_source(
            project.path(),
            native.path(),
            "clone-switch-omit-draft",
            &["keep me"],
        );

        let (manager, _dir) = production_manager(spawner, "clone-switch-omit").await;
        let opened = switch_production_session(&manager, "open-source", &path).await;
        let data = opened.data.as_ref().expect("switch snapshot");
        assert!(data.get("editorText").is_none(), "{data}");
        let session_id = data["sessionId"].as_str().expect("session id").to_owned();

        let cloned = manager
            .dispatch(
                RpcCommand::Clone {
                    id: Some("clone-omit-draft".into()),
                    session_path: None,
                },
                Some(session_id.clone()),
            )
            .await;
        assert!(cloned.success, "{}", cloned.error.unwrap_or_default());
        let cloned_data = cloned.data.as_ref().expect("clone snapshot");
        assert_eq!(
            snapshot_user_texts(cloned_data),
            vec!["keep me".to_owned()]
        );
        assert!(
            cloned_data.get("editorText").is_none(),
            "{cloned_data}"
        );

        let fresh = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: Some("new-omit-draft".into()),
                    parent_session: None,
                    cwd: None,
                },
                Some(session_id),
            )
            .await;
        assert!(fresh.success, "{}", fresh.error.unwrap_or_default());
        assert!(
            fresh.data.as_ref().unwrap().get("editorText").is_none(),
            "{:?}",
            fresh.data
        );
        manager.shutdown().await.expect("shutdown manager");
        service.shutdown().await.expect("shutdown Todo service");
    }


    #[tokio::test]
    async fn production_new_explicit_cwd_envelope_routes_immediate_fork_to_target() {
        // User-flow regression: active A (cwd A) -> Manage New with explicit
        // cwd B -> New succeeds -> immediate Fork. The success response's
        // TOP-LEVEL sessionId must own the TARGET runtime (never the stale
        // source A) so the panel routes the immediate fork to B, and the
        // payload, state, and authoritative session row must all identify B
        // so the child row can be injected without a catalog refresh.
        let native = tempfile::tempdir().expect("native root");
        let cli = <crate::args::Cli as clap::Parser>::try_parse_from([
            "rpi",
            "--api-key",
            "test-key",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
        ])
        .expect("cli");
        let service = TodoChildService::new();
        let spawner = crate::session_run::RunSessionSpawner::from_startup_with_service(
            &cli,
            crate::session_run::SessionStorage::DefaultTree {
                native_root: native.path().to_path_buf(),
            },
            service.clone(),
        );
        let (model, registration) = faux_model("fork-lifecycle-target");
        let primary_root = tempfile::tempdir().expect("primary root");
        let session =
            isolated_session_with_auth_owner(model.clone(), primary_root.path(), true).await;
        let session_dir = session.session_dir();
        let recorder = pi_coding::start_session_in(
            &session_dir,
            Some(&model),
            Some("off"),
            Some(primary_root.path()),
            None,
            None,
        )
        .expect("primary recorder");
        recorder.persist_now().expect("persist primary header");
        session.record(recorder).expect("record primary");
        let primary = isolated_application(session).await;
        let manager = SessionRuntimeManager::new(
            primary,
            ExtensionUiAdapter::default(),
            Some(Arc::new(spawner)),
            None,
        )
        .await
        .expect("production manager");

        let source_id = manager.primary_session_id();
        let source_cwd = manager.primary.application.session().cwd().to_path_buf();
        let (_, source_file, _) = manager
            .writer_info_for_test(&source_id)
            .await
            .expect("source writer");
        let source_tree_before =
            pi_coding::load_session_tree(&source_file).expect("load source tree");

        // New with explicit cwd B from active A.
        let target = tempfile::tempdir().expect("target cwd");
        let opened = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: Some("new-explicit-b".into()),
                    parent_session: None,
                    cwd: Some(target.path().to_string_lossy().into_owned()),
                },
                Some(source_id.clone()),
            )
            .await;
        assert!(opened.success, "{}", opened.error.unwrap_or_default());
        let data = opened.data.as_ref().expect("new snapshot");
        let target_id = opened
            .session_id
            .as_deref()
            .expect("success envelope must carry the target sessionId")
            .to_owned();
        assert_ne!(target_id, source_id);
        assert_eq!(data["sessionId"].as_str(), Some(target_id.as_str()));
        assert_eq!(data["state"]["sessionId"].as_str(), Some(target_id.as_str()));
        assert_eq!(
            data["state"]["cwd"].as_str(),
            Some(target.path().to_string_lossy().as_ref()),
            "target state cwd must be B"
        );
        let target_incarnation = data["runtimeIncarnation"]
            .as_str()
            .expect("target incarnation")
            .to_owned();
        assert!(!target_incarnation.is_empty());
        let row = &data["session"];
        assert_eq!(row["source"], "rpi");
        assert_eq!(row["sessionId"].as_str(), Some(target_id.as_str()));
        assert_eq!(
            row["cwd"].as_str(),
            Some(target.path().to_string_lossy().as_ref())
        );
        assert_eq!(row["status"], "native");
        assert_eq!(row["loaded"], true);
        assert_eq!(
            row["runtimeIncarnation"].as_str(),
            Some(target_incarnation.as_str())
        );
        let target_file =
            PathBuf::from(row["path"].as_str().expect("target session file path"));
        assert_eq!(
            data["state"]["sessionFile"].as_str(),
            Some(target_file.to_string_lossy().as_ref()),
            "row path must be the exact target session file"
        );
        // The fresh target publishes into the cwd-encoded native tree for B.
        assert_eq!(
            target_file.parent(),
            Some(pi_coding::default_session_dir(target.path()).as_path()),
            "New target must publish into the cwd-encoded native tree for B"
        );
        assert_eq!(
            manager
                .writer_info_for_test(&target_id)
                .await
                .expect("target writer")
                .1,
            target_file,
            "session row path must match the live target recorder"
        );

        // Seed B with one forkable user entry (a real prompt on the target
        // runtime), exactly the prerequisite of an immediate fork.
        registration.append_response(FauxResponse::text("target reply"));
        let prompted = manager
            .dispatch(
                RpcCommand::Prompt {
                    id: Some("seed-target".into()),
                    message: "seed prompt for fork".into(),
                    images: Vec::new(),
                    streaming_behavior: None,
                },
                Some(target_id.clone()),
            )
            .await;
        assert!(prompted.success, "{}", prompted.error.unwrap_or_default());
        let target_runtime = manager
            .resolve(Some(&target_id))
            .await
            .expect("target runtime");
        let mut entry_id = None;
        for _ in 0..200 {
            if let Ok(entries) = target_runtime
                .application
                .session()
                .session_entries(None)
            {
                if let Some(entry) = entries
                    .entries
                    .iter()
                    .rev()
                    .find(|entry| matches!(entry.message, Some(Message::User(_))))
                {
                    entry_id = Some(entry.id.clone());
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let entry_id = entry_id.expect("target user entry recorded");

        // Immediate Fork routed by the TOP-LEVEL sessionId returned by New.
        let forked = manager
            .dispatch(
                RpcCommand::Fork {
                    id: Some("fork-from-target".into()),
                    entry_id,
                    session_path: None,
                },
                Some(target_id.clone()),
            )
            .await;
        assert!(forked.success, "{}", forked.error.unwrap_or_default());
        let fork_data = forked.data.as_ref().expect("fork snapshot");
        let fork_id = forked
            .session_id
            .as_deref()
            .expect("fork envelope must carry the child sessionId")
            .to_owned();
        assert_ne!(fork_id, target_id);
        assert_eq!(fork_data["sessionId"].as_str(), Some(fork_id.as_str()));
        assert_eq!(
            fork_data["state"]["sessionId"].as_str(),
            Some(fork_id.as_str())
        );
        assert_eq!(
            fork_data["state"]["cwd"].as_str(),
            Some(target.path().to_string_lossy().as_ref()),
            "child state cwd must be B"
        );
        let fork_row = &fork_data["session"];
        assert_eq!(fork_row["source"], "rpi");
        assert_eq!(fork_row["sessionId"].as_str(), Some(fork_id.as_str()));
        assert_eq!(
            fork_row["cwd"].as_str(),
            Some(target.path().to_string_lossy().as_ref())
        );
        assert_eq!(fork_row["status"], "native");
        assert_eq!(fork_row["loaded"], true);
        let fork_file =
            PathBuf::from(fork_row["path"].as_str().expect("fork session file path"));
        assert_eq!(
            fork_data["state"]["sessionFile"].as_str(),
            Some(fork_file.to_string_lossy().as_ref())
        );
        // The child publishes into the cwd-encoded native tree for B
        // (the production spawner's native root keyed by cwd B).
        let encoded_target = pi_coding::default_session_dir(target.path())
            .file_name()
            .expect("encoded target directory")
            .to_owned();
        assert_eq!(
            fork_file.parent(),
            Some(native.path().join(encoded_target).as_path()),
            "immediate fork of B must publish under B's encoded native tree"
        );
        assert_eq!(
            manager
                .writer_info_for_test(&fork_id)
                .await
                .expect("fork writer")
                .1,
            fork_file,
            "fork session row path must match the live child recorder"
        );
        let fork_tree = pi_coding::load_session_tree(&fork_file).expect("reload fork tree");
        assert_eq!(fork_tree.header.id, fork_id);
        assert_eq!(
            fork_tree.header.cwd,
            target.path(),
            "child header cwd must be B"
        );
        assert_eq!(
            fork_tree.header.parent_session.as_deref(),
            Some(target_file.to_string_lossy().as_ref()),
            "child parent lineage must point at B's session file"
        );

        // A is untouched: same cwd, same session file, same durable tree.
        assert_eq!(
            manager.primary.application.session().cwd(),
            source_cwd.as_path()
        );
        assert_eq!(
            manager
                .writer_info_for_test(&source_id)
                .await
                .expect("source writer after fork")
                .1,
            source_file
        );
        let source_tree_after =
            pi_coding::load_session_tree(&source_file).expect("reload source tree after fork");
        assert_eq!(source_tree_after.header, source_tree_before.header);
        assert_eq!(
            source_tree_after
                .entries
                .iter()
                .filter_map(|entry| entry.message.as_ref())
                .count(),
            source_tree_before
                .entries
                .iter()
                .filter_map(|entry| entry.message.as_ref())
                .count(),
        );
        manager.shutdown().await.expect("shutdown manager");
        service.shutdown().await.expect("shutdown Todo service");
    }

    #[tokio::test]
    async fn path_targeted_empty_and_image_only_user_fork_yields_empty_editor_text() {
        let (native, project, service, spawner) = production_manager_spawner();
        let source = pi_coding::start_session_in(
            project.path(),
            None,
            None,
            Some(native.path()),
            Some("empty-image-fork"),
            None,
        )
        .expect("selected recorder");
        let empty_entry = source
            .record_message(&Message::user_text("", 0))
            .expect("empty user");
        let image_entry = source
            .record_message(&Message::User(pi_ai::UserMessage {
                content: vec![pi_ai::ContentBlock::Image {
                    data: "aW1n".into(),
                    mime_type: "image/png".into(),
                }],
                timestamp: 1,
            }))
            .expect("image-only user");
        source.persist_now_durable().expect("persist selected session");
        let selected_path = source.path();
        drop(source);

        let (manager, _dir) = production_manager(spawner, "empty-image-fork").await;
        for (request_id, entry_id, expected_user_texts) in [
            ("empty-text-fork", empty_entry, Vec::<String>::new()),
            ("image-only-fork", image_entry, vec![String::new()]),
        ] {
            let forked = manager
                .dispatch(
                    RpcCommand::Fork {
                        id: Some(request_id.into()),
                        entry_id,
                        session_path: Some(selected_path.to_string_lossy().into_owned()),
                    },
                    None,
                )
                .await;
            assert!(forked.success, "{}", forked.error.unwrap_or_default());
            let data = forked.data.as_ref().expect("fork snapshot");
            assert_eq!(data.get("editorText").and_then(Value::as_str), Some(""));
            assert_eq!(snapshot_user_texts(data), expected_user_texts);
            let fork_id = data["sessionId"].as_str().expect("fork id");
            let fork_path = manager
                .writer_info_for_test(fork_id)
                .await
                .expect("fork writer")
                .1;
            let fork_tree = pi_coding::load_session_tree(&fork_path).expect("fork tree");
            assert_eq!(tree_user_texts(&fork_tree), expected_user_texts);
            assert_eq!(
                fork_tree.header.parent_session.as_deref(),
                Some(selected_path.to_string_lossy().as_ref())
            );
        }
        manager.shutdown().await.expect("shutdown manager");
        service.shutdown().await.expect("shutdown Todo service");
    }

    #[tokio::test]
    async fn fork_editor_text_stays_on_originating_response_under_concurrent_switch() {
        let (native, project, service, spawner) = production_manager_spawner();
        let (path, entry_ids) = persist_production_source(
            project.path(),
            native.path(),
            "owned-draft-source",
            &["keep me", "fork me exactly"],
        );

        let (manager, _dir) = production_manager(spawner, "owned-draft").await;
        let opened = switch_production_session(&manager, "open-source", &path).await;
        let session_id = opened.data.as_ref().unwrap()["sessionId"]
            .as_str()
            .expect("session id")
            .to_owned();

        let fork_manager = manager.clone();
        let switch_manager = manager.clone();
        let fork_session = session_id.clone();
        let switch_path = path.clone();
        let selected = entry_ids[1].clone();
        let (forked, switched) = tokio::join!(
            async move {
                fork_manager
                    .dispatch(
                        RpcCommand::Fork {
                            id: Some("owned-draft-fork".into()),
                            entry_id: selected,
                            session_path: None,
                        },
                        Some(fork_session),
                    )
                    .await
            },
            async move {
                switch_manager
                    .dispatch(
                        RpcCommand::SwitchSession {
                            id: Some("owned-draft-switch".into()),
                            session_path: switch_path.to_string_lossy().into_owned(),
                            catalog_source: None,
                            catalog_session_id: None,
                            catalog_status: None,
                            response_mode: None,
                        },
                        None,
                    )
                    .await
            },
        );
        assert!(forked.success, "{}", forked.error.unwrap_or_default());
        assert!(switched.success, "{}", switched.error.unwrap_or_default());
        let fork_data = forked.data.as_ref().expect("fork snapshot");
        assert_eq!(
            snapshot_user_texts(fork_data),
            vec!["keep me".to_owned()]
        );
        assert_eq!(
            fork_data.get("editorText").and_then(Value::as_str),
            Some("fork me exactly")
        );
        let switch_data = switched.data.as_ref().expect("switch snapshot");
        assert!(
            switch_data.get("editorText").is_none(),
            "{switch_data}"
        );
        manager.shutdown().await.expect("shutdown manager");
        service.shutdown().await.expect("shutdown Todo service");
    }

    #[tokio::test]
    async fn loaded_path_rename_fails_without_writer_and_preserves_name() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let opened = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: Some("rename-target".into()),
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(opened.success, "{}", opened.error.unwrap_or_default());
        let session_id = opened.data.as_ref().unwrap()["sessionId"]
            .as_str()
            .expect("session id")
            .to_owned();
        let path = manager
            .writer_info_for_test(&session_id)
            .await
            .expect("writer")
            .1;
        let held = path.with_extension("held.jsonl");
        std::fs::rename(&path, &held).expect("move exact writer inode");
        let replacement = std::fs::read(&held).expect("read held journal");
        std::fs::write(&path, replacement).expect("write replacement journal");
        assert!(manager.writer_info_for_test(&session_id).await.is_err());

        let renamed = manager
            .dispatch(
                RpcCommand::SetSessionName {
                    id: Some("rename-dead-writer".into()),
                    name: "must not persist".into(),
                    session_path: Some(path.to_string_lossy().into_owned()),
                },
                Some(session_id.clone()),
            )
            .await;
        assert!(!renamed.success);
        assert!(
            manager
                .resolve(Some(&session_id))
                .await
                .expect("runtime")
                .application
                .session()
                .session_name()
                .is_none()
        );
        assert!(
            pi_coding::load_session_tree(&held)
                .expect("reload held")
                .session_name()
                .is_none()
        );
        assert!(
            pi_coding::load_session_tree(&path)
                .expect("reload replacement")
                .session_name()
                .is_none()
        );
        manager.shutdown().await.expect("shutdown manager");
    }

    #[tokio::test]
    async fn open_registration_failure_never_discards_existing_journal() {
        let inner = TestSpawner::default();
        let spawner = Arc::new(IdentityMismatchSpawner {
            inner,
            mismatches_remaining: Arc::new(AtomicUsize::new(1)),
        });
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let directory = tempfile::tempdir().expect("existing session directory");
        let recorder = pi_coding::start_session_in(
            directory.path(),
            None,
            None,
            Some(directory.path()),
            Some("existing-open"),
            None,
        )
        .expect("existing recorder");
        recorder
            .record_message(&Message::user_text("must survive", 0))
            .expect("existing message");
        recorder.persist_now_durable().expect("persist existing");
        let path = recorder.path();
        let original = std::fs::read(&path).expect("read original");
        drop(recorder);

        let failed = manager
            .dispatch(
                RpcCommand::SwitchSession {
                    id: Some("open-mismatch".into()),
                    session_path: path.to_string_lossy().into_owned(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                None,
            )
            .await;
        assert!(!failed.success);
        assert_eq!(std::fs::read(&path).expect("read surviving journal"), original);
        assert_eq!(manager.registry.read().await.distinct_runtimes().len(), 1);
        manager.shutdown().await.expect("shutdown manager");
    }

    #[tokio::test]
    async fn concurrent_failed_forks_roll_back_each_identity_without_publication() {
        let inner = TestSpawner::default();
        let spawner = Arc::new(IdentityMismatchSpawner {
            inner: inner.clone(),
            mismatches_remaining: Arc::new(AtomicUsize::new(2)),
        });
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let permits = manager.loaded_slots.available_permits();
        let fork = |id: &'static str| {
            let manager = manager.clone();
            tokio::spawn(async move {
                manager
                    .dispatch(
                        RpcCommand::Fork {
                            id: Some(id.into()),
                            entry_id: "ignored-by-test-spawner".into(),
                            session_path: None,
                        },
                        None,
                    )
                    .await
            })
        };
        let (first, second) = tokio::join!(fork("fork-one"), fork("fork-two"));
        assert!(!first.expect("first fork task").success);
        assert!(!second.expect("second fork task").success);
        assert_eq!(manager.loaded_slots.available_permits(), permits);
        assert_eq!(manager.registry.read().await.distinct_runtimes().len(), 1);
        let paths = inner
            .spawned_paths
            .lock()
            .expect("spawned paths")
            .clone();
        assert_eq!(paths.len(), 2);
        assert!(paths.iter().all(|path| !path.exists()));
        manager.shutdown().await.expect("shutdown manager");
    }


    #[tokio::test]
    async fn path_targeted_fork_excludes_prompt_and_clone_preserves_leaf() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let directory = tempfile::tempdir().expect("selected session directory");
        let (model, _registration) = faux_model("selected-lifecycle");
        let recorder = pi_coding::start_session_in(
            directory.path(),
            Some(&model),
            Some("off"),
            Some(directory.path()),
            Some("selected-lifecycle"),
            None,
        )
        .expect("selected recorder");
        let selected_entry = recorder
            .record_message(&Message::user_text("selected prompt", 0))
            .expect("selected user message");
        recorder.persist_now().expect("persist selected session");
        let selected_path = recorder.path();
        recorder.close().expect("close selected recorder");

        let messages = manager
            .dispatch(
                RpcCommand::GetForkMessages {
                    id: None,
                    session_path: Some(selected_path.to_string_lossy().into_owned()),
                    cursor: None,
                    limit: None,
                },
                None,
            )
            .await;
        assert!(messages.success, "{}", messages.error.unwrap_or_default());
        assert_eq!(
            messages.data.as_ref().unwrap()["messages"][0]["text"],
            "selected prompt"
        );

        let forked = manager
            .dispatch(
                RpcCommand::SessionForkLatest {
                    id: None,
                    session_path: selected_path.to_string_lossy().into_owned(),
                },
                None,
            )
            .await;
        assert!(forked.success, "{}", forked.error.unwrap_or_default());
        assert_eq!(
            forked.data.as_ref().unwrap().get("editorText").and_then(Value::as_str),
            Some("selected prompt")
        );
        assert_eq!(
            snapshot_user_texts(forked.data.as_ref().unwrap()),
            Vec::<String>::new()
        );
        let forked_id = forked.data.as_ref().unwrap()["sessionId"]
            .as_str()
            .expect("forked session id");
        let forked_runtime = manager
            .resolve(Some(forked_id))
            .await
            .expect("forked runtime");
        assert!(forked_runtime.application.messages().is_empty());

        let cloned = manager
            .dispatch(
                RpcCommand::Clone {
                    id: None,
                    session_path: Some(selected_path.to_string_lossy().into_owned()),
                },
                None,
            )
            .await;
        assert!(cloned.success, "{}", cloned.error.unwrap_or_default());
        assert!(
            cloned.data.as_ref().unwrap().get("editorText").is_none(),
            "clone must omit editorText"
        );
        let cloned_id = cloned.data.as_ref().unwrap()["sessionId"]
            .as_str()
            .expect("cloned session id");
        let cloned_runtime = manager
            .resolve(Some(cloned_id))
            .await
            .expect("cloned runtime");
        assert_eq!(cloned_runtime.application.messages().len(), 1);
        assert_eq!(
            cloned_runtime
                .application
                .session()
                .session_tree()
                .expect("clone tree")
                .active_leaf_id
                .as_deref(),
            Some(selected_entry.as_str()),
        );
        assert!(spawner.opened.lock().expect("opened sessions").is_empty());
        drop(forked_runtime);
        drop(cloned_runtime);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn session_delete_rejects_path_targeted_active_runtime() {
        let (primary_application, primary_id, _primary_dir) =
            recorded_application("active-delete").await;
        let active_path = primary_application
            .session()
            .recorder_info()
            .expect("active recorder")
            .1;
        let manager = SessionRuntimeManager::new(
            primary_application,
            ExtensionUiAdapter::default(),
            Some(Arc::new(TestSpawner::default())),
            None,
        )
        .await
        .expect("session runtime manager");
        let response = manager
            .dispatch(
                RpcCommand::SessionDelete {
                    id: None,
                    session_path: active_path.to_string_lossy().into_owned(),
                },
                Some(primary_id),
            )
            .await;
        assert!(!response.success);
        assert!(response.error.unwrap().contains("active session"));
        assert!(active_path.exists());
        manager.shutdown().await;
        manager.primary.application.cleanup().await;
    }

    #[tokio::test]
    async fn session_delete_returns_undo_and_restore_preserves_archive_on_conflict() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let agent = tempfile::tempdir().expect("archive agent base");
        let cwd = tempfile::tempdir().expect("archive cwd");
        std::fs::create_dir(cwd.path().join(".git")).expect("archive git anchor");
        let directory = agent.path().join("sessions").join("--archive--");
        std::fs::create_dir_all(&directory).expect("archive session directory");
        let (model, _registration) = faux_model("archive-lifecycle");
        let recorder = pi_coding::start_session_in(
            cwd.path(),
            Some(&model),
            Some("off"),
            Some(&directory),
            Some("archive-lifecycle"),
            None,
        )
        .expect("archive recorder");
        recorder
            .record_message(&Message::user_text("archive prompt", 0))
            .expect("archive message");
        recorder.persist_now().expect("persist archive session");
        let path = recorder.path();
        recorder.close().expect("close archive recorder");

        let deleted = manager
            .dispatch(
                RpcCommand::SessionDelete {
                    id: None,
                    session_path: path.to_string_lossy().into_owned(),
                },
                None,
            )
            .await;
        assert!(deleted.success, "{}", deleted.error.unwrap_or_default());
        assert!(!path.exists());
        let token = deleted.data.as_ref().unwrap()["undoToken"]
            .as_str()
            .expect("undo token")
            .to_owned();

        std::fs::write(&path, b"conflict").expect("restore conflict");
        let conflict = manager
            .dispatch(
                RpcCommand::SessionRestore {
                    id: None,
                    undo_token: token.clone(),
                },
                None,
            )
            .await;
        assert!(!conflict.success);
        std::fs::remove_file(&path).expect("remove conflict");

        let restored = manager
            .dispatch(
                RpcCommand::SessionRestore {
                    id: None,
                    undo_token: token,
                },
                None,
            )
            .await;
        assert!(restored.success, "{}", restored.error.unwrap_or_default());
        assert!(path.exists());
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn failed_delete_preflight_keeps_loaded_secondary_registered() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let opened = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(opened.success, "{}", opened.error.unwrap_or_default());
        let opened_data = opened.data.expect("opened data");
        let session_id = opened_data["sessionId"]
            .as_str()
            .expect("session id")
            .to_owned();
        let runtime = manager.resolve(Some(&session_id)).await.expect("runtime");
        let session_path = runtime
            .application
            .session()
            .recorder_info()
            .expect("recorder")
            .1;
        let children = session_path
            .parent()
            .expect("session directory")
            .join("children")
            .join(&session_id);
        std::fs::create_dir_all(children).expect("durable child guard");

        let deleted = manager
            .dispatch(
                RpcCommand::SessionDelete {
                    id: None,
                    session_path: session_path.to_string_lossy().into_owned(),
                },
                None,
            )
            .await;
        assert!(!deleted.success);
        let state = manager
            .dispatch(RpcCommand::GetState { id: None }, Some(session_id))
            .await;
        assert!(
            state.success,
            "loaded secondary remains routable after failed preflight"
        );
        assert!(session_path.exists());
        drop(runtime);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn update_identity_removes_stale_file_alias() {
        let (model, _registration) = faux_model("identity-cutover");
        let directory = tempfile::tempdir().expect("identity session directory");
        let session = isolated_session(model.clone(), directory.path()).await;
        let recorder = pi_coding::start_session_in(
            directory.path(),
            Some(&model),
            Some("off"),
            Some(directory.path()),
            Some("identity-cutover"),
            None,
        )
        .expect("identity recorder");
        recorder
            .record_message(&Message::user_text("identity cutover", 0))
            .expect("cutover message");
        recorder.persist_now().expect("persist cutover session");
        session.record(recorder).expect("attach cutover recorder");
        let application = isolated_application(session).await;
        let manager = SessionRuntimeManager::new(
            application,
            ExtensionUiAdapter::default(),
            Some(Arc::new(TestSpawner::default())),
            None,
        )
        .await
        .expect("session runtime manager");
        let runtime = manager.primary.clone();
        let old_identity = runtime.current_file_identity().expect("old file identity");
        let outcome = runtime
            .application
            .clone_session()
            .await
            .expect("clone cutover");
        assert!(!outcome.cancelled);
        let new_identity = runtime.current_file_identity().expect("new file identity");
        assert_ne!(old_identity, new_identity);

        manager.update_identity(&runtime).await;
        let registry = manager.registry.read().await;
        assert!(!registry.by_file.contains_key(&old_identity));
        assert!(
            registry
                .by_file
                .get(&new_identity)
                .is_some_and(|candidate| Arc::ptr_eq(candidate, &runtime))
        );
        assert!(registry.by_id.contains_key("identity-cutover"));
        drop(registry);
        manager.shutdown().await;
        manager.primary.application.cleanup().await;
    }

    #[tokio::test]
    async fn identity_update_cannot_reinsert_unregistered_runtime() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let opened = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(opened.success, "{}", opened.error.unwrap_or_default());
        let session_id = opened.data.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        let runtime = manager.resolve(Some(&session_id)).await.expect("runtime");

        manager.unregister_runtime(&runtime).await;
        manager.update_identity(&runtime).await;

        let registry = manager.registry.read().await;
        assert!(!registry.contains(&runtime));
        assert!(!registry.by_id.contains_key(&session_id));
        assert!(
            runtime
                .current_file_identity()
                .is_none_or(|identity| !registry.by_file.contains_key(&identity))
        );
        drop(registry);
        manager.shutdown().await;
    }
    #[tokio::test]
    async fn archive_failure_reopens_loaded_secondary() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let agent = tempfile::tempdir().expect("archive agent base");
        let cwd = tempfile::tempdir().expect("archive cwd");
        let directory = agent.path().join("sessions").join("--archive-failure--");
        let (model, _registration) = faux_model("archive-recovery");
        let recorder = pi_coding::start_session_in(
            cwd.path(),
            Some(&model),
            Some("off"),
            Some(&directory),
            Some("secondary-archive-recovery"),
            None,
        )
        .expect("archive failure recorder");
        recorder
            .persist_now()
            .expect("persist archive failure session");
        let session_path = recorder.path();
        recorder.close().expect("close archive failure recorder");
        let digest = Sha256::digest(cwd.path().as_os_str().as_encoded_bytes());
        let repo_digest = digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let archive_directory = agent.path().join("sessions-archived").join(repo_digest);
        std::fs::create_dir_all(&archive_directory).expect("archive directory");
        let primary_name = session_path.file_name().expect("session file name");
        for attempt in 0..=1000 {
            let mut collision = primary_name.to_os_string();
            if attempt > 0 {
                collision.push(format!(".{attempt}"));
            }
            std::fs::create_dir(archive_directory.join(collision))
                .expect("blocking archive collision");
        }

        let unopened_delete = manager
            .dispatch(
                RpcCommand::SessionDelete {
                    id: None,
                    session_path: session_path.to_string_lossy().into_owned(),
                },
                None,
            )
            .await;
        assert!(!unopened_delete.success);
        assert!(
            unopened_delete
                .error
                .unwrap()
                .contains("collision limit exceeded")
        );
        assert!(session_path.exists());
        assert!(spawner.opened.lock().expect("opened sessions").is_empty());

        let opened = manager
            .dispatch(
                RpcCommand::SwitchSession {
                    id: None,
                    session_path: session_path.to_string_lossy().into_owned(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                None,
            )
            .await;
        assert!(opened.success, "{}", opened.error.unwrap_or_default());
        let session_id = opened.data.as_ref().unwrap()["sessionId"]
            .as_str()
            .expect("session id")
            .to_owned();

        let deleted = manager
            .dispatch(
                RpcCommand::SessionDelete {
                    id: None,
                    session_path: session_path.to_string_lossy().into_owned(),
                },
                None,
            )
            .await;
        assert!(!deleted.success);
        assert!(deleted.error.unwrap().contains("collision limit exceeded"));
        let reopened = manager
            .resolve(Some(&session_id))
            .await
            .expect("reopened runtime");
        assert_eq!(
            reopened
                .application
                .session()
                .recorder_info()
                .expect("reopened recorder")
                .1,
            session_path,
        );
        assert!(session_path.exists());
        assert_eq!(spawner.opened.lock().expect("opened sessions").len(), 2);
        drop(reopened);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn session_move_moves_unloaded_native_row_without_spawning_and_replays_retry() {
        let (_home, native, catalog) = web_catalog_fixture();
        let cwd = tempfile::tempdir().expect("source cwd");
        let (path, _) = persist_production_source(
            cwd.path(),
            native.path(),
            "move-success",
            &["prompt before move"],
        );
        let target = tempfile::tempdir().expect("move target");
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog));
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;

        let moved = manager
            .dispatch(
                RpcCommand::SessionMove {
                    id: Some("move".into()),
                    session_path: path.to_string_lossy().into_owned(),
                    target_cwd: target.path().to_string_lossy().into_owned(),
                },
                None,
            )
            .await;
        assert!(moved.success, "{}", moved.error.unwrap_or_default());
        let data = moved.data.expect("move data");
        let destination = PathBuf::from(data["to"].as_str().expect("destination path"));
        assert_eq!(data["moved"], json!(true));
        assert_eq!(data["sessionId"], json!("move-success"));
        assert_eq!(data["from"], json!(path.to_string_lossy()));
        assert_eq!(
            data["session"]["path"],
            json!(destination.to_string_lossy()),
            "response carries the destination catalog row"
        );
        assert_eq!(
            destination.parent(),
            Some(pi_coding::default_session_dir(target.path()).as_path()),
            "destination publishes into the target's native session store"
        );
        assert!(!path.exists(), "source row removed");
        assert!(destination.exists(), "destination published");
        assert_eq!(
            spawner.spawns.load(Ordering::SeqCst),
            0,
            "move never spawns a runtime"
        );
        assert!(spawner.opened.lock().expect("opened sessions").is_empty());

        // Identical retry replays the recorded outcome (the original path no
        // longer resolves).
        let replay = manager
            .dispatch(
                RpcCommand::SessionMove {
                    id: Some("move-retry".into()),
                    session_path: path.to_string_lossy().into_owned(),
                    target_cwd: target.path().to_string_lossy().into_owned(),
                },
                None,
            )
            .await;
        assert!(replay.success, "{}", replay.error.unwrap_or_default());
        assert_eq!(
            replay.data.as_ref().unwrap()["to"],
            json!(destination.to_string_lossy())
        );
        assert_eq!(
            replay.data.as_ref().unwrap()["moved"],
            json!(true),
            "replayed moves stay moved"
        );

        // Retry naming the destination row itself: already resident.
        let resident = manager
            .dispatch(
                RpcCommand::SessionMove {
                    id: Some("move-resident".into()),
                    session_path: destination.to_string_lossy().into_owned(),
                    target_cwd: target.path().to_string_lossy().into_owned(),
                },
                None,
            )
            .await;
        assert!(resident.success, "{}", resident.error.unwrap_or_default());
        assert_eq!(resident.data.as_ref().unwrap()["moved"], json!(false));

        let _ = std::fs::remove_dir_all(pi_coding::default_session_dir(target.path()));
        manager.shutdown().await;
    }

    /// Builds the cross-directory cutover candidate for the active-session
    /// move test: a bare isolated session at the target cwd that installs
    /// the moved destination's resume recorder — the same shape the
    /// production blueprint produces for a cross-directory `switch_session`.
    struct ActiveMoveRuntimeFactory;

    impl pi_coding::ApplicationRuntimeFactory for ActiveMoveRuntimeFactory {
        fn build_runtime_candidate(
            &self,
            cwd: PathBuf,
            options: SessionOptions,
            resume: Option<pi_coding::PreparedSessionResume>,
        ) -> pi_coding::ApplicationRuntimeFuture {
            Box::pin(async move {
                let session = Session::new_with_additional_tools_filtered_and_discovery(
                    options,
                    Vec::new(),
                    ToolSelection::default(),
                    ResourceDiscovery::Disabled,
                )?;
                session.set_session_dir(pi_coding::default_session_dir(&cwd));
                session.attach_resources(isolated_resources(&cwd)).await?;
                if let Some(resume) = resume {
                    let context = resume.build_context();
                    let recorder = resume.into_recorder()?;
                        session.resume_recording(recorder, context.messages).await?;
                    }
                Ok(pi_coding::ApplicationRuntimeCandidate::new(session))
            })
        }
    }

    /// Park the active-session move INSIDE its cutover while it holds the
    /// file-conversation fence: the runtime factory future signals entry
    /// and awaits release before building the cross-directory candidate.
    struct GatedCutoverFactory {
        entered: CancellationToken,
        release: CancellationToken,
    }

    impl pi_coding::ApplicationRuntimeFactory for GatedCutoverFactory {
        fn build_runtime_candidate(
            &self,
            cwd: PathBuf,
            options: SessionOptions,
            resume: Option<pi_coding::PreparedSessionResume>,
        ) -> pi_coding::ApplicationRuntimeFuture {
            let entered = self.entered.clone();
            let release = self.release.clone();
            Box::pin(async move {
                entered.cancel();
                release.cancelled().await;
                let session = Session::new_with_additional_tools_filtered_and_discovery(
                    options,
                    Vec::new(),
                    ToolSelection::default(),
                    ResourceDiscovery::Disabled,
                )?;
                session.set_session_dir(pi_coding::default_session_dir(&cwd));
                session.attach_resources(isolated_resources(&cwd)).await?;
                if let Some(resume) = resume {
                    let context = resume.build_context();
                    let recorder = resume.into_recorder()?;
                    session.resume_recording(recorder, context.messages).await?;
                }
                Ok(pi_coding::ApplicationRuntimeCandidate::new(session))
            })
        }
    }

    #[tokio::test]
    async fn session_move_moves_the_idle_active_session_and_rejects_busy_one() {
        let (_home, _native, catalog) = web_catalog_fixture();
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog));
        // The busy window is HELD, never timed: the unpaced faux stream
        // completes an entire 200k turn within one scheduling burst after
        // prompt admission, which raced the busy probe and let a busy active
        // move slip through as a success. The gated primary stream keeps the
        // turn verifiably in flight until the test releases it.
        let release = CancellationToken::new();
        let (manager, _dir) =
            manager_with_gated_primary_stream(Some(spawner.clone()), release.clone(), "held busy turn")
                .await;
        manager
            .primary
            .application
            .attach_runtime_factory(Arc::new(ActiveMoveRuntimeFactory))
            .expect("attach active-move runtime factory");
        let (active_session_id, active_path) = manager
            .primary
            .application
            .session()
            .recorder_info()
            .expect("active recorder");
        let active_id = manager.primary_session_id().to_owned();
        assert_eq!(active_session_id, active_id);
        let old_identity = manager.primary.current_file_identity().expect("active identity");
        let target = tempfile::tempdir().expect("move target");
        let move_command = |tag: &str| RpcCommand::SessionMove {
            id: Some(tag.into()),
            session_path: active_path.to_string_lossy().into_owned(),
            target_cwd: target.path().to_string_lossy().into_owned(),
        };

        // Busy: a turn is verifiably in flight on the PRIMARY when the move
        // is attempted; the fan-in subscription precedes the prompt dispatch
        // so the turn_started frame can never be missed by a late subscriber.
        let mut events = manager.events();
        let busy_manager = manager.clone();
        let busy_task = tokio::spawn(async move {
            busy_manager
                .dispatch(
                    RpcCommand::Prompt {
                        id: None,
                        message: "busy".into(),
                        images: Vec::new(),
                        streaming_behavior: None,
                    },
                    None,
                )
                .await
        });
        // Deterministic busy evidence, never a fixed sleep: the turn's
        // stream is gated on `release`, so the turn stays verifiably in
        // flight from its turn_started frame on the pre-subscribed fan-in
        // stream until the test decides to end it.
        let turn_started = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let frame = loop {
                    match events.recv().await {
                        Ok(frame) => break frame,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            let _ = n;
                        }
                        Err(e) => panic!("fan-in stream closed: {e}"),
                    }
                };
                if frame.get("type").and_then(Value::as_str) == Some("turn_started")
                    && frame.get("sessionId").and_then(Value::as_str) == Some(active_id.as_str())
                {
                    break;
                }
            }
        })
        .await
        .expect("busy turn started");
        let _ = turn_started;
        let prompt_result = tokio::time::timeout(std::time::Duration::from_secs(10), busy_task)
            .await
            .expect("busy prompt admission joins")
            .expect("prompt task");
        assert!(prompt_result.success, "busy turn admission succeeded");
        let busy = manager.dispatch(move_command("busy"), None).await;
        assert!(!busy.success);
        assert_eq!(
            busy.error.as_deref(),
            Some("session is busy: a turn is in progress"),
            "busy active move carries the exact ensure_idle reason"
        );
        assert!(active_path.exists(), "a busy active move never mutates files");
        // Unhook the gated stream and abort: the held turn drains to idle
        // like any aborted turn before the idle move is attempted.
        release.cancel();
        let aborted = manager
            .dispatch(RpcCommand::Abort { id: None }, Some(active_id.clone()))
            .await;
        assert!(aborted.success, "{}", aborted.error.unwrap_or_default());
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let state = manager.dispatch(RpcCommand::GetState { id: None }, None).await;
            if state.data.as_ref().and_then(|data| data.get("isStreaming"))
                == Some(&json!(false))
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "primary never reached idle"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        // Idle: the active move relocates the saved file and rebinds the
        // SAME primary runtime to the destination (the TUI `/move` effect):
        // session stays active, cwd is the target, old path gone.
        let moved = manager.dispatch(move_command("active-move"), None).await;
        assert!(moved.success, "{}", moved.error.unwrap_or_default());
        let data = moved.data.expect("active move data");
        let destination = PathBuf::from(data["to"].as_str().expect("destination path"));
        assert_eq!(data["moved"], json!(true));
        assert_eq!(data["sessionId"], json!(active_id));
        assert_eq!(data["from"], json!(active_path.to_string_lossy()));
        assert_eq!(
            data["session"]["path"],
            json!(destination.to_string_lossy()),
            "response carries the destination catalog row"
        );
        assert!(!active_path.exists(), "source row removed");
        assert!(destination.exists(), "destination published");
        assert_eq!(
            destination.parent(),
            Some(pi_coding::default_session_dir(target.path()).as_path()),
            "destination publishes into the target's native session store"
        );
        assert_eq!(
            spawner.spawns.load(Ordering::SeqCst),
            0,
            "the cutover reuses the active runtime; the manager never spawns"
        );
        let rebound = manager.resolve(None).await.expect("primary resolves");
        assert!(
            Arc::ptr_eq(&rebound, &manager.primary),
            "the same runtime stays active after the move"
        );
        assert_eq!(
            manager.primary.application.session().cwd(),
            target.path(),
            "the active runtime's cwd is the move target"
        );
        let (rebound_id, rebound_path) = manager
            .primary
            .application
            .session()
            .recorder_info()
            .expect("rebound recorder");
        assert_eq!(rebound_id, active_session_id, "session identity survives the move");
        assert_eq!(rebound_path, destination, "the recorder is bound to the moved file");
        let registry = manager.registry.read().await;
        assert!(
            !registry.by_file.contains_key(&old_identity),
            "the stale file index entry is gone"
        );
        let new_identity = manager.primary.current_file_identity().expect("rebound identity");
        assert_ne!(
            new_identity, old_identity,
            "the cutover re-keyed the runtime onto the moved file"
        );
        assert!(
            registry
                .by_file
                .get(&new_identity)
                .is_some_and(|candidate| Arc::ptr_eq(candidate, &manager.primary)),
            "the file index keys the moved identity to the same runtime"
        );
        drop(registry);
        let state = manager.dispatch(RpcCommand::GetState { id: None }, None).await;
        assert!(state.success, "{}", state.error.unwrap_or_default());
        assert_eq!(
            state.data.as_ref().unwrap()["cwd"],
            json!(target.path().to_string_lossy()),
            "the active session now serves the target cwd"
        );

        let _ = std::fs::remove_dir_all(pi_coding::default_session_dir(target.path()));
        manager.shutdown().await;
        manager.primary.application.cleanup().await;
    }

    /// A streaming file conversation blocks the active-session move through
    /// the real idle gate, and aborting the conversation releases it: the
    /// collection engine is inspected under the move fence and owner
    /// capture failures are never treated as silently idle.
    #[tokio::test]
    async fn file_conversation_streaming_blocks_active_move() {
        let (_home, _native, catalog) = web_catalog_fixture();
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog));
        let release = CancellationToken::new();
        let (manager, _dir) = manager_with_gated_primary_stream(
            Some(spawner.clone()),
            release.clone(),
            "held file turn",
        )
        .await;
        manager
            .primary
            .application
            .attach_runtime_factory(Arc::new(ActiveMoveRuntimeFactory))
            .expect("attach active-move runtime factory");
        let (_active_session_id, active_path) = manager
            .primary
            .application
            .session()
            .recorder_info()
            .expect("active recorder");
        let cwd = manager.primary.application.session().cwd().to_path_buf();
        std::fs::write(cwd.join("move-fence.txt"), "l1\nl2\n").expect("context file");
        let target = tempfile::tempdir().expect("move target");
        let move_command = |tag: &str| RpcCommand::SessionMove {
            id: Some(tag.into()),
            session_path: active_path.to_string_lossy().into_owned(),
            target_cwd: target.path().to_string_lossy().into_owned(),
        };
        let open = manager
            .dispatch(
                RpcCommand::FileConversationOpen {
                    id: Some("open".into()),
                    conversation_id: "move-fence-c1".into(),
                    path: "move-fence.txt".into(),
                },
                None,
            )
            .await;
        assert!(open.success, "{}", open.error.unwrap_or_default());
        let prompt = manager
            .dispatch(
                RpcCommand::FileConversationPrompt {
                    id: Some("prompt".into()),
                    conversation_id: "move-fence-c1".into(),
                    message: "explain this file".into(),
                    selection: None,
                },
                None,
            )
            .await;
        assert!(prompt.success, "{}", prompt.error.unwrap_or_default());
        // submit_prompt latches is_streaming before the acknowledgement
        // returns, and the forked controller streams on the same gated
        // stream until the test releases it, so the busy evidence is
        // deterministic rather than a timing window.
        assert_eq!(
            prompt.data.as_ref().expect("prompt ack")["accepted"],
            json!(true),
            "the file conversation accepted the prompt"
        );
        assert!(
            manager
                .primary
                .dispatcher
                .file_conversation_busy()
                .await
                .expect("busy probe"),
            "the accepted file-conversation turn is verifiably in flight"
        );
        let busy_move = manager.dispatch(move_command("busy"), None).await;
        assert!(
            !busy_move.success,
            "a streaming file conversation must block the active move"
        );
        assert_eq!(
            busy_move.error.as_deref(),
            Some("session is busy: a file conversation is streaming"),
            "the move carries the exact file-conversation busy reason"
        );
        assert!(active_path.exists(), "a busy active move never mutates files");

        let abort = manager
            .dispatch(
                RpcCommand::FileConversationAbort {
                    id: None,
                    conversation_id: "move-fence-c1".into(),
                },
                None,
            )
            .await;
        assert!(abort.success, "{}", abort.error.unwrap_or_default());
        assert!(
            !manager
                .primary
                .dispatcher
                .file_conversation_busy()
                .await
                .expect("busy probe"),
            "the aborted conversation cleared the busy state"
        );
        let moved = manager.dispatch(move_command("after-abort"), None).await;
        assert!(moved.success, "{}", moved.error.unwrap_or_default());
        release.cancel();
        let _ = std::fs::remove_dir_all(pi_coding::default_session_dir(target.path()));
        manager.shutdown().await;
        manager.primary.application.cleanup().await;
    }

    /// The active-move admission fence: while the collection mutex is held
    /// (exactly what move_saved_session holds from before its idle check
    /// through cutover and recovery), file-conversation admission stays
    /// excluded, and the idle check inspects the held engine in place
    /// instead of reacquiring the non-reentrant mutex.
    #[tokio::test]
    async fn held_file_conversation_fence_excludes_admission_and_skips_reacquire() {
        let (manager, _registration) = manager_with(None).await;
        let cwd = manager.primary.application.session().cwd().to_path_buf();
        std::fs::write(cwd.join("fence.txt"), "x\n").expect("context file");
        let mut fence = manager.primary.dispatcher.lock_file_conversations().await;
        // The admission future is interrogated deterministically: while the
        // fence is held it cannot complete — the collection mutex is the
        // only gate between a dispatched file-conversation command and its
        // engine — so a direct poll must report Pending, with no timing
        // window and no scheduling assumption. Uncontended admission awaits
        // poll through, so Pending means parked at the collection lock.
        let manager_for_open = manager.clone();
        let open = async move {
            manager_for_open
                .dispatch(
                    RpcCommand::FileConversationOpen {
                        id: None,
                        conversation_id: "fence-c1".into(),
                        path: "fence.txt".into(),
                    },
                    None,
                )
                .await
        };
        tokio::pin!(open);
        std::future::poll_fn(|cx| match open.as_mut().poll(cx) {
            std::task::Poll::Pending => std::task::Poll::Ready(()),
            std::task::Poll::Ready(response) => panic!(
                "file-conversation admission completed while the move fence is held: {response:?}"
            ),
        })
        .await;
        // The idle check must run against the HELD engine: reacquiring the
        // collection mutex here would self-deadlock the move.
        tokio::time::timeout(
            Duration::from_secs(10),
            manager.ensure_idle_rejecting_queued(&manager.primary, false, Some(&mut *fence)),
        )
        .await
        .expect("idle check with the held fence must not reacquire the collection")
        .expect("idle runtime passes the file-conversation gate");
        // Causal control: a command that never touches the collection stays
        // serviceable while the fence is held, so the open's non-completion
        // below is attributable to the collection gate itself, not to a
        // jammed runtime or admission path.
        let state = tokio::time::timeout(
            Duration::from_secs(10),
            manager.dispatch(RpcCommand::GetState { id: None }, None),
        )
        .await
        .expect("control command stays serviceable while the fence is held");
        assert!(state.success, "{}", state.error.unwrap_or_default());
        // Still parked under the held fence. Release it and the SAME
        // future completes with the exact result.
        drop(fence);
        let open = tokio::time::timeout(Duration::from_secs(10), open.as_mut())
            .await
            .expect("admission joins once the fence releases");
        assert!(open.success, "{}", open.error.unwrap_or_default());
        manager.shutdown().await;
        manager.primary.application.cleanup().await;
    }

    /// Real-move causal exclusion: the production `session_move` path is
    /// parked INSIDE its cutover — proven by the runtime-factory entry
    /// handshake, which can only fire after the idle check completed under
    /// the held fence (a collection reacquire there would deadlock the move
    /// before the gate is ever reached). Inside that window the disk move
    /// has already renamed the source file away, so production admission
    /// writer-gates every file-conversation command: the in-window open
    /// must fail closed with the exact reopen reason — no file work ever
    /// starts against the owner being replaced — and only after the
    /// cutover releases does a fresh open admit against the rebound owner.
    #[tokio::test]
    async fn active_move_cutover_window_excludes_file_conversation_admission() {
        let (_home, _native, catalog) = web_catalog_fixture();
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog));
        let primary_stream = CancellationToken::new();
        let (manager, _dir) = manager_with_gated_primary_stream(
            Some(spawner.clone()),
            primary_stream,
            "unused",
        )
        .await;
        let entered = CancellationToken::new();
        let cutover_release = CancellationToken::new();
        manager
            .primary
            .application
            .attach_runtime_factory(Arc::new(GatedCutoverFactory {
                entered: entered.clone(),
                release: cutover_release.clone(),
            }))
            .expect("attach gated cutover factory");
        let (_session_id, active_path) = manager
            .primary
            .application
            .session()
            .recorder_info()
            .expect("active recorder");
        let cwd = manager.primary.application.session().cwd().to_path_buf();
        std::fs::write(cwd.join("fenced.txt"), "x\n").expect("source context file");
        let target = tempfile::tempdir().expect("move target");
        std::fs::write(target.path().join("fenced.txt"), "x\n")
            .expect("target context file: the parked open resolves after the cutover");
        let session_path = active_path.to_string_lossy().into_owned();
        let target_cwd = target.path().to_string_lossy().into_owned();
        let move_command = move |tag: &str| RpcCommand::SessionMove {
            id: Some(tag.into()),
            session_path: session_path.clone(),
            target_cwd: target_cwd.clone(),
        };
        let manager_for_move = manager.clone();
        let move_task = tokio::spawn(async move {
            manager_for_move
                .dispatch(move_command("fenced"), None)
                .await
        });
        tokio::time::timeout(Duration::from_secs(10), entered.cancelled())
            .await
            .expect("the move reached its cutover while holding the file-conversation fence");
        // Mid-window admission is writer-gated by production design: the
        // disk move has already renamed the source file away, so every
        // writer-requiring command — including file-conversation admission
        // — fails closed at admit with the exact reopen reason instead of
        // starting any file work against the owner being replaced. The
        // rejection is a ready admission waiter, so the response is
        // deterministic with no timing window.
        let rejected = manager
            .dispatch(
                RpcCommand::FileConversationOpen {
                    id: Some("in-window".into()),
                    conversation_id: "cutover-c1".into(),
                    path: "fenced.txt".into(),
                },
                None,
            )
            .await;
        assert!(
            !rejected.success,
            "mid-cutover file-conversation admission must fail closed: {rejected:?}"
        );
        assert_eq!(
            rejected.error.as_deref(),
            Some("session writer is unavailable; reopen the session and retry"),
            "the in-window refusal is the exact admission writer gate"
        );
        cutover_release.cancel();
        let moved = tokio::time::timeout(Duration::from_secs(10), move_task)
            .await
            .expect("move completes after the gate releases")
            .expect("move task");
        assert!(moved.success, "{}", moved.error.unwrap_or_default());
        let open = tokio::time::timeout(
            Duration::from_secs(10),
            manager.dispatch(
                RpcCommand::FileConversationOpen {
                    id: Some("post-cutover".into()),
                    conversation_id: "cutover-c2".into(),
                    path: "fenced.txt".into(),
                },
                None,
            ),
        )
        .await
        .expect("post-cutover admission joins once the fence drops");
        assert!(open.success, "{}", open.error.unwrap_or_default());
        assert_eq!(
            manager.primary.application.session().cwd(),
            target.path(),
            "the parked open joined against the rebound owner, not the pre-move one"
        );
        let snapshot = tokio::time::timeout(
            Duration::from_secs(10),
            manager.dispatch(
                RpcCommand::FileConversationSnapshot {
                    id: None,
                    conversation_id: "cutover-c2".into(),
                },
                None,
            ),
        )
        .await
        .expect("snapshot of the admitted conversation joins");
        assert!(snapshot.success, "{}", snapshot.error.unwrap_or_default());
        let _ = std::fs::remove_dir_all(pi_coding::default_session_dir(target.path()));
        manager.shutdown().await;
        manager.primary.application.cleanup().await;
    }

    /// Runtime cleanup must quiesce and drain file conversations BEFORE the
    /// application teardown they were forked from: a live turn is shut
    /// down, and the collection is already empty when cleanup returns.
    #[tokio::test]
    async fn cleanup_runtime_contents_shuts_down_file_conversations() {
        let release = CancellationToken::new();
        let (manager, _dir) =
            manager_with_gated_primary_stream(None, release.clone(), "held cleanup turn").await;
        let cwd = manager.primary.application.session().cwd().to_path_buf();
        std::fs::write(cwd.join("cleanup.txt"), "x\n").expect("context file");
        let open = manager
            .dispatch(
                RpcCommand::FileConversationOpen {
                    id: Some("open".into()),
                    conversation_id: "cleanup-c1".into(),
                    path: "cleanup.txt".into(),
                },
                None,
            )
            .await;
        assert!(open.success, "{}", open.error.unwrap_or_default());
        let prompt = manager
            .dispatch(
                RpcCommand::FileConversationPrompt {
                    id: Some("prompt".into()),
                    conversation_id: "cleanup-c1".into(),
                    message: "summarize".into(),
                    selection: None,
                },
                None,
            )
            .await;
        assert!(prompt.success, "{}", prompt.error.unwrap_or_default());
        let mut engine = manager.primary.dispatcher.lock_file_conversations().await;
        assert!(
            engine
                .is_busy(&manager.primary.application)
                .await
                .expect("busy probe"),
            "the conversation is live with a gated turn before cleanup"
        );
        drop(engine);
        manager
            .cleanup_runtime_contents(&manager.primary)
            .await
            .expect("runtime contents cleanup");
        let mut engine = manager.primary.dispatcher.lock_file_conversations().await;
        assert_eq!(
            engine.shutdown().await,
            0,
            "cleanup_runtime_contents already drained the file-conversation collection"
        );
        release.cancel();
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn session_move_relocates_idle_active_session_with_queued_messages() {
        let (_home, _native, catalog) = web_catalog_fixture();
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog));
        let release = CancellationToken::new();
        let (manager, _dir) =
            manager_with_gated_primary_stream(Some(spawner), release, "unused").await;
        manager
            .primary
            .application
            .attach_runtime_factory(Arc::new(ActiveMoveRuntimeFactory))
            .expect("attach active-move runtime factory");
        let (_session_id, active_path) = manager
            .primary
            .application
            .session()
            .recorder_info()
            .expect("active recorder");
        manager
            .primary
            .application
            .steer("parked-steer".to_owned(), Vec::new())
            .await
            .expect("queue steer");
        manager
            .primary
            .application
            .follow_up("parked-follow".to_owned(), Vec::new())
            .await
            .expect("queue follow-up");
        let queued = manager.dispatch(RpcCommand::GetState { id: None }, None).await;
        assert!(queued.success, "{}", queued.error.unwrap_or_default());
        assert_eq!(queued.data.as_ref().unwrap()["isStreaming"], json!(false));
        assert_eq!(queued.data.as_ref().unwrap()["pendingMessageCount"], json!(2));
        let target = tempfile::tempdir().expect("move target");
        let moved = manager
            .dispatch(
                RpcCommand::SessionMove {
                    id: Some("queued-active-move".into()),
                    session_path: active_path.to_string_lossy().into_owned(),
                    target_cwd: target.path().to_string_lossy().into_owned(),
                },
                None,
            )
            .await;
        assert!(moved.success, "{}", moved.error.unwrap_or_default());
        assert_eq!(
            moved.error.as_deref(),
            None,
            "queued steer/follow-up must not reject an idle active move"
        );
        let after = manager.dispatch(RpcCommand::GetState { id: None }, None).await;
        assert!(after.success, "{}", after.error.unwrap_or_default());
        assert_eq!(
            after.data.as_ref().unwrap()["cwd"],
            json!(target.path().to_string_lossy())
        );
        // The cross-directory cutover rebuilds the runtime generation; the
        // drained steer/follow-up prompts must be re-enqueued into the
        // rebound runtime, not silently dropped with the old generation.
        assert_eq!(
            after.data.as_ref().unwrap()["pendingMessageCount"],
            json!(2),
            "queued steer/follow-up must survive the active-move cutover"
        );
        let (steering, follow_up) = manager
            .primary
            .application
            .session()
            .queued_messages()
            .await;
        assert_eq!(steering.len(), 1, "the drained steer must be re-enqueued");
        assert_eq!(follow_up.len(), 1, "the drained follow-up must be re-enqueued");
        assert!(
            format!("{steering:?}").contains("parked-steer"),
            "steer content must survive the cutover: {steering:?}"
        );
        assert!(
            format!("{follow_up:?}").contains("parked-follow"),
            "follow-up content must survive the cutover: {follow_up:?}"
        );
        let _ = std::fs::remove_dir_all(pi_coding::default_session_dir(target.path()));
        manager.shutdown().await;
        manager.primary.application.cleanup().await;
    }

    /// The bootstrap (sessionId-less) `get_state`, the explicit
    /// (sessionId-bound) `get_state`, and the subscription snapshot must all
    /// share ONE live recompute of the pending steering projection: a queued
    /// message can never report pending=0/empty rows on one surface while the
    /// other surfaces report it — reconnect reconciles tray rows from the
    /// snapshot and would wipe acknowledged rows.
    #[tokio::test]
    async fn bootstrap_and_subscription_snapshots_share_queue_projection() {
        let (manager, _registration) = manager_with(None).await;
        manager
            .primary
            .application
            .steer("snapshot-row".to_owned(), Vec::new())
            .await
            .expect("queue steer");

        let bootstrap = manager.dispatch(RpcCommand::GetState { id: None }, None).await;
        assert!(
            bootstrap.success,
            "{}",
            bootstrap.error.unwrap_or_default()
        );
        let bootstrap_state = bootstrap.data.expect("bootstrap get_state data");
        assert_eq!(bootstrap_state["pendingMessageCount"], json!(1));
        let queued_steering = bootstrap_state["queuedSteering"].clone();
        assert_eq!(
            queued_steering.as_array().map(Vec::len),
            Some(1),
            "bootstrap get_state must recompute the live queue: {bootstrap_state}"
        );

        let explicit = manager
            .dispatch(
                RpcCommand::GetState {
                    id: Some("explicit".into()),
                },
                Some(manager.primary_session_id()),
            )
            .await;
        assert!(explicit.success, "{}", explicit.error.unwrap_or_default());
        let explicit_state = explicit.data.expect("explicit get_state data");
        assert_eq!(
            explicit_state["queuedSteering"], queued_steering,
            "explicit get_state must agree with the bootstrap projection"
        );

        let snapshot = manager
            .subscription_snapshot_for_identity(
                RuntimeIdentity::new(manager.primary_session_id(), String::new()),
                0,
                CancellationToken::new(),
                false,
            )
            .await
            .expect("subscription snapshot");
        assert_eq!(
            snapshot["state"]["queuedSteering"], queued_steering,
            "subscription snapshot must carry the same live projection"
        );
        assert_eq!(snapshot["state"]["pendingMessageCount"], json!(1));

        manager.shutdown().await;
    }
    /// P0 regression (dual-open after a deadline expiry): when a stalled
    /// creation is parked PAST its client deadline, the in-flight Create
    /// still owns the journal. A quick second open of the SAME row must be
    /// REFUSED with a bounded error (the parked creation holds the per-
    /// identity open lane until terminal register-or-rollback) — never
    /// dual-open the .jsonl with two Applications.
    #[tokio::test]
    async fn open_deadline_expiry_refuses_quick_retry_while_create_in_flight() {
        let (_home, native, catalog) = web_catalog_fixture();
        let cwd = tempfile::tempdir().expect("session cwd");
        let cwd_json =
            serde_json::to_string(&cwd.path().to_string_lossy()).expect("cwd json");
        let session_path = native.path().join("dual-open-row.jsonl");
        let session_body = format!(
            "{{\"type\":\"session\",\"version\":3,\"id\":\"{}\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":{cwd_json}}}\n{{\"type\":\"message\",\"id\":\"u\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01Z\",\"message\":{{\"role\":\"user\",\"content\":\"dual open probe\"}}}}\n",
            unique("dual-open-row")
        );
        std::fs::write(&session_path, &session_body).expect("write native session row");
        let spawner = Arc::new(GatedSpawner {
            inner: TestSpawner::default().with_catalog(catalog.clone()),
            ..GatedSpawner::default()
        });
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        manager.set_switch_open_deadline_for_test(Duration::from_millis(250));
        let switch_command = |id_tag: &'static str| RpcCommand::SwitchSession {
            id: Some(id_tag.into()),
            session_path: session_path.to_string_lossy().into_owned(),
            catalog_source: None,
            catalog_session_id: None,
            catalog_status: None,
            response_mode: None,
        };

        // First attempt: stalls past the deadline, fails terminally for the
        // client, and the creation is PARKED still holding the open lane.
        let first = tokio::time::timeout(
            Duration::from_secs(5),
            manager.dispatch(switch_command("dual-open-first"), None),
        )
        .await
        .expect("the first open must settle");
        assert!(!first.success, "{first:?}");

        // Quick retry while the first creation is STILL gated: refused with
        // the bounded error (never reaches the spawner, never dual-opens).
        let second = tokio::time::timeout(
            Duration::from_secs(5),
            manager.dispatch(switch_command("dual-open-second"), None),
        )
        .await
        .expect("the refused retry must settle");
        assert!(
            !second.success,
            "an in-flight creation must refuse a quick retry: {second:?}"
        );
        assert_eq!(
            second.error.as_deref(),
            Some("session could not be opened right now; verify the selection and retry"),
            "the refusal is bounded and path-free"
        );
        // The refusal must not add a SECOND spawn: the total spawn count
        // after the parked creation settles is asserted below (== 1).
        assert!(
            spawner.inner.spawns.load(Ordering::SeqCst) <= 1,
            "the refused retry must never spawn a duplicate"
        );

        // Release the parked creation: exactly ONE runtime registers for the
        // file identity, and a final open reuses it (no dual-open, and the
        // journal was never written by two writers — only one spawn ran).
        spawner.release.add_permits(1);
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if manager
                    .loaded_runtime_for_exact_path(&session_path)
                    .await
                    .is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the parked creation must register exactly one runtime");
        let final_open = tokio::time::timeout(
            Duration::from_secs(10),
            manager.dispatch(switch_command("dual-open-final"), None),
        )
        .await
        .expect("the final open must settle");
        assert!(
            final_open.success,
            "the final open reuses the single registered runtime: {final_open:?}"
        );
        assert_eq!(
            spawner.inner.spawns.load(Ordering::SeqCst),
            1,
            "exactly ONE creation ever ran for the file identity"
        );
        let registry = manager.registry.read().await;
        let runtimes_for_identity = registry
            .distinct_runtimes()
            .into_iter()
            .filter(|runtime| runtime.current_file_identity().is_some())
            .filter(|runtime| {
                runtime
                    .application
                    .session()
                    .recorder_info()
                    .is_some_and(|(_, path)| path == session_path)
            })
            .count();
        drop(registry);
        assert_eq!(
            runtimes_for_identity, 1,
            "exactly ONE runtime owns the file identity after both attempts"
        );
        manager.shutdown().await;
        manager.primary.application.cleanup().await;
    }

    /// `GatedSpawner` delegates only `spawn`, so the manager-level
    /// `factory.catalog()` probe (used by `web_session_catalog` and thus the
    /// move path) sees no catalog. This wrapper adds the catalog/storage
    /// delegation without changing the shared GatedSpawner fixture for the
    /// other gated tests.
    struct GatedCatalogSpawner {
        gated: Arc<GatedSpawner>,
    }

    impl SessionSpawner for GatedCatalogSpawner {
        fn spawn(
            &self,
            request: SessionSpawnRequest,
        ) -> Pin<Box<dyn Future<Output = Result<SessionSpawnResult>> + Send>> {
            self.gated.spawn(request)
        }
        fn catalog(&self) -> Option<pi_coding::SessionCatalog> {
            self.gated.inner.catalog()
        }
        fn session_storage(&self) -> Option<crate::session_run::SessionStorage> {
            self.gated.inner.session_storage()
        }
        fn todo_child_service(&self) -> Option<Arc<TodoChildService>> {
            self.gated.todo_child_service()
        }
    }


    /// C1 regression (catalog plane frozen behind an open lane): a parked
    /// Open(X) holds the per-identity open lane until register-or-rollback.
    /// A concurrent Move(X) used to hold `catalog_lock` while waiting
    /// UNBOUNDED on that lane, so every other catalog operation (rename,
    /// delete, prepare, other moves) froze for the whole parked duration.
    /// The move must instead FAIL BOUNDED with a readable error, and an
    /// unrelated catalog mutation (rename of another row) must complete
    /// while the move is still settling — never hang behind it.
    #[tokio::test]

    async fn move_with_parked_open_fails_bounded_and_catalog_plane_stays_live() {
        let (_home, native, catalog) = web_catalog_fixture();
        let cwd = tempfile::tempdir().expect("session cwd");
        let (parked_path, _) = persist_production_source(
            cwd.path(),
            native.path(),
            "parked-open-row",
            &["parked open probe"],
        );
        let (unrelated_path, _) = persist_production_source(
            cwd.path(),
            native.path(),
            "unrelated-row",
            &["unrelated rename probe"],
        );
        let target = tempfile::tempdir().expect("move target");
        let gated = Arc::new(GatedSpawner {
            inner: TestSpawner::default().with_catalog(catalog.clone()),
            ..GatedSpawner::default()
        });
        let spawner: Arc<dyn SessionSpawner> = Arc::new(GatedCatalogSpawner {
            gated: gated.clone(),
        });
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        manager.set_switch_open_deadline_for_test(Duration::from_millis(250));

        // Park an Open(parked-open-row): the switch fails terminally at the
        // deadline while the gated creation stays parked holding the
        // per-identity open lane (exact P0-1 fixture).
        let switch = tokio::time::timeout(
            Duration::from_secs(10),
            manager.dispatch(
                RpcCommand::SwitchSession {
                    id: Some("parked-open".into()),
                    session_path: parked_path.to_string_lossy().into_owned(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                None,
            ),
        )
        .await
        .expect("the parked open must settle");
        assert!(!switch.success, "{switch:?}");

        // Coordination: the unrelated rename must COMPLETE before the move
        // task even settles. The move is gated until the rename has finished,
        // and the parked creation must still be in flight the whole time.
        // Raise the open deadline to 30s: a refusal that arrives within
        // seconds cannot have been waiting out any open-lane deadline.
        manager.set_switch_open_deadline_for_test(Duration::from_secs(30));
        let move_gate = Arc::new(tokio::sync::Notify::new());
        let move_release = move_gate.clone();
        let mover = manager.clone();
        let parked_path_string = parked_path.to_string_lossy().into_owned();
        let target_string = target.path().to_string_lossy().into_owned();
        let move_task = tokio::spawn(async move {
            move_release.notified().await;
            mover
                .dispatch(
                    RpcCommand::SessionMove {
                        id: Some("move-while-parked".into()),
                        session_path: parked_path_string,
                        target_cwd: target_string,
                    },
                    None,
                )
                .await
        });
        // Hold the move back; run the rename to completion first.
        let renamed = tokio::time::timeout(
            Duration::from_secs(10),
            manager.dispatch(
                RpcCommand::SetSessionName {
                    id: Some("rename-unrelated".into()),
                    name: "renamed while parked".into(),
                    session_path: Some(unrelated_path.to_string_lossy().into_owned()),
                },
                None,
            ),
        )
        .await
        .expect("an unrelated rename must NOT freeze behind the parked open");
        assert!(
            renamed.success,
            "rename of an unrelated row must succeed while a creation is parked on the open lane: {renamed:?}"
        );
        assert_eq!(
            gated.completed.available_permits(),
            0,
            "the parked creation must still be in flight when the rename completes"
        );

        // Now let the move run: it must fail closed immediately (try_lock),
        // never by waiting out any open-lane deadline.
        let move_started = std::time::Instant::now();
        move_gate.notify_one();
        let moved = tokio::time::timeout(Duration::from_secs(10), move_task)
            .await
            .expect("the move must fail closed, not hang")
            .expect("move task join");
        let move_elapsed = move_started.elapsed();
        assert!(
            !moved.success,
            "a move behind an in-flight open must fail closed: {moved:?}"
        );
        assert!(
            move_elapsed < Duration::from_secs(5),
            "the move refusal must be immediate (try_lock), never an open-lane wait; took {move_elapsed:?} with a 30s deadline"
        );
        assert_eq!(
            moved.error.as_deref(),
            Some(
                "session could not be moved right now; an open of this session is still in progress; retry shortly"
            ),
            "the move refusal is bounded and path-free"
        );
        // Release the parked creation: exactly one runtime registers, and
        // the catalog plane is fully usable again (retry the move now).
        gated.release.add_permits(1);
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if manager
                    .loaded_runtime_for_exact_path(&parked_path)
                    .await
                    .is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the parked creation must register exactly one runtime");
        let retried = tokio::time::timeout(
            Duration::from_secs(10),
            manager.dispatch(
                RpcCommand::SessionMove {
                    id: Some("move-after-settle".into()),
                    session_path: parked_path.to_string_lossy().into_owned(),
                    target_cwd: target.path().to_string_lossy().into_owned(),
                },
                None,
            ),
        )
        .await
        .expect("the retried move must settle");
        assert!(
            retried.success,
            "after the parked open settles, the catalog plane serves the move: {retried:?}"
        );
        let _ = std::fs::remove_dir_all(pi_coding::default_session_dir(target.path()));
        manager.shutdown().await;
        manager.primary.application.cleanup().await;
    }

    /// A stalled creation must NEVER leave a switch in limbo: past the
    /// bounded open deadline the client receives the terminal bounded switch
    /// failure, and once the stalled creation finally completes, an
    /// immediate retry of the SAME row settles through the exact-path reuse
    /// guard (a late settle can never duplicate or deadlock the row).
    #[tokio::test]
    async fn switch_open_deadline_fails_terminal_and_retry_reuses_late_settle() {
        let (_home, native, catalog) = web_catalog_fixture();
        let cwd = tempfile::tempdir().expect("session cwd");
        let cwd_json =
            serde_json::to_string(&cwd.path().to_string_lossy()).expect("cwd json");
        let session_path = native.path().join("stalled-row.jsonl");
        std::fs::write(
            &session_path,
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"{}\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":{cwd_json}}}\n{{\"type\":\"message\",\"id\":\"u\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01Z\",\"message\":{{\"role\":\"user\",\"content\":\"stalled row prompt\"}}}}\n",
                unique("stalled-row")
            ),
        )
        .expect("write native session row");
        let spawner = Arc::new(GatedSpawner {
            inner: TestSpawner::default().with_catalog(catalog.clone()),
            ..GatedSpawner::default()
        });
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        manager.set_switch_open_deadline_for_test(Duration::from_millis(250));
        let switch_command = |id_tag: &'static str| RpcCommand::SwitchSession {
            id: Some(id_tag.into()),
            session_path: session_path.to_string_lossy().into_owned(),
            catalog_source: None,
            catalog_session_id: None,
            catalog_status: None,
            response_mode: None,
        };
        let stalled = tokio::time::timeout(
            Duration::from_secs(5),
            manager.dispatch(switch_command("stalled-open"), None),
        )
        .await
        .expect("the stalled switch must settle within the test timeout");
        assert!(
            !stalled.success,
            "a stalled open must fail terminally, not hang: {stalled:?}"
        );
        assert_eq!(
            stalled.error.as_deref(),
            Some("session could not be opened right now; verify the selection and retry"),
            "the deadline expiry surfaces the bounded switch failure class"
        );

        // The gated creation was abandoned by the client but keeps running:
        // release it and let it settle. The late completion registers the
        // runtime, so the retry of the SAME row reuses it through the
        // exact-path guard instead of spawning a duplicate.
        spawner.release.add_permits(1);
        tokio::time::timeout(Duration::from_secs(10), async {
            spawner.completed.acquire().await.expect("completed semaphore");
        })
        .await
        .expect("the released creation must settle");
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if manager
                    .loaded_runtime_for_exact_path(&session_path)
                    .await
                    .is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the late-settled runtime must register");
        let settled = tokio::time::timeout(
            Duration::from_secs(10),
            manager.dispatch(switch_command("stalled-retry"), None),
        )
        .await
        .expect("the retried switch must settle");
        assert!(
            settled.success,
            "the retried switch must complete against the late-settled runtime: {settled:?}"
        );
        assert_eq!(
            spawner.inner.spawns.load(Ordering::SeqCst),
            1,
            "the retry must reuse the late-settled runtime, not spawn a duplicate"
        );
        manager.shutdown().await;
        manager.primary.application.cleanup().await;
    }

    /// Cross-directory cutover factory that fails the FIRST candidate build
    /// and succeeds afterwards: simulates a transient cutover failure after
    /// the core file move is already durable, exercising the in-process
    /// recovery retry on the SAME runtime.
    struct FlakyMoveRuntimeFactory(Arc<AtomicUsize>);

    impl pi_coding::ApplicationRuntimeFactory for FlakyMoveRuntimeFactory {
        fn build_runtime_candidate(
            &self,
            cwd: PathBuf,
            options: SessionOptions,
            resume: Option<pi_coding::PreparedSessionResume>,
        ) -> pi_coding::ApplicationRuntimeFuture {
            let attempts = self.0.clone();
            Box::pin(async move {
                if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    anyhow::bail!("injected cutover staging failure");
                }
                let session = Session::new_with_additional_tools_filtered_and_discovery(
                    options,
                    Vec::new(),
                    ToolSelection::default(),
                    ResourceDiscovery::Disabled,
                )?;
                session.set_session_dir(pi_coding::default_session_dir(&cwd));
                session.attach_resources(isolated_resources(&cwd)).await?;
                if let Some(resume) = resume {
                    let context = resume.build_context();
                    let recorder = resume.into_recorder()?;
                    session.resume_recording(recorder, context.messages).await?;
                }
                Ok(pi_coding::ApplicationRuntimeCandidate::new(session))
            })
        }
    }

    #[tokio::test]
    async fn session_move_failed_cutover_recovers_and_reopen_reuses_runtime() {
        let (_home, _native, catalog) = web_catalog_fixture();
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog));
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        manager
            .primary
            .application
            .attach_runtime_factory(Arc::new(FlakyMoveRuntimeFactory(Arc::new(AtomicUsize::new(
                0,
            )))))
            .expect("attach flaky runtime factory");
        let (active_session_id, active_path) = manager
            .primary
            .application
            .session()
            .recorder_info()
            .expect("active recorder");
        let old_identity = manager.primary.current_file_identity().expect("active identity");
        let target = tempfile::tempdir().expect("move target");

        // The core file move succeeds, the first cutover attempt fails in
        // the factory, and the in-process recovery retry rebinds the SAME
        // runtime — the move response succeeds and reopen stays executable.
        let moved = manager
            .dispatch(
                RpcCommand::SessionMove {
                    id: Some("flaky-move".into()),
                    session_path: active_path.to_string_lossy().into_owned(),
                    target_cwd: target.path().to_string_lossy().into_owned(),
                },
                None,
            )
            .await;
        assert!(
            moved.success,
            "in-process recovery must finish the cutover: {}",
            moved.error.unwrap_or_default()
        );
        let data = moved.data.expect("move data");
        let destination = PathBuf::from(data["to"].as_str().expect("destination path"));
        assert!(!active_path.exists(), "source row removed");
        assert!(destination.exists(), "destination published");

        // The SAME runtime was rebound in process: recorder on the moved
        // file, cwd at the target, registry re-keyed onto the moved identity.
        assert_eq!(
            manager.primary.application.session().cwd(),
            target.path(),
            "the active runtime's cwd is the move target"
        );
        let (_, rebound_path) = manager
            .primary
            .application
            .session()
            .recorder_info()
            .expect("rebound recorder");
        assert_eq!(
            rebound_path, destination,
            "the recorder is bound to the moved file"
        );
        let new_identity = manager.primary.current_file_identity().expect("rebound identity");
        assert_ne!(
            new_identity, old_identity,
            "the cutover re-keyed the runtime onto the moved file"
        );
        {
            let registry = manager.registry.read().await;

            assert!(
                !registry.by_file.contains_key(&old_identity),
                "the stale source-identity entry is gone"
            );
            assert!(
                registry
                    .by_file
                    .get(&new_identity)
                    .is_some_and(|candidate| Arc::ptr_eq(candidate, &manager.primary)),
                "the moved identity keys the same live runtime"
            );
        }
        assert_eq!(
            spawner.spawns.load(Ordering::SeqCst),
            0,
            "recovery never spawns a replacement runtime"
        );
        let rebound = manager.resolve(None).await.expect("primary resolves");
        assert!(
            Arc::ptr_eq(&rebound, &manager.primary),
            "the session stays active on the same runtime"
        );

        // The advertised reopen is executable: the exact-path reuse and the
        // loaded-id reuse both resolve the moved row to the SAME runtime
        // instead of rejecting it as a dual registration.
        let reopened = manager
            .open_persisted(&manager.primary, &destination.to_string_lossy(), None)
            .await
            .expect("reopen of the moved row rebinds the live runtime");
        assert!(Arc::ptr_eq(&reopened.0, &manager.primary));
        let prepared = pi_coding::PreparedSessionResume::prepare_path(&destination)
            .expect("prepare moved destination");
        let by_id = manager
            .existing_runtime_for_id(&active_session_id, Some(prepared.identity()))
            .await
            .expect("loaded-id reuse must not treat the moved runtime as dual registration");
        assert!(
            by_id.is_some_and(|runtime| Arc::ptr_eq(&runtime, &manager.primary)),
            "the moved session id resolves to the same live runtime"
        );

        let _ = std::fs::remove_dir_all(pi_coding::default_session_dir(target.path()));
        manager.shutdown().await;
        manager.primary.application.cleanup().await;
    }

    /// OMP-parity `session_handoff`: the response envelope is the same
    /// subscribe-handoff-v1 switch handoff the web consumes (native tuple
    /// bound to the NEW session), the new session lives in the SAME cwd with
    /// the handoff document as its FIRST record, and the source session
    /// stays untouched.
    #[tokio::test]
    async fn session_handoff_creates_new_session_with_handoff_record_first() {
        let (manager, primary_registration) =
            manager_with(Some(Arc::new(TestSpawner::default()))).await;
        let source_id = manager.primary_session_id();
        let source_cwd = manager.primary.application.session().cwd().to_path_buf();
        // The compaction summarization makes one bounded provider call.
        primary_registration.set_responses(vec![FauxResponse::text(
            "Continue the audit from where it stopped.",
        )]);

        let response = manager
            .dispatch(
                RpcCommand::SessionHandoff {
                    id: Some("handoff-1".into()),
                    custom_instructions: Some("finish the audit".into()),
                    response_mode: Some(crate::modes::rpc::SubscribeHandoffV1::SubscribeHandoffV1),
                },
                None,
            )
            .await;
        assert!(response.success, "{}", response.error.unwrap_or_default());
        let data = response.data.expect("session_handoff envelope");

        // The envelope parses under the web's parseSwitchHandoff contract:
        // native tuple bound to the NEW session, subscription required, v1,
        // non-negative cursor, no forbidden snapshot payload.
        assert!(data.get("state").is_none() && data.get("messages").is_none());
        assert_eq!(data["subscriptionRequired"], json!(true));
        assert_eq!(data["lifecycleResponseVersion"], json!(1));
        let new_session_id = data["sessionId"].as_str().expect("sessionId").to_owned();
        assert!(
            !new_session_id.is_empty() && new_session_id != source_id,
            "the handoff target is a NEW session, not the source"
        );
        assert_eq!(data["catalogSource"], json!("rpi"));
        assert_eq!(data["catalogSessionId"], json!(new_session_id));
        assert_eq!(data["catalogStatus"], json!("native"));
        assert!(
            data["cursor"].as_u64().is_some(),
            "the handoff carries a subscription cursor"
        );

        // The new runtime is registered and runs in the SAME cwd.
        let runtime = manager
            .resolve(Some(&new_session_id))
            .await
            .expect("the handoff runtime is registered");
        assert_eq!(runtime.application.session().cwd(), source_cwd);

        // The handoff document is the FIRST content record of the new
        // session, DURABLE in the journal (creation-internal bookkeeping
        // like model_change may precede it, but no user-visible record
        // does).
        let (_, session_file) = runtime
            .application
            .session()
            .recorder_info()
            .expect("handoff recorder");
        let tree = pi_coding::load_session_tree(&session_file).expect("handoff session tree");
        let mut handoff_entry: Option<&pi_coding::SessionEntry> = None;
        for entry in &tree.entries {
            if handoff_entry.is_none()
                && entry.entry_type == "custom"
                && entry.custom_type.as_deref() == Some("handoff")
            {
                handoff_entry = Some(entry);
            }
            if handoff_entry.is_none() && entry.entry_type == "message" {
                panic!("a message record precedes the handoff record: {entry:?}");
            }
        }
        // Durable proof: the record is asserted from the FILE on disk after
        // persist_recording_now_durable (write + flush + fsync), not from
        // the in-memory recorder.
        let handoff_entry = handoff_entry.unwrap_or_else(|| {
            panic!(
                "the new session has no durable handoff record: {} entries",
                tree.entries.len()
            )
        });
        let text = handoff_entry
            .data
            .as_ref()
            .and_then(|data| data.get("text").and_then(Value::as_str))
            .unwrap_or_default()
            .to_owned();
        let expected = manager
            .primary
            .application
            .generate_handoff()
            .expect("source generate_handoff");
        assert!(
            expected.prose.is_none(),
            "generate_handoff is the non-prose envelope"
        );
        let mut expected_document = expected.render();
        expected_document.push_str("\n\n## Operator instructions\n");
        expected_document.push_str("finish the audit");
        let expected_text =
            format!("<handoff-context>\n{expected_document}\n</handoff-context>");
        assert_eq!(
            text, expected_text,
            "session_handoff_command must persist generate_handoff() (not generate_handoff_with_prose): {text}"
        );
        assert!(
            !text.contains("Continue the audit from where it stopped."),
            "summarizer prose must not enter the Web handoff document: {text}"
        );
        // The source session stays authoritative and untouched.
        assert_eq!(manager.primary_session_id(), source_id);
        manager.shutdown().await;
        manager.primary.application.cleanup().await;
    }

    /// Production-path active move: a REAL recorded application whose
    /// recorder holds the session file OPEN, real sidecars (.loops.json) and
    /// a children/ subtree next to it, and a queued steer — the exact live
    /// deployment shape. The move must SUCCEED end to end (core move + cutover).
    #[tokio::test]
    async fn active_move_with_open_recorder_sidecars_and_children_succeeds() {
        let (native, project, _service, spawner) = production_manager_spawner();
        let (manager, _dir) = production_manager(spawner, "active-move-live-recorder").await;
        let (session_id, session_file) = manager
            .primary
            .application
            .session()
            .recorder_info()
            .expect("active recorder");
        // Production cutover wiring: the application's cross-directory
        // switch needs the runtime factory the startup path attaches.
        let cli = <crate::args::Cli as clap::Parser>::try_parse_from([
            "rpi",
            "--api-key",
            "test-key",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
        ])
        .expect("cli");
        let agent_dir = native.path().join("agent");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");
        std::fs::write(
            agent_dir.join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )
        .expect("unconfined sandbox settings");
        let mut resource_options = pi_coding::ResourceManagerOptions::new(project.path());
        resource_options.agent_dir = agent_dir;
        resource_options.project_trust_override = Some(true);
        resource_options.disable_extensions = true;
        resource_options.disable_skills = true;
        resource_options.disable_prompt_templates = true;
        resource_options.disable_themes = true;
        resource_options.disable_context_files = true;
        let mut blueprint = crate::session_run_blueprint::RunSessionBlueprint::from_cli(
            &cli,
            resource_options,
            None,
        );
        blueprint.set_session_storage(crate::session_run::SessionStorage::ExplicitRoot(
            native.path().to_path_buf(),
        ));
        manager
            .primary
            .application
            .attach_runtime_factory(Arc::new(blueprint))
            .expect("attach production runtime factory");
        // Live deployment shape: sidecar + children subtree next to the open primary.
        let loops_sidecar = session_file
            .parent()
            .expect("session dir")
            .join(format!(
                "{}.loops.json",
                session_file
                    .file_name()
                    .expect("primary name")
                    .to_string_lossy()
            ));
        std::fs::write(&loops_sidecar, b"{\"version\":1,\"tasks\":[]}\n")
            .expect("loops sidecar");
        let children_dir = session_file
            .parent()
            .expect("session dir")
            .join("children")
            .join(&session_id);
        std::fs::create_dir_all(&children_dir).expect("children dir");
        std::fs::write(children_dir.join("child.jsonl"), b"{}\n").expect("child file");

        // The queued steer rides through the move (reject_queued=false).
        manager
            .primary
            .application
            .steer("parked-steer".to_owned(), Vec::new())
            .await
            .expect("queue steer");

        let target = tempfile::tempdir().expect("move target");
        let moved = manager
            .dispatch(
                RpcCommand::SessionMove {
                    id: Some("active-live-move".into()),
                    session_path: session_file.to_string_lossy().into_owned(),
                    target_cwd: target.path().to_string_lossy().into_owned(),
                },
                None,
            )
            .await;
        assert!(
            moved.success,
            "the production-path active move must succeed: {:?}",
            moved.error
        );
        let destination = PathBuf::from(
            moved.data.as_ref().expect("move data")["to"]
                .as_str()
                .expect("destination path"),
        );
        assert!(destination.is_file(), "the primary moved");
        assert!(
            destination
                .parent()
                .expect("destination dir")
                .join(format!(
                    "{}.loops.json",
                    destination
                        .file_name()
                        .expect("primary name")
                        .to_string_lossy()
                ))
                .is_file(),
            "the loops sidecar moved"
        );
        assert!(
            !session_file.exists(),
            "the source primary must be removed by the move"
        );
        assert_eq!(
            manager.primary.application.session().cwd(),
            target.path(),
            "the cutover rebound the runtime to the destination"
        );
        let _ = (native, project, children_dir, session_file, session_id);
        let _ = std::fs::remove_dir_all(pi_coding::default_session_dir(target.path()));
        manager.shutdown().await;
        manager.primary.application.cleanup().await;
    }

    /// A handoff is refused while the session streams a turn (TUI guard
    /// parity): no new runtime is created and the old session stays
    /// authoritative.
    #[tokio::test]
    async fn session_handoff_refuses_while_streaming() {
        let spawner = Arc::new(TestSpawner::default());
        let release = CancellationToken::new();
        let (manager, _dir) =
            manager_with_gated_primary_stream(Some(spawner.clone()), release.clone(), "held")
                .await;
        let source_id = manager.primary_session_id();
        let mut events = manager.events();
        let manager_for_prompt = manager.clone();
        let prompt_task = tokio::spawn(async move {
            manager_for_prompt
                .dispatch(
                    RpcCommand::Prompt {
                        id: Some("held-prompt".into()),
                        message: "held busy turn".into(),
                        images: Vec::new(),
                        streaming_behavior: None,
                    },
                    None,
                )
                .await
        });
        // The gated stream keeps the turn verifiably in flight.
        let turn_started = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let frame = loop {
                    match events.recv().await {
                        Ok(frame) => break frame,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            let _ = n;
                        }
                        Err(e) => panic!("fan-in stream closed: {e}"),
                    }
                };
                if frame.get("type").and_then(Value::as_str) == Some("turn_started") {
                    break;
                }
            }
        })
        .await
        .expect("busy turn started");
        let _ = turn_started;

        let response = manager
            .dispatch(
                RpcCommand::SessionHandoff {
                    id: Some("handoff-streaming".into()),
                    custom_instructions: None,
                    response_mode: Some(crate::modes::rpc::SubscribeHandoffV1::SubscribeHandoffV1),
                },
                None,
            )
            .await;
        assert!(
            !response.success,
            "a streaming session must refuse the handoff: {response:?}"
        );
        assert!(
            response
                .error
                .as_deref()
                .is_some_and(|error| error.contains("streaming")),
            "the refusal names the streaming guard: {response:?}"
        );
        assert_eq!(
            manager.primary_session_id(),
            source_id,
            "the old session stays authoritative"
        );
        assert_eq!(
            spawner.spawns.load(Ordering::SeqCst),
            0,
            "no new runtime is created for a refused handoff"
        );

        release.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(10), prompt_task).await;
        manager.shutdown().await;
        manager.primary.application.cleanup().await;
    }

    /// A creation failure keeps the old session authoritative: no new
    /// runtime registers and the source session id is unchanged.
    #[tokio::test]
    async fn session_handoff_failure_keeps_old_session_authoritative() {
        let spawner = FailingSpawner::new();
        spawner.fail_next_with("injected handoff creation failure");
        let (manager, primary_registration) = manager_with(Some(Arc::new(spawner))).await;
        let source_id = manager.primary_session_id();
        // The summarization succeeds; the CREATION is what fails.
        primary_registration.set_responses(vec![FauxResponse::text("handoff prose")]);
        let response = manager
            .dispatch(
                RpcCommand::SessionHandoff {
                    id: Some("handoff-fail".into()),
                    custom_instructions: None,
                    response_mode: Some(crate::modes::rpc::SubscribeHandoffV1::SubscribeHandoffV1),
                },
                None,
            )
            .await;
        assert!(
            !response.success,
            "a failed creation must fail the handoff: {response:?}"
        );
        assert_eq!(
            response.error.as_deref(),
            Some("session could not be opened right now; verify the selection and retry"),
            "creation failures surface the bounded open failure"
        );
        assert!(
            !response
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("injected"),
            "the bounded failure never echoes the raw cause chain"
        );
        let registry = manager.registry.read().await;
        assert_eq!(
            registry.distinct_runtimes().len(),
            1,
            "no half-made session lingers in the registry"
        );
        drop(registry);
        manager.shutdown().await;
        manager.primary.application.cleanup().await;
    }

    #[tokio::test]
    async fn session_move_unrecovered_cutover_does_not_advertise_reopen() {
        let (_home, _native, catalog) = web_catalog_fixture();
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog));
        // No runtime factory: the cross-directory switch fails on BOTH the
        // attempt and the in-process recovery retry, so the cutover is
        // unrecoverable and the runtime stays keyed on the deleted source
        // identity.
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let (active_session_id, active_path) = manager
            .primary
            .application
            .session()
            .recorder_info()
            .expect("active recorder");
        let target = tempfile::tempdir().expect("move target");

        let moved = manager
            .dispatch(
                RpcCommand::SessionMove {
                    id: Some("unrecovered".into()),
                    session_path: active_path.to_string_lossy().into_owned(),
                    target_cwd: target.path().to_string_lossy().into_owned(),
                },
                None,
            )
            .await;
        assert!(!moved.success, "an unrecovered cutover must fail the move");
        assert_eq!(
            moved.error.as_deref(),
            Some("session moved but the active cutover failed"),
            "the failure is a fixed literal that never advertises reopen"
        );
        let destination = pi_coding::default_session_dir(target.path())
            .join(active_path.file_name().expect("source file name"));
        assert!(destination.exists(), "the file move is durable");

        // The reopen the old message advertised is genuinely broken: the
        // live primary is still keyed on the deleted source identity, so the
        // loaded-id reuse path rejects the moved row.
        let reopened = manager
            .open_persisted(&manager.primary, &destination.to_string_lossy(), None)
            .await;
        assert!(
            reopened.is_err(),
            "the unrecovered runtime cannot serve the moved row"
        );

        let _ = std::fs::remove_dir_all(pi_coding::default_session_dir(target.path()));
        manager.shutdown().await;
        manager.primary.application.cleanup().await;
    }


    /// P0 regression (WS 1006 on every loaded-row switch): `start_owner`
    /// must REGISTER its JoinHandle on the runtime. With the handle
    /// unregistered, `abort_owner`/`finish_owner` silently no-op: retiring a
    /// loaded runtime "succeeds" without stopping its owner, and the
    /// follow-up cleanup tears the application down UNDER the live owner —
    /// the owner's event forwarding dies mid-connection and the client's
    /// WebSocket closes 1006. The owner handle must be registered at start
    /// and actually consumed (task terminated) by retirement.
    #[tokio::test]
    async fn retired_runtime_owner_is_registered_and_actually_terminates() {
        let (manager, _primary_registration) =
            manager_with(Some(Arc::new(TestSpawner::default()))).await;
        assert!(
            manager.primary.owner.lock().expect("owner lock").is_some(),
            "start_owner must register its handle so retirement can abort/join it"
        );

        // A spawned (loaded secondary) runtime must equally carry a
        // registered owner handle.
        let opened = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(opened.success, "{}", opened.error.unwrap_or_default());
        let secondary = manager
            .registry
            .read()
            .await
            .distinct_runtimes()
            .into_iter()
            .find(|runtime| !Arc::ptr_eq(runtime, &manager.primary))
            .expect("the spawned secondary runtime is registered");
        assert!(
            secondary.owner.lock().expect("owner lock").is_some(),
            "a spawned runtime's owner handle must be registered"
        );

        // Retirement must take the handle AND terminate the owner task: the
        // protocol receiver lives in the owner, so once the owner is gone
        // the protocol sender can no longer reserve capacity.
        manager.shutdown_runtime(&secondary).await;
        assert!(
            secondary.owner.lock().expect("owner lock").is_none(),
            "retirement must consume the owner handle"
        );
        let owner_gone = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if secondary
                    .protocol_tx
                    .reserve()
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            owner_gone.is_ok(),
            "the retired runtime's owner task must terminate; a live owner under cleanup is the WS-1006 mechanism"
        );
        manager.shutdown().await;
        manager.primary.application.cleanup().await;
    }

    #[tokio::test]
    async fn session_move_rejects_busy_loaded_runtime_and_unloads_idle_one() {
        let (_home, _native, catalog) = web_catalog_fixture();
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog));
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let opened = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(opened.success, "{}", opened.error.unwrap_or_default());
        let session_id = opened.data.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        let session_path = manager
            .resolve(Some(&session_id))
            .await
            .expect("loaded runtime")
            .application
            .session()
            .recorder_info()
            .expect("recorder")
            .1;
        let target = tempfile::tempdir().expect("move target");

        // Busy: a turn is verifiably in flight when the move is attempted;
        // the fan-in subscription precedes the prompt dispatch so the
        // turn_started frame can never be missed by a late subscriber.
        let mut events = manager.events();
        spawner
            .registration
            .set_responses(vec![FauxResponse::text("x".repeat(200_000))]);
        let busy_manager = manager.clone();
        let busy_id = session_id.clone();
        let busy_task = tokio::spawn(async move {
            busy_manager
                .dispatch(
                    RpcCommand::Prompt {
                        id: None,
                        message: "busy".into(),
                        images: Vec::new(),
                        streaming_behavior: None,
                    },
                    Some(busy_id),
                )
                .await
        });
        // Deterministic busy evidence, never a fixed sleep: the target's
        // turn_started frame on the pre-subscribed fan-in stream, then the
        // prompt admission joined and the authoritative application busy
        // field read through the runtime itself.
        let turn_started = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let frame = loop {
                    match events.recv().await {
                        Ok(frame) => break frame,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            let _ = n;
                        }
                        Err(e) => panic!("fan-in stream closed: {e}"),
                    }
                };
                if frame.get("type").and_then(Value::as_str) == Some("turn_started")
                    && frame.get("sessionId").and_then(Value::as_str) == Some(session_id.as_str())
                {
                    break;
                }
            }
        })
        .await
        .expect("busy turn started");
        let _ = turn_started;
        let prompt_result = tokio::time::timeout(std::time::Duration::from_secs(10), busy_task)
            .await
            .expect("busy prompt admission joins")
            .expect("prompt task");
        assert!(prompt_result.success, "busy turn admission succeeded");
        let busy_state = manager
            .dispatch(RpcCommand::GetState { id: None }, Some(session_id.clone()))
            .await;
        assert!(
            busy_state.success,
            "{}",
            busy_state.error.unwrap_or_default()
        );
        assert_eq!(
            busy_state.data.unwrap()["isStreaming"],
            json!(true),
            "authoritative runtime state confirms the turn is in flight"
        );
        // The move is dispatched with the PRIMARY as the command source
        // (sessionId None): the source runtime must never BE the move target,
        // or the active-session guard would shadow the busy lifecycle
        // rejection under test.
        let busy = manager
            .dispatch(
                RpcCommand::SessionMove {
                    id: Some("busy".into()),
                    session_path: session_path.to_string_lossy().into_owned(),
                    target_cwd: target.path().to_string_lossy().into_owned(),
                },
                None,
            )
            .await;
        assert!(!busy.success);
        assert_eq!(
            busy.error.as_deref(),
            Some("session is busy: a turn is in progress"),
            "busy rejection carries the exact ensure_idle reason"
        );
        let aborted = manager
            .dispatch(RpcCommand::Abort { id: None }, Some(session_id.clone()))
            .await;
        assert!(aborted.success, "{}", aborted.error.unwrap_or_default());
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let state = manager
                .dispatch(RpcCommand::GetState { id: None }, Some(session_id.clone()))
                .await;
            if state.data.as_ref().and_then(|data| data.get("isStreaming"))
                == Some(&json!(false))
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "runtime never reached idle"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        // Idle: the loaded secondary unloads, then the saved row moves.
        let moved = manager
            .dispatch(
                RpcCommand::SessionMove {
                    id: Some("idle-move".into()),
                    session_path: session_path.to_string_lossy().into_owned(),
                    target_cwd: target.path().to_string_lossy().into_owned(),
                },
                None,
            )
            .await;
        assert!(moved.success, "{}", moved.error.unwrap_or_default());
        assert!(
            manager.resolve(Some(&session_id)).await.is_err(),
            "no ghost loaded runtime survives the move"
        );
        assert!(!session_path.exists());
        let _ = std::fs::remove_dir_all(pi_coding::default_session_dir(target.path()));
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn session_move_collision_fails_bounded_without_path_echo() {
        let (_home, native, catalog) = web_catalog_fixture();
        let cwd = tempfile::tempdir().expect("source cwd");
        let (path, _) = persist_production_source(
            cwd.path(),
            native.path(),
            "move-collision",
            &["collision prompt"],
        );
        let target = tempfile::tempdir().expect("move target");
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog));
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let destination_dir = pi_coding::default_session_dir(target.path());
        std::fs::create_dir_all(&destination_dir).expect("destination store");
        let destination = destination_dir.join(path.file_name().expect("session file name"));
        // The collision must be a VALID same-id session body: the migration
        // header-validates an existing destination before comparing content,
        // so raw non-JSONL bytes would be refused earlier with a different
        // (generic) bounded error.
        let mut collision = std::fs::read_to_string(&path).expect("source session body");
        collision.push_str("{}\n");
        std::fs::write(&destination, collision).expect("collision file");

        let rejected = manager
            .dispatch(
                RpcCommand::SessionMove {
                    id: None,
                    session_path: path.to_string_lossy().into_owned(),
                    target_cwd: target.path().to_string_lossy().into_owned(),
                },
                None,
            )
            .await;
        assert!(!rejected.success);
        assert_eq!(
            rejected.error.as_deref(),
            Some("move destination already exists with different content")
        );
        assert!(path.exists(), "source untouched by a refused collision");
        let error_text = rejected.error.unwrap_or_default();
        assert!(
            !error_text.contains(path.to_string_lossy().as_ref())
                && !error_text.contains(target.path().to_string_lossy().as_ref()),
            "no local-path echo: {error_text}"
        );

        let _ = std::fs::remove_dir_all(&destination_dir);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn session_move_is_visible_after_manager_recreation_and_switches_to_destination() {
        let (_home, native, catalog) = web_catalog_fixture();
        let cwd = tempfile::tempdir().expect("source cwd");
        let (path, _) = persist_production_source(
            cwd.path(),
            native.path(),
            "move-recreation",
            &["before recreation"],
        );
        let target = tempfile::tempdir().expect("move target");
        let spawner = Arc::new(TestSpawner::default().with_catalog(catalog.clone()));
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let moved = manager
            .dispatch(
                RpcCommand::SessionMove {
                    id: Some("recreate".into()),
                    session_path: path.to_string_lossy().into_owned(),
                    target_cwd: target.path().to_string_lossy().into_owned(),
                },
                None,
            )
            .await;
        assert!(moved.success, "{}", moved.error.unwrap_or_default());
        let destination =
            PathBuf::from(moved.data.as_ref().unwrap()["to"].as_str().expect("destination path"));
        manager.shutdown().await;
        drop(manager);

        // A fresh manager over the same catalog sees the moved world only:
        // the old row is gone, and the destination opens by explicit switch.
        let recreated_spawner = Arc::new(TestSpawner::default().with_catalog(catalog));
        let (recreated, _recreated_registration) = manager_with(Some(recreated_spawner.clone())).await;
        assert!(!path.exists());
        assert!(destination.exists());
        let switched = recreated
            .dispatch(
                RpcCommand::SwitchSession {
                    id: Some("switch-moved".into()),
                    session_path: destination.to_string_lossy().into_owned(),
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: None,
                },
                None,
            )
            .await;
        assert!(switched.success, "{}", switched.error.unwrap_or_default());
        assert_eq!(
            switched.data.as_ref().unwrap()["sessionId"],
            json!("move-recreation"),
            "the moved session keeps its identity across the switch"
        );
        let opened = recreated_spawner.opened.lock().expect("opened sessions");
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0], destination);
        drop(opened);

        let _ = std::fs::remove_dir_all(pi_coding::default_session_dir(target.path()));
        recreated.shutdown().await;
    }
    #[tokio::test]
    async fn subscription_snapshot_fails_closed_while_runtime_retires() {
        let (manager, _primary_registration) =
            manager_with(Some(Arc::new(TestSpawner::default()))).await;
        let opened = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(opened.success, "{}", opened.error.unwrap_or_default());
        let session_id = opened.data.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        let runtime = manager
            .resolve(Some(&session_id))
            .await
            .expect("secondary runtime");
        let retiring = runtime.lifecycle_lock.clone().write_owned().await;

        let error = manager
            .subscription_snapshot(&session_id, 0)
            .await
            .expect_err("retiring runtime cannot publish subscription snapshot");
        assert_eq!(
            error.to_string(),
            "session could not be opened right now; verify the selection and retry"
        );

        manager.unregister_runtime(&runtime).await;
        drop(retiring);
        assert!(manager.subscription_snapshot(&session_id, 0).await.is_err());
        manager.cleanup_runtime_contents(&runtime).await;
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn collaboration_snapshot_fails_closed_while_runtime_retires() {
        let (manager, _primary_registration) =
            manager_with(Some(Arc::new(TestSpawner::default()))).await;
        let opened = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(opened.success, "{}", opened.error.unwrap_or_default());
        let session_id = opened.data.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        let runtime = manager
            .resolve(Some(&session_id))
            .await
            .expect("secondary runtime");
        let retiring = runtime.lifecycle_lock.clone().write_owned().await;

        let error = manager
            .collab_snapshot(Some(&session_id), 16, 16 * 1024)
            .await
            .expect_err("retiring runtime cannot publish collaboration snapshot");
        assert_eq!(
            error.to_string(),
            "session could not be opened right now; verify the selection and retry"
        );

        manager.unregister_runtime(&runtime).await;
        drop(retiring);
        assert!(
            manager
                .collab_snapshot(Some(&session_id), 16, 16 * 1024)
                .await
                .is_err()
        );
        manager.cleanup_runtime_contents(&runtime).await;
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn session_list_includes_every_loaded_runtime() {
        let spawner = Arc::new(TestSpawner::default());
        let (primary_application, primary_id, _primary_dir) =
            recorded_application("primary-list").await;
        let manager = SessionRuntimeManager::new(
            primary_application,
            ExtensionUiAdapter::default(),
            Some(spawner.clone()),
            None,
        )
        .await
        .expect("session runtime manager");
        let opened = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        let b_id = opened.data.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();

        let listed = manager
            .overlay_loaded_sessions(RpcResponse::success(
                None,
                "session_list",
                Some(json!({"sessions": []})),
            ))
            .await;
        let listed_data = listed.data.unwrap();
        let sessions = listed_data["sessions"].as_array().expect("sessions");
        let child = sessions
            .iter()
            .find(|row| row.get("sessionId").and_then(Value::as_str) == Some(b_id.as_str()))
            .expect("fresh child row in session_list");
        assert_eq!(child.get("loaded").and_then(Value::as_bool), Some(true));
        assert!(
            child
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|path| !path.is_empty())
        );
        assert_eq!(
            child.get("source").and_then(Value::as_str),
            Some(pi_coding::SessionSourceKind::NativePi.label())
        );
        assert_eq!(child.get("status").and_then(Value::as_str), Some("native"));
        let primary = sessions
            .iter()
            .find(|row| row.get("sessionId").and_then(Value::as_str) == Some(primary_id.as_str()))
            .expect("fresh primary row in session_list");
        assert_eq!(primary.get("loaded").and_then(Value::as_bool), Some(true));
        assert_eq!(
            primary.get("source").and_then(Value::as_str),
            Some(pi_coding::SessionSourceKind::NativePi.label())
        );
        assert_eq!(primary.get("status").and_then(Value::as_str), Some("native"));
        let imported_listed = manager
            .overlay_loaded_sessions(RpcResponse::success(
                None,
                "session_list",
                Some(json!({"sessions": [json!({
                    "source": "grok",
                    "sessionId": "grok-source",
                    "nativeSessionId": b_id,
                    "nativePath": child["path"],
                    "path": "<workspace>/grok/summary.json",
                    "status": "already_imported",
                })]})),
            ))
            .await;
        let imported_rows = imported_listed.data.unwrap()["sessions"]
            .as_array()
            .expect("imported sessions")
            .clone();
        let imported = imported_rows
            .iter()
            .find(|row| row["sessionId"] == "grok-source")
            .expect("imported source row");
        let imported_native = imported_rows
            .iter()
            .find(|row| row["sessionId"] == b_id && row["source"] == "rpi")
            .expect("native loaded row beside imported source");
        assert_eq!(imported["status"], "already_imported");
        assert_eq!(imported["loaded"], true);
        assert_eq!(imported["runtimeIncarnation"], imported_native["runtimeIncarnation"]);


        let stale_listed = manager
            .overlay_loaded_sessions(RpcResponse::success(
                None,
                "session_list",
                Some(json!({"sessions": [json!({
                    "source": "codex",
                    "sessionId": b_id,
                    "name": "stale-foreign",
                    "cwd": "/tmp/stale",
                    "displayTime": "",
                    "modifiedEpoch": 0.0,
                    "summary": "",
                    "path": "/tmp/stale/stale.jsonl",
                    "size": 0,
                    "messageCount": 0,
                    "status": "foreign",
                })]})),
            ))
            .await;
        let stale_sessions = stale_listed.data.unwrap()["sessions"]
            .as_array()
            .expect("sessions")
            .clone();
        let stale_foreign = stale_sessions
            .iter()
            .find(|row| {
                row.get("sessionId").and_then(Value::as_str) == Some(b_id.as_str())
                    && row.get("source").and_then(Value::as_str) == Some("codex")
            })
            .expect("same-id foreign collision row left in place");
        assert_eq!(
            stale_foreign.get("status").and_then(Value::as_str),
            Some("foreign")
        );
        assert_eq!(stale_foreign.get("loaded").and_then(Value::as_bool), None);
        assert_eq!(
            stale_foreign.get("path").and_then(Value::as_str),
            Some("/tmp/stale/stale.jsonl")
        );
        let stale_native = stale_sessions
            .iter()
            .find(|row| {
                row.get("sessionId").and_then(Value::as_str) == Some(b_id.as_str())
                    && row.get("source").and_then(Value::as_str)
                        == Some(pi_coding::SessionSourceKind::NativePi.label())
            })
            .expect("real rpi/native loaded row synthesized beside the foreign row");
        assert_eq!(
            stale_native.get("status").and_then(Value::as_str),
            Some("native")
        );
        assert_eq!(stale_native.get("loaded").and_then(Value::as_bool), Some(true));
        assert_ne!(
            stale_native.get("path").and_then(Value::as_str),
            Some("/tmp/stale/stale.jsonl"),
            "the synthesized row carries the actual runtime path, not the foreign path"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn session_forked_projection_uses_forked_session_id() {
        let target = pi_coding::SessionForkedEvent {
            target_id: "entry-1".into(),
            session_id: "fork-target-id".into(),
            session_file: "/tmp/fork.jsonl".into(),
            editor_text: "prompt text".into(),
        };
        let projected =
            project_application_event(ApplicationEvent::SessionForked(target)).expect("project");
        assert_eq!(projected["type"], "session_forked");
        assert_eq!(projected["forkedSessionId"], "fork-target-id");
        assert!(projected.get("sessionId").is_none(), "{projected}");
    }

    #[tokio::test]
    async fn event_envelopes_are_owned_sequenced_and_replayable() {
        let spawner = Arc::new(TestSpawner::default());
        let (application, session_id, _session_dir) = recorded_application("sequence-replay").await;
        let manager =
            SessionRuntimeManager::new(application, ExtensionUiAdapter::default(), Some(spawner), None)
                .await
                .expect("session runtime manager");
        let runtime = manager.resolve(Some(&session_id)).await.expect("runtime");
        let first = runtime
            .envelope_test_event(ApplicationEvent::TurnStarted {
                turn_id: "turn-a".to_owned(),
                model: None,
            })
            .await
            .expect("turn start envelope");
        let second = runtime
            .envelope_test_event(ApplicationEvent::TurnTerminal {
                turn_id: "turn-a".to_owned(),
                status: pi_coding::TurnTerminalStatus::Succeeded,
                message: None,
            })
            .await
            .expect("turn terminal envelope");
        assert_eq!(first["sessionId"], session_id);
        assert_eq!(first["sequence"], 1);
        assert_eq!(second["sequence"], 2);
        let snapshot = manager
            .subscription_snapshot(&session_id, 1)
            .await
            .expect("subscription snapshot");
        assert_eq!(snapshot["watermark"], 2);
        assert_eq!(snapshot["type"], "subscription_snapshot");
        assert_eq!(snapshot["events"].as_array().expect("events").len(), 1);
        assert_eq!(snapshot["events"][0]["type"], "turn_terminal");
        manager.shutdown().await;
    }
    #[tokio::test]
    async fn initial_subscription_uses_messages_without_replaying_completed_turn() {
        let (manager, _registration) = manager_with(None).await;
        let session_id = manager.primary_session_id();
        let runtime = manager.resolve(Some(&session_id)).await.expect("runtime");
        runtime
            .envelope_test_event(ApplicationEvent::TurnStarted {
                turn_id: "turn-complete".to_owned(),
                model: None,
            })
            .await
            .expect("turn start");
        runtime
            .envelope_test_event(ApplicationEvent::TurnTerminal {
                turn_id: "turn-complete".to_owned(),
                status: pi_coding::TurnTerminalStatus::Succeeded,
                message: None,
            })
            .await
            .expect("turn terminal");

        let initial = manager
            .subscription_snapshot(&session_id, 0)
            .await
            .expect("initial subscription");
        assert_eq!(initial["watermark"], 2);
        assert!(initial["events"].as_array().expect("events").is_empty());
        // The first snapshot consumed the single test-delivery permit; the
        // reconnect snapshot needs one more (same convention as the
        // dual-Running recovery test).
        manager.release_subscription_snapshots_for_test(1);
        let reconnect = manager
            .subscription_snapshot(&session_id, 1)
            .await
            .expect("reconnect subscription");
        assert_eq!(reconnect["events"].as_array().expect("events").len(), 1);
        assert_eq!(reconnect["events"][0]["type"], "turn_terminal");
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn cursor_zero_snapshot_excludes_completed_history_events_from_later_replay() {
        let (manager, registration) = manager_with(None).await;
        registration.set_responses(vec![FauxResponse::text("queued assistant")]);
        let session_id = manager.primary_session_id();
        let runtime = manager.resolve(Some(&session_id)).await.expect("runtime");
        runtime.set_application_event_drain_for_test(false).await;
        let prompt = runtime
            .application
            .prompt("snapshot race".to_owned(), Vec::new(), None)
            .await;
        assert!(prompt.is_ok(), "prompt admission: {prompt:?}");
        runtime.application.wait_for_idle().await;
        let queued_snapshot = runtime.application.protocol_snapshot().await;
        assert!(
            queued_snapshot.event_barrier > 0,
            "completed history must expose queued application events"
        );
        drop(queued_snapshot);

        let snapshot = tokio::time::timeout(
            Duration::from_secs(10),
            manager.subscription_snapshot(&session_id, 0),
        )
        .await
        .expect("initial subscription must not hang")
        .expect("initial subscription");
        assert!(snapshot["events"].as_array().expect("events").is_empty());
        let messages = snapshot["messages"].as_array().expect("messages");
        assert_eq!(
            messages
                .iter()
                .filter(|entry| entry["message"]["role"] == "assistant")
                .count(),
            1,
            "authoritative transcript contains one final assistant: {messages:?}",
        );
        let watermark = snapshot["watermark"].as_u64().expect("watermark");
        assert!(
            watermark
                >= snapshot["state"]["messageCount"]
                    .as_u64()
                    .unwrap_or_default()
        );

        // The first snapshot consumed the single test-delivery permit.
        manager.release_subscription_snapshots_for_test(1);
        let reconnect = tokio::time::timeout(
            Duration::from_secs(2),
            manager.subscription_snapshot(&session_id, watermark),
        )
        .await
        .expect("post-snapshot reconnect timeout")
        .expect("post-snapshot reconnect");
        assert!(
            reconnect["events"]
                .as_array()
                .expect("reconnect events")
                .is_empty(),
            "pre-snapshot message events must not arrive after the cursor-0 cut: {reconnect}",
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn queued_sequenced_agent_is_received_during_idle_barrier_wait() {
        let (manager, _registration) = manager_with(None).await;
        let (sender, mut receiver) = tokio::sync::broadcast::channel(16);
        let mut snapshot = manager.primary.application.protocol_snapshot().await;
        assert!(
            snapshot.lifecycle.active_turn_id.is_none(),
            "fixture must be idle"
        );
        snapshot.event_barrier = 1;
        let producer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = sender.send(pi_coding::ApplicationEvent::AgentPublished {
                event: pi_agent::AgentEvent::AgentStart,
                epoch: snapshot.epoch,
                barrier: 1,
            });
        });
        let deadline = tokio::time::Instant::now() + SNAPSHOT_BARRIER_ATTEMPT_DEADLINE;
        let event = match next_snapshot_barrier_event(&mut receiver, &snapshot, deadline).await {
            SnapshotBarrierDrain::Event(event) => event,
            other => panic!("queued agent must arrive during the idle wait, got {other:?}"),
        };
        assert!(matches!(
            event,
            pi_coding::ApplicationEvent::AgentPublished { barrier: 1, .. }
        ));
        producer.await.expect("producer");
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn continuous_non_agent_deficit_is_bounded_and_owner_stays_serviceable() {
        let (manager, _registration) = manager_with(None).await;
        let (sender, mut receiver) = tokio::sync::broadcast::channel(8192);
        let mut snapshot = manager.primary.application.protocol_snapshot().await;
        snapshot.event_barrier = 1;
        let producer = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(1));
            loop {
                ticker.tick().await;
                if sender
                    .send(pi_coding::ApplicationEvent::AgentSettled)
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut state = ProtocolState::new(snapshot.epoch);
        let (events_tx, _events_rx) = tokio::sync::broadcast::channel(64);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(150);
        let started = tokio::time::Instant::now();
        let outcome = drain_snapshot_barrier(
            primary(&manager),
            &mut state,
            &mut receiver,
            &events_tx,
            &snapshot,
            deadline,
        )
        .await;
        let elapsed = started.elapsed();
        producer.abort();
        assert!(
            matches!(outcome, SnapshotDrainOutcome::Settled),
            "idle deficit must complete by the shared deadline: {outcome:?}"
        );
        assert_eq!(
            state.forwarded_agent_event_barrier, 1,
            "idle completion advances to the authoritative barrier"
        );
        assert!(
            elapsed >= Duration::from_millis(100) && elapsed < SNAPSHOT_BARRIER_ATTEMPT_DEADLINE,
            "a continuous non-Agent stream must be bounded by the attempt budget: {elapsed:?}"
        );
        // A following request is serviceable immediately: the owner was never
        // monopolized beyond the budget.
        let service_started = tokio::time::Instant::now();
        let outcome = drain_snapshot_barrier(
            primary(&manager),
            &mut state,
            &mut receiver,
            &events_tx,
            &snapshot,
            service_started + Duration::from_millis(200),
        )
        .await;
        assert!(
            matches!(outcome, SnapshotDrainOutcome::Settled),
            "following request must be serviceable: {outcome:?}"
        );
        assert!(
            service_started.elapsed() < Duration::from_millis(500),
            "following request must complete promptly: {:?}",
            service_started.elapsed()
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn repeated_retry_shares_one_attempt_deadline() {
        let (manager, _registration) = manager_with(None).await;
        let (sender, mut receiver) = tokio::sync::broadcast::channel(2);
        let mut snapshot = manager.primary.application.protocol_snapshot().await;
        snapshot.event_barrier = 1;
        // Force a receiver lag so the first pull reports Resync immediately.
        for _ in 0..8 {
            let _ = sender.send(pi_coding::ApplicationEvent::AgentSettled);
        }
        let started = tokio::time::Instant::now();
        let deadline = started + Duration::from_millis(300);
        let first = next_snapshot_barrier_event(&mut receiver, &snapshot, deadline).await;
        assert!(
            matches!(first, SnapshotBarrierDrain::Resync),
            "lag must resync: {first:?}"
        );
        loop {
            let outcome = next_snapshot_barrier_event(&mut receiver, &snapshot, deadline).await;
            if matches!(outcome, SnapshotBarrierDrain::Event(_)) {
                continue;
            }
            assert!(
                matches!(outcome, SnapshotBarrierDrain::Complete),
                "idle attempt must complete on the shared deadline: {outcome:?}"
            );
            break;
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(250) && elapsed < Duration::from_millis(600),
            "the retry must consume the SHARED deadline, not a fresh budget: {elapsed:?}"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn lagged_resync_coverage_suppresses_retained_sequenced_agents() {
        let (manager, _registration) = manager_with(None).await;
        let mut snapshot = manager.primary.application.protocol_snapshot().await;
        snapshot.event_barrier = 3;
        let mut state = ProtocolState::new(snapshot.epoch);
        state.next_sequence = 9;
        let marker = push_session_resync(primary(&manager), &mut state, &snapshot);
        assert_eq!(marker["type"], "session_resync_required");
        assert_eq!(state.forwarded_agent_event_barrier, 3);
        // Retained queued events beneath the authoritative coverage are
        // discarded by ordinal without consuming a sequence.
        let covered = envelope_application_event(
            primary(&manager),
            &mut state,
            pi_coding::ApplicationEvent::AgentPublished {
                event: pi_agent::AgentEvent::AgentStart,
                epoch: snapshot.epoch,
                barrier: 2,
            },
        );
        assert!(
            covered.is_none(),
            "covered sequenced agent must be suppressed"
        );
        assert_eq!(state.forwarded_agent_event_barrier, 3);
        // The first new ordinal is projected exactly once, at its exact value.
        let sequence_before = state.next_sequence;
        let first_new = envelope_application_event(
            primary(&manager),
            &mut state,
            pi_coding::ApplicationEvent::AgentPublished {
                event: pi_agent::AgentEvent::AgentStart,
                epoch: snapshot.epoch,
                barrier: 4,
            },
        )
        .expect("first new ordinal must project");
        assert_eq!(state.forwarded_agent_event_barrier, 4);
        assert_eq!(first_new["sequence"], sequence_before + 1);
        // The same ordinal arriving again produces no payload.
        let duplicate = envelope_application_event(
            primary(&manager),
            &mut state,
            pi_coding::ApplicationEvent::AgentPublished {
                event: pi_agent::AgentEvent::AgentStart,
                epoch: snapshot.epoch,
                barrier: 4,
            },
        );
        assert!(duplicate.is_none(), "replayed ordinal must be suppressed");
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn zero_barrier_runtime_change_is_consumed_before_new_epoch_cut() {
        let (manager, _registration) = manager_with(None).await;
        let mut snapshot = manager.primary.application.protocol_snapshot().await;
        let previous_epoch = snapshot.epoch;
        snapshot.epoch = previous_epoch + 1;
        snapshot.event_barrier = 0;
        let mut state = ProtocolState::new(previous_epoch);
        let (application_tx, mut application_rx) =
            tokio::sync::broadcast::channel::<pi_coding::ApplicationEvent>(16);
        let (events_tx, mut events_rx) = tokio::sync::broadcast::channel(16);
        let _ = application_tx.send(pi_coding::ApplicationEvent::RuntimeChanged {
            epoch: snapshot.epoch,
        });
        let deadline = tokio::time::Instant::now() + SNAPSHOT_BARRIER_ATTEMPT_DEADLINE;
        let outcome = drain_snapshot_barrier(
            primary(&manager),
            &mut state,
            &mut application_rx,
            &events_tx,
            &snapshot,
            deadline,
        )
        .await;
        assert!(
            matches!(outcome, SnapshotDrainOutcome::Restart),
            "epoch changed during drain; outer must restart with fresh snapshot: {outcome:?}"
        );
        assert_eq!(
            state.epoch, snapshot.epoch,
            "the queued runtime change must be consumed before the cut"
        );
        let forwarded = events_rx
            .try_recv()
            .expect("runtime change forwarded pre-cut");
        assert_eq!(forwarded["type"], "runtime_changed");
        assert_eq!(forwarded["sessionId"], manager.primary_session_id());
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn future_epoch_event_does_not_spin_live_owner_before_runtime_changed() {
        let (manager, _registration) = manager_with(None).await;
        let mut events = manager.events();
        let application = manager.primary.application.clone();
        let old_epoch = application.runtime_epoch();
        let new_epoch = old_epoch + 1;
        let runtime_events = application.runtime_events_for_test();
        runtime_events
            .send(pi_coding::ApplicationEvent::AgentPublished {
                event: pi_agent::AgentEvent::AgentStart,
                epoch: new_epoch,
                barrier: 1,
            })
            .expect("future event broadcast");
        // Before the epoch authority, the owner must keep serving protocol
        // commands; a requeue spin would make this request hang.
        let state_response = tokio::time::timeout(
            Duration::from_secs(2),
            manager.dispatch(
                RpcCommand::GetState {
                    id: Some("future-epoch-service".into()),
                },
                Some(manager.primary_session_id()),
            ),
        )
        .await
        .expect("get_state must remain serviceable behind a future event");
        assert!(
            state_response.success,
            "get_state succeeds before RuntimeChanged"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), events.recv())
                .await
                .is_err(),
            "future event emits nothing before RuntimeChanged"
        );
        runtime_events
            .send(pi_coding::ApplicationEvent::RuntimeChanged { epoch: new_epoch })
            .expect("runtime changed broadcast");
        let first = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("runtime changed timeout")
            .expect("runtime changed");
        assert_eq!(first["type"], "runtime_changed");
        assert_eq!(first["sequence"], 1);
        let second = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("retained future event timeout")
            .expect("retained future event");
        assert_eq!(second["type"], "agent_start");
        assert_eq!(second["sequence"], 2);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), events.recv())
                .await
                .is_err(),
            "retained event emitted exactly once"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn live_owner_flushes_all_pending_epoch_events_after_runtime_changed() {
        let (manager, _registration) = manager_with(None).await;
        let mut events = manager.events();
        let application = manager.primary.application.clone();
        let old_epoch = application.runtime_epoch();
        let new_epoch = old_epoch + 1;
        let runtime_events = application.runtime_events_for_test();
        let publish = |event: pi_coding::ApplicationEvent| {
            runtime_events.send(event).expect("runtime event broadcast");
        };
        publish(pi_coding::ApplicationEvent::AgentPublished {
            event: pi_agent::AgentEvent::AgentStart,
            epoch: new_epoch,
            barrier: 1,
        });
        publish(pi_coding::ApplicationEvent::AgentPublished {
            event: pi_agent::AgentEvent::AgentStart,
            epoch: new_epoch,
            barrier: 2,
        });
        publish(pi_coding::ApplicationEvent::RuntimeChanged { epoch: new_epoch });
        let first = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("runtime changed timeout")
            .expect("runtime changed");
        assert_eq!(first["type"], "runtime_changed");
        assert_eq!(first["epoch"], new_epoch);
        assert_eq!(first["sequence"], 1);
        let second = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("barrier 1 timeout")
            .expect("barrier 1");
        assert_eq!(second["type"], "agent_start");
        assert_eq!(second["sequence"], 2);
        let third = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("barrier 2 timeout")
            .expect("barrier 2");
        assert_eq!(third["type"], "agent_start");
        assert_eq!(third["sequence"], 3);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), events.recv())
                .await
                .is_err(),
            "both retained events emitted exactly once before later traffic"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn sequenced_agent_publication_preserves_all_new_epoch_events() {
        let (manager, _registration) = manager_with(None).await;
        let mut snapshot = manager.primary.application.protocol_snapshot().await;
        let old_epoch = snapshot.epoch;
        let new_epoch = old_epoch + 1;
        snapshot.epoch = new_epoch;
        snapshot.event_barrier = 2;
        let mut state = ProtocolState::new(old_epoch);
        state.forwarded_agent_event_barrier = 7;
        let (application_tx, mut application_rx) =
            tokio::sync::broadcast::channel::<pi_coding::ApplicationEvent>(16);
        let (events_tx, mut events_rx) = tokio::sync::broadcast::channel(16);
        let first_new = pi_coding::ApplicationEvent::AgentPublished {
            event: pi_agent::AgentEvent::AgentStart,
            epoch: new_epoch,
            barrier: 1,
        };
        let mut kept_message = pi_ai::AssistantMessage::pending(&pi_ai::Model::default());
        kept_message.content.push(pi_ai::ContentBlock::text("kept"));
        kept_message.stop_reason = pi_ai::StopReason::Stop;
        let second_new = pi_coding::ApplicationEvent::AgentPublished {
            event: pi_agent::AgentEvent::MessageEnd {
                message: pi_ai::Message::Assistant(kept_message),
            },
            epoch: new_epoch,
            barrier: 2,
        };
        let _ = application_tx.send(first_new);
        let _ = application_tx.send(second_new);
        let _ =
            application_tx.send(pi_coding::ApplicationEvent::RuntimeChanged { epoch: new_epoch });
        let deadline = tokio::time::Instant::now() + SNAPSHOT_BARRIER_ATTEMPT_DEADLINE;
        let first = drain_snapshot_barrier(
            primary(&manager),
            &mut state,
            &mut application_rx,
            &events_tx,
            &snapshot,
            deadline,
        )
        .await;
        assert!(
            matches!(first, SnapshotDrainOutcome::Restart),
            "queued epoch authority restarts the snapshot: {first:?}"
        );
        let runtime_changed = events_rx.try_recv().expect("runtime cut pre-cut");
        assert_eq!(runtime_changed["type"], "runtime_changed");
        assert_eq!(runtime_changed["sequence"], 1);
        let first = events_rx
            .try_recv()
            .expect("first new epoch event retained");
        assert_eq!(first["type"], "agent_start");
        assert_eq!(first["sequence"], 2);
        let second = events_rx
            .try_recv()
            .expect("second new epoch event retained");
        assert_eq!(second["type"], "message_end");
        assert_eq!(second["sequence"], 3);
        assert!(
            events_rx.try_recv().is_err(),
            "each new event emitted exactly once"
        );
        assert_eq!(state.forwarded_agent_event_barrier, 2);
        assert!(state.pending_future_epoch_events.is_empty());
        // Bounded overflow cannot evict an unadmitted event: it returns the
        // dedicated overflow authority and leaves queued events untouched.
        for barrier in 3..=(2 + MAX_PENDING_FUTURE_EPOCH_EVENTS as u64) {
            let pending = envelope_application_event(
                primary(&manager),
                &mut state,
                pi_coding::ApplicationEvent::AgentPublished {
                    event: pi_agent::AgentEvent::AgentStart,
                    epoch: new_epoch + 1,
                    barrier,
                },
            );
            assert!(pending.is_none(), "future event waits for RuntimeChanged");
        }
        let queued_len = state.pending_future_epoch_events.len();
        let overflow = envelope_application_event(
            primary(&manager),
            &mut state,
            pi_coding::ApplicationEvent::AgentPublished {
                event: pi_agent::AgentEvent::AgentStart,
                epoch: new_epoch + 1,
                barrier: 3 + MAX_PENDING_FUTURE_EPOCH_EVENTS as u64,
            },
        );
        assert!(
            overflow.is_none(),
            "overflowing event triggers authority, not payload"
        );
        assert!(
            state.pending_future_epoch_overflow,
            "overflow must be explicit"
        );
        assert_eq!(
            state.pending_future_epoch_events.len(),
            queued_len,
            "overflow never evicts already retained events"
        );
        let outcome = drain_snapshot_barrier(
            primary(&manager),
            &mut state,
            &mut application_rx,
            &events_tx,
            &snapshot,
            deadline,
        )
        .await;
        assert!(
            matches!(outcome, SnapshotDrainOutcome::PendingEpochOverflow),
            "owner must converge through resnapshot authority: {outcome:?}"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn sequenced_agent_publication_binds_exact_runtime_epoch() {
        let (manager, _registration) = manager_with(None).await;
        let mut snapshot = manager.primary.application.protocol_snapshot().await;
        let old_epoch = snapshot.epoch;
        let new_epoch = old_epoch + 1;
        snapshot.epoch = new_epoch;
        snapshot.event_barrier = 1;
        let mut state = ProtocolState::new(old_epoch);
        state.forwarded_agent_event_barrier = 7;
        let (application_tx, mut application_rx) =
            tokio::sync::broadcast::channel::<pi_coding::ApplicationEvent>(16);
        let (events_tx, mut events_rx) = tokio::sync::broadcast::channel(16);
        let _ = application_tx.send(pi_coding::ApplicationEvent::AgentPublished {
            event: pi_agent::AgentEvent::AgentStart,
            epoch: old_epoch,
            barrier: 1,
        });
        let _ =
            application_tx.send(pi_coding::ApplicationEvent::RuntimeChanged { epoch: new_epoch });
        let _ = application_tx.send(pi_coding::ApplicationEvent::AgentPublished {
            event: pi_agent::AgentEvent::AgentStart,
            epoch: new_epoch,
            barrier: 1,
        });
        let deadline = tokio::time::Instant::now() + SNAPSHOT_BARRIER_ATTEMPT_DEADLINE;
        let first = drain_snapshot_barrier(
            primary(&manager),
            &mut state,
            &mut application_rx,
            &events_tx,
            &snapshot,
            deadline,
        )
        .await;
        assert!(
            matches!(first, SnapshotDrainOutcome::Restart),
            "new authoritative epoch requires the outer snapshot restart: {first:?}"
        );
        let runtime_changed = events_rx.try_recv().expect("runtime cut pre-cut");
        assert_eq!(runtime_changed["type"], "runtime_changed");
        assert_eq!(runtime_changed["epoch"], new_epoch);
        assert_eq!(runtime_changed["sequence"], 1);
        let new_agent = events_rx.try_recv().expect("first genuine epoch event");
        assert_eq!(new_agent["type"], "agent_start");
        assert_eq!(new_agent["sequence"], 2);
        assert!(
            events_rx.try_recv().is_err(),
            "old-epoch agent must emit no payload and new event must emit exactly once"
        );
        assert_eq!(state.epoch, new_epoch);
        assert_eq!(state.observed_epoch, new_epoch);
        assert_eq!(state.forwarded_agent_event_barrier, 1);
        assert!(
            state.pending_future_epoch_events.is_empty(),
            "admitted event must not remain queued"
        );
        let settled = drain_snapshot_barrier(
            primary(&manager),
            &mut state,
            &mut application_rx,
            &events_tx,
            &snapshot,
            deadline,
        )
        .await;
        assert!(
            matches!(settled, SnapshotDrainOutcome::Settled),
            "the matching queued RuntimeChanged cut owns the epoch without blind reset: {settled:?}"
        );
        assert!(
            events_rx.try_recv().is_err(),
            "settled replay must not duplicate the admitted event"
        );
        let duplicate = envelope_application_event(
            primary(&manager),
            &mut state,
            pi_coding::ApplicationEvent::AgentPublished {
                event: pi_agent::AgentEvent::AgentStart,
                epoch: new_epoch,
                barrier: 1,
            },
        );
        assert!(
            duplicate.is_none(),
            "same (epoch, barrier) replay is suppressed"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn synthetic_agent_events_keep_increment_behavior() {
        let (manager, _registration) = manager_with(None).await;
        let mut state = ProtocolState::new(1);
        let first = envelope_application_event(
            primary(&manager),
            &mut state,
            pi_coding::ApplicationEvent::Agent(pi_agent::AgentEvent::AgentStart),
        )
        .expect("synthetic agent projects");
        assert_eq!(state.forwarded_agent_event_barrier, 1);
        assert_eq!(first["type"], "agent_start");
        assert_eq!(first["sequence"], 1);
        let second = envelope_application_event(
            primary(&manager),
            &mut state,
            pi_coding::ApplicationEvent::Agent(pi_agent::AgentEvent::AgentStart),
        )
        .expect("second synthetic agent projects");
        assert_eq!(state.forwarded_agent_event_barrier, 2);
        assert_eq!(second["sequence"], 2);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn active_deficit_fails_closed_by_deadline_and_closed_idle_completes() {
        let (manager, _registration) = manager_with(None).await;
        let mut snapshot = manager.primary.application.protocol_snapshot().await;
        snapshot.event_barrier = 2;
        let (events_tx, _events_rx) = tokio::sync::broadcast::channel(16);
        // Active turn with a coverable-but-uncovered barrier: the attempt must
        // fail closed within the shared deadline, not hang.
        snapshot.lifecycle.active_turn_id = Some("turn-live".to_owned());
        let mut active_state = ProtocolState::new(snapshot.epoch);
        let (_application_tx, mut application_rx) =
            tokio::sync::broadcast::channel::<pi_coding::ApplicationEvent>(4);
        let started = tokio::time::Instant::now();
        let outcome = drain_snapshot_barrier(
            primary(&manager),
            &mut active_state,
            &mut application_rx,
            &events_tx,
            &snapshot,
            started + Duration::from_millis(120),
        )
        .await;
        let error = match outcome {
            SnapshotDrainOutcome::Unavailable(error) => error,
            other => panic!("active deficit must fail closed: {other:?}"),
        };
        assert!(
            format!("{error:#}").contains("attempt deadline"),
            "deadline failure must be bounded and actionable: {error:#}"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(100)
                && started.elapsed() < Duration::from_millis(600),
            "active failure must honor the finite deadline: {:?}",
            started.elapsed()
        );
        // Closed sender + idle runtime: the attempt completes by advancing to
        // the authoritative barrier.
        snapshot.lifecycle.active_turn_id = None;
        let (sender, mut closed_receiver) =
            tokio::sync::broadcast::channel::<pi_coding::ApplicationEvent>(4);
        drop(sender);
        let mut idle_state = ProtocolState::new(snapshot.epoch);
        let outcome = drain_snapshot_barrier(
            primary(&manager),
            &mut idle_state,
            &mut closed_receiver,
            &events_tx,
            &snapshot,
            tokio::time::Instant::now() + Duration::from_millis(120),
        )
        .await;
        assert!(
            matches!(outcome, SnapshotDrainOutcome::Settled),
            "closed idle must complete"
        );
        assert_eq!(idle_state.forwarded_agent_event_barrier, 2);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn newer_turn_start_replaces_stale_protocol_owner_turn() {
        let (manager, _registration) = manager_with(None).await;
        let session_id = manager.primary_session_id();
        let runtime = manager.resolve(Some(&session_id)).await.expect("runtime");
        let first = runtime
            .envelope_test_event(ApplicationEvent::TurnStarted {
                turn_id: "turn-old".to_owned(),
                model: None,
            })
            .await
            .expect("old turn start");
        let replacement = runtime
            .envelope_test_event(ApplicationEvent::TurnStarted {
                turn_id: "turn-new".to_owned(),
                model: None,
            })
            .await
            .expect("new turn start must replace stale owner state");
        assert_eq!(first["sequence"], 1);
        assert_eq!(replacement["sequence"], 2);
        assert_eq!(replacement["turnId"], "turn-new");
        assert!(
            runtime
                .envelope_test_event(ApplicationEvent::TurnTerminal {
                    turn_id: "turn-old".to_owned(),
                    status: pi_coding::TurnTerminalStatus::Succeeded,
                    message: None,
                })
                .await
                .is_none()
        );
        let continued = runtime
            .envelope_test_event(ApplicationEvent::AgentSettled)
            .await
            .expect("new turn remains authoritative");
        assert_eq!(continued["turnId"], "turn-new");
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn stale_terminal_cannot_clear_a_newer_turn() {
        let (manager, _registration) = manager_with(None).await;
        let session_id = manager.primary_session_id();
        let runtime = manager.resolve(Some(&session_id)).await.expect("runtime");
        assert_eq!(
            runtime
                .envelope_test_event(ApplicationEvent::TurnStarted {
                    turn_id: "turn-new".to_owned(),
                    model: None,
                })
                .await
                .expect("turn start envelope")["sequence"],
            1,
        );
        assert!(
            runtime
                .envelope_test_event(ApplicationEvent::TurnTerminal {
                    turn_id: "turn-old".to_owned(),
                    status: pi_coding::TurnTerminalStatus::Succeeded,
                    message: None,
                })
                .await
                .is_none()
        );
        let continued = runtime
            .envelope_test_event(ApplicationEvent::AgentSettled)
            .await
            .expect("continued turn envelope");
        assert_eq!(continued["turnId"], "turn-new");
        let snapshot = manager
            .subscription_snapshot(&session_id, 0)
            .await
            .expect("subscription snapshot");
        assert_eq!(snapshot["watermark"], 2);
        assert!(snapshot["activeTurnId"].is_null());
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn runtime_change_during_snapshot_drain_rebases_the_barrier() {
        let (manager, _registration) = manager_with(None).await;
        let mut state = ProtocolState::new(1);
        state.forwarded_agent_event_barrier = 4;
        let event = envelope_application_event(
            primary(&manager),
            &mut state,
            ApplicationEvent::RuntimeChanged { epoch: 2 },
        )
        .expect("runtime change envelope");
        assert_eq!(event["type"], "runtime_changed");
        assert_eq!(state.epoch, 2);
        assert_eq!(state.forwarded_agent_event_barrier, 0);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn lagged_application_events_emit_sequenced_resync() {
        let (manager, _registration) = manager_with(None).await;
        let snapshot = manager.primary.application.protocol_snapshot().await;
        let mut state = ProtocolState::new(snapshot.epoch);
        state.next_sequence = 9;
        state.active_turn_id = Some("stale-turn".to_owned());
        let event = push_session_resync(primary(&manager), &mut state, &snapshot);
        assert_eq!(event["type"], "session_resync_required");
        assert_eq!(event["sessionId"], manager.primary_session_id());
        assert_eq!(event["sequence"], 10);
        assert_eq!(state.epoch, snapshot.epoch);
        assert_eq!(state.active_turn_id, snapshot.lifecycle.active_turn_id);
        assert_eq!(state.replay.back(), Some(&event));
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn reconnect_after_missed_resync_marker_requires_authoritative_snapshot() {
        let (manager, _registration) = manager_with(None).await;
        let session_id = manager.primary_session_id();
        let runtime = manager.resolve(Some(&session_id)).await.expect("runtime");
        let snapshot = runtime.application.protocol_snapshot().await;
        let mut state = ProtocolState::new(snapshot.epoch);
        state.next_sequence = 10;
        let marker = push_session_resync(&runtime, &mut state, &snapshot);
        assert_eq!(marker["sequence"], 11);
        let cut = protocol_snapshot_cut(&runtime, &mut state, snapshot, 10, false)
            .expect("authoritative reconnect cut");
        assert!(
            cut.cut.has_gap,
            "missed resync marker must force authoritative snapshot"
        );
        assert!(
            cut.cut.events.is_empty(),
            "marker is not replayed as an ordinary delta"
        );
        assert_eq!(cut.cut.watermark, 11);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn reconnect_cursor_ahead_of_watermark_requires_authoritative_snapshot() {
        let (manager, _registration) = manager_with(None).await;
        let session_id = manager.primary_session_id();
        let runtime = manager.resolve(Some(&session_id)).await.expect("runtime");
        let snapshot = runtime.application.protocol_snapshot().await;
        let mut state = ProtocolState::new(snapshot.epoch);
        state.next_sequence = 2;
        let cut = protocol_snapshot_cut(&runtime, &mut state, snapshot, 100, false)
            .expect("stale high cursor must produce a cut");
        assert!(
            cut.cut.has_gap,
            "cursor > watermark must force session_resync"
        );
        assert!(
            cut.cut.events.is_empty(),
            "epoch rollback must not replay old-listener sequences"
        );
        assert_eq!(cut.cut.watermark, 2);
        let payload = json!({
            "type": if cut.cut.has_gap { "session_resync" } else { "subscription_snapshot" },
            "watermark": cut.cut.watermark,
            "events": cut.cut.events,
        });
        assert_eq!(payload["type"], "session_resync");
        assert_eq!(payload["watermark"], 2);
        manager.shutdown().await;
    }
    fn oversized_message_end_event(epoch: u64, barrier: u64) -> pi_coding::ApplicationEvent {
        let mut message = pi_ai::AssistantMessage::pending(&pi_ai::Model::default());
        message
            .content
            .push(pi_ai::ContentBlock::text("x".repeat(EVENT_REPLAY_BYTE_BUDGET + 1)));
        message.stop_reason = pi_ai::StopReason::Stop;
        pi_coding::ApplicationEvent::AgentPublished {
            event: pi_agent::AgentEvent::MessageEnd {
                message: pi_ai::Message::Assistant(message),
            },
            epoch,
            barrier,
        }
    }

    #[tokio::test]
    async fn oversized_event_gap_blocks_warm_replay_before_boundary() {
        let (manager, _registration) = manager_with(None).await;
        let mut state = ProtocolState::new(1);
        let epoch = state.epoch;
        let marker = envelope_application_event(
            primary(&manager),
            &mut state,
            oversized_message_end_event(epoch, 1),
        )
        .expect("oversized event must project as an in-band resync marker");
        assert_eq!(marker["type"], "session_resync_required");
        assert_eq!(marker["reason"], "overflow");
        let boundary = marker["sequence"].as_u64().expect("marker sequence");
        let snapshot = primary(&manager).application.protocol_snapshot().await;
        // A client behind the boundary cannot warm-replay across the omitted
        // event: the in-band marker forces an authoritative snapshot.
        let cut = protocol_snapshot_cut(primary(&manager), &mut state, snapshot, boundary - 1, false)
            .expect("behind-boundary cut");
        assert!(cut.cut.has_gap, "cursor < boundary must force resync");
        assert!(cut.cut.events.is_empty(), "no warm replay across the gap");
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn oversized_event_gap_allows_warm_replay_at_and_after_boundary() {
        let (manager, _registration) = manager_with(None).await;
        let mut state = ProtocolState::new(1);
        let epoch = state.epoch;
        let marker = envelope_application_event(
            primary(&manager),
            &mut state,
            oversized_message_end_event(epoch, 1),
        )
        .expect("oversized event must project as an in-band resync marker");
        let boundary = marker["sequence"].as_u64().expect("marker sequence");
        // Ordinary events after the boundary stay contiguous.
        let after = envelope_application_event(
            primary(&manager),
            &mut state,
            pi_coding::ApplicationEvent::AgentPublished {
                event: pi_agent::AgentEvent::AgentStart,
                epoch,
                barrier: 2,
            },
        )
        .expect("ordinary event after boundary");
        assert_eq!(after["sequence"].as_u64(), Some(boundary + 1));
        let snapshot = primary(&manager).application.protocol_snapshot().await;
        // At the boundary: later retained contiguous events warm-replay.
        let cut = protocol_snapshot_cut(primary(&manager), &mut state, snapshot.clone(), boundary, false)
            .expect("at-boundary cut");
        assert!(!cut.cut.has_gap, "cursor == boundary must warm-replay");
        assert_eq!(cut.cut.events.len(), 1);
        assert_eq!(cut.cut.events[0]["sequence"].as_u64(), Some(boundary + 1));
        // After the boundary: the next ordinary event warm-replays.
        let cut = protocol_snapshot_cut(primary(&manager), &mut state, snapshot, boundary + 1, false)
            .expect("after-boundary cut");
        assert!(!cut.cut.has_gap, "cursor > boundary must warm-replay");
        assert!(cut.cut.events.is_empty());
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn oversized_event_gap_is_not_cleared_by_another_clients_snapshot() {
        let (manager, _registration) = manager_with(None).await;
        let mut state = ProtocolState::new(1);
        let epoch = state.epoch;
        let marker = envelope_application_event(
            primary(&manager),
            &mut state,
            oversized_message_end_event(epoch, 1),
        )
        .expect("oversized event must project as an in-band resync marker");
        let boundary = marker["sequence"].as_u64().expect("marker sequence");
        let snapshot = primary(&manager).application.protocol_snapshot().await;
        // Client A catches up to the boundary and warm-replays.
        let first = protocol_snapshot_cut(primary(&manager), &mut state, snapshot.clone(), boundary, false)
            .expect("caught-up client cut");
        assert!(!first.cut.has_gap);
        // Client B, still behind the boundary, must still be forced to resync:
        // the marker survives other clients' cuts.
        let second = protocol_snapshot_cut(primary(&manager), &mut state, snapshot, boundary - 1, false)
            .expect("behind-boundary client cut");
        assert!(second.cut.has_gap, "gap requirement is per-client, never globally cleared");
        assert!(second.cut.events.is_empty());
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn runtime_epoch_reset_clears_sticky_gap_but_keeps_conservative_boundary() {
        let (manager, _registration) = manager_with(None).await;
        let mut state = ProtocolState::new(1);
        let epoch = state.epoch;
        let marker = envelope_application_event(
            primary(&manager),
            &mut state,
            oversized_message_end_event(epoch, 1),
        )
        .expect("oversized event must project as an in-band resync marker");
        let boundary = marker["sequence"].as_u64().expect("marker sequence");
        // The runtime change is itself an ordinary sequenced event; the new
        // epoch's events follow contiguously.
        let changed = envelope_application_event(
            primary(&manager),
            &mut state,
            pi_coding::ApplicationEvent::RuntimeChanged { epoch: 2 },
        )
        .expect("runtime change projects");
        assert_eq!(changed["sequence"].as_u64(), Some(boundary + 1));
        let new_epoch_event = envelope_application_event(
            primary(&manager),
            &mut state,
            pi_coding::ApplicationEvent::AgentPublished {
                event: pi_agent::AgentEvent::AgentStart,
                epoch: 2,
                barrier: 1,
            },
        )
        .expect("new epoch event projects");
        assert_eq!(new_epoch_event["sequence"].as_u64(), Some(boundary + 2));
        let mut snapshot = primary(&manager).application.protocol_snapshot().await;
        snapshot.epoch = 2;
        // A client that consumed the runtime change warm-replays the new
        // epoch: the epoch reset cleared the sticky gap.
        let cut = protocol_snapshot_cut(primary(&manager), &mut state, snapshot.clone(), boundary + 1, false)
            .expect("post-reset cut");
        assert!(!cut.cut.has_gap, "epoch reset must clear the sticky gap");
        assert_eq!(cut.cut.events.len(), 1);
        assert_eq!(cut.cut.events[0]["sequence"].as_u64(), Some(boundary + 2));
        // A client still behind the old boundary keeps the conservative gap.
        let cut = protocol_snapshot_cut(primary(&manager), &mut state, snapshot, boundary - 1, false)
            .expect("behind-boundary cut after reset");
        assert!(cut.cut.has_gap, "old boundary stays conservative across epochs");
        assert!(cut.cut.events.is_empty());
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn reopened_runtime_starts_without_gap_state() {
        let (manager, _registration) = manager_with(None).await;
        // A reopened runtime builds a fresh protocol state: no retained
        // marker, no gap, and a fresh client cut is clean.
        let mut state = ProtocolState::new(primary(&manager).application.runtime_epoch());
        let snapshot = primary(&manager).application.protocol_snapshot().await;
        let cut = protocol_snapshot_cut(primary(&manager), &mut state, snapshot, 0, false)
            .expect("fresh cut");
        assert!(!cut.cut.has_gap, "reopen must not inherit gap state");
        assert!(cut.cut.events.is_empty());
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn command_ids_replay_outcomes_and_reject_conflicts() {
        let (manager, _registration) = manager_with(None).await;
        let session_id = manager.primary_session_id();
        let first = manager
            .dispatch(
                RpcCommand::GetState {
                    id: Some("state-command".into()),
                },
                Some(session_id.clone()),
            )
            .await;
        let replay = manager
            .dispatch(
                RpcCommand::GetState {
                    id: Some("state-command".into()),
                },
                Some(session_id.clone()),
            )
            .await;
        assert_eq!(
            serde_json::to_value(&first).expect("first"),
            serde_json::to_value(&replay).expect("replay")
        );

        let conflict = manager
            .dispatch(
                RpcCommand::GetMessages {
                    id: Some("state-command".into()),
                },
                Some(session_id),
            )
            .await;
        assert!(!conflict.success);
        assert!(conflict.error.unwrap().contains("different request"));
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn duplicate_in_flight_prompt_joins_one_admission() {
        let (manager, registration) = manager_with_sized(None, 1).await;
        let session_id = manager.primary_session_id();
        registration.set_responses(vec![FauxResponse::text("response ".repeat(20_000))]);
        let command = RpcCommand::Prompt {
            id: Some("prompt-once".into()),
            message: "hello".into(),
            images: Vec::new(),
            streaming_behavior: None,
        };
        let first = tokio::spawn({
            let manager = manager.clone();
            let session_id = session_id.clone();
            let command = command.clone();
            async move { manager.dispatch_spawned(command, Some(session_id)).await }
        });
        let second = tokio::spawn({
            let manager = manager.clone();
            let session_id = session_id.clone();
            async move { manager.dispatch_spawned(command, Some(session_id)).await }
        });
        let first = first.await.expect("first duplicate");
        let second = second.await.expect("second duplicate");
        assert_eq!(
            serde_json::to_value(&first).expect("first"),
            serde_json::to_value(&second).expect("second")
        );
        manager.primary.application.abort().await;
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn parse_input_extracts_top_level_session_id() {
        use crate::modes::rpc::parse_input;
        let Ok(crate::modes::rpc::RpcInput::Command {
            command,
            session_id,
        }) = parse_input(br#"{"type":"get_state","id":"x","sessionId":"sess-1"}"#)
        else {
            panic!("parse must succeed")
        };
        assert_eq!(session_id.as_deref(), Some("sess-1"));
        assert_eq!(command.id().as_deref(), Some("x"));
        let Ok(crate::modes::rpc::RpcInput::Command { session_id, .. }) =
            parse_input(br#"{"type":"workflow_list","id":"w","sessionId":"sess-2"}"#)
        else {
            panic!("workflow parse must succeed")
        };
        assert_eq!(session_id.as_deref(), Some("sess-2"));
    }

    #[test]
    fn parse_failures_preserve_valid_requested_session_id() {
        use crate::modes::rpc::parse_input;

        let ordinary = parse_input(
            br#"{"type":"set_todos","id":"ordinary","sessionId":"sess-1","phases":null}"#,
        )
        .expect_err("invalid ordinary command");
        assert_eq!(ordinary.session_id.as_deref(), Some("sess-1"));
        assert!(
            ordinary
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Invalid command set_todos")
        );

        let workflow = parse_input(
            br#"{"type":"workflow_create","id":"workflow","sessionId":"sess-2","name":"ship"}"#,
        )
        .expect_err("invalid workflow command");
        assert_eq!(workflow.session_id.as_deref(), Some("sess-2"));
        assert!(
            workflow
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Invalid command")
        );
    }

    /// The RPC prompt response is acceptance (the turn runs asynchronously);
    /// the turn's completion is the `message_end` application event tagged
    /// with the owning session. Wait for it before asserting recorded state.
    async fn wait_message_end(events: &mut broadcast::Receiver<Value>, session_id: &str) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let frame = match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Ok(frame)) => frame,
                // A burst of stream deltas can outrun this receiver; skip the
                // lagged records and keep waiting for the completion marker.
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    panic!("fan-in closed before message_end for {session_id}")
                }
                Err(_) => panic!("timed out waiting for message_end for {session_id}"),
            };
            if frame.get("type").and_then(Value::as_str) == Some("message_end")
                && frame.get("sessionId").and_then(Value::as_str) == Some(session_id)
            {
                return;
            }
        }
    }

    fn history_texts(response: &RpcResponse) -> Vec<String> {
        let mut texts = Vec::new();
        if let Some(messages) = response
            .data
            .as_ref()
            .and_then(|data| data.get("messages"))
            .and_then(Value::as_array)
        {
            for message in messages {
                let message = message.get("message").unwrap_or(message);
                if let Some(content) = message.get("content").and_then(Value::as_array) {
                    for block in content {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            texts.push(text.to_owned());
                        }
                    }
                }
            }
        }
        texts
    }

    trait SuccessOr {
        fn success_or(self, context: &str);
    }
    impl SuccessOr for RpcResponse {
        fn success_or(self, context: &str) {
            assert!(
                self.success,
                "{context}: {}",
                self.error.unwrap_or_default()
            );
        }
    }

    #[tokio::test]
    async fn runtime_reopen_changes_incarnation() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let opened = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(opened.success, "{}", opened.error.unwrap_or_default());
        let session_id = opened.data.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        let first = manager
            .current_runtime_identity(&session_id)
            .await
            .expect("first identity");
        assert!(!first.runtime_incarnation.is_empty());
        let closed = manager
            .dispatch(RpcCommand::CloseSession { id: None }, Some(session_id.clone()))
            .await;
        assert!(closed.success, "{}", closed.error.unwrap_or_default());
        assert_eq!(
            closed.data.as_ref().unwrap()["runtimeIncarnation"],
            first.runtime_incarnation
        );
        assert!(
            manager
                .current_runtime_identity(&session_id)
                .await
                .is_err(),
            "closed identity must not resolve"
        );

        let reopened = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(reopened.success, "{}", reopened.error.unwrap_or_default());
        let second_id = reopened.data.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        let second = manager
            .current_runtime_identity(&second_id)
            .await
            .expect("second identity");
        assert_ne!(first.runtime_incarnation, second.runtime_incarnation);
        let listed = manager
            .overlay_loaded_sessions(RpcResponse::success(
                None,
                "session_list",
                Some(json!({"sessions": []})),
            ))
            .await;
        let listed_data = listed.data.expect("catalog data");
        let row = listed_data["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["sessionId"] == second_id)
            .expect("catalog row");
        assert_eq!(row["runtimeIncarnation"], second.runtime_incarnation);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn lru_emits_unload_and_handoff_before_cleanup() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner.clone())).await;
        let mut events = manager.events();
        let mut opened_ids = Vec::new();
        for _ in 0..(MAX_LOADED_SESSIONS - 1) {
            let response = manager
                .dispatch(
                    RpcCommand::NewSession {
                        id: None,
                        parent_session: None,
                        cwd: None,
                    },
                    None,
                )
                .await;
            assert!(response.success, "{}", response.error.unwrap_or_default());
            opened_ids.push(
                response.data.unwrap()["sessionId"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            );
        }
        let victim_id = opened_ids[0].clone();
        let victim_incarnation = manager
            .current_runtime_identity(&victim_id)
            .await
            .expect("victim identity")
            .runtime_incarnation;
        let overflow = manager
            .dispatch(
                RpcCommand::NewSession {
                    id: None,
                    parent_session: None,
                    cwd: None,
                },
                None,
            )
            .await;
        assert!(overflow.success, "{}", overflow.error.unwrap_or_default());
        let replacement_id = overflow.data.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            manager
                .current_runtime_identity(&replacement_id)
                .await
                .is_ok()
        );
        assert!(manager.current_runtime_identity(&victim_id).await.is_err());
        let unload = loop {
            let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .expect("unload timeout")
                .expect("unload event");
            if event["type"] == "session_unloaded" {
                break event;
            }
        };
        assert_eq!(unload["sessionId"], victim_id);
        assert_eq!(unload["runtimeIncarnation"], victim_incarnation);
        assert_eq!(unload["reason"], "lru");
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn handoff_cursor_not_blocked_by_in_flight_snapshot() {
        let spawner = Arc::new(TestSpawner::default());
        let (manager, _primary_registration) = manager_with(Some(spawner)).await;
        let dir = tempfile::tempdir().expect("session dir");
        let (model, _registration) = faux_model("cursor-priority");
        let recorder = pi_coding::start_session_in(
            dir.path(),
            Some(&model),
            Some("off"),
            Some(dir.path()),
            None,
            None,
        )
        .expect("start target session");
        recorder.persist_now().expect("persist target");
        let path = recorder.path().to_string_lossy().into_owned();
        let source_id = manager.primary_session_id();
        let _gate = manager.hold_subscription_snapshot_for_test().await;
        let started = Instant::now();
        let web = manager
            .admit_web(
                RpcCommand::SwitchSession {
                    id: Some("priority-switch".into()),
                    session_path: path,
                    catalog_source: None,
                    catalog_session_id: None,
                    catalog_status: None,
                    response_mode: Some(crate::modes::rpc::SubscribeHandoffV1::SubscribeHandoffV1),
                },
                Some(source_id.clone()),
                false,
            )
            .await
            .wait()
            .await;
        let elapsed = started.elapsed();
        assert!(web.success, "{web:?}");
        assert!(
            elapsed < Duration::from_millis(500),
            "handoff must not wait on snapshot barrier: {elapsed:?}"
        );
        let data = web.data.expect("handoff");
        assert!(data.get("state").is_none());
        assert!(data.get("messages").is_none());
        assert_eq!(manager.protocol_snapshot_count_for_test(), 0);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn transcript_ledger_is_ephemeral() {
        let (manager, _primary_registration) = manager_with(None).await;
        let session_id = manager.primary_session_id();
        let waiter = manager
            .admit_web(
                RpcCommand::GetMessages { id: Some("messages-1".into()) },
                Some(session_id.clone()),
                false,
            )
            .await;
        let response = waiter.wait().await;
        assert!(response.success, "{response:?}");
        let runtime = manager.resolve(Some(&session_id)).await.expect("runtime");
        let (reply, wait) = oneshot::channel();
        runtime
            .protocol_tx
            .send(ProtocolMessage::AdmitDirect {
                command: RpcCommand::GetMessages { id: Some("messages-1".into()) },
                digest: [0; 32],
                manager_level: false,
                use_per_session_slots: false,
                lifecycle_response_mode: LifecycleResponseMode::FullSnapshot,
                reply,
                lifecycle: runtime.lifecycle_lock.clone().try_read_owned().expect("lifecycle"),
            })
            .await
            .expect("admit duplicate");
        let duplicate = wait.await.expect("duplicate waiter").wait().await;
        assert!(
            duplicate
                .error
                .as_deref()
                .is_some_and(|error| error.contains("already used")
                    || error.contains("expired")
                    || error.contains("unknown")),
            "transcript-bearing identified responses must not remain replayable: {duplicate:?}"
        );
        manager.shutdown().await;
    }
}
