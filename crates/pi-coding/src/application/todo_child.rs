use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use std::sync::atomic::AtomicUsize;

use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    sync::{Mutex as AsyncMutex, mpsc, oneshot, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{Application, ApplicationRuntimeFactory};
use crate::{
    Session, SessionOptions,
    fs::{CapabilityAccess, CapabilityRootLease, FileMode, FsError, Replacement, RootCreation},
    session_store::{ParentForkPoint, PreparedSessionResume, create_durable_forked_child, is_archive_sidecar},
};

pub const TODO_CHILD_RECEIPT_CAP: usize = 100;
pub const TODO_CHILD_MESSAGE_MAX_BYTES: usize = 64 * 1024;
pub const TODO_CHILD_MESSAGE_QUEUE_CAP: usize = 64;
/// Bounded deadline a terminal shutdown gives a cancelled in-flight creation
/// to settle its rollback, and the bounded window it grants the same-gate
/// admission acquire. Covers only the settle path — never the build itself,
/// which a never-returning factory can park forever: the shutdown abandons
/// the wait rather than hanging.
pub const TODO_CHILD_CREATE_SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
pub const TODO_CHILD_MANIFEST_VERSION: u32 = 1;
pub const TODO_MANIFEST_FILENAME: &str = "todo-manifest.json";

const TODO_CHILD_MANIFEST_MAX_BYTES: u64 = 4 * 1024 * 1024;
const TODO_CHILD_TASK_RECORD: &str = "pi.todo_child.task";
const TODO_CHILD_RECEIPT_RECORD: &str = "pi.todo_child.receipt";
const TODO_CHILD_DERIVATION_RECORD: &str = "pi.todo_child.derivation";
const PRUNE_STAGING_DIRNAME: &str = ".prune-staging";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoChildIdentity {
    pub parent_session_id: String,
    pub child_session_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoChildKind {
    Todo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoChildStatus {
    Created,
    Idle,
    Running,
    Paused,
    Stopped,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoChildReceiptStatus {
    Queued,
    Running,
    Completed,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoChildTaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TodoChildMessage {
    id: String,
    parent_session_id: String,
    child_session_id: String,
    body: String,
    accepted_at: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    images: Vec<pi_ai::ContentBlock>,
    status: TodoChildReceiptStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoChildReceipt {
    pub id: String,
    pub parent_session_id: String,
    pub child_session_id: String,
    pub message_id: String,
    pub accepted_at: i64,
    pub status: TodoChildReceiptStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoChildTask {
    pub id: String,
    pub content: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub status: TodoChildTaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoChildAddTaskRequest {
    pub content: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoChildAddTaskResult {
    pub task: TodoChildTask,
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoChildSnapshot {
    pub parent_session_id: String,
    pub child_session_id: String,
    pub status: TodoChildStatus,
    pub is_running: bool,
    pub transcript_entries: usize,
    pub receipts: Vec<TodoChildReceipt>,
    pub revision: u64,
    pub tasks: Vec<TodoChildTask>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoChildParentProvenance {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoChildWorkspaceIdentity {
    pub workspace: String,
    pub cwd: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoChildDetailAgent {
    pub id: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub status: crate::AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoChildDetailJob {
    pub job_id: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub status: crate::JobStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoChildDetailActivityKind {
    Task,
    Irc,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoChildDetailActivity {
    pub at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    pub kind: TodoChildDetailActivityKind,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoChildDetailIrcMessage {
    pub message_id: String,
    pub from: String,
    pub to: String,
    pub text: String,
    pub at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoChildDetailTranscriptEntry {
    pub id: String,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_ms: Option<i64>,
    #[serde(default)]
    pub interrupted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoChildDetailSnapshot {
    pub parent_session_id: String,
    pub child_session_id: String,
    pub todo_id: String,
    pub parent: TodoChildParentProvenance,
    pub workspace: TodoChildWorkspaceIdentity,
    pub status: TodoChildStatus,
    pub is_running: bool,
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub transcript_entries: usize,
    pub tasks: Vec<TodoChildTask>,
    pub receipts: Vec<TodoChildReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor: Option<TodoChildDetailAgent>,
    pub agents: Vec<TodoChildDetailAgent>,
    pub jobs: Vec<TodoChildDetailJob>,
    pub activity: Vec<TodoChildDetailActivity>,
    pub irc: Vec<TodoChildDetailIrcMessage>,
    pub transcript: Vec<TodoChildDetailTranscriptEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoChildManifestEntry {
    pub child_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub created_at: i64,
    pub status: TodoChildStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_entry_id: Option<String>,
    pub parent_revision: u64,
    pub kind: TodoChildKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoChildManifest {
    pub version: u32,
    pub parent_session_id: String,
    pub entries: Vec<TodoChildManifestEntry>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoChildCreateRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoChildCreateResult {
    pub identity: TodoChildIdentity,
    pub snapshot: TodoChildSnapshot,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedReceiptEvent {
    identity: TodoChildIdentity,
    receipt: TodoChildReceipt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message: Option<TodoChildMessage>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum TodoChildTaskEvent {
    Added {
        revision: u64,
        task: TodoChildTask,
    },
    Started {
        revision: u64,
        task_id: String,
    },
    Completed {
        revision: u64,
        task_id: String,
        summary: Option<String>,
    },
    Failed {
        revision: u64,
        task_id: String,
        summary: String,
    },
    Interrupted {
        revision: u64,
        task_id: String,
    },
}

enum TodoTaskCommand {
    Add {
        request: TodoChildAddTaskRequest,
        reply: oneshot::Sender<Result<TodoChildAddTaskResult>>,
    },
    Finished {
        task_id: String,
        outcome: Result<Option<String>, String>,
    },
    Wake,
    Stop {
        reply: oneshot::Sender<Result<()>>,
    },
}

#[derive(Clone, Default)]
pub struct TodoChildClient {
    service: Weak<TodoChildService>,
}

impl TodoChildClient {
    pub(super) fn service(&self) -> Result<Arc<TodoChildService>> {
        self.service
            .upgrade()
            .ok_or_else(|| anyhow!("Todo child service is unavailable"))
    }

    fn points_to(&self, service: &Arc<TodoChildService>) -> bool {
        self.service.ptr_eq(&Arc::downgrade(service))
    }
}

fn child_key(parent_session_id: &str, child_session_id: &str) -> TodoChildKey {
    TodoChildKey {
        parent_session_id: parent_session_id.to_owned(),
        child_session_id: child_session_id.to_owned(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct TodoChildKey {
    parent_session_id: String,
    child_session_id: String,
}

/// Stable safe category for an aborted Todo child creation. The user-facing
/// message names no paths and no factory internals.
const TODO_CHILD_CREATE_CANCELLED: &str = "Todo child creation was cancelled";

/// Deterministic interleaving gate for Todo child creation tests, mirroring
/// [`super::ApplicationCutoverTestGate`]: the creator parks inside `enter`
/// while the test observes `wait_entered`, then releases the exact window
/// under test (post-build/pre-publication, or post-publication/pre-spawn).
#[doc(hidden)]
#[derive(Clone, Default)]
pub struct TodoChildCreationTestGate {
    entered: CancellationToken,
    release: CancellationToken,
}

impl TodoChildCreationTestGate {
    #[must_use]
    pub async fn wait_entered(&self) {
        self.entered.cancelled().await;
    }

    pub fn release(&self) {
        self.release.cancel();
    }

    async fn enter(&self) {
        self.entered.cancel();
        self.release.cancelled().await;
    }
}

/// One authoritative in-progress creation per [`TodoChildKey`].
///
/// `cancel` aborts the candidate build future; the creator owns every
/// provisional artifact and completes rollback before removing this ticket
/// and flipping `done`, and on success it publishes and spawns the supervisor
/// before the same atomic settle step. A racing cancel therefore either waits
/// for and cancels the published supervisor, or observes a fully rolled-back
/// failure — never a zombie runtime, transcript, manifest entry, or receipt,
/// and never a false "nothing to cancel" while the creator can still publish.
/// Exactly one state exists per key for the whole create, and the settle
/// signal fires exactly once, after the outcome is complete.
struct TodoChildCreateState {
    cancel: CancellationToken,
    done: watch::Sender<bool>,
}

impl TodoChildCreateState {
    fn new() -> Self {
        let (done, _) = watch::channel(false);
        Self {
            cancel: CancellationToken::new(),
            done,
        }
    }

    /// Resolves once the creator removed this state: the child is published
    /// and spawned, or every provisional artifact was rolled back.
    async fn settled(&self) {
        let mut done = self.done.subscribe();
        while !*done.borrow_and_update() {
            if done.changed().await.is_err() {
                // Sender dropped after settle; the outcome is published.
                return;
            }
        }
    }
}

struct TodoChildProvisional {
    storage: Arc<ManifestStorage>,
    child_session_id: String,
    child_relative: PathBuf,
}

impl TodoChildProvisional {
    async fn discard(self) -> Result<()> {
        self.storage
            .remove_entry(&self.child_session_id)
            .await
            .context("removing the provisional Todo child manifest entry")?;
        match self.storage.child_root.remove_file(&self.child_relative) {
            Ok(()) => Ok(()),
            Err(error) if is_missing(&error) => Ok(()),
            Err(error) => Err(error).context("removing the provisional Todo child transcript"),
        }
    }
}

struct InitSlotLease {
    service: Arc<TodoChildService>,
    key: TodoChildKey,
    slot: Arc<AsyncMutex<()>>,
}

impl Drop for InitSlotLease {
    fn drop(&mut self) {
        self.service.release_init_slot(&self.key, &self.slot);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TodoChildDerivation {
    identity: TodoChildIdentity,
    kind: TodoChildKind,
    parent_entry_id: Option<String>,
    parent_revision: u64,
    runtime: TodoChildRuntimeDerivation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TodoChildRuntimeDerivation {
    model_provider: String,
    model_id: String,
    retry_settings: crate::RetrySettings,
    retry_role: Option<String>,
    thinking_level: pi_agent::ThinkingLevel,
}

struct TodoSupervisor {
    identity: TodoChildIdentity,
    application: Application,
    state: Mutex<TodoSupervisorState>,
    task_tx: mpsc::Sender<TodoTaskCommand>,
    task_rx: Mutex<Option<mpsc::Receiver<TodoTaskCommand>>>,
    task_actor: Mutex<Option<JoinHandle<()>>>,
    turn_gate: Arc<AsyncMutex<()>>,
    lifecycle_gate: Arc<AsyncMutex<()>>,
    manifest_entry: Mutex<TodoChildManifestEntry>,
    manifest: Arc<ManifestStorage>,
}

struct TodoSupervisorState {
    status: TodoChildStatus,
    message_queue: VecDeque<TodoChildMessage>,
    receipts: VecDeque<TodoChildReceipt>,
    recovered_receipt_updates: Vec<TodoChildReceipt>,
    revision: u64,
    tasks: Vec<TodoChildTask>,
    recovered_task_interruptions: Vec<String>,
    active_task_turn: Option<JoinHandle<Result<(), String>>>,
    active_turn: Option<JoinHandle<()>>,
    is_turn_loop_running: bool,
    last_error: Option<String>,
}

struct ManifestStorage {
    parent_session_id: String,
    child_root: CapabilityRootLease,
    gate: Arc<AsyncMutex<()>>,
}
pub struct TodoChildService {
    children: Mutex<HashMap<TodoChildKey, Arc<TodoSupervisor>>>,
    init_slots: Mutex<HashMap<TodoChildKey, Arc<AsyncMutex<()>>>>,
    admission_gate: AsyncMutex<()>,
    selected: Mutex<HashMap<String, String>>,
    creation_before_publication_gate: Mutex<Option<TodoChildCreationTestGate>>,
    creation_before_spawn_gate: Mutex<Option<TodoChildCreationTestGate>>,
    #[doc(hidden)]
    creation_before_root_gate: Mutex<Option<TodoChildCreationTestGate>>,
    creating: Mutex<HashMap<TodoChildKey, Arc<TodoChildCreateState>>>,
    factory: Mutex<Option<Arc<dyn ApplicationRuntimeFactory>>>,
    manifest_slots: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    is_shutdown: AtomicBool,
    /// Test seam: armed provisional-discard faults, consumed one per discard.
    #[cfg(any(test, feature = "test-support"))]
    provisional_discard_faults: AtomicUsize,
    /// Prune-phase fault seams: each consumed exactly once at its phase
    /// (manifest remove, payload finalize).
    prune_manifest_remove_faults: AtomicUsize,
    prune_payload_finalize_faults: AtomicUsize,
}

impl Default for TodoChildService {
    fn default() -> Self {
        Self {
            children: Mutex::new(HashMap::new()),
            init_slots: Mutex::new(HashMap::new()),
            selected: Mutex::new(HashMap::new()),
            admission_gate: AsyncMutex::new(()),
            creation_before_spawn_gate: Mutex::new(None),
            creation_before_root_gate: Mutex::new(None),
            creation_before_publication_gate: Mutex::new(None),
            creating: Mutex::new(HashMap::new()),
            factory: Mutex::new(None),
            manifest_slots: Mutex::new(HashMap::new()),
            is_shutdown: AtomicBool::new(false),
            #[cfg(any(test, feature = "test-support"))]
            provisional_discard_faults: AtomicUsize::new(0),
            prune_manifest_remove_faults: AtomicUsize::new(0),
            prune_payload_finalize_faults: AtomicUsize::new(0),
        }
    }
}


impl TodoChildService {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    #[must_use]
    pub fn client(self: &Arc<Self>) -> TodoChildClient {
        TodoChildClient {
            service: Arc::downgrade(self),
        }
    }

    /// Parks the next creation after ticket registration but before the
    /// durable child root is created, for deterministic pre-root remove races.
    #[doc(hidden)]
    pub fn arm_creation_before_root_gate_for_test(&self) -> TodoChildCreationTestGate {
        let gate = TodoChildCreationTestGate::default();
        *self.creation_before_root_gate.lock() = Some(gate.clone());
        gate
    }
    pub fn configure_factory(&self, factory: Arc<dyn ApplicationRuntimeFactory>) -> Result<()> {
        let mut configured = self.factory.lock();
        if let Some(current) = configured.as_ref() {
            if Arc::ptr_eq(current, &factory) {
                return Ok(());
            }
            bail!("Todo child service is already configured with a different runtime factory");
        }
        *configured = Some(factory);
        Ok(())
    }

    /// Parks the next creation between the completed runtime build and its
    /// publication into the children map, for deterministic cancel-race tests.
    #[doc(hidden)]
    pub fn arm_creation_before_publication_gate_for_test(&self) -> TodoChildCreationTestGate {
        let gate = TodoChildCreationTestGate::default();
        *self.creation_before_publication_gate.lock() = Some(gate.clone());
        gate
    }

    /// Parks the next creation after publication into the children map but
    /// before its execution loops spawn, for deterministic cancel-race tests.
    #[doc(hidden)]
    pub fn arm_creation_before_spawn_gate_for_test(&self) -> TodoChildCreationTestGate {
        let gate = TodoChildCreationTestGate::default();
        *self.creation_before_spawn_gate.lock() = Some(gate.clone());
        gate
    }

    /// Test seam: arms one provisional-discard fault. The next rollback of a
    /// failed create fails before removing any artifacts, as if the manifest
    /// entry or transcript removal faulted.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-support"))]
    pub fn inject_provisional_discard_fault_for_test(&self) {
        self.provisional_discard_faults.fetch_add(1, Ordering::SeqCst);
    }

    /// Test seam: number of creation tickets currently registered.
    #[doc(hidden)]
    #[cfg(any(test, feature = "test-support"))]
    pub fn creation_ticket_count_for_test(&self) -> usize {
        self.creating.lock().len()
    }

    #[doc(hidden)]
    #[cfg(any(test, feature = "test-support"))]
    pub fn init_slot_count_for_test(&self) -> usize {
        self.init_slots.lock().len()
    }

    #[doc(hidden)]
    pub fn inject_prune_manifest_remove_fault_for_test(&self) {
        self.prune_manifest_remove_faults.fetch_add(1, Ordering::SeqCst);
    }

    #[doc(hidden)]
    pub fn inject_prune_payload_finalize_fault_for_test(&self) {
        self.prune_payload_finalize_faults.fetch_add(1, Ordering::SeqCst);
    }


    fn take_prune_fault(&self, faults: &AtomicUsize) -> bool {
        faults
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                (count > 0).then(|| count - 1)
            })
            .is_ok()
    }

    #[cfg(any(test, feature = "test-support"))]
    fn take_provisional_discard_fault(&self) -> bool {
        self.provisional_discard_faults
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                (count > 0).then(|| count - 1)
            })
            .is_ok()
    }

    /// Rolls back a failed create's provisional artifacts. Test builds may
    /// fault the discard via [`Self::inject_provisional_discard_fault_for_test`];
    /// production builds compile the plain forwarding variant below.
    #[cfg(any(test, feature = "test-support"))]
    async fn discard_provisional(&self, provisional: TodoChildProvisional) -> Result<()> {
        if self.take_provisional_discard_fault() {
            bail!("injected provisional discard fault");
        }
        provisional.discard().await
    }

    #[cfg(not(any(test, feature = "test-support")))]
    async fn discard_provisional(&self, provisional: TodoChildProvisional) -> Result<()> {
        provisional.discard().await
    }

    fn ensure_available(&self) -> Result<()> {
        if self.is_shutdown.load(Ordering::Acquire) {
            bail!("Todo child service is shut down");
        }
        Ok(())
    }
}

impl TodoChildService {
    pub async fn create_child(
        self: &Arc<Self>,
        parent: &Session,
        request: TodoChildCreateRequest,
    ) -> Result<TodoChildCreateResult> {
        self.ensure_available()?;
        let parent_session_id = parent_identity(parent, None)?;
        let child_session_id = child_id_for_request(&parent_session_id, &request)?;
        let child = self
            .ensure_child(parent, &parent_session_id, &child_session_id, request.label)
            .await?;
        self.selected
            .lock()
            .insert(parent_session_id, child_session_id);
        Ok(TodoChildCreateResult {
            identity: child.identity.clone(),
            snapshot: child.snapshot()?,
        })
    }

    pub async fn send(
        self: &Arc<Self>,
        parent: &Session,
        requested_parent_session_id: Option<&str>,
        requested_child_session_id: Option<&str>,
        message: String,
    ) -> Result<TodoChildReceipt> {
        self.send_with_images(
            parent,
            requested_parent_session_id,
            requested_child_session_id,
            message,
            Vec::new(),
        )
        .await
    }

    pub(crate) async fn send_with_images(
        self: &Arc<Self>,
        parent: &Session,
        requested_parent_session_id: Option<&str>,
        requested_child_session_id: Option<&str>,
        message: String,
        images: Vec<pi_ai::ContentBlock>,
    ) -> Result<TodoChildReceipt> {
        self.ensure_available()?;
        let parent_session_id = parent_identity(parent, requested_parent_session_id)?;
        let child = match requested_child_session_id {
            Some(child_session_id) => {
                validate_child_session_id(child_session_id)?;
                self.ensure_child(parent, &parent_session_id, child_session_id, None)
                    .await?
            }
            None => {
                self.resolve_existing_child(parent, Some(&parent_session_id), None)
                    .await?
            }
        };
        self.selected
            .lock()
            .insert(parent_session_id, child.identity.child_session_id.clone());
        child.send(message, images).await
    }
    pub(crate) async fn route_code_task(
        self: &Arc<Self>,
        parent: &Session,
        message: String,
        images: Vec<pi_ai::ContentBlock>,
    ) -> Result<TodoChildReceipt> {
        self.ensure_available()?;
        let parent_session_id = parent_identity(parent, None)?;
        let child_session_id = self
            .selected_for_parent(&parent_session_id)
            .or_else(|| self.single_active_child(&parent_session_id))
            .unwrap_or(child_id_for_request(
                &parent_session_id,
                &TodoChildCreateRequest {
                    label: None,
                    idempotency_key: Some("natural-language-code-task".to_owned()),
                },
            )?);
        let child = self
            .ensure_child(
                parent,
                &parent_session_id,
                &child_session_id,
                Some("Auto-routed Todo".to_owned()),
            )
            .await?;
        self.selected
            .lock()
            .insert(parent_session_id, child_session_id);
        child.send(message, images).await
    }

    pub async fn add_task(
        self: &Arc<Self>,
        parent: &Session,
        requested_parent_session_id: Option<&str>,
        child_session_id: &str,
        request: TodoChildAddTaskRequest,
    ) -> Result<TodoChildAddTaskResult> {
        self.ensure_available()?;
        let parent_session_id = parent_identity(parent, requested_parent_session_id)?;
        validate_child_session_id(child_session_id)?;
        let child = self
            .ensure_existing_child(parent, &parent_session_id, child_session_id)
            .await?;
        child.add_task(request).await
    }

    pub async fn detail(
        self: &Arc<Self>,
        parent: &Session,
        requested_parent_session_id: Option<&str>,
        child_session_id: &str,
    ) -> Result<TodoChildDetailSnapshot> {
        self.ensure_available()?;
        let parent_session_id = parent_identity(parent, requested_parent_session_id)?;
        validate_child_session_id(child_session_id)?;
        let child = self
            .ensure_existing_child(parent, &parent_session_id, child_session_id)
            .await?;
        child.validate(&parent_session_id, child_session_id)?;
        child.detail_snapshot()
    }

    pub async fn hub_send(
        self: &Arc<Self>,
        parent: &Session,
        requested_parent_session_id: Option<&str>,
        child_session_id: &str,
        recipient: &str,
        body: &str,
        reply_to: Option<String>,
    ) -> Result<Vec<crate::DeliveryReceipt>> {
        self.ensure_available()?;
        let parent_session_id = parent_identity(parent, requested_parent_session_id)?;
        validate_child_session_id(child_session_id)?;
        let child = self
            .ensure_existing_child(parent, &parent_session_id, child_session_id)
            .await?;
        child.validate(&parent_session_id, child_session_id)?;
        let _lifecycle = child.lifecycle_gate.lock().await;
        match child.state.lock().status {
            TodoChildStatus::Paused => bail!("todo child is paused"),
            TodoChildStatus::Failed => bail!("todo child has failed; resume it before sending IRC"),
            TodoChildStatus::Stopped => bail!("todo child is stopped"),
            TodoChildStatus::Created | TodoChildStatus::Idle | TodoChildStatus::Running => {}
        }
        let recipient = recipient.trim();
        let body = body.trim();
        if recipient.is_empty() || body.is_empty() {
            bail!("Todo child hub recipient and body must not be blank");
        }
        if recipient.eq_ignore_ascii_case("all") {
            bail!("Todo child hub broadcast is not supported");
        }
        let orchestration = child
            .application
            .orchestration_runtime()
            .ok_or_else(|| anyhow!("Todo child orchestration runtime is unavailable"))?;
        Ok(orchestration.send(orchestration.main_agent_id(), recipient, body, reply_to))
    }

    pub async fn status(
        self: &Arc<Self>,
        parent: &Session,
        requested_parent_session_id: Option<&str>,
        requested_child_session_id: Option<&str>,
    ) -> Result<TodoChildSnapshot> {
        self.ensure_available()?;
        let child = self
            .resolve_existing_child(
                parent,
                requested_parent_session_id,
                requested_child_session_id,
            )
            .await?;
        child.snapshot()
    }

    pub async fn stop(
        self: &Arc<Self>,
        parent: &Session,
        requested_parent_session_id: Option<&str>,
        requested_child_session_id: Option<&str>,
    ) -> Result<TodoChildSnapshot> {
        self.ensure_available()?;
        let child = self
            .resolve_existing_child(
                parent,
                requested_parent_session_id,
                requested_child_session_id,
            )
            .await?;
        child.stop_and_cleanup().await?;
        child.snapshot()
    }

    pub async fn pause(
        self: &Arc<Self>,
        parent: &Session,
        requested_parent_session_id: Option<&str>,
        requested_child_session_id: Option<&str>,
    ) -> Result<TodoChildSnapshot> {
        self.ensure_available()?;
        let child = self
            .resolve_existing_child(
                parent,
                requested_parent_session_id,
                requested_child_session_id,
            )
            .await?;
        child.pause().await?;
        child.snapshot()
    }

    pub async fn resume(
        self: &Arc<Self>,
        parent: &Session,
        requested_parent_session_id: Option<&str>,
        requested_child_session_id: Option<&str>,
    ) -> Result<TodoChildSnapshot> {
        self.ensure_available()?;
        let child = self
            .resolve_existing_child(
                parent,
                requested_parent_session_id,
                requested_child_session_id,
            )
            .await?;
        child.resume().await?;
        child.spawn_task_if_needed();
        child.snapshot()
    }

    /// Cancels in-flight work for the parent.
    ///
    /// Resolution order: an explicit child id first targets that key's
    /// in-progress creation (if any), then its published supervisor; without
    /// an explicit id the published selection/single active child wins, then
    /// the parent's single in-progress creation. When nothing is addressable —
    /// including after a create already failed and rolled back — cancel is an
    /// idempotent success (`Ok(None)`), never a stale-selection error.
    pub async fn cancel(
        self: &Arc<Self>,
        parent: &Session,
        requested_parent_session_id: Option<&str>,
        requested_child_session_id: Option<&str>,
    ) -> Result<Option<TodoChildSnapshot>> {
        self.ensure_available()?;
        let parent_session_id = parent_identity(parent, requested_parent_session_id)?;
        if let Some(child_session_id) = requested_child_session_id {
            validate_child_session_id(child_session_id)?;
            let key = child_key(&parent_session_id, child_session_id);
            let pending = { self.creating.lock().get(&key).cloned() };
            if let Some(pending) = pending {
                return self.settle_cancelled_creation(&key, &pending).await;
            }
            return Ok(self.cancel_published(parent, &parent_session_id, child_session_id).await?);
        }
        // An in-progress creation registers its ticket for the whole build,
        // so it is discoverable here without waiting on the admission gate
        // (the gate covers only the creator's closed-check + insert section,
        // never the build).
        let child_session_id = match self
            .selected_for_parent(&parent_session_id)
            .or_else(|| self.single_active_child(&parent_session_id))
        {
            Some(child_session_id) => child_session_id,
            None => {
                let pending = {
                    self.creating
                        .lock()
                        .iter()
                        .find(|(key, _)| key.parent_session_id == parent_session_id)
                        .map(|(key, state)| (key.child_session_id.clone(), state.clone()))
                };
                return match pending {
                    Some((child_session_id, pending)) => {
                        let key = child_key(&parent_session_id, &child_session_id);
                        self.settle_cancelled_creation(&key, &pending).await
                    }
                    None => Ok(None),
                };
            }
        };
        validate_child_session_id(&child_session_id)?;
        Ok(self
            .cancel_published(parent, &parent_session_id, &child_session_id)
            .await?)
    }
    /// Cancels an in-progress creation: signal the build owner, wait for it to
    /// settle every provisional artifact, then cancel the child if the build
    /// raced past the cancellation and published anyway. Absent children are
    /// an idempotent `None`.
    async fn settle_cancelled_creation(
        &self,
        key: &TodoChildKey,
        pending: &Arc<TodoChildCreateState>,
    ) -> Result<Option<TodoChildSnapshot>> {
        pending.cancel.cancel();
        pending.settled().await;
        let Some(child) = ({ self.children.lock().get(key).cloned() }) else {
            return Ok(None);
        };
        child.cancel().await?;
        Ok(Some(child.snapshot()?))
    }

    /// Cancels a published or manifest-registered child.
    ///
    /// Only an authoritative not-found — a manifest that loaded cleanly
    /// without the requested child — maps to the idempotent `Ok(None)`.
    /// Lease, I/O, factory, derivation, and supervisor cancel/shutdown
    /// failures propagate to the caller instead of being swallowed.
    async fn cancel_published(
        self: &Arc<Self>,
        parent: &Session,
        parent_session_id: &str,
        child_session_id: &str,
    ) -> Result<Option<TodoChildSnapshot>> {
        let key = child_key(parent_session_id, child_session_id);
        let child = match { self.children.lock().get(&key).cloned() } {
            Some(child) => child,
            None => {
                let storage = self.manifest_storage(parent, parent_session_id)?;
                let listed = storage
                    .load()?
                    .0
                    .entries
                    .iter()
                    .any(|entry| entry.child_session_id == child_session_id);
                if !listed {
                    return Ok(None);
                }
                self.ensure_child(parent, parent_session_id, child_session_id, None)
                    .await?
            }
        };
        child.cancel().await?;
        Ok(Some(child.snapshot()?))
    }
    pub async fn list(
        self: &Arc<Self>,
        parent: &Session,
        requested_parent_session_id: Option<&str>,
    ) -> Result<Vec<TodoChildManifestEntry>> {
        self.ensure_available()?;
        let parent_session_id = parent_identity(parent, requested_parent_session_id)?;
        let storage = self.manifest_storage(parent, &parent_session_id)?;
        Ok(storage.load()?.0.entries)
    }

    pub async fn select(
        self: &Arc<Self>,
        parent: &Session,
        requested_parent_session_id: Option<&str>,
        child_session_id: &str,
    ) -> Result<()> {
        self.ensure_available()?;
        let parent_session_id = parent_identity(parent, requested_parent_session_id)?;
        validate_child_session_id(child_session_id)?;
        let key = child_key(&parent_session_id, child_session_id);
        if !self.children.lock().contains_key(&key) {
            let storage = self.manifest_storage(parent, &parent_session_id)?;
            if !storage
                .load()?
                .0
                .entries
                .iter()
                .any(|entry| entry.child_session_id == child_session_id)
            {
                bail!(
                    "todo child {child_session_id:?} does not exist for the requested parent session"
                );
            }
        }
        self.selected
            .lock()
            .insert(parent_session_id, child_session_id.to_owned());
        Ok(())
    }
    async fn teardown_and_prune_child(
        self: &Arc<Self>,
        parent: &Session,
        parent_session_id: &str,
        child_session_id: &str,
    ) -> Result<()> {
        let key = child_key(parent_session_id, child_session_id);
        let fork = parent
            .recorder()
            .ok_or_else(|| anyhow!("parent session recording is unavailable"))?
            .capture_fork_point()?;
        if fork.parent_session_id != parent_session_id {
            bail!("todo child parent session identity mismatch");
        }
        let _slot_lease = InitSlotLease {
            service: self.clone(),
            key: key.clone(),
            slot: {
                let mut slots = self.init_slots.lock();
                slots
                    .entry(key.clone())
                    .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                    .clone()
            },
        };
        let _prune = _slot_lease.slot.lock().await;
        let child_root_relative = Path::new("children").join(parent_session_id);
        if !fork.lease.entry_exists(&child_root_relative)? {
            return Ok(());
        }
        let storage = self.manifest_storage_from_fork(&fork)?;
        let session_relative = PathBuf::from(format!("{child_session_id}.jsonl"));
        let orchestration_relative = Path::new("children").join(child_session_id);
        let staging_relative = Path::new(PRUNE_STAGING_DIRNAME);
        let staged_orchestration = staging_relative.join(child_session_id);
        let (manifest, manifest_exists) = match storage.load() {
            Ok((manifest, _)) => (manifest, true),
            Err(error) => {
                let missing_manifest = storage
                    .child_root
                    .metadata(Path::new(TODO_MANIFEST_FILENAME))
                    .is_err_and(|error| is_missing(&error));
                if !missing_manifest {
                    return Err(error);
                }
                (TodoChildManifest {
                    version: TODO_CHILD_MANIFEST_VERSION,
                    parent_session_id: parent_session_id.to_owned(),
                    entries: Vec::new(),
                }, false)
            }
        };
        let in_memory = self.children.lock().contains_key(&key);
        let manifest_listed = manifest
            .entries
            .iter()
            .any(|entry| entry.child_session_id == child_session_id);
        let pre_staged_files = child_payload_files(
            &storage.child_root,
            &session_relative,
            staging_relative,
        )?;
        let mut payload_present = storage.child_root.entry_exists(&orchestration_relative)?
            || storage.child_root.entry_exists(&staged_orchestration)?;
        for (original, staged) in &pre_staged_files {
            payload_present = payload_present
                || storage.child_root.entry_exists(original)?
                || storage.child_root.entry_exists(staged)?;
        }
        if !in_memory && !manifest_listed && !payload_present {
            return Ok(());
        }
        let child = { self.children.lock().get(&key).cloned() };
        if let Some(child) = child {
            child.stop_and_cleanup().await?;
        }
        let staged_files = child_payload_files(
            &storage.child_root,
            &session_relative,
            staging_relative,
        )?;
        let staging_lease = storage
            .child_root
            .lease_directory(
                staging_relative,
                CapabilityAccess::PrivateReadWrite,
                RootCreation::CreateDirectory,
            )
            .context("creating Todo child prune staging directory")?;
        let mut stage_error = None;
        for (original, staged) in &staged_files {
            match storage.child_root.rename(original, staged) {
                Ok(()) => {}
                Err(error) if is_missing(&error) => {}
                Err(error) => {
                    stage_error = Some(anyhow::Error::new(error).context(format!(
                        "staging the Todo child payload at {}",
                        original.display()
                    )));
                    break;
                }
            }
        }
        if stage_error.is_none() {
            match storage
                .child_root
                .rename(&orchestration_relative, &staged_orchestration)
            {
                Ok(()) => {}
                Err(error) if is_missing(&error) => {}
                Err(error) => {
                    stage_error = Some(
                        anyhow::Error::new(error).context("staging Todo child orchestration state"),
                    )
                }
            }
        }
        // Disk truth: a prior failed attempt leaves staged tombstones to finalize.
        let mut staged_present = Vec::new();
        for (original, staged) in staged_files {
            if storage.child_root.entry_exists(&staged)? {
                staged_present.push((original, staged));
            }
        }
        let orchestration_staged = storage.child_root.entry_exists(&staged_orchestration)?;
        if let Some(error) = stage_error {
            if let Err(restore) = restore_staged_child_payload(
                &storage.child_root,
                &staged_present,
                &staged_orchestration,
                &orchestration_relative,
            ) {
                return Err(error.context(format!(
                    "restoring the staged Todo child payload also failed: {restore:#}; staged payload remains recoverable"
                )));
            }
            return Err(error);
        }
        let manifest_entry = manifest
            .entries
            .iter()
            .find(|entry| entry.child_session_id == child_session_id)
            .cloned();
        if manifest_exists {
            let remove_error = if self.take_prune_fault(&self.prune_manifest_remove_faults) {
                Some(anyhow!("injected Todo child manifest remove fault"))
            } else {
                storage.remove_entry(child_session_id).await.err()
            };
            if let Some(error) = remove_error {
                if let Err(restore) = restore_staged_child_payload(
                    &storage.child_root,
                    &staged_present,
                    &staged_orchestration,
                    &orchestration_relative,
                )
                {
                    return Err(error.context(format!(
                        "restoring the staged Todo child payload also failed: {restore:#}; staged payload remains recoverable"
                    )));
                }
                return Err(error);
            }
        }
        let mut finalize_error = None;
        for (_, staged) in &staged_present {
            if finalize_error.is_some() {
                break;
            }
            if self.take_prune_fault(&self.prune_payload_finalize_faults) {
                finalize_error = Some(anyhow!("injected Todo child payload finalize fault"));
                break;
            }
            if let Err(error) = storage.child_root.remove_file(staged) {
                if !is_missing(&error) {
                    finalize_error = Some(
                        anyhow::Error::new(error)
                            .context(format!("removing the staged Todo child payload at {}", staged.display())),
                    );
                }
            }
        }
        if finalize_error.is_none() && orchestration_staged {
            if self.take_prune_fault(&self.prune_payload_finalize_faults) {
                finalize_error = Some(anyhow!("injected Todo child payload finalize fault"));
            } else if let Err(error) = storage.child_root.remove_tree(&staged_orchestration) {
                if !is_missing(&error) {
                    finalize_error = Some(
                        anyhow::Error::new(error)
                            .context("removing staged Todo child orchestration state"),
                    );
                }
            }
        }
        self.children.lock().remove(&key);
        if self
            .selected
            .lock()
            .get(parent_session_id)
            .is_some_and(|selected| selected == child_session_id)
        {
            self.selected.lock().remove(parent_session_id);
        }
        if let Some(error) = finalize_error {
            // Commit point: restoring could resurrect a deleted transcript.
            drop(_prune);
            drop(staging_lease);
            return Err(error);
        }
        drop(_prune);
        drop(staging_lease);
        Ok(())
    }
    pub async fn archive(
        self: &Arc<Self>,
        parent: &Session,
        requested_parent_session_id: Option<&str>,
        child_session_id: &str,
    ) -> Result<()> {
        self.ensure_available()?;
        let parent_session_id = parent_identity(parent, requested_parent_session_id)?;
        validate_child_session_id(child_session_id)?;
        self.teardown_and_prune_child(parent, &parent_session_id, child_session_id)
            .await
    }

    /// Typed removal by exact child identity: any active execution is
    /// cancelled and awaited per the existing terminal-teardown contract,
    /// then the transcript, receipts, and manifest entry are pruned.
    /// Idempotent on an absent child; siblings and selection fallback untouched.
    pub async fn remove(
        self: &Arc<Self>,
        parent: &Session,
        requested_parent_session_id: Option<&str>,
        child_session_id: &str,
    ) -> Result<()> {
        self.ensure_available()?;
        let parent_session_id = parent_identity(parent, requested_parent_session_id)?;
        validate_child_session_id(child_session_id)?;
        self.teardown_and_prune_child(parent, &parent_session_id, child_session_id)
            .await
    }

    pub fn checkpoint_applications_for_restart<F>(&self, mut checkpoint: F) -> Result<()>
    where
        F: FnMut(&Application) -> Result<()>,
    {
        let applications = self
            .children
            .lock()
            .values()
            .map(|child| child.application.clone())
            .collect::<Vec<_>>();
        for application in applications {
            checkpoint(&application)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn restart_checkpoint_runtimes_for_test(
        &self,
    ) -> Vec<Option<crate::OrchestrationRuntime>> {
        self.children
            .lock()
            .values()
            .map(|child| child.application.orchestration_runtime())
            .collect()
    }

    #[cfg(test)]
    pub(super) fn restart_checkpoint_applications_for_test(&self) -> Vec<Application> {
        self.children
            .lock()
            .values()
            .map(|child| child.application.clone())
            .collect()
    }
    pub async fn shutdown(&self) -> Result<()> {
        // The creator's admission section is check-closed + insert-ticket
        // under the SAME gate this shutdown closes with: acquire the gate,
        // store `is_shutdown`, snapshot the tickets, release. A creator
        // either inserted before the close — its ticket is in the snapshot
        // below and gets cancelled — or runs after and its in-gate
        // `ensure_available` rejects it with the exact shut-down cause
        // before any ticket exists. The gate section is synchronous after
        // the acquire, so no post-insert recheck is required, and a
        // never-returning factory build (which runs OUTSIDE the gate) can
        // never hang this acquire; the bounded timeout only covers the
        // short in-flight sections.
        let tickets = {
            if tokio::time::timeout(
                TODO_CHILD_CREATE_SETTLE_TIMEOUT,
                self.admission_gate.lock(),
            )
            .await
            .is_err()
            {
                eprintln!("todo_child_shutdown category=admission_gate_abandoned");
            }
            self.is_shutdown.store(true, Ordering::Release);
            self.creating
                .lock()
                .values()
                .cloned()
                .collect::<Vec<_>>()
        };
        // Cancel every captured in-flight creation ticket: the build races
        // its ticket's cancellation token, settles its rollback, and
        // removes the ticket; the shutdown waits for exactly that settle —
        // bounded, and never for the build itself.
        for ticket in &tickets {
            ticket.cancel.cancel();
        }
        let settle_deadline = tokio::time::Instant::now() + TODO_CHILD_CREATE_SETTLE_TIMEOUT;
        for ticket in &tickets {
            let remaining = settle_deadline
                .saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                eprintln!(
                    "todo_child_shutdown category=creation_settle_deadline_exhausted"
                );
                break;
            }
            if tokio::time::timeout(remaining, ticket.settled())
                .await
                .is_err()
            {
                eprintln!("todo_child_shutdown category=creation_settle_timeout");
            }
        }
        let children = self
            .children
            .lock()
            .iter()
            .map(|(key, child)| (key.clone(), child.clone()))
            .collect::<Vec<_>>();
        let mut errors = Vec::new();
        for (key, child) in children {
            match child.shutdown_runtime().await {
                Ok(()) => {
                    self.children.lock().remove(&key);
                }
                Err(error) => {
                    errors.push(format!("{}: {error:#}", child.identity.child_session_id));
                }
            }
        }
        if !errors.is_empty() {
            bail!("Todo child shutdown failed: {}", errors.join("; "));
        }
        self.selected.lock().clear();
        Ok(())
    }

    pub fn snapshot_selected(&self, parent: &Session) -> Result<Option<TodoChildSnapshot>> {
        let parent_session_id = parent_identity(parent, None)?;
        let Some(child_session_id) = self.selected_for_parent(&parent_session_id) else {
            return Ok(None);
        };
        let key = child_key(&parent_session_id, &child_session_id);
        self.children
            .lock()
            .get(&key)
            .map(|child| child.snapshot())
            .transpose()
    }

    #[cfg(test)]
    pub(super) fn application_for_child(&self, child_session_id: &str) -> Option<Application> {
        self.children
            .lock()
            .values()
            .find(|child| child.identity.child_session_id == child_session_id)
            .map(|child| child.application.clone())
    }

    async fn resolve_existing_child(
        self: &Arc<Self>,
        parent: &Session,
        requested_parent_session_id: Option<&str>,
        requested_child_session_id: Option<&str>,
    ) -> Result<Arc<TodoSupervisor>> {
        let parent_session_id = parent_identity(parent, requested_parent_session_id)?;
        let child_session_id = requested_child_session_id
            .map(str::to_owned)
            .or_else(|| self.selected_for_parent(&parent_session_id))
            .or_else(|| self.single_active_child(&parent_session_id))
            .ok_or_else(|| anyhow!("no Todo child is selected for the active parent session"))?;
        validate_child_session_id(&child_session_id)?;
        self.ensure_existing_child(parent, &parent_session_id, &child_session_id)
            .await
    }
    async fn ensure_existing_child(
        self: &Arc<Self>,
        parent: &Session,
        parent_session_id: &str,
        child_session_id: &str,
    ) -> Result<Arc<TodoSupervisor>> {
        let key = child_key(parent_session_id, child_session_id);
        if let Some(child) = self.children.lock().get(&key).cloned() {
            return Ok(child);
        }
        let storage = self.manifest_storage(parent, parent_session_id)?;
        if !storage
            .load()?
            .0
            .entries
            .iter()
            .any(|entry| entry.child_session_id == child_session_id)
        {
            bail!("todo child {child_session_id:?} does not exist for the active parent session");
        }
        self.ensure_child(parent, parent_session_id, child_session_id, None)
            .await
    }

    async fn ensure_child(
        self: &Arc<Self>,
        parent: &Session,
        parent_session_id: &str,
        child_session_id: &str,
        label: Option<String>,
    ) -> Result<Arc<TodoSupervisor>> {
        self.ensure_available()?;
        validate_child_session_id(child_session_id)?;
        let key = child_key(parent_session_id, child_session_id);
        if let Some(child) = self.children.lock().get(&key).cloned() {
            return Ok(child);
        }
        let init_slot = {
            let mut slots = self.init_slots.lock();
            slots
                .entry(key.clone())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let _initialization = init_slot.lock().await;
        // Admission section: the closed check and the ticket insert are ONE
        // short critical section under the same gate the terminal shutdown
        // closes with. It is synchronous after the acquire, so no
        // post-insert recheck is required: either the shutdown closed first
        // (rejected here, no ticket) or this ticket is captured by the
        // shutdown's under-gate snapshot and cancelled. The build runs
        // OUTSIDE the gate; its cancellation is owned by the ticket token.
        let create_state = {
            let _admission = self.admission_gate.lock().await;
            self.ensure_available()?;
            if let Some(child) = self.children.lock().get(&key).cloned() {
                self.release_init_slot(&key, &init_slot);
                return Ok(child);
            }
            // The creation ticket exists for the whole build, so a bare
            // cancel can always find and settle an in-progress creation.
            let mut creating = self.creating.lock();
            creating
                .entry(key.clone())
                .or_insert_with(|| Arc::new(TodoChildCreateState::new()))
                .clone()
        };
        let gate = self.creation_before_root_gate.lock().take();
        if let Some(gate) = gate {
            gate.enter().await;
        }

        let mut provisional: Option<TodoChildProvisional> = None;
        let built = async {
        let factory =
            self.factory.lock().clone().ok_or_else(|| {
                anyhow!("todo child application runtime factory is not configured")
            })?;
        let fork = parent
            .recorder()
            .ok_or_else(|| anyhow!("parent session recording is unavailable"))?
            .capture_fork_point()?;
        if fork.parent_session_id != parent_session_id {
            bail!("todo child parent session identity changed during initialization");
        }
        let storage = self.manifest_storage_from_fork(&fork)?;
        let child_relative = PathBuf::from(format!("{child_session_id}.jsonl"));
        let existing_entry = storage
            .load()?
            .0
            .entries
            .into_iter()
            .find(|entry| entry.child_session_id == child_session_id);
        let entry = match existing_entry {
            Some(entry) => entry,
            None if storage.child_root.entry_exists(&child_relative)? => {
                bail!(
                    "todo child {child_session_id:?} is absent from the manifest but its transcript remains; retry archive before creating or sending"
                )
            }
            None => {
                let entry = TodoChildManifestEntry {
                    child_session_id: child_session_id.to_owned(),
                    label,
                    created_at: now_millis(),
                    status: TodoChildStatus::Created,
                    parent_entry_id: fork.parent_active_leaf_id.clone(),
                    parent_revision: fork.parent_revision,
                    kind: TodoChildKind::Todo,
                };
                let snapshot = parent.child_session_options_snapshot();
                let recorder =
                    create_durable_forked_child(fork, &storage.child_root, child_session_id)?;
                let derivation = TodoChildDerivation {
                    identity: TodoChildIdentity {
                        parent_session_id: parent_session_id.to_owned(),
                        child_session_id: child_session_id.to_owned(),
                    },
                    kind: TodoChildKind::Todo,
                    parent_entry_id: entry.parent_entry_id.clone(),
                    parent_revision: entry.parent_revision,
                    runtime: TodoChildRuntimeDerivation {
                        model_provider: snapshot.model.provider,
                        model_id: snapshot.model.id,
                        retry_settings: snapshot.retry_settings,
                        retry_role: snapshot.retry_role,
                        thinking_level: snapshot.thinking_level,
                    },
                };
                let (_, token) = recorder.tree_with_append_token()?;
                recorder.record_custom_entry_durable_checked(
                    &token,
                    TODO_CHILD_DERIVATION_RECORD,
                    &derivation,
                    |tree| {
                        if tree.header.id != child_session_id {
                            bail!("Todo child derivation transcript identity mismatch");
                        }
                        if tree.entries.iter().any(|entry| {
                            entry.custom_type.as_deref() == Some(TODO_CHILD_DERIVATION_RECORD)
                        }) {
                            bail!("Todo child derivation already exists");
                        }
                        Ok(())
                    },
                )?;
                drop(recorder);
                if let Err(error) = storage.insert_entry(entry.clone()).await {
                    return match storage.child_root.remove_file(&child_relative) {
                        Ok(()) => Err(error),
                        Err(rollback) => Err(error).context(format!(
                            "rolling back Todo child transcript after manifest insertion failure also failed: {rollback}"
                        )),
                    };
                }
                provisional = Some(TodoChildProvisional {
                    storage: storage.clone(),
                    child_session_id: child_session_id.to_owned(),
                    child_relative: child_relative.clone(),
                });
                entry
            }
        };

            self
                .build_child_candidate(
                    parent,
                    parent_session_id,
                    child_session_id,
                    &entry,
                    &storage,
                    &child_relative,
                    factory,
                    &create_state,
                )
                .await
        }
        .await;
        let child = match built {
            Ok(child) => child,
            Err(error) => {
                // The ticket stays visible until the rollback is attempted, so
                // a racing cancel waits for it and then finds neither ticket
                // nor child (idempotent `None`) instead of observing a
                // half-discarded creation. Even when the rollback itself
                // fails, the exact ticket is removed before settling: a
                // retained artifact must never poison a retry with a stale,
                // possibly-cancelled creation ticket.
                let mut failure = error;
                if let Some(provisional) = provisional.take() {
                    if let Err(rollback) = self.discard_provisional(provisional).await {
                        failure = failure.context(format!(
                            "discarding the provisional Todo child state after failure also failed: {rollback:#}"
                        ));
                    }
                }
                self.settle_creation(&key, &create_state);
                self.release_init_slot(&key, &init_slot);
                return Err(failure);
            }
        };
        // No publish after shutdown: a terminal shutdown that closed the
        // admission intake after this build started must never observe a
        // later publish resurrecting work past its terminal outcome (the
        // ticket cancellation cannot reach a build that already returned).
        // The built candidate settles exactly like a failed build —
        // application cleanup, provisional rollback, ticket removal, settle.
        if let Err(shutdown) = self.ensure_available() {
            let cleanup = child.application.cleanup().await;
            let mut failure = shutdown;
            if let Err(cleanup_error) = cleanup {
                failure = failure.context(format!(
                    "Todo child application cleanup also failed: {cleanup_error:#}"
                ));
            }
            if let Some(provisional) = provisional.take() {
                if let Err(rollback) = self.discard_provisional(provisional).await {
                    failure = failure.context(format!(
                        "discarding the provisional Todo child state after failure also failed: {rollback:#}"
                    ));
                }
            }
            self.settle_creation(&key, &create_state);
            self.release_init_slot(&key, &init_slot);
            return Err(failure);
        }
        // Publication and spawn strictly precede the settle signal: a cancel
        // waiting on the creation must observe the published, fully spawned
        let gate = self.creation_before_publication_gate.lock().take();
        if let Some(gate) = gate {
            gate.enter().await;
        }
        let existing = {
            let mut children = self.children.lock();
            match children.get(&key).cloned() {
                Some(existing) => Some(existing),
                None => {
                    children.insert(key.clone(), child.clone());
                    None
                }
            }
        };
        let gate = self.creation_before_spawn_gate.lock().take();
        if let Some(gate) = gate {
            gate.enter().await;
        }
        let outcome: Result<Option<Arc<TodoSupervisor>>> = async {
            if let Some(existing) = existing {
                child.application.cleanup().await?;
                return Ok(Some(existing));
            }
            child.spawn_if_needed();
            child.spawn_task_if_needed();
            Ok(None)
        }
        .await;
        self.settle_creation(&key, &create_state);
        self.release_init_slot(&key, &init_slot);
        Ok(outcome?.unwrap_or(child))
    }

    /// Single terminal cleanup for [`Self::ensure_child`]: removes the exact
    /// creation ticket of this attempt and only then signals settle. Removal
    /// strictly precedes the signal on every terminal exit — including a
    /// failed rollback — so a cancel waiting on the ticket either observes
    /// the in-progress creation or a fully settled state with no ticket, and
    /// a retry can never reuse a stale, possibly-cancelled ticket.
    fn settle_creation(&self, key: &TodoChildKey, create_state: &Arc<TodoChildCreateState>) {
        let mut creating = self.creating.lock();
        if creating
            .get(key)
            .is_some_and(|state| Arc::ptr_eq(state, create_state))
        {
            creating.remove(key);
        }
        create_state.done.send_replace(true);
    }

    /// Removes the slot only while no waiter holds a clone: the map entry plus
    /// the caller's Arc are exactly two strong refs; waiters clone under the
    /// same lock this check runs under, so a larger count proves a waiter.
    fn release_init_slot(&self, key: &TodoChildKey, slot: &Arc<AsyncMutex<()>>) {
        let mut slots = self.init_slots.lock();
        if slots
            .get(key)
            .is_some_and(|existing| Arc::ptr_eq(existing, slot))
            && Arc::strong_count(slot) == 2
        {
            slots.remove(key);
        }
    }

    /// Builds and activates the Todo child runtime candidate and publishes it.
    ///
    /// This is the one authoritative creation build. The factory call races
    /// the creation's cancellation token: a cancel wins by aborting the build
    /// future, and the caller rolls back every provisional artifact. Steps
    /// after the factory returned run to completion (an [`Application`]
    /// already exists by then); a cancel that arrives that late is picked up
    /// by the published supervisor instead, so either the build owner or the
    /// supervisor — never both, never neither — receives the cancellation.
    async fn build_child_candidate(
        self: &Arc<Self>,
        parent: &Session,
        parent_session_id: &str,
        child_session_id: &str,
        entry: &TodoChildManifestEntry,
        storage: &Arc<ManifestStorage>,
        child_relative: &Path,
        factory: Arc<dyn ApplicationRuntimeFactory>,
        create_state: &TodoChildCreateState,
    ) -> Result<Arc<TodoSupervisor>> {
        let prepared =
            PreparedSessionResume::prepare_under_lease(&storage.child_root, child_relative)
                .context("preparing durable Todo child session")?;
        let durable_derivation = todo_child_derivation_from_tree(prepared.tree())?;
        if durable_derivation.identity.parent_session_id != parent_session_id
            || durable_derivation.identity.child_session_id != child_session_id
            || durable_derivation.kind != TodoChildKind::Todo
            || durable_derivation.parent_entry_id != entry.parent_entry_id
            || durable_derivation.parent_revision != entry.parent_revision
        {
            bail!("Todo child derivation does not match its registered manifest provenance");
        }
        let (options, retry_settings, retry_role) =
            child_session_options(parent, &durable_derivation.runtime).await?;
        let built = tokio::select! {
            biased;
            _ = create_state.cancel.cancelled() => None,
            built = factory.build_runtime_candidate(
                parent.cwd().to_path_buf(),
                options,
                Some(prepared),
            ) => Some(built),
        };
        let candidate = match built {
            Some(candidate) => {
                candidate.context("building complete Todo child application runtime")?
            }
            None => bail!(TODO_CHILD_CREATE_CANCELLED),
        };
        candidate.session.set_retry_settings(retry_settings.clone());
        candidate.session.set_retry_role(retry_role.clone());
        candidate
            .session
            .resume_durable_child_under_lease(&storage.child_root, child_relative)
            .await
            .context("retaining durable Todo child recorder authority")?;
        let resumed_tree = candidate
            .session
            .recorder()
            .ok_or_else(|| anyhow!("Todo child recorder is unavailable after durable resume"))?
            .tree()
            .context("reading resumed Todo child derivation")?;
        let derived = manifest_entry_from_tree(&resumed_tree, parent_session_id, child_session_id)?;
        if derived.parent_entry_id != entry.parent_entry_id
            || derived.parent_revision != entry.parent_revision
            || derived.kind != entry.kind
        {
            bail!("Todo child derivation does not match its registered manifest provenance");
        }
        let application = Application::from_runtime_candidate(candidate)
            .await
            .context("activating complete Todo child application")?;
        application.session().set_retry_settings(retry_settings);
        application.session().set_retry_role(retry_role);
        if let Err(error) = application.attach_todo_child_client(self.client()) {
            let cleanup = application.cleanup().await;
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup_error) => error.context(format!(
                    "Todo child application cleanup also failed: {cleanup_error:#}"
                )),
            })
            .context("attaching Todo child service to child application");
        }
        if let Err(error) = application.attach_runtime_factory(factory) {
            let cleanup = application.cleanup().await;
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup_error) => error.context(format!(
                    "Todo child application cleanup also failed: {cleanup_error:#}"
                )),
            })
            .context("attaching runtime factory to child application");
        }
        let actual_child_session_id = application
            .session()
            .recorder_info()
            .map(|(id, _)| id)
            .ok_or_else(|| anyhow!("Todo child recorder is unavailable after activation"))?;
        if actual_child_session_id != child_session_id {
            application.cleanup().await?;
            bail!("Todo child recorder identity does not match its manifest identity");
        }
        let child = match TodoSupervisor::from_application(
            TodoChildIdentity {
                parent_session_id: parent_session_id.to_owned(),
                child_session_id: child_session_id.to_owned(),
            },
            application.clone(),
            entry.clone(),
            storage.clone(),
        ) {
            Ok(child) => Arc::new(child),
            Err(error) => {
                let cleanup = application.cleanup().await;
                return Err(match cleanup {
                    Ok(()) => error,
                    Err(cleanup_error) => error.context(format!(
                        "Todo child application cleanup also failed: {cleanup_error:#}"
                    )),
                })
                .context("recovering Todo child runtime state");
            }
        };
        if let Err(error) = child.persist_recovered_receipts() {
            child.application.cleanup().await?;
            return Err(error).context("persisting recovered Todo child receipts");
        }
        if let Err(error) = child.persist_recovered_tasks() {
            child.application.cleanup().await?;
            return Err(error).context("persisting recovered Todo child tasks");
        }
        let is_created = { child.state.lock().status == TodoChildStatus::Created };
        if is_created && let Err(error) = child.set_status(TodoChildStatus::Idle, None).await {
            child.application.cleanup().await?;
            return Err(error).context("persisting initialized Todo child status");
        }
        Ok(child)
    }

    fn selected_for_parent(&self, parent_session_id: &str) -> Option<String> {
        self.selected.lock().get(parent_session_id).cloned()
    }

    fn single_active_child(&self, parent_session_id: &str) -> Option<String> {
        let children = self.children.lock();
        let mut matching = children.values().filter(|child| {
            child.identity.parent_session_id == parent_session_id
                && !matches!(child.aggregate_status(), TodoChildStatus::Stopped)
        });
        let first = matching.next()?.identity.child_session_id.clone();
        matching.next().is_none().then_some(first)
    }

    fn manifest_storage(
        &self,
        parent: &Session,
        parent_session_id: &str,
    ) -> Result<Arc<ManifestStorage>> {
        let fork = parent
            .recorder()
            .ok_or_else(|| anyhow!("parent session recording is unavailable"))?
            .capture_fork_point()?;
        if fork.parent_session_id != parent_session_id {
            bail!("todo child parent session identity mismatch");
        }
        self.manifest_storage_from_fork(&fork)
    }

    fn manifest_storage_from_fork(&self, fork: &ParentForkPoint) -> Result<Arc<ManifestStorage>> {
        let child_root = fork
            .lease
            .lease_directory(
                &Path::new("children").join(&fork.parent_session_id),
                CapabilityAccess::PrivateReadWrite,
                RootCreation::CreateDirectory,
            )
            .context("leasing durable Todo child root")?;
        let gate = {
            let mut gates = self.manifest_slots.lock();
            gates
                .entry(fork.parent_session_id.clone())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        Ok(Arc::new(ManifestStorage {
            parent_session_id: fork.parent_session_id.clone(),
            child_root,
            gate,
        }))
    }
}

impl TodoSupervisor {
    fn from_application(
        identity: TodoChildIdentity,
        application: Application,
        manifest_entry: TodoChildManifestEntry,
        manifest: Arc<ManifestStorage>,
    ) -> Result<Self> {
        let (message_queue, receipts, recovered_receipt_updates) =
            recover_receipts(&application.session(), &identity)?;
        let (revision, tasks, recovered_task_interruptions) =
            recover_tasks(&application.session())?;
        let has_interrupted_work =
            !recovered_receipt_updates.is_empty() || !recovered_task_interruptions.is_empty();
        let (status, last_error) = if has_interrupted_work {
            (
                TodoChildStatus::Failed,
                Some("Todo child execution outcome is unknown after restart; running work was not replayed".to_owned()),
            )
        } else {
            (manifest_entry.status, None)
        };
        let (task_tx, task_rx) = mpsc::channel(TODO_CHILD_MESSAGE_QUEUE_CAP);
        Ok(Self {
            identity,
            application,
            state: Mutex::new(TodoSupervisorState {
                status,
                message_queue,
                receipts,
                revision,
                recovered_receipt_updates,
                tasks,
                recovered_task_interruptions,
                active_task_turn: None,
                active_turn: None,
                is_turn_loop_running: false,
                last_error,
            }),
            task_tx,
            task_rx: Mutex::new(Some(task_rx)),
            task_actor: Mutex::new(None),
            turn_gate: Arc::new(AsyncMutex::new(())),
            lifecycle_gate: Arc::new(AsyncMutex::new(())),
            manifest_entry: Mutex::new(manifest_entry),
            manifest,
        })
    }

    fn validate(&self, parent_session_id: &str, child_session_id: &str) -> Result<()> {
        if self.identity.parent_session_id != parent_session_id
            || self.identity.child_session_id != child_session_id
        {
            bail!("todo child identity mismatch");
        }
        Ok(())
    }

    async fn send(
        self: &Arc<Self>,
        message: String,
        images: Vec<pi_ai::ContentBlock>,
    ) -> Result<TodoChildReceipt> {
        let _lifecycle = self.lifecycle_gate.lock().await;
        let body = message.trim();
        if body.is_empty() {
            bail!("todo child message must not be blank");
        }
        if body.len() > TODO_CHILD_MESSAGE_MAX_BYTES {
            bail!("todo child message exceeds {TODO_CHILD_MESSAGE_MAX_BYTES} bytes");
        }
        {
            let state = self.state.lock();
            if matches!(state.status, TodoChildStatus::Stopped) {
                bail!("todo child is stopped");
            }
            if state.message_queue.len() >= TODO_CHILD_MESSAGE_QUEUE_CAP {
                bail!("todo child message queue is full");
            }
        }

        let accepted_at = now_millis();
        let message = TodoChildMessage {
            id: Uuid::now_v7().to_string(),
            parent_session_id: self.identity.parent_session_id.clone(),
            child_session_id: self.identity.child_session_id.clone(),
            body: body.to_owned(),
            accepted_at,
            status: TodoChildReceiptStatus::Queued,
            images,
        };
        let receipt = TodoChildReceipt {
            id: Uuid::now_v7().to_string(),
            parent_session_id: self.identity.parent_session_id.clone(),
            child_session_id: self.identity.child_session_id.clone(),
            message_id: message.id.clone(),
            accepted_at,
            status: TodoChildReceiptStatus::Queued,
            summary: None,
        };

        self.persist_receipt(&receipt, Some(&message))?;
        {
            let mut state = self.state.lock();
            state.message_queue.push_back(message);
            state.receipts.push_back(receipt.clone());
            trim_receipts(&mut state.receipts);
            state.last_error = None;
        }
        if let Err(error) = self.persist_manifest_status().await {
            self.note_projection_error(&error);
        }
        self.spawn_if_needed();
        Ok(receipt)
    }

    async fn add_task(
        self: &Arc<Self>,
        request: TodoChildAddTaskRequest,
    ) -> Result<TodoChildAddTaskResult> {
        if self.state.lock().status == TodoChildStatus::Stopped {
            bail!("todo child is stopped");
        }
        let (reply, response) = oneshot::channel();
        self.task_tx
            .send(TodoTaskCommand::Add { request, reply })
            .await
            .map_err(|_| anyhow!("Todo child task actor is unavailable"))?;
        response
            .await
            .map_err(|_| anyhow!("Todo child task actor dropped its add-task response"))?
    }

    fn spawn_task_if_needed(self: &Arc<Self>) {
        let receiver = self.task_rx.lock().take();
        let Some(receiver) = receiver else {
            match self.task_tx.try_send(TodoTaskCommand::Wake) {
                Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    self.fail_persistence("Todo child task actor is unavailable");
                }
            }
            return;
        };
        let supervisor = Arc::downgrade(self);
        *self.task_actor.lock() = Some(tokio::spawn(async move {
            TodoSupervisor::run_task_actor(supervisor, receiver).await;
        }));
    }

    async fn run_task_actor(
        supervisor: Weak<TodoSupervisor>,
        mut receiver: mpsc::Receiver<TodoTaskCommand>,
    ) {
        if let Some(supervisor) = supervisor.upgrade() {
            supervisor.launch_next_task();
        }
        while let Some(command) = receiver.recv().await {
            let Some(supervisor) = supervisor.upgrade() else {
                break;
            };
            match command {
                TodoTaskCommand::Add { request, reply } => {
                    let outcome = supervisor.commit_task_add(request);
                    let should_schedule = outcome.is_ok();
                    drop(reply.send(outcome));
                    if should_schedule {
                        supervisor.launch_next_task();
                    }
                }
                TodoTaskCommand::Finished { task_id, outcome } => {
                    let terminal_lifecycle = matches!(
                        supervisor.state.lock().status,
                        TodoChildStatus::Paused | TodoChildStatus::Stopped
                    );
                    if terminal_lifecycle {
                        if let Err(error) = supervisor.join_task_turn(true).await {
                            supervisor.fail_persistence(&format!(
                                "joining interrupted Todo child task turn: {error}"
                            ));
                        }
                        if let Err(error) = supervisor.interrupt_running_task() {
                            supervisor.fail_persistence(&format!(
                                "interrupting settled Todo child task during lifecycle transition: {error}"
                            ));
                        }
                    } else if let Err(error) = supervisor.join_task_turn(false).await {
                        supervisor
                            .fail_persistence(&format!("joining Todo child task turn: {error}"));
                    } else if let Err(error) = supervisor.finish_task(&task_id, outcome) {
                        supervisor.fail_persistence(&format!("settling Todo child task: {error}"));
                    } else {
                        supervisor.launch_next_task();
                    }
                }
                TodoTaskCommand::Wake => supervisor.launch_next_task(),
                TodoTaskCommand::Stop { reply } => {
                    let outcome = supervisor.interrupt_running_task();
                    drop(reply.send(outcome));
                    break;
                }
            }
        }
    }

    fn commit_task_add(&self, request: TodoChildAddTaskRequest) -> Result<TodoChildAddTaskResult> {
        let content = request.content.trim();
        if content.is_empty() {
            bail!("Todo child task content must not be blank");
        }
        if content.len() > TODO_CHILD_MESSAGE_MAX_BYTES {
            bail!("Todo child task content exceeds {TODO_CHILD_MESSAGE_MAX_BYTES} bytes");
        }
        let mut unique_dependencies = HashSet::new();
        if request
            .depends_on
            .iter()
            .any(|dependency| !unique_dependencies.insert(dependency.as_str()))
        {
            bail!("Todo child task dependencies must be unique");
        }

        let mut state = self.state.lock();
        if state.status == TodoChildStatus::Stopped {
            bail!("todo child is stopped");
        }
        if let Some(expected) = request.expected_revision
            && expected != state.revision
        {
            bail!(
                "stale Todo child task revision: expected {expected}, current {}",
                state.revision
            );
        }
        let known_tasks = state
            .tasks
            .iter()
            .map(|task| task.id.as_str())
            .collect::<HashSet<_>>();
        if let Some(missing) = request
            .depends_on
            .iter()
            .find(|dependency| !known_tasks.contains(dependency.as_str()))
        {
            bail!("Todo child task dependency {missing:?} does not exist");
        }

        let revision = next_task_revision(state.revision)?;
        let task = TodoChildTask {
            id: Uuid::now_v7().to_string(),
            content: content.to_owned(),
            depends_on: request.depends_on,
            status: TodoChildTaskStatus::Pending,
            agent: normalized_task_field(request.agent, "agent")?,
            role: normalized_task_field(request.role, "role")?,
            created_at: now_millis(),
            summary: None,
        };
        self.persist_task_event(&TodoChildTaskEvent::Added {
            revision,
            task: task.clone(),
        })?;
        state.revision = revision;
        state.tasks.push(task.clone());
        state.last_error = None;
        Ok(TodoChildAddTaskResult { task, revision })
    }

    fn launch_next_task(self: &Arc<Self>) {
        let Some(task) = self.next_ready_task() else {
            return;
        };
        if let Err(error) = self.start_task(&task.id) {
            self.fail_persistence(&format!("starting Todo child task: {error}"));
            return;
        }
        let application = self.application.clone();
        let turn_gate = self.turn_gate.clone();
        let task_tx = self.task_tx.clone();
        let handle = tokio::spawn(async move {
            let outcome = TodoSupervisor::execute_task(application, turn_gate, task.content).await;
            task_tx
                .send(TodoTaskCommand::Finished {
                    task_id: task.id,
                    outcome,
                })
                .await
                .map_err(|_| "Todo child task actor dropped a completed task outcome".to_owned())
        });
        self.state.lock().active_task_turn = Some(handle);
    }

    fn next_ready_task(&self) -> Option<TodoChildTask> {
        let state = self.state.lock();
        if matches!(
            state.status,
            TodoChildStatus::Paused | TodoChildStatus::Stopped | TodoChildStatus::Failed
        ) || state
            .tasks
            .iter()
            .any(|task| task.status == TodoChildTaskStatus::Running)
        {
            return None;
        }
        let completed = state
            .tasks
            .iter()
            .filter(|task| task.status == TodoChildTaskStatus::Completed)
            .map(|task| task.id.as_str())
            .collect::<HashSet<_>>();
        state
            .tasks
            .iter()
            .find(|task| {
                task.status == TodoChildTaskStatus::Pending
                    && task
                        .depends_on
                        .iter()
                        .all(|dependency| completed.contains(dependency.as_str()))
            })
            .cloned()
    }

    fn start_task(&self, task_id: &str) -> Result<()> {
        let mut state = self.state.lock();
        let index = state
            .tasks
            .iter()
            .position(|task| task.id == task_id)
            .ok_or_else(|| anyhow!("Todo child task {task_id:?} disappeared before start"))?;
        if state.tasks[index].status != TodoChildTaskStatus::Pending {
            bail!("Todo child task {task_id:?} is not pending");
        }
        let revision = next_task_revision(state.revision)?;
        self.persist_task_event(&TodoChildTaskEvent::Started {
            revision,
            task_id: task_id.to_owned(),
        })?;
        state.revision = revision;
        state.tasks[index].status = TodoChildTaskStatus::Running;
        state.tasks[index].summary = None;
        state.status = TodoChildStatus::Running;
        state.last_error = None;
        Ok(())
    }

    async fn execute_task(
        application: Application,
        turn_gate: Arc<AsyncMutex<()>>,
        content: String,
    ) -> Result<Option<String>, String> {
        let _turn = turn_gate.lock().await;
        let accepted = application
            .prompt_without_auto_todo(content, Vec::new(), None)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "Todo child task prompt did not accept a turn".to_owned())?;
        application.wait_for_idle().await;
        match application.turn_lifecycle_snapshot().last_terminal {
            Some(terminal)
                if terminal.turn_id == accepted.turn_id
                    && terminal.status == super::TurnTerminalStatus::Succeeded =>
            {
                let summary = application.session().last_assistant_text();
                Ok((!summary.trim().is_empty()).then(|| bounded_summary(&summary)))
            }
            Some(terminal) if terminal.turn_id == accepted.turn_id => Err(terminal
                .message
                .unwrap_or_else(|| format!("Todo child task turn {:?}", terminal.status))),
            Some(_) => {
                Err("Todo child task turn observed a stale terminal lifecycle outcome".to_owned())
            }
            None => Err(
                "Todo child task turn completed without a terminal lifecycle outcome".to_owned(),
            ),
        }
    }

    fn finish_task(&self, task_id: &str, outcome: Result<Option<String>, String>) -> Result<()> {
        let mut state = self.state.lock();
        let index = state
            .tasks
            .iter()
            .position(|task| task.id == task_id)
            .ok_or_else(|| anyhow!("Todo child task {task_id:?} disappeared before settlement"))?;
        if matches!(
            state.status,
            TodoChildStatus::Paused | TodoChildStatus::Stopped
        ) {
            return Ok(());
        }
        if state.tasks[index].status != TodoChildTaskStatus::Running {
            bail!("Todo child task {task_id:?} is not running");
        }
        let revision = next_task_revision(state.revision)?;
        let event = match &outcome {
            Ok(summary) => TodoChildTaskEvent::Completed {
                revision,
                task_id: task_id.to_owned(),
                summary: summary.clone(),
            },
            Err(summary) => TodoChildTaskEvent::Failed {
                revision,
                task_id: task_id.to_owned(),
                summary: bounded_summary(summary),
            },
        };
        self.persist_task_event(&event)?;
        state.revision = revision;
        match outcome {
            Ok(summary) => {
                state.tasks[index].status = TodoChildTaskStatus::Completed;
                state.tasks[index].summary = summary;
                if !matches!(
                    state.status,
                    TodoChildStatus::Paused | TodoChildStatus::Stopped
                ) {
                    state.status = TodoChildStatus::Idle;
                    state.last_error = None;
                }
            }
            Err(summary) => {
                let summary = bounded_summary(&summary);
                state.tasks[index].status = TodoChildTaskStatus::Failed;
                state.tasks[index].summary = Some(summary.clone());
                if !matches!(
                    state.status,
                    TodoChildStatus::Paused | TodoChildStatus::Stopped
                ) {
                    state.status = TodoChildStatus::Failed;
                    state.last_error = Some(summary);
                }
            }
        }
        Ok(())
    }

    fn interrupt_running_task(&self) -> Result<()> {
        let mut state = self.state.lock();
        let Some(index) = state
            .tasks
            .iter()
            .position(|task| task.status == TodoChildTaskStatus::Running)
        else {
            return Ok(());
        };
        let revision = next_task_revision(state.revision)?;
        let task_id = state.tasks[index].id.clone();
        self.persist_task_event(&TodoChildTaskEvent::Interrupted { revision, task_id })?;
        state.revision = revision;
        state.tasks[index].status = TodoChildTaskStatus::Interrupted;
        state.tasks[index].summary =
            Some("execution was interrupted before its outcome was known".to_owned());
        Ok(())
    }

    async fn join_task_turn(&self, cancelled: bool) -> Result<()> {
        let task = self.state.lock().active_task_turn.take();
        let Some(task) = task else { return Ok(()) };
        match task.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(anyhow!(error)),
            Err(error) if cancelled && error.is_cancelled() => Ok(()),
            Err(error) => Err(anyhow!("Todo child task turn join failed: {error}")),
        }
    }

    async fn abort_application_turns(&self) {
        {
            let state = self.state.lock();
            if let Some(task) = state.active_task_turn.as_ref() {
                task.abort();
            }
            if let Some(turn) = state.active_turn.as_ref() {
                turn.abort();
            }
        }
        self.application.abort().await;
    }

    async fn join_execution_loops(&self) -> Result<()> {
        let (task_turn, message_turn) = {
            let mut state = self.state.lock();
            (state.active_task_turn.take(), state.active_turn.take())
        };
        let mut errors = Vec::new();
        if let Some(task) = task_turn {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => errors.push(error),
                Err(error) if error.is_cancelled() => {}
                Err(error) => errors.push(format!("Todo child task turn join failed: {error}")),
            }
        }
        if let Some(task) = message_turn {
            match task.await {
                Ok(()) => {}
                Err(error) if error.is_cancelled() => {}
                Err(error) => errors.push(format!("Todo child message turn join failed: {error}")),
            }
        }
        self.application.wait_for_idle().await;
        self.state.lock().is_turn_loop_running = false;
        if errors.is_empty() {
            Ok(())
        } else {
            bail!("{}", errors.join("; "))
        }
    }

    async fn stop_task_actor(&self) -> Result<()> {
        let (reply, response) = oneshot::channel();
        let response_outcome = if self
            .task_tx
            .send(TodoTaskCommand::Stop { reply })
            .await
            .is_err()
        {
            Ok(())
        } else {
            match response.await {
                Ok(outcome) => outcome,
                Err(_) => Err(anyhow!("Todo child task actor dropped its stop response")),
            }
        };
        let actor = self.task_actor.lock().take();
        let actor_outcome = match actor {
            Some(actor) => actor
                .await
                .map_err(|error| anyhow!("Todo child task actor join failed: {error}")),
            None => Ok(()),
        };
        match (response_outcome, actor_outcome) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(response), Ok(())) => Err(response),
            (Ok(()), Err(actor)) => Err(actor),
            (Err(response), Err(actor)) => {
                bail!("{response:#}; {actor:#}")
            }
        }
    }

    async fn pause(&self) -> Result<()> {
        let _lifecycle = self.lifecycle_gate.lock().await;
        {
            let mut state = self.state.lock();
            if state.status == TodoChildStatus::Stopped {
                bail!("todo child is stopped");
            }
            state.status = TodoChildStatus::Paused;
            state.last_error = None;
        }
        self.abort_application_turns().await;
        self.interrupt_running_receipts("pause")?;
        self.interrupt_running_task()?;
        self.join_execution_loops().await?;
        self.persist_manifest_status().await
    }

    async fn resume(self: &Arc<Self>) -> Result<()> {
        let _lifecycle = self.lifecycle_gate.lock().await;
        {
            let mut state = self.state.lock();
            match state.status {
                TodoChildStatus::Paused | TodoChildStatus::Failed => {
                    state.status = TodoChildStatus::Idle;
                    state.last_error = None;
                }
                TodoChildStatus::Stopped => bail!("todo child is stopped"),
                TodoChildStatus::Created | TodoChildStatus::Idle | TodoChildStatus::Running => {}
            }
        }
        self.persist_manifest_status().await?;
        self.spawn_if_needed();
        Ok(())
    }

    async fn cancel(&self) -> Result<()> {
        let _lifecycle = self.lifecycle_gate.lock().await;
        self.terminal_teardown("cancel", "cancelled before execution")
            .await
    }

    fn snapshot(&self) -> Result<TodoChildSnapshot> {
        let state = self.state.lock();
        let status = aggregate_status(&state);
        let entry = self.manifest_entry.lock().clone();
        let transcript_entries = self
            .application
            .session()
            .recorder()
            .ok_or_else(|| anyhow!("Todo child recorder is unavailable"))?
            .tree()
            .context("reading Todo child transcript for status")?
            .entries
            .len();
        Ok(TodoChildSnapshot {
            parent_session_id: self.identity.parent_session_id.clone(),
            child_session_id: self.identity.child_session_id.clone(),
            status,
            is_running: status == TodoChildStatus::Running,
            transcript_entries,
            receipts: state.receipts.iter().cloned().collect(),
            revision: state.revision,
            tasks: state.tasks.clone(),
            last_error: state.last_error.clone(),
            label: entry.label,
            created_at: entry.created_at,
        })
    }

    fn detail_snapshot(&self) -> Result<TodoChildDetailSnapshot> {
        const TRANSCRIPT_LIMIT: usize = 100;
        const ACTIVITY_LIMIT: usize = 100;
        let snapshot = self.snapshot()?;
        let entry = self.manifest_entry.lock().clone();
        let session = self.application.session();
        let workspace_roots = session.workspace_roots();
        let cwd = workspace_roots.cwd().to_string_lossy().into_owned();
        let workspace = workspace_roots
            .roots()
            .iter()
            .map(|root| root.to_string_lossy())
            .collect::<Vec<_>>()
            .join(", ");
        let tree = session
            .recorder()
            .ok_or_else(|| anyhow!("Todo child recorder is unavailable"))?
            .tree()
            .context("reading authoritative Todo child transcript")?;
        if tree.header.id != self.identity.child_session_id {
            bail!("Todo child transcript identity mismatch");
        }
        let transcript_entries = tree.entries.len();
        let transcript = tree
            .branch(None)
            .into_iter()
            .rev()
            .take(TRANSCRIPT_LIMIT)
            .rev()
            .filter_map(project_transcript_entry)
            .collect::<Vec<_>>();
        const PROJECTION_LIMIT: usize = 100;
        let runtime = self
            .application
            .orchestration_runtime()
            .ok_or_else(|| anyhow!("Todo child orchestration runtime is unavailable"))?;
        let main_id = runtime.main_agent_id().to_owned();
        let mut job_snapshots = runtime.jobs(None);
        job_snapshots.sort_by_key(|job| job.created_at);
        if job_snapshots.len() > PROJECTION_LIMIT {
            job_snapshots.drain(..job_snapshots.len() - PROJECTION_LIMIT);
        }
        let task_ids = job_snapshots
            .iter()
            .filter_map(|job| {
                job.todo_task_id
                    .clone()
                    .map(|task_id| (job.agent_id.clone(), task_id))
            })
            .collect::<HashMap<_, _>>();
        let mut agent_snapshots = runtime.list("");
        agent_snapshots.sort_by(|left, right| left.id.cmp(&right.id));
        let mut projected_agents = agent_snapshots
            .into_iter()
            .map(|agent| TodoChildDetailAgent {
                task_id: task_ids.get(&agent.id).cloned(),
                id: agent.id,
                display_name: bounded_detail_text(&agent.display_name, 160),
                role: (!agent.agent.trim().is_empty())
                    .then(|| bounded_detail_text(&agent.agent, 160)),
                status: agent.status,
            })
            .collect::<Vec<_>>();
        let supervisor = projected_agents
            .iter()
            .find(|agent| agent.id == main_id)
            .cloned();
        projected_agents.retain(|agent| agent.id != main_id);
        projected_agents.truncate(PROJECTION_LIMIT);
        let jobs = job_snapshots
            .into_iter()
            .map(|job| {
                let display_name = runtime.resolve_agent_display_name(&job.agent_id);
                let summary = job
                    .result
                    .as_ref()
                    .and_then(|result| result.error.as_deref().or(Some(result.output.as_str())))
                    .filter(|summary| !summary.trim().is_empty())
                    .map(|summary| bounded_detail_text(summary, 600));
                TodoChildDetailJob {
                    job_id: job.id,
                    display_name: bounded_detail_text(&display_name, 160),
                    role: (!job.agent.trim().is_empty())
                        .then(|| bounded_detail_text(&job.agent, 160)),
                    status: job.status,
                    task_id: job.todo_task_id,
                    summary,
                }
            })
            .collect::<Vec<_>>();
        let mut delivered = runtime.delivered_messages();
        if delivered.len() > PROJECTION_LIMIT {
            delivered.drain(..delivered.len() - PROJECTION_LIMIT);
        }
        let irc = delivered
            .into_iter()
            .map(|message| TodoChildDetailIrcMessage {
                message_id: message.id,
                from: runtime.resolve_agent_display_name(&message.from),
                to: runtime.resolve_agent_display_name(&message.to),
                text: bounded_detail_text(&message.body, 600),
                at_ms: message.timestamp,
                reply_to: message.reply_to,
            })
            .collect::<Vec<_>>();
        let mut activity = irc
            .iter()
            .map(|message| TodoChildDetailActivity {
                at_ms: message.at_ms,
                actor: Some(message.from.clone()),
                kind: TodoChildDetailActivityKind::Irc,
                text: message.text.clone(),
            })
            .collect::<Vec<_>>();
        let agents = projected_agents;
        activity.sort_by_key(|entry| entry.at_ms);
        if activity.len() > ACTIVITY_LIMIT {
            activity.drain(..activity.len() - ACTIVITY_LIMIT);
        }
        Ok(TodoChildDetailSnapshot {
            parent_session_id: self.identity.parent_session_id.clone(),
            child_session_id: self.identity.child_session_id.clone(),
            todo_id: self.identity.child_session_id.clone(),
            parent: TodoChildParentProvenance {
                session_id: self.identity.parent_session_id.clone(),
                entry_id: entry.parent_entry_id,
                revision: entry.parent_revision,
            },
            workspace: TodoChildWorkspaceIdentity { workspace, cwd },
            status: snapshot.status,
            is_running: snapshot.is_running,
            revision: snapshot.revision,
            label: snapshot.label,
            created_at: snapshot.created_at,
            last_error: snapshot.last_error,
            transcript_entries,
            tasks: snapshot.tasks,
            receipts: snapshot.receipts,
            supervisor,
            agents,
            jobs,
            activity,
            irc,
            transcript,
        })
    }

    async fn shutdown_runtime(&self) -> Result<()> {
        let _lifecycle = self.lifecycle_gate.lock().await;
        let durable_status = match self.state.lock().status {
            TodoChildStatus::Paused => TodoChildStatus::Paused,
            TodoChildStatus::Stopped => TodoChildStatus::Stopped,
            TodoChildStatus::Failed => TodoChildStatus::Failed,
            TodoChildStatus::Created | TodoChildStatus::Idle | TodoChildStatus::Running => {
                TodoChildStatus::Idle
            }
        };
        self.state.lock().status = TodoChildStatus::Paused;
        self.abort_application_turns().await;
        let mut errors = Vec::new();
        if let Err(error) = self.interrupt_running_receipts("host shutdown") {
            errors.push(format!("interrupting running receipts: {error:#}"));
        }
        if let Err(error) = self.interrupt_running_task() {
            errors.push(format!("interrupting running task: {error:#}"));
        }
        if let Err(error) = self.stop_task_actor().await {
            errors.push(format!("stopping task actor: {error:#}"));
        }
        if let Err(error) = self.join_execution_loops().await {
            errors.push(format!("joining execution loops: {error:#}"));
        }
        {
            let mut state = self.state.lock();
            state.status = durable_status;
            if durable_status == TodoChildStatus::Idle {
                state.last_error = None;
            }
        }
        if let Err(error) = self.persist_manifest_status().await {
            errors.push(format!(
                "persisting restartable lifecycle status: {error:#}"
            ));
        }
        if let Err(error) = self.application.cleanup().await {
            errors.push(format!("cleaning up Todo child application: {error:#}"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            bail!("Todo child host shutdown failed: {}", errors.join("; "))
        }
    }

    async fn stop_and_cleanup(&self) -> Result<()> {
        let _lifecycle = self.lifecycle_gate.lock().await;
        self.terminal_teardown("stop", "stopped before execution")
            .await
    }

    async fn terminal_teardown(&self, operation: &str, queued_summary: &str) -> Result<()> {
        self.state.lock().status = TodoChildStatus::Stopped;
        self.abort_application_turns().await;
        let mut errors = Vec::new();
        if let Err(error) = self.interrupt_running_receipts(operation) {
            errors.push(format!("interrupting running receipts: {error:#}"));
        }
        if let Err(error) = self.stop_task_actor().await {
            errors.push(format!("stopping task actor: {error:#}"));
        }
        if let Err(error) = self.join_execution_loops().await {
            errors.push(format!("joining execution loops: {error:#}"));
        }
        if let Err(error) = self
            .terminalize_queued_messages(operation, queued_summary)
            .await
        {
            errors.push(format!("terminalizing queued messages: {error:#}"));
        }
        if let Err(error) = Box::pin(self.application.cleanup()).await {
            errors.push(format!("cleaning up Todo child application: {error:#}"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            bail!(
                "Todo child {operation} teardown failed: {}",
                errors.join("; ")
            )
        }
    }

    fn interrupt_running_receipts(&self, operation: &str) -> Result<()> {
        loop {
            let mut state = self.state.lock();
            let Some(index) = state
                .receipts
                .iter()
                .position(|receipt| receipt.status == TodoChildReceiptStatus::Running)
            else {
                return Ok(());
            };
            let mut terminal = state.receipts[index].clone();
            terminal.status = TodoChildReceiptStatus::Failed;
            terminal.summary = Some(format!(
                "execution outcome is unknown after {operation}; message was not replayed"
            ));
            self.persist_receipt(&terminal, None)?;
            state.receipts[index] = terminal;
        }
    }

    async fn terminalize_queued_messages(&self, operation: &str, summary: &str) -> Result<()> {
        let message_ids = {
            let state = self.state.lock();
            queued_receipts(&state, operation)?;
            state
                .message_queue
                .iter()
                .map(|message| message.id.clone())
                .collect::<Vec<_>>()
        };
        for message_id in &message_ids {
            let terminal = {
                let state = self.state.lock();
                let receipt = state
                    .receipts
                    .iter()
                    .find(|receipt| receipt.message_id == *message_id)
                    .ok_or_else(|| {
                        anyhow!("queued Todo child receipt disappeared during {operation}")
                    })?;
                let mut terminal = receipt.clone();
                terminal.status = TodoChildReceiptStatus::Failed;
                terminal.summary = Some(summary.to_owned());
                terminal
            };
            if let Err(error) = self.persist_receipt(&terminal, None) {
                self.fail_persistence(&format!(
                    "persisting {operation} Todo child receipt: {error}"
                ));
                return Err(error);
            }
            let mut state = self.state.lock();
            let receipt = state
                .receipts
                .iter_mut()
                .find(|receipt| receipt.id == terminal.id)
                .ok_or_else(|| anyhow!("{operation} Todo child receipt disappeared"))?;
            *receipt = terminal;
            if let Some(index) = state
                .message_queue
                .iter()
                .position(|message| message.id == *message_id)
            {
                state.message_queue.remove(index);
            }
        }
        {
            let mut state = self.state.lock();
            state.status = TodoChildStatus::Stopped;
        }
        self.persist_terminal_manifest_status().await?;
        Ok(())
    }

    fn aggregate_status(&self) -> TodoChildStatus {
        aggregate_status(&self.state.lock())
    }

    fn spawn_if_needed(self: &Arc<Self>) {
        let mut state = self.state.lock();
        if matches!(
            state.status,
            TodoChildStatus::Paused | TodoChildStatus::Stopped | TodoChildStatus::Failed
        ) || state.message_queue.is_empty()
            || state.is_turn_loop_running
        {
            return;
        }
        state.status = TodoChildStatus::Running;
        state.is_turn_loop_running = true;
        let supervisor = self.clone();
        state.active_turn = Some(tokio::spawn(async move {
            supervisor.run_queued_turns().await;
        }));
    }

    async fn run_queued_turns(self: Arc<Self>) {
        let _turn = self.turn_gate.lock().await;
        loop {
            let next = {
                let mut state = self.state.lock();
                if matches!(
                    state.status,
                    TodoChildStatus::Paused | TodoChildStatus::Stopped | TodoChildStatus::Failed
                ) {
                    None
                } else {
                    state.message_queue.pop_front()
                }
            };
            let Some(mut message) = next else { break };
            message.status = TodoChildReceiptStatus::Running;
            let running_receipt = {
                let mut state = self.state.lock();
                let receipt = state
                    .receipts
                    .iter_mut()
                    .find(|receipt| receipt.message_id == message.id);
                match receipt {
                    Some(receipt) => {
                        receipt.status = TodoChildReceiptStatus::Running;
                        receipt.clone()
                    }
                    None => continue,
                }
            };
            if let Err(error) = self.persist_receipt(&running_receipt, None) {
                message.status = TodoChildReceiptStatus::Queued;
                {
                    let mut state = self.state.lock();
                    if let Some(receipt) = state
                        .receipts
                        .iter_mut()
                        .find(|receipt| receipt.id == running_receipt.id)
                    {
                        receipt.status = TodoChildReceiptStatus::Queued;
                        receipt.summary = None;
                    }
                    state.message_queue.push_front(message);
                }
                self.fail_persistence(&format!("persisting running Todo child receipt: {error}"));
                break;
            }
            if let Err(error) = self.persist_manifest_status().await {
                self.fail_persistence(&format!("persisting running Todo child manifest: {error}"));
                break;
            }

            let result = match self
                .application
                .prompt_without_auto_todo(message.body, message.images, None)
                .await
            {
                Ok(Some(accepted)) => {
                    self.application.wait_for_idle().await;
                    match self.application.turn_lifecycle_snapshot().last_terminal {
                        Some(terminal)
                            if terminal.turn_id == accepted.turn_id
                                && terminal.status == super::TurnTerminalStatus::Succeeded =>
                        {
                            Ok(self.application.session().last_assistant_text())
                        }
                        Some(terminal) if terminal.turn_id == accepted.turn_id => Err(terminal
                            .message
                            .unwrap_or_else(|| format!("Todo child turn {:?}", terminal.status))),
                        Some(_) => Err(
                            "Todo child turn observed a stale terminal lifecycle outcome"
                                .to_owned(),
                        ),
                        None => Err(
                            "Todo child turn completed without a terminal lifecycle outcome"
                                .to_owned(),
                        ),
                    }
                }
                Ok(None) => Err("Todo child prompt did not accept a turn".to_owned()),
                Err(error) => Err(error.to_string()),
            };
            let terminal = match result {
                Ok(summary) => self.terminal_receipt(
                    &running_receipt.id,
                    TodoChildReceiptStatus::Completed,
                    (!summary.trim().is_empty()).then(|| bounded_summary(&summary)),
                    None,
                ),
                Err(error) => self.terminal_receipt(
                    &running_receipt.id,
                    TodoChildReceiptStatus::Failed,
                    None,
                    Some(error),
                ),
            };
            let (settled, last_error) = match terminal {
                Ok(terminal) => terminal,
                Err(error) => {
                    self.fail_persistence(&format!(
                        "constructing terminal Todo child receipt: {error}"
                    ));
                    break;
                }
            };
            if let Err(error) = self.persist_receipt(&settled, None) {
                self.fail_persistence(&format!("persisting terminal Todo child receipt: {error}"));
                break;
            }
            let settled_id = settled.id.clone();
            if let Err(error) = self.commit_settled_receipt(&settled_id, settled, last_error) {
                self.fail_persistence(&format!("committing terminal Todo child receipt: {error}"));
                break;
            }
        }
        let should_restart = {
            let mut state = self.state.lock();
            if !matches!(
                state.status,
                TodoChildStatus::Paused | TodoChildStatus::Stopped | TodoChildStatus::Failed
            ) {
                state.status =
                    last_settled_status(&state.receipts).unwrap_or(TodoChildStatus::Idle);
            }
            state.is_turn_loop_running = false;
            state.active_turn.take();
            !matches!(
                state.status,
                TodoChildStatus::Paused | TodoChildStatus::Stopped | TodoChildStatus::Failed
            ) && !state.message_queue.is_empty()
        };
        if should_restart {
            self.spawn_if_needed();
        }
        if let Err(error) = self.persist_manifest_status().await {
            self.fail_persistence(&format!("persisting final Todo child manifest: {error}"));
        }
    }

    fn terminal_receipt(
        &self,
        receipt_id: &str,
        status: TodoChildReceiptStatus,
        summary: Option<String>,
        error: Option<String>,
    ) -> Result<(TodoChildReceipt, Option<String>)> {
        let mut terminal = self
            .state
            .lock()
            .receipts
            .iter()
            .find(|receipt| receipt.id == receipt_id)
            .cloned()
            .ok_or_else(|| anyhow!("Todo child receipt disappeared before settlement"))?;
        terminal.status = status;
        terminal.summary = summary.or_else(|| error.as_deref().map(bounded_summary));
        Ok((terminal, error.map(|error| bounded_summary(&error))))
    }

    fn commit_settled_receipt(
        &self,
        receipt_id: &str,
        terminal: TodoChildReceipt,
        last_error: Option<String>,
    ) -> Result<()> {
        let mut state = self.state.lock();
        let receipt = state
            .receipts
            .iter_mut()
            .find(|receipt| receipt.id == receipt_id)
            .ok_or_else(|| anyhow!("Todo child receipt disappeared during settlement"))?;
        let status = terminal.status;
        *receipt = terminal;
        state.last_error = last_error;
        if !matches!(
            state.status,
            TodoChildStatus::Paused | TodoChildStatus::Stopped
        ) {
            state.status = match status {
                TodoChildReceiptStatus::Failed => TodoChildStatus::Failed,
                TodoChildReceiptStatus::Queued | TodoChildReceiptStatus::Running => {
                    TodoChildStatus::Running
                }
                TodoChildReceiptStatus::Completed => TodoChildStatus::Idle,
            };
        }
        Ok(())
    }
    fn persist_task_event(&self, event: &TodoChildTaskEvent) -> Result<()> {
        let recorder = self
            .application
            .session()
            .recorder()
            .ok_or_else(|| anyhow!("Todo child recorder is unavailable"))?;
        let expected_revision = task_event_revision(event)
            .checked_sub(1)
            .ok_or_else(|| anyhow!("Todo child task revision must start at one"))?;
        let (tree, token) = recorder.tree_with_append_token()?;
        let (journal_revision, _, _) = replay_task_events(&tree, false)?;
        if journal_revision != expected_revision {
            bail!(
                "Todo child task journal revision changed: expected {expected_revision}, current {journal_revision}"
            );
        }
        recorder.record_custom_entry_durable_checked(
            &token,
            TODO_CHILD_TASK_RECORD,
            event,
            move |tree| {
                let (journal_revision, _, _) = replay_task_events(tree, false)?;
                if journal_revision != expected_revision {
                    bail!(
                        "Todo child task journal revision changed: expected {expected_revision}, current {journal_revision}"
                    );
                }
                Ok(())
            },
        )?;
        Ok(())
    }

    fn persist_recovered_tasks(&self) -> Result<()> {
        loop {
            let Some(task_id) = self
                .state
                .lock()
                .recovered_task_interruptions
                .first()
                .cloned()
            else {
                return Ok(());
            };
            let revision = {
                let state = self.state.lock();
                next_task_revision(state.revision)?
            };
            self.persist_task_event(&TodoChildTaskEvent::Interrupted {
                revision,
                task_id: task_id.clone(),
            })?;
            let mut state = self.state.lock();
            state.revision = revision;
            if state.recovered_task_interruptions.first() == Some(&task_id) {
                state.recovered_task_interruptions.remove(0);
            } else {
                bail!("Todo child recovered-task persistence order changed");
            }
        }
    }

    async fn set_status(&self, status: TodoChildStatus, error: Option<String>) -> Result<()> {
        {
            let mut state = self.state.lock();
            state.status = status;
            state.last_error = error;
        }
        self.persist_manifest_status().await
    }
    fn note_projection_error(&self, error: &anyhow::Error) {
        self.state.lock().last_error = Some(bounded_summary(&format!(
            "Todo child manifest projection is stale: {error}"
        )));
    }

    fn fail_persistence(&self, error: &str) {
        let mut state = self.state.lock();
        if !matches!(
            state.status,
            TodoChildStatus::Paused | TodoChildStatus::Stopped
        ) {
            state.status = TodoChildStatus::Failed;
        }
        state.last_error = Some(bounded_summary(error));
    }

    fn persist_receipt(
        &self,
        receipt: &TodoChildReceipt,
        message: Option<&TodoChildMessage>,
    ) -> Result<()> {
        let recorder = self
            .application
            .session()
            .recorder()
            .ok_or_else(|| anyhow!("Todo child recorder is unavailable"))?;
        let event = PersistedReceiptEvent {
            identity: self.identity.clone(),
            receipt: receipt.clone(),
            message: message.cloned(),
        };
        for _ in 0..8 {
            let (_, token) = recorder.tree_with_append_token()?;
            let identity = self.identity.clone();
            let receipt = receipt.clone();
            let message = message.cloned();
            match recorder.record_custom_entry_durable_checked(
                &token,
                TODO_CHILD_RECEIPT_RECORD,
                &event,
                move |tree| validate_receipt_append(tree, &identity, &receipt, message.as_ref()),
            ) {
                Ok(_) => return Ok(()),
                Err(error)
                    if error.to_string() == "session changed before durable custom append" => {}
                Err(error) => return Err(error),
            }
        }
        bail!("Todo child receipt journal changed repeatedly during durable append")
    }

    fn persist_recovered_receipts(&self) -> Result<()> {
        loop {
            let Some(receipt) = self.state.lock().recovered_receipt_updates.first().cloned() else {
                return Ok(());
            };
            self.persist_receipt(&receipt, None)?;
            let mut state = self.state.lock();
            if state
                .recovered_receipt_updates
                .first()
                .is_some_and(|pending| pending.id == receipt.id)
            {
                state.recovered_receipt_updates.remove(0);
            } else {
                bail!("Todo child recovered-receipt persistence order changed");
            }
        }
    }

    async fn persist_terminal_manifest_status(&self) -> Result<()> {
        let listed = self
            .manifest
            .load()?
            .0
            .entries
            .iter()
            .any(|entry| entry.child_session_id == self.identity.child_session_id);
        if !listed {
            return Ok(());
        }
        self.persist_manifest_status().await
    }

    async fn persist_manifest_status(&self) -> Result<()> {
        let status = self.aggregate_status();
        let mut entry = self.manifest_entry.lock().clone();
        entry.status = status;
        self.manifest.update_entry(entry.clone()).await?;
        *self.manifest_entry.lock() = entry;
        Ok(())
    }
}

impl ManifestStorage {
    fn load(&self) -> Result<(TodoChildManifest, Option<crate::fs::FileIdentity>)> {
        let relative = Path::new(TODO_MANIFEST_FILENAME);
        let metadata = match self.child_root.metadata(relative) {
            Ok(metadata) => Some(metadata),
            Err(error) if is_missing(&error) => None,
            Err(error) => return Err(error).context("reading Todo child manifest identity"),
        };
        let Some(metadata) = metadata else {
            return Ok((
                TodoChildManifest {
                    version: TODO_CHILD_MANIFEST_VERSION,
                    parent_session_id: self.parent_session_id.clone(),
                    entries: Vec::new(),
                },
                None,
            ));
        };
        let bytes = self
            .child_root
            .read_bounded(relative, TODO_CHILD_MANIFEST_MAX_BYTES)
            .context("reading Todo child manifest")?;
        let manifest: TodoChildManifest =
            serde_json::from_slice(&bytes).context("decoding Todo child manifest")?;
        if manifest.version != TODO_CHILD_MANIFEST_VERSION {
            bail!(
                "unsupported Todo child manifest version {}; expected {TODO_CHILD_MANIFEST_VERSION}",
                manifest.version
            );
        }
        if manifest.parent_session_id != self.parent_session_id {
            bail!("Todo child manifest parent identity mismatch");
        }
        Ok((manifest, Some(metadata.identity)))
    }

    async fn insert_entry(&self, entry: TodoChildManifestEntry) -> Result<()> {
        self.mutate(move |manifest| {
            if manifest
                .entries
                .iter()
                .any(|existing| existing.child_session_id == entry.child_session_id)
            {
                bail!("Todo child manifest entry already exists");
            }
            manifest.entries.push(entry.clone());
            manifest
                .entries
                .sort_by(|left, right| left.created_at.cmp(&right.created_at));
            Ok(())
        })
        .await
    }

    async fn update_entry(&self, entry: TodoChildManifestEntry) -> Result<()> {
        self.mutate(move |manifest| {
            let existing = manifest
                .entries
                .iter_mut()
                .find(|existing| existing.child_session_id == entry.child_session_id)
                .ok_or_else(|| anyhow!("Todo child manifest entry is missing"))?;
            *existing = entry.clone();
            Ok(())
        })
        .await
    }

    async fn remove_entry(&self, child_session_id: &str) -> Result<()> {
        let child_session_id = child_session_id.to_owned();
        self.mutate(move |manifest| {
            manifest
                .entries
                .retain(|entry| entry.child_session_id != child_session_id);
            Ok(())
        })
        .await
    }
    async fn mutate<F>(&self, mut mutation: F) -> Result<()>
    where
        F: FnMut(&mut TodoChildManifest) -> Result<()>,
    {
        let _manifest = self.gate.lock().await;
        for _ in 0..8 {
            let (mut manifest, expected) = self.load()?;
            mutation(&mut manifest)?;
            let bytes =
                serde_json::to_vec_pretty(&manifest).context("serializing Todo child manifest")?;
            match self.child_root.replace_file(Replacement {
                relative: PathBuf::from(TODO_MANIFEST_FILENAME),
                expected,
                bytes,
                mode: FileMode::Private,
            }) {
                Ok(_) => return Ok(()),
                Err(FsError::ReplacementConflict | FsError::IdentityChanged) => continue,
                Err(error) => return Err(error).context("persisting Todo child manifest"),
            }
        }
        bail!("Todo child manifest changed concurrently too many times")
    }
}

fn recover_tasks(session: &Session) -> Result<(u64, Vec<TodoChildTask>, Vec<String>)> {
    let recorder = session
        .recorder()
        .ok_or_else(|| anyhow!("Todo child recorder is unavailable"))?;
    let tree = recorder.tree()?;
    replay_task_events(&tree, true)
}

fn replay_task_events(
    tree: &crate::SessionTree,
    recover_running: bool,
) -> Result<(u64, Vec<TodoChildTask>, Vec<String>)> {
    let mut revision = 0_u64;
    let mut tasks = Vec::<TodoChildTask>::new();
    for entry in tree.branch(None) {
        if entry.custom_type.as_deref() != Some(TODO_CHILD_TASK_RECORD) {
            continue;
        }
        let data = entry
            .data
            .clone()
            .ok_or_else(|| anyhow!("Todo child task entry {} has no data", entry.id))?;
        let event: TodoChildTaskEvent = serde_json::from_value(data)
            .with_context(|| format!("decoding durable Todo child task entry {}", entry.id))?;
        let expected = next_task_revision(revision)?;
        if task_event_revision(&event) != expected {
            bail!(
                "invalid Todo child task journal revision: expected {expected}, found {}",
                task_event_revision(&event)
            );
        }
        apply_task_event(&mut tasks, &event)?;
        revision = expected;
    }
    let mut recovered = Vec::new();
    if recover_running {
        for task in &mut tasks {
            if task.status == TodoChildTaskStatus::Running {
                task.status = TodoChildTaskStatus::Interrupted;
                task.summary = Some(
                    "execution outcome is unknown after restart; task was not replayed".to_owned(),
                );
                recovered.push(task.id.clone());
            }
        }
    }
    Ok((revision, tasks, recovered))
}

fn apply_task_event(tasks: &mut Vec<TodoChildTask>, event: &TodoChildTaskEvent) -> Result<()> {
    match event {
        TodoChildTaskEvent::Added { task, .. } => {
            if task.id.trim().is_empty() || task.content.trim().is_empty() {
                bail!("invalid Added Todo child task event");
            }
            if task.status != TodoChildTaskStatus::Pending || task.summary.is_some() {
                bail!("Added Todo child task must be pending without a summary");
            }
            if tasks.iter().any(|existing| existing.id == task.id) {
                bail!("duplicate Todo child task id {:?}", task.id);
            }
            let known = tasks
                .iter()
                .map(|task| task.id.as_str())
                .collect::<HashSet<_>>();
            let mut dependencies = HashSet::new();
            for dependency in &task.depends_on {
                if !dependencies.insert(dependency.as_str()) {
                    bail!("Todo child task dependencies must be unique");
                }
                if !known.contains(dependency.as_str()) {
                    bail!("Todo child task dependency {dependency:?} does not exist");
                }
            }
            tasks.push(task.clone());
        }
        TodoChildTaskEvent::Started { task_id, .. } => {
            let task = task_by_id_mut(tasks, task_id)?;
            if task.status != TodoChildTaskStatus::Pending {
                bail!(
                    "Todo child task {task_id:?} cannot start from {:?}",
                    task.status
                );
            }
            task.status = TodoChildTaskStatus::Running;
            task.summary = None;
        }
        TodoChildTaskEvent::Completed {
            task_id, summary, ..
        } => {
            let task = task_by_id_mut(tasks, task_id)?;
            if task.status != TodoChildTaskStatus::Running {
                bail!(
                    "Todo child task {task_id:?} cannot complete from {:?}",
                    task.status
                );
            }
            task.status = TodoChildTaskStatus::Completed;
            task.summary = summary.clone();
        }
        TodoChildTaskEvent::Failed {
            task_id, summary, ..
        } => {
            let task = task_by_id_mut(tasks, task_id)?;
            if task.status != TodoChildTaskStatus::Running {
                bail!(
                    "Todo child task {task_id:?} cannot fail from {:?}",
                    task.status
                );
            }
            task.status = TodoChildTaskStatus::Failed;
            task.summary = Some(summary.clone());
        }
        TodoChildTaskEvent::Interrupted { task_id, .. } => {
            let task = task_by_id_mut(tasks, task_id)?;
            if task.status != TodoChildTaskStatus::Running {
                bail!(
                    "Todo child task {task_id:?} cannot be interrupted from {:?}",
                    task.status
                );
            }
            task.status = TodoChildTaskStatus::Interrupted;
            task.summary =
                Some("execution was interrupted before its outcome was known".to_owned());
        }
    }
    Ok(())
}

fn task_by_id_mut<'a>(
    tasks: &'a mut [TodoChildTask],
    task_id: &str,
) -> Result<&'a mut TodoChildTask> {
    tasks
        .iter_mut()
        .find(|task| task.id == task_id)
        .ok_or_else(|| anyhow!("Todo child task {task_id:?} does not exist"))
}

fn task_event_revision(event: &TodoChildTaskEvent) -> u64 {
    match event {
        TodoChildTaskEvent::Added { revision, .. }
        | TodoChildTaskEvent::Started { revision, .. }
        | TodoChildTaskEvent::Completed { revision, .. }
        | TodoChildTaskEvent::Failed { revision, .. }
        | TodoChildTaskEvent::Interrupted { revision, .. } => *revision,
    }
}

fn next_task_revision(revision: u64) -> Result<u64> {
    revision
        .checked_add(1)
        .ok_or_else(|| anyhow!("Todo child task revision overflow"))
}

fn normalized_task_field(value: Option<String>, field: &str) -> Result<Option<String>> {
    let Some(value) = value else { return Ok(None) };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > 256 {
        bail!("Todo child task {field} exceeds 256 bytes");
    }
    if value.chars().any(char::is_control) {
        bail!("Todo child task {field} contains control characters");
    }
    Ok(Some(value.to_owned()))
}
fn project_transcript_entry(entry: &crate::SessionEntry) -> Option<TodoChildDetailTranscriptEntry> {
    let message = entry.message.as_ref()?;
    let (role, actor, text, at_ms, interrupted) = match message {
        pi_ai::Message::User(message) => (
            "user".to_owned(),
            None,
            detail_content_text(&message.content),
            Some(message.timestamp),
            false,
        ),
        pi_ai::Message::Assistant(message) => (
            "assistant".to_owned(),
            Some("Todo".to_owned()),
            message.text(),
            Some(message.timestamp),
            message.stop_reason == pi_ai::StopReason::Aborted,
        ),
        pi_ai::Message::ToolResult(message) => (
            "tool_result".to_owned(),
            Some(message.tool_name.clone()),
            detail_content_text(&message.content),
            Some(message.timestamp),
            false,
        ),
        pi_ai::Message::BashExecution(message) => (
            "bash".to_owned(),
            Some("bash".to_owned()),
            format!("$ {}\n{}", message.command, message.output),
            Some(message.timestamp),
            message.cancelled,
        ),
        pi_ai::Message::Custom(message) => (
            "custom".to_owned(),
            Some(message.custom_type.clone()),
            match &message.content {
                pi_ai::CustomMessageContent::Text(text) => text.clone(),
                pi_ai::CustomMessageContent::Blocks(blocks) => detail_content_text(blocks),
            },
            Some(message.timestamp),
            false,
        ),
        pi_ai::Message::BranchSummary(message) => (
            "branch_summary".to_owned(),
            None,
            message.summary.clone(),
            Some(message.timestamp),
            false,
        ),
        pi_ai::Message::CompactionSummary(message) => (
            "compaction_summary".to_owned(),
            None,
            message.summary.clone(),
            Some(message.timestamp),
            false,
        ),
    };
    Some(TodoChildDetailTranscriptEntry {
        id: entry.id.clone(),
        role,
        actor,
        text: bounded_detail_text(&text, 2_000),
        at_ms,
        interrupted,
    })
}

fn detail_content_text(content: &[pi_ai::ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            pi_ai::ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn bounded_detail_text(text: &str, max_chars: usize) -> String {
    let redacted = crate::redact_value(&serde_json::Value::String(text.to_owned()))
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if redacted.chars().count() <= max_chars {
        return redacted;
    }
    let mut output = redacted
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    output.push('…');
    output
}
fn validate_receipt_append(
    tree: &crate::SessionTree,
    identity: &TodoChildIdentity,
    receipt: &TodoChildReceipt,
    message: Option<&TodoChildMessage>,
) -> Result<()> {
    if receipt.parent_session_id != identity.parent_session_id
        || receipt.child_session_id != identity.child_session_id
        || message.is_some_and(|message| {
            message.parent_session_id != identity.parent_session_id
                || message.child_session_id != identity.child_session_id
                || message.id != receipt.message_id
        })
    {
        bail!("Todo child receipt identity mismatch");
    }
    let mut current = None;
    let mut body_persisted = false;
    for entry in tree.branch(None) {
        if entry.custom_type.as_deref() != Some(TODO_CHILD_RECEIPT_RECORD) {
            continue;
        }
        let Some(data) = entry.data.as_ref() else {
            continue;
        };
        let event: PersistedReceiptEvent = serde_json::from_value(data.clone())
            .context("decoding durable Todo child receipt during append")?;
        if event.identity != *identity || event.receipt.id != receipt.id {
            continue;
        }
        body_persisted |= event
            .message
            .as_ref()
            .is_some_and(|message| message.id == receipt.message_id);
        current = Some(event.receipt);
    }
    match current {
        None => {
            if receipt.status != TodoChildReceiptStatus::Queued || message.is_none() {
                bail!("Todo child receipt must be durably accepted before status changes");
            }
        }
        Some(current) => {
            if current.message_id != receipt.message_id
                || current.parent_session_id != receipt.parent_session_id
                || current.child_session_id != receipt.child_session_id
                || current.accepted_at != receipt.accepted_at
            {
                bail!("Todo child receipt immutable fields changed");
            }
            let valid = match current.status {
                TodoChildReceiptStatus::Queued => matches!(
                    receipt.status,
                    TodoChildReceiptStatus::Queued
                        | TodoChildReceiptStatus::Running
                        | TodoChildReceiptStatus::Failed
                ),
                TodoChildReceiptStatus::Running => matches!(
                    receipt.status,
                    TodoChildReceiptStatus::Running
                        | TodoChildReceiptStatus::Completed
                        | TodoChildReceiptStatus::Failed
                ),
                TodoChildReceiptStatus::Completed => {
                    receipt.status == TodoChildReceiptStatus::Completed
                }
                TodoChildReceiptStatus::Failed => receipt.status == TodoChildReceiptStatus::Failed,
            };
            if !valid {
                bail!(
                    "invalid Todo child receipt transition from {:?} to {:?}",
                    current.status,
                    receipt.status
                );
            }
            if message.is_none()
                && !body_persisted
                && receipt.status != TodoChildReceiptStatus::Failed
            {
                bail!("durable Todo child message body is unavailable");
            }
        }
    }
    Ok(())
}

fn recover_receipts(
    session: &Session,
    identity: &TodoChildIdentity,
) -> Result<(
    VecDeque<TodoChildMessage>,
    VecDeque<TodoChildReceipt>,
    Vec<TodoChildReceipt>,
)> {
    let Some(recorder) = session.recorder() else {
        return Ok((VecDeque::new(), VecDeque::new(), Vec::new()));
    };
    let tree = recorder.tree()?;
    let mut receipts = VecDeque::<TodoChildReceipt>::new();
    let mut bodies = HashMap::<String, TodoChildMessage>::new();
    for entry in tree.branch(None) {
        if entry.custom_type.as_deref() != Some(TODO_CHILD_RECEIPT_RECORD) {
            continue;
        }
        let Some(data) = entry.data.as_ref() else {
            continue;
        };
        let event: PersistedReceiptEvent =
            serde_json::from_value(data.clone()).context("decoding durable Todo child receipt")?;
        if event.identity != *identity {
            continue;
        }
        if let Some(message) = event.message {
            bodies.insert(message.id.clone(), message);
        }
        if let Some(existing) = receipts
            .iter_mut()
            .find(|receipt| receipt.id == event.receipt.id)
        {
            *existing = event.receipt;
        } else {
            receipts.push_back(event.receipt);
        }
    }
    let mut recovered_updates = Vec::new();
    let mut message_queue = VecDeque::new();
    for receipt in &mut receipts {
        let original_status = receipt.status;
        match receipt.status {
            TodoChildReceiptStatus::Queued => {
                if let Some(mut message) = bodies.remove(&receipt.message_id) {
                    message.status = TodoChildReceiptStatus::Queued;
                    if message_queue.len() < TODO_CHILD_MESSAGE_QUEUE_CAP {
                        message_queue.push_back(message);
                    } else {
                        receipt.status = TodoChildReceiptStatus::Failed;
                        receipt.summary =
                            Some("message was not recovered because the queue is full".to_owned());
                    }
                } else {
                    receipt.status = TodoChildReceiptStatus::Failed;
                    receipt.summary =
                        Some("durable Todo child message body is unavailable".to_owned());
                }
            }
            TodoChildReceiptStatus::Running => {
                receipt.status = TodoChildReceiptStatus::Failed;
                receipt.summary = Some(
                    "execution outcome is unknown after restart; message was not replayed"
                        .to_owned(),
                );
            }
            TodoChildReceiptStatus::Completed | TodoChildReceiptStatus::Failed => {}
        }
        if receipt.status != original_status {
            recovered_updates.push(receipt.clone());
        }
    }
    trim_receipts(&mut receipts);
    Ok((message_queue, receipts, recovered_updates))
}

fn parent_identity(parent: &Session, requested: Option<&str>) -> Result<String> {
    let (actual, _) = parent
        .recorder_info()
        .ok_or_else(|| anyhow!("parent session recording is unavailable"))?;
    crate::session_store::validate_session_id(&actual)
        .context("invalid active parent session id")?;
    if let Some(requested) = requested {
        crate::session_store::validate_session_id(requested)
            .context("invalid requested parent session id")?;
        if requested != actual {
            bail!("todo child parent session identity mismatch");
        }
    }
    Ok(actual)
}

fn validate_child_session_id(child_session_id: &str) -> Result<()> {
    crate::session_store::validate_session_id(child_session_id)
        .context("invalid Todo child session id")?;
    Uuid::parse_str(child_session_id)
        .map(|_| ())
        .map_err(|_| anyhow!("Todo child session id must be a UUID"))
}

fn child_id_for_request(
    parent_session_id: &str,
    request: &TodoChildCreateRequest,
) -> Result<String> {
    match request.idempotency_key.as_deref() {
        None => Ok(Uuid::now_v7().to_string()),
        Some(key) => {
            let key = key.trim();
            if key.is_empty() {
                bail!("Todo child idempotency key must not be blank");
            }
            if key.len() > 1024 {
                bail!("Todo child idempotency key exceeds 1024 bytes");
            }
            let digest =
                Sha256::digest([parent_session_id.as_bytes(), b"\0", key.as_bytes()].concat());
            let mut bytes = [0_u8; 16];
            bytes.copy_from_slice(&digest[..16]);
            bytes[6] = (bytes[6] & 0x0f) | 0x50;
            bytes[8] = (bytes[8] & 0x3f) | 0x80;
            Ok(Uuid::from_bytes(bytes).to_string())
        }
    }
}

async fn child_session_options(
    parent: &Session,
    durable: &TodoChildRuntimeDerivation,
) -> Result<(SessionOptions, crate::RetrySettings, Option<String>)> {
    let snapshot = parent.child_session_options_snapshot();
    let model = pi_ai::get_model(&durable.model_provider, &durable.model_id).ok_or_else(|| {
        anyhow!(
            "Todo child model {}/{} is unavailable during recovery",
            durable.model_provider,
            durable.model_id
        )
    })?;
    let api_key = match snapshot.auth_resolver.as_ref() {
        Some(resolver) => resolver(model.clone()).await?.api_key,
        None => snapshot.api_key.clone(),
    };
    let mut stream_options = snapshot.stream_options;
    stream_options.stream.session_id = None;
    Ok((
        SessionOptions {
            model,
            cwd: snapshot.workspace.cwd().to_path_buf(),
            system_prompt: todo_child_system_prompt(),
            thinking_level: durable.thinking_level,
            api_key,
            compaction: parent.compaction_settings(),
            stream_options,
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(snapshot.stream_fn),
            auth_resolver: snapshot.auth_resolver,
            auth_manager: snapshot.auth_manager,
        },
        durable.retry_settings.clone(),
        durable.retry_role.clone(),
    ))
}

fn todo_child_derivation_from_tree(tree: &crate::SessionTree) -> Result<TodoChildDerivation> {
    tree.branch(None)
        .into_iter()
        .rev()
        .find(|entry| entry.custom_type.as_deref() == Some(TODO_CHILD_DERIVATION_RECORD))
        .and_then(|entry| entry.data.clone())
        .map(serde_json::from_value::<TodoChildDerivation>)
        .transpose()
        .context("decoding durable Todo child derivation")?
        .ok_or_else(|| anyhow!("unregistered Todo child has no derivation metadata"))
}

fn manifest_entry_from_tree(
    tree: &crate::SessionTree,
    parent_session_id: &str,
    child_session_id: &str,
) -> Result<TodoChildManifestEntry> {
    let derivation = todo_child_derivation_from_tree(tree)?;
    if derivation.identity.parent_session_id != parent_session_id
        || derivation.identity.child_session_id != child_session_id
        || derivation.kind != TodoChildKind::Todo
    {
        bail!("unregistered Todo child derivation identity mismatch");
    }
    Ok(TodoChildManifestEntry {
        child_session_id: child_session_id.to_owned(),
        label: None,
        created_at: now_millis(),
        status: TodoChildStatus::Created,
        parent_entry_id: derivation.parent_entry_id,
        parent_revision: derivation.parent_revision,
        kind: derivation.kind,
    })
}

fn aggregate_status(state: &TodoSupervisorState) -> TodoChildStatus {
    if matches!(
        state.status,
        TodoChildStatus::Paused | TodoChildStatus::Stopped | TodoChildStatus::Failed
    ) {
        return state.status;
    }
    if state.is_turn_loop_running
        || !state.message_queue.is_empty()
        || state
            .receipts
            .iter()
            .any(|receipt| receipt.status == TodoChildReceiptStatus::Running)
        || state
            .tasks
            .iter()
            .any(|task| task.status == TodoChildTaskStatus::Running)
    {
        return TodoChildStatus::Running;
    }
    state.status
}

fn last_settled_status(receipts: &VecDeque<TodoChildReceipt>) -> Option<TodoChildStatus> {
    receipts
        .iter()
        .rev()
        .find_map(|receipt| match receipt.status {
            TodoChildReceiptStatus::Completed => Some(TodoChildStatus::Idle),
            TodoChildReceiptStatus::Failed => Some(TodoChildStatus::Failed),
            TodoChildReceiptStatus::Queued | TodoChildReceiptStatus::Running => None,
        })
}

fn trim_receipts(receipts: &mut VecDeque<TodoChildReceipt>) {
    while receipts.len() > TODO_CHILD_RECEIPT_CAP {
        let Some(index) = receipts.iter().position(|receipt| {
            matches!(
                receipt.status,
                TodoChildReceiptStatus::Completed | TodoChildReceiptStatus::Failed
            )
        }) else {
            break;
        };
        receipts.remove(index);
    }
}

fn child_payload_files(
    child_root: &CapabilityRootLease,
    session_relative: &Path,
    staging_relative: &Path,
) -> Result<Vec<(PathBuf, PathBuf)>> {
    let primary_name = session_relative
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("Todo child transcript name is invalid"))?
        .to_owned();
    let mut originals = vec![session_relative.to_path_buf()];
    let mut collect = |names: Vec<std::ffi::OsString>, originals: &mut Vec<PathBuf>| {
        for name in names {
            if name.as_encoded_bytes() != primary_name.as_bytes()
                && is_archive_sidecar(&primary_name, &name)
            {
                originals.push(PathBuf::from(name));
            }
        }
    };
    match child_root.open_directory(Path::new("")) {
        Ok(directory) => {
            let names = directory
                .read_dir_names()
                .map_err(|error| anyhow::Error::new(error).context("enumerating Todo child payload"))?;
            collect(names, &mut originals);
        }
        Err(error) if is_missing(&error) => {}
        Err(error) => {
            return Err(anyhow::Error::new(error).context("enumerating Todo child payload"))
        }
    }
    match child_root.open_directory(staging_relative) {
        Ok(directory) => {
            let names = directory.read_dir_names().map_err(|error| {
                anyhow::Error::new(error).context("enumerating staged Todo child payload")
            })?;
            collect(names, &mut originals);
        }
        Err(error) if is_missing(&error) => {}
        Err(error) => {
            return Err(anyhow::Error::new(error).context("enumerating staged Todo child payload"))
        }
    }
    Ok(originals
        .into_iter()
        .map(|original| {
            let staged = staging_relative.join(original.file_name().expect("file name"));
            (original, staged)
        })
        .collect())
}

/// Rolls every staged payload back to its live location: the transcript,
/// sidecars, and the orchestration tree.
fn restore_staged_child_payload(
    child_root: &CapabilityRootLease,
    staged_files: &[(PathBuf, PathBuf)],
    staged_orchestration: &Path,
    orchestration_relative: &Path,
) -> Result<()> {
    for (original, staged) in staged_files {
        match child_root.rename(staged, original) {
            Ok(()) => {}
            Err(error) if is_missing(&error) => {}
            Err(error) => {
                return Err(anyhow::Error::new(error).context(format!(
                    "restoring the staged Todo child payload at {}",
                    staged.display()
                )))
            }
        }
    }
    match child_root.rename(staged_orchestration, orchestration_relative) {
        Ok(()) => {}
        Err(error) if is_missing(&error) => {}
        Err(error) => {
            return Err(anyhow::Error::new(error).context(format!(
                "restoring the staged Todo child orchestration state at {}",
                staged_orchestration.display()
            )))
        }
    }
    Ok(())
}
fn is_missing(error: &FsError) -> bool {
    matches!(
        error,
        FsError::System {
            kind: std::io::ErrorKind::NotFound,
            ..
        }
    )
}

fn queued_receipts(state: &TodoSupervisorState, operation: &str) -> Result<()> {
    if state.message_queue.iter().all(|message| {
        state
            .receipts
            .iter()
            .any(|receipt| receipt.message_id == message.id)
    }) {
        return Ok(());
    }
    bail!("queued Todo child receipt is unavailable before {operation}")
}
fn todo_child_system_prompt() -> String {
    "You are an independent durable Todo child application. Execute user-directed background work with your own complete orchestration runtime, Todo DAG, subagents, jobs, processes, and IRC. Report concise outcomes in the final assistant message. The retained parent branch is context, but lifecycle and execution ownership are isolated to this child.".to_owned()
}

fn bounded_summary(text: &str) -> String {
    const MAX_CHARS: usize = 512;
    let mut chars = text.chars();
    let mut summary = chars.by_ref().take(MAX_CHARS).collect::<String>();
    if chars.next().is_some() {
        summary.push('…');
    }
    summary
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
