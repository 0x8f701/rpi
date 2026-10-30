use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use base64::Engine as _;
use parking_lot::Mutex;
use tokio::sync::{Notify, broadcast};

use super::backend::{self, BackendController};
use super::log::ProcessLog;
use super::{
    DEFAULT_LOG_READ_BYTES, MAX_PROCESS_LABEL_BYTES, ProcessEvent, ProcessId, ProcessInfo,
    ProcessKey, ProcessLogs, ProcessManagerConfig, ProcessOwnerId, ProcessSignal, ProcessSpawnSpec,
    ProcessState, ProcessTerminalSize, authorize_spawn_with_auth,
};

const EVENT_CHANNEL_CAPACITY: usize = 512;
const PROCESS_EXIT_OUTPUT_GRACE: Duration = Duration::from_millis(250);

#[derive(Clone)]
pub struct ProcessManager {
    inner: Arc<ManagerInner>,
}

impl std::fmt::Debug for ProcessManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProcessManager")
            .field("session_count", &self.inner.sessions.lock().len())
            .field("max_processes", &self.inner.config.lock().max_processes)
            .field("max_output_bytes", &self.inner.config.lock().max_output_bytes)
            .finish()
    }
}

struct ManagerInner {
    config: Mutex<ProcessManagerConfig>,
    publication: OnceLock<Weak<crate::application::generation::ApplicationPublication>>,
    auth_manager: OnceLock<Arc<crate::AuthManager>>,
    sessions: Mutex<HashMap<ProcessId, Arc<ProcessSession>>>,
    /// Exit snapshots kept after explicit stop removes the live row.
    settled: Mutex<Vec<ProcessInfo>>,
    events: broadcast::Sender<ProcessEvent>,
    shutdown: AtomicBool,
    idle_reaper_started: AtomicBool,
    idle_changed: Notify,
}

struct ProcessSession {
    id: ProcessId,
    owner_id: ProcessOwnerId,
    label: Option<String>,
    tty: bool,
    started_at_ms: u64,
    runtime: Mutex<ProcessRuntime>,
    log: Mutex<ProcessLog>,
    controller: Mutex<Option<Arc<BackendController>>>,
    changed: Notify,
    last_activity_ms: AtomicU64,
}

struct ProcessRuntime {
    state: ProcessState,
    terminal_override: Option<ProcessState>,
    pid: Option<u32>,
    exited_at_ms: Option<u64>,
    exit_code: Option<i32>,
}

impl ProcessManager {
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(ProcessManagerConfig::default())
    }

    #[must_use]
    pub fn with_config(config: ProcessManagerConfig) -> Self {
        assert!(
            config.max_processes > 0,
            "max_processes must be greater than zero"
        );
        assert!(
            config.max_output_bytes > 0,
            "max_output_bytes must be greater than zero"
        );
        assert!(
            config.idle_timeout.is_none() || !config.idle_scan_interval.is_zero(),
            "idle_scan_interval must be greater than zero when idle expiration is enabled"
        );
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let inner = Arc::new(ManagerInner {
            publication: OnceLock::new(),
            auth_manager: OnceLock::new(),
            config: Mutex::new(config),
            sessions: Mutex::new(HashMap::new()),
            settled: Mutex::new(Vec::new()),
            events,
            shutdown: AtomicBool::new(false),
            idle_reaper_started: AtomicBool::new(false),
            idle_changed: Notify::new(),
        });
        spawn_idle_reaper(&inner);
        Self { inner }
    }

    pub(crate) fn bind_publication(
        &self,
        publication: &Arc<crate::application::generation::ApplicationPublication>,
    ) -> Result<()> {
        self.inner
            .publication
            .set(Arc::downgrade(publication))
            .map_err(|_| anyhow!("process manager publication is already bound"))
    }

    pub(crate) fn bind_auth_manager(&self, auth_manager: Arc<crate::AuthManager>) -> Result<()> {
        if let Some(existing) = self.inner.auth_manager.get() {
            return if Arc::ptr_eq(existing, &auth_manager) {
                Ok(())
            } else {
                Err(anyhow!(
                    "process manager authentication owner is already bound"
                ))
            };
        }
        self.inner
            .auth_manager
            .set(auth_manager)
            .map_err(|_| anyhow!("process manager authentication owner is already bound"))
    }

    pub fn replace_config(&self, config: ProcessManagerConfig) -> Result<()> {
        if config.max_processes == 0 {
            bail!("max_processes must be greater than zero");
        }
        if config.max_output_bytes == 0 {
            bail!("max_output_bytes must be greater than zero");
        }
        if config.idle_timeout.is_some() && config.idle_scan_interval.is_zero() {
            bail!("idle_scan_interval must be greater than zero when idle expiration is enabled");
        }
        *self.inner.config.lock() = config;
        spawn_idle_reaper(&self.inner);
        self.inner.idle_changed.notify_waiters();
        Ok(())
    }

    fn capture_generation(
        &self,
    ) -> Result<Arc<crate::application::generation::ApplicationGeneration>> {
        self.inner
            .publication
            .get()
            .and_then(Weak::upgrade)
            .map(|publication| publication.capture())
            .ok_or_else(|| anyhow!("process manager generation is unavailable"))
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<ProcessEvent> {
        self.inner.events.subscribe()
    }

    pub async fn spawn(
        &self,
        owner_id: ProcessOwnerId,
        spec: ProcessSpawnSpec,
    ) -> Result<ProcessInfo> {
        validate_spawn_spec(&spec, &self.inner.config.lock())?;
        if self.inner.shutdown.load(Ordering::Acquire) {
            bail!("process manager is shutting down");
        }
        spawn_idle_reaper(&self.inner);

        let generation = self.capture_generation()?;
        let authorized = authorize_spawn_with_auth(
            generation.clone(),
            self.inner.auth_manager.get().map(Arc::as_ref),
            crate::sandbox::RawSpawnRequest {
                authorization: crate::sandbox::SpawnAuthorization::Captured(
                    generation.sandbox().clone(),
                ),
                family: if spec.tty {
                    pi_sandbox::SpawnFamily::ManagedPtyProcess
                } else {
                    pi_sandbox::SpawnFamily::ManagedPipeProcess
                },
                audience: crate::sandbox::CredentialAudience::new_spawn_family(if spec.tty {
                    pi_sandbox::SpawnFamily::ManagedPtyProcess
                } else {
                    pi_sandbox::SpawnFamily::ManagedPipeProcess
                })?,
                argv: spec.argv.iter().cloned().map(Into::into).collect(),
                cwd: spec.cwd.clone(),
                requested_env: spec
                    .env
                    .iter()
                    .map(|(name, value)| {
                        (
                            std::ffi::OsString::from(name),
                            value.clone().map(Into::into),
                        )
                    })
                    .collect(),
                requested_grants: BTreeSet::new(),
                stdio: if spec.tty {
                    pi_sandbox::StdioMode::Pty
                } else {
                    pi_sandbox::StdioMode::Merged
                },
                terminal_size: spec
                    .terminal_size
                    .map(backend::sandbox_terminal_size)
                    .transpose()?,
                timeout_ms: spec.timeout_ms,
                target_fd_mappings: Vec::new(),
                #[cfg(unix)]
                owned_target_fd_mappings: Vec::new(),
                #[cfg(unix)]
                mount_sources: Vec::new(),
                #[cfg(unix)]
                retained_executable: None,
                #[cfg(unix)]
                retained_working_directory: None,
            },
        )
        .await?;
        let output_bytes = spec
            .output_bytes
            .unwrap_or(self.inner.config.lock().max_output_bytes);
        let id = ProcessId::generate();
        let started_at_ms = now_ms();
        let session = Arc::new(ProcessSession {
            id: id.clone(),
            owner_id,
            label: spec.label.clone(),
            tty: spec.tty,
            started_at_ms,
            runtime: Mutex::new(ProcessRuntime {
                state: ProcessState::Starting,
                terminal_override: None,
                pid: None,
                exited_at_ms: None,
                exit_code: None,
            }),
            log: Mutex::new(ProcessLog::new(output_bytes)),
            controller: Mutex::new(None),
            changed: Notify::new(),
            last_activity_ms: AtomicU64::new(started_at_ms),
        });
        {
            let mut sessions = self.inner.sessions.lock();
            let active = sessions
                .values()
                .filter(|session| !session.runtime.lock().state.is_terminal())
                .count();
            if active >= self.inner.config.lock().max_processes {
                bail!(
                    "process limit reached (maximum {})",
                    self.inner.config.lock().max_processes
                );
            }
            sessions.insert(id.clone(), session.clone());
        }

        let backend = match backend::spawn(authorized).await {
            Ok(backend) => backend,
            Err(error) => {
                self.inner.sessions.lock().remove(&id);
                return Err(error);
            }
        };
        {
            let mut runtime = session.runtime.lock();
            runtime.pid = backend.pid;
            runtime.state = ProcessState::Running;
        }
        *session.controller.lock() = Some(backend.controller.clone());
        let info = session.info();
        let _ = self.inner.events.send(ProcessEvent::ProcessStarted {
            process: info.clone(),
        });
        session.changed.notify_waiters();

        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(supervise(
            weak,
            session,
            backend.controller,
            backend.output_rx,
            backend.exit_rx,
            spec.timeout_ms.map(Duration::from_millis),
        ));
        Ok(info)
    }

    pub fn list(&self, owner_id: &ProcessOwnerId) -> Vec<ProcessInfo> {
        let mut processes = self
            .inner
            .sessions
            .lock()
            .values()
            .filter(|session| &session.owner_id == owner_id)
            .map(|session| session.info())
            .collect::<Vec<_>>();
        processes.sort_by(|left, right| {
            left.started_at_ms
                .cmp(&right.started_at_ms)
                .then_with(|| left.id.as_str().cmp(right.id.as_str()))
        });
        processes
    }

    pub fn describe(&self, owner_id: &ProcessOwnerId, id: &ProcessId) -> Result<ProcessInfo> {
        Ok(self.session(owner_id, id)?.info())
    }

    pub async fn logs(
        &self,
        owner_id: &ProcessOwnerId,
        id: &ProcessId,
        cursor: u64,
        max_bytes: Option<usize>,
        follow: bool,
        timeout: Option<Duration>,
    ) -> Result<ProcessLogs> {
        let session = self.session(owner_id, id)?;
        session.touch();
        let max_bytes = max_bytes.unwrap_or(DEFAULT_LOG_READ_BYTES);
        let read = || {
            let terminal = session.runtime.lock().state.is_terminal();
            session.log.lock().read(cursor, max_bytes, terminal)
        };
        let initial = read();
        if !follow || !initial.chunks.is_empty() || initial.eof {
            return Ok(initial);
        }

        let notified = session.changed.notified();
        let terminal = session.runtime.lock().state.is_terminal();
        let cursor_advanced = session.log.lock().read(cursor, 1, terminal).cursor > cursor;
        if !terminal && !cursor_advanced {
            match timeout {
                Some(timeout) => {
                    let _ = tokio::time::timeout(timeout, notified).await;
                }
                None => notified.await,
            }
        }
        Ok(read())
    }

    pub async fn write(
        &self,
        owner_id: &ProcessOwnerId,
        id: &ProcessId,
        bytes: Vec<u8>,
        close_stdin: bool,
    ) -> Result<()> {
        if bytes.is_empty() && !close_stdin {
            bail!("write requires bytes, close_stdin, or both");
        }
        let session = self.session(owner_id, id)?;
        session.require_active()?;
        session.touch();
        let controller = session.controller()?;
        if !bytes.is_empty() {
            controller.write(bytes).await?;
        }
        if close_stdin {
            controller.close_stdin();
        }
        Ok(())
    }

    pub async fn send_keys(
        &self,
        owner_id: &ProcessOwnerId,
        id: &ProcessId,
        keys: &[ProcessKey],
    ) -> Result<()> {
        let capacity = keys.iter().map(|key| key.bytes().len()).sum();
        let mut bytes = Vec::with_capacity(capacity);
        for key in keys {
            bytes.extend_from_slice(key.bytes());
        }
        self.write(owner_id, id, bytes, false).await
    }

    pub fn resize(
        &self,
        owner_id: &ProcessOwnerId,
        id: &ProcessId,
        size: ProcessTerminalSize,
    ) -> Result<()> {
        validate_terminal_size(size)?;
        let session = self.session(owner_id, id)?;
        session.require_active()?;
        session.touch();
        session.controller()?.resize(size)
    }

    pub fn signal(
        &self,
        owner_id: &ProcessOwnerId,
        id: &ProcessId,
        signal: ProcessSignal,
    ) -> Result<()> {
        let session = self.session(owner_id, id)?;
        if session.runtime.lock().state.is_terminal() {
            return Ok(());
        }
        session.touch();
        session.controller()?.signal(signal)
    }

    pub async fn stop(
        &self,
        owner_id: &ProcessOwnerId,
        id: &ProcessId,
        grace: Option<Duration>,
    ) -> Result<ProcessInfo> {
        let session = self.session(owner_id, id)?;
        // Subscribe BEFORE the stop signal: supervise emits ProcessExited as
        // part of reaching the terminal state, and the stop response must not
        // unblock the caller before that event lands. The registry row is
        // reclaimed only after the event is observed.
        let mut exited_event = self.inner.events.subscribe();
        let info = if session.runtime.lock().state.is_terminal() {
            session.info()
        } else {
            {
                let mut runtime = session.runtime.lock();
                runtime.state = ProcessState::Stopping;
            }
            session.controller()?.signal(ProcessSignal::Sigterm)?;
            let wait = self.wait(
                owner_id,
                id,
                grace.or(Some(self.inner.config.lock().terminate_grace)),
            );
            match wait.await {
                Ok(info) => info,
                Err(_) => {
                    session.controller()?.signal(ProcessSignal::Sigkill)?;
                    self.wait(owner_id, id, Some(Duration::from_secs(5))).await?
                }
            }
        };
        if info.state.is_terminal() {
            loop {
                match exited_event.try_recv() {
                    Ok(ProcessEvent::ProcessExited { process })
                        if process.id == *id =>
                    {
                        break;
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                }
            }
            self.remember_settled(info.clone());
            self.inner.sessions.lock().remove(id);
        }
        Ok(info)
    }

    pub async fn wait(
        &self,
        owner_id: &ProcessOwnerId,
        id: &ProcessId,
        timeout: Option<Duration>,
    ) -> Result<ProcessInfo> {
        let Some(session) = self.try_session(owner_id, id) else {
            return self
                .settled(owner_id, id)
                .ok_or_else(|| anyhow!("process not found"));
        };
        let future = async {
            loop {
                let notified = session.changed.notified();
                let info = session.info();
                if info.state.is_terminal() {
                    return info;
                }
                notified.await;
            }
        };
        if let Some(timeout) = timeout {
            tokio::time::timeout(timeout, future)
                .await
                .map_err(|_| anyhow!("timed out waiting for process {id}"))
        } else {
            Ok(future.await)
        }
    }

    pub async fn shutdown_owner(&self, owner_id: &ProcessOwnerId) {
        let sessions = self
            .owner_sessions(owner_id)
            .into_iter()
            .filter(|session| !session.runtime.lock().state.is_terminal())
            .collect::<Vec<_>>();
        if !sessions.is_empty() {
            for session in &sessions {
                if let Ok(controller) = session.controller() {
                    let _ = controller.signal(ProcessSignal::Sigterm);
                }
            }
            // Notify-driven wait bounded by the configured grace: it returns
            // as soon as every cooperative process reaches a terminal state,
            // so a SIGTERM-respecting owner settles far before the full grace
            // instead of consuming it in a fixed sleep.
            let grace = self.inner.config.lock().terminate_grace;
            let _ = tokio::time::timeout(grace, wait_all_terminal(&sessions)).await;
            let stragglers: Vec<_> = sessions
                .iter()
                .filter(|session| !session.runtime.lock().state.is_terminal())
                .cloned()
                .collect();
            if !stragglers.is_empty() {
                for session in &stragglers {
                    if let Ok(controller) = session.controller() {
                        let _ = controller.signal(ProcessSignal::Sigkill);
                    }
                }
                // One shared final bound for the survivors SIGKILL should
                // reap, avoiding the per-process 5s multiplication of the old
                // sequential wait loop.
                let _ =
                    tokio::time::timeout(Duration::from_secs(5), wait_all_terminal(&stragglers))
                        .await;
            }
        }

        // Shutdown transfers this owner's processes out of the live registry.
        // Keep any process whose termination could not be confirmed visible so
        // callers never mistake registry reclamation for successful teardown.
        self.inner.sessions.lock().retain(|_, session| {
            &session.owner_id != owner_id || !session.runtime.lock().state.is_terminal()
        });
    }

    pub fn shutdown_owner_now(&self, owner_id: &ProcessOwnerId) {
        for session in self.owner_sessions(owner_id) {
            if let Ok(controller) = session.controller() {
                let _ = controller.signal(ProcessSignal::Sigkill);
            }
        }
    }

    pub fn shutdown_now(&self) {
        self.inner.shutdown.store(true, Ordering::Release);
        let sessions = self
            .inner
            .sessions
            .lock()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for session in sessions {
            if let Ok(controller) = session.controller() {
                let _ = controller.signal(ProcessSignal::Sigkill);
            }
        }
    }

    fn session(&self, owner_id: &ProcessOwnerId, id: &ProcessId) -> Result<Arc<ProcessSession>> {
        self.try_session(owner_id, id)
            .ok_or_else(|| anyhow!("process not found"))
    }

    fn try_session(&self, owner_id: &ProcessOwnerId, id: &ProcessId) -> Option<Arc<ProcessSession>> {
        self.inner
            .sessions
            .lock()
            .get(id)
            .filter(|session| &session.owner_id == owner_id)
            .cloned()
    }

    fn remember_settled(&self, info: ProcessInfo) {
        let mut settled = self.inner.settled.lock();
        settled.retain(|row| row.id != info.id);
        settled.push(info);
        let cap = self.inner.config.lock().max_processes.max(1);
        if settled.len() > cap {
            let overflow = settled.len() - cap;
            settled.drain(0..overflow);
        }
    }

    fn settled(&self, owner_id: &ProcessOwnerId, id: &ProcessId) -> Option<ProcessInfo> {
        self.inner
            .settled
            .lock()
            .iter()
            .rev()
            .find(|row| row.id == *id && &row.owner_id == owner_id)
            .cloned()
    }

    fn owner_sessions(&self, owner_id: &ProcessOwnerId) -> Vec<Arc<ProcessSession>> {
        self.inner
            .sessions
            .lock()
            .values()
            .filter(|session| &session.owner_id == owner_id)
            .cloned()
            .collect()
    }
}

impl Default for ProcessManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ManagerInner {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        for session in self.sessions.get_mut().values() {
            if let Some(controller) = session.controller.lock().as_ref() {
                let _ = controller.signal(ProcessSignal::Sigkill);
            }
        }
    }
}

impl ProcessSession {
    fn info(&self) -> ProcessInfo {
        let runtime = self.runtime.lock();
        let (output_start_cursor, output_cursor) = self.log.lock().bounds();
        ProcessInfo {
            id: self.id.clone(),
            owner_id: self.owner_id.clone(),
            label: self.label.clone(),
            state: runtime.state,
            pid: runtime.pid,
            tty: self.tty,
            started_at_ms: self.started_at_ms,
            exited_at_ms: runtime.exited_at_ms,
            exit_code: runtime.exit_code,
            output_start_cursor,
            output_cursor,
        }
    }

    fn touch(&self) {
        self.last_activity_ms.store(now_ms(), Ordering::Release);
    }

    fn controller(&self) -> Result<Arc<BackendController>> {
        self.controller
            .lock()
            .clone()
            .ok_or_else(|| anyhow!("process is not running"))
    }

    fn require_active(&self) -> Result<()> {
        if self.runtime.lock().state.is_terminal() {
            bail!("process has exited");
        }
        Ok(())
    }
}

async fn supervise(
    manager: std::sync::Weak<ManagerInner>,
    session: Arc<ProcessSession>,
    controller: Arc<BackendController>,
    mut output_rx: tokio::sync::mpsc::Receiver<(super::ProcessStream, Vec<u8>)>,
    mut exit_rx: tokio::sync::oneshot::Receiver<i32>,
    timeout: Option<Duration>,
) {
    let timeout_sleep = timeout.map(tokio::time::sleep);
    tokio::pin!(timeout_sleep);
    let post_exit_sleep = None::<tokio::time::Sleep>;
    tokio::pin!(post_exit_sleep);
    let mut exit_code = None;
    let mut output_open = true;
    loop {
        tokio::select! {
            biased;
            output = output_rx.recv(), if output_open => match output {
                Some((stream, bytes)) => {
                    session.touch();
                    let (start_cursor, cursor) = session.log.lock().append(stream, &bytes);
                    if let Some(manager) = manager.upgrade() {
                        let _ = manager.events.send(ProcessEvent::ProcessOutput {
                            id: session.id.clone(),
                            owner_id: session.owner_id.clone(),
                            stream,
                            start_cursor,
                            cursor,
                            data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                        });
                    }
                    session.changed.notify_waiters();
                }
                None => {
                    output_open = false;
                    if exit_code.is_some() { break; }
                }
            },
            exit = &mut exit_rx, if exit_code.is_none() => {
                exit_code = Some(exit.unwrap_or(-1));
                timeout_sleep.set(None);
                if !output_open {
                    break;
                }
                post_exit_sleep.set(Some(tokio::time::sleep(PROCESS_EXIT_OUTPUT_GRACE)));
            },
            () = async {
                if let Some(sleep) = post_exit_sleep.as_mut().as_pin_mut() {
                    sleep.await;
                } else {
                    std::future::pending::<()>().await;
                }
            }, if post_exit_sleep.is_some() => {
                controller.close_output();
                let _ = controller.signal(ProcessSignal::Sigkill);
                break;
            },
            () = async {
                if let Some(sleep) = timeout_sleep.as_mut().as_pin_mut() {
                    sleep.await;
                } else {
                    std::future::pending::<()>().await;
                }
            }, if timeout_sleep.is_some() => {
                {
                    let mut runtime = session.runtime.lock();
                    runtime.state = ProcessState::Stopping;
                    runtime.terminal_override = Some(ProcessState::TimedOut);
                }
                let _ = controller.signal(ProcessSignal::Sigterm);
                let grace = manager.upgrade().map_or(Duration::from_secs(1), |manager| manager.config.lock().terminate_grace);
                tokio::time::sleep(grace).await;
                let _ = controller.signal(ProcessSignal::Sigkill);
                timeout_sleep.set(None);
            }
        }
        if exit_code.is_some() && !output_open {
            break;
        }
    }

    let code = exit_code.unwrap_or(-1);
    {
        let mut runtime = session.runtime.lock();
        runtime.state = runtime.terminal_override.unwrap_or(ProcessState::Exited);
        runtime.exit_code = Some(if runtime.state == ProcessState::TimedOut {
            124
        } else {
            code
        });
        runtime.exited_at_ms = Some(now_ms());
    }
    session.controller.lock().take();
    if let Some(manager) = manager.upgrade() {
        let _ = manager.events.send(ProcessEvent::ProcessExited {
            process: session.info(),
        });
        prune_terminal_sessions(&manager, &session.id);
    }
    session.changed.notify_waiters();
}

fn prune_terminal_sessions(manager: &ManagerInner, newest_id: &ProcessId) {
    let mut sessions = manager.sessions.lock();
    let terminal_count = sessions
        .values()
        .filter(|session| session.runtime.lock().state.is_terminal())
        .count();
    if terminal_count <= manager.config.lock().max_processes {
        return;
    }
    let remove_count = terminal_count - manager.config.lock().max_processes;
    let mut terminal = sessions
        .values()
        .filter(|session| session.id != *newest_id && session.runtime.lock().state.is_terminal())
        .map(|session| (session.started_at_ms, session.id.clone()))
        .collect::<Vec<_>>();
    terminal.sort_by_key(|(started_at, _)| *started_at);
    for (_, id) in terminal.into_iter().take(remove_count) {
        sessions.remove(&id);
    }
}

fn spawn_idle_reaper(inner: &Arc<ManagerInner>) {
    if inner.config.lock().idle_timeout.is_none() {
        return;
    }
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    if inner
        .idle_reaper_started
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let weak = Arc::downgrade(inner);
    tokio::spawn(async move {
        loop {
            let Some(inner) = weak.upgrade() else {
                break;
            };
            if inner.shutdown.load(Ordering::Acquire) {
                break;
            }
            let tick = {
                let config = inner.config.lock();
                config
                    .idle_timeout
                    .map(|idle_timeout| (idle_timeout, config.idle_scan_interval))
            };
            let (idle_timeout, interval) = match tick {
                Some(tick) => tick,
                None => {
                    inner.idle_changed.notified().await;
                    continue;
                }
            };
            tokio::select! {
                () = tokio::time::sleep(interval) => {}
                () = inner.idle_changed.notified() => continue,
            }
            if inner.shutdown.load(Ordering::Acquire) {
                break;
            }
            let now = now_ms();
            let idle_ms = idle_timeout.as_millis().min(u128::from(u64::MAX)) as u64;
            let expired = inner
                .sessions
                .lock()
                .values()
                .filter(|session| {
                    !session.runtime.lock().state.is_terminal()
                        && now.saturating_sub(session.last_activity_ms.load(Ordering::Acquire))
                            >= idle_ms
                })
                .cloned()
                .collect::<Vec<_>>();
            for session in expired {
                {
                    let mut runtime = session.runtime.lock();
                    if runtime.state.is_terminal() {
                        continue;
                    }
                    runtime.state = ProcessState::Stopping;
                    runtime.terminal_override = Some(ProcessState::Expired);
                }
                if let Ok(controller) = session.controller() {
                    let _ = controller.signal(ProcessSignal::Sigterm);
                    let controller = controller.clone();
                    let grace = inner.config.lock().terminate_grace;
                    tokio::spawn(async move {
                        tokio::time::sleep(grace).await;
                        let _ = controller.signal(ProcessSignal::Sigkill);
                    });
                }
            }
        }
    });
}

fn validate_spawn_spec(spec: &ProcessSpawnSpec, config: &ProcessManagerConfig) -> Result<()> {
    if spec.argv.is_empty() || spec.argv[0].is_empty() {
        bail!("argv must contain a non-empty application");
    }
    if spec.argv.iter().any(|argument| argument.contains('\0')) {
        bail!("argv contains a NUL byte");
    }
    if !spec.cwd.is_absolute() {
        bail!("cwd must be absolute");
    }
    let metadata = std::fs::metadata(&spec.cwd).map_err(|_| anyhow!("cwd does not exist"))?;
    if !metadata.is_dir() {
        bail!("cwd is not a directory");
    }
    if spec.env.iter().any(|(key, value)| {
        key.is_empty()
            || key.contains('=')
            || key.contains('\0')
            || value.as_ref().is_some_and(|value| value.contains('\0'))
    }) {
        bail!("environment contains an invalid entry");
    }
    if let Some(label) = &spec.label
        && (label.is_empty() || label.len() > MAX_PROCESS_LABEL_BYTES || label.contains('\0'))
    {
        bail!("label must contain 1 to {MAX_PROCESS_LABEL_BYTES} bytes");
    }
    if spec.terminal_size.is_some() && !spec.tty {
        bail!("terminal_size requires tty=true");
    }
    if let Some(size) = spec.terminal_size {
        validate_terminal_size(size)?;
    }
    if let Some(output_bytes) = spec.output_bytes
        && output_bytes > config.max_output_bytes
    {
        bail!(
            "output_bytes exceeds the manager maximum of {}",
            config.max_output_bytes
        );
    }
    Ok(())
}

fn validate_terminal_size(size: ProcessTerminalSize) -> Result<()> {
    if size.rows == 0 || size.cols == 0 {
        bail!("terminal rows and cols must be greater than zero");
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

/// Concurrently wait for every session to reach a terminal state.
///
/// Each wait future registers its `Notify` permit before checking the terminal
/// flag (matching `ProcessManager::wait`), so a transition observed between the
/// check and the await is not lost. `join_all` drives them concurrently, so the
/// returned future resolves as soon as *all* sessions have settled.
async fn wait_all_terminal(sessions: &[Arc<ProcessSession>]) {
    let waits = sessions.iter().cloned().map(|session| async move {
        loop {
            let notified = session.changed.notified();
            if session.runtime.lock().state.is_terminal() {
                return;
            }
            notified.await;
        }
    });
    futures_util::future::join_all(waits).await;
}
