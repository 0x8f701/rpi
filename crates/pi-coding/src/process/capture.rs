use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use tokio::io::AsyncReadExt;

use super::{
    ProcessSignal, ProcessStream, ProcessTerminalSize, authorize_spawn_with_auth,
    launch_authorized_pipe,
};
use crate::AuthManager;
use crate::application::generation::ApplicationGeneration;
use crate::sandbox::{CredentialAudience, RawSpawnRequest, SpawnAuthorization};

const PROCESS_SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
#[cfg(test)]
pub(crate) type RepositoryTestExecutor = Arc<
    dyn Fn(&super::AuthorizedSpawn, usize, usize) -> Result<GovernedProcessOutput> + Send + Sync,
>;

/// Hermetic, test-only repository helper executor for external integration
/// tests. Runs the authorized `git` argv on the host against the authorized
/// working directory, but only AFTER the governed bridge
/// (`prepare_repository_spawn` + spawn authorization) has validated the
/// retained repository authority and cwd provenance.
///
/// Invariant (structurally enforced, see `run_with_abort`): the executor is
/// consulted ONLY after `prepare_repository_spawn` has bound the cwd to the
/// retained source/managed lease AND `authorize_spawn_with_auth` has validated
/// pointer-identical generation, the retained working directory identity, and
/// the canonical spawn call. It is therefore impossible for the executor to
/// run before production prepare+authorize, and a foreign/stale cwd is
/// rejected before the executor is reached (covered by the workflow_application
/// foreign/stale cwd rejection tests). Production never installs it; the
/// governed launch path (`run_authorized_pipe_bounded`) is unchanged.
#[derive(Clone)]
pub struct HermeticRepositoryExecutor(
    pub(crate)  Arc<
        dyn Fn(&super::AuthorizedSpawn, usize, usize) -> Result<GovernedProcessOutput>
            + Send
            + Sync,
    >,
);

impl HermeticRepositoryExecutor {
    /// Standard executor that runs the authorized `git` argv on the host and
    /// returns a bounded [`GovernedProcessOutput`]. Mirrors the in-crate
    /// governed repository test fixture: the governed bridge has already
    /// validated the retained cwd against the retained source/managed lease
    /// before this runs, so no caller-forged path reaches execution.
    #[must_use]
    pub fn default_executor() -> Self {
        use std::path::PathBuf;
        use std::process::Command;
        Self(Arc::new(|authorized, stdout_bytes, stderr_bytes| {
            assert!(matches!(
                authorized.audience(),
                crate::sandbox::CredentialAudience::SpawnFamily {
                    family: pi_sandbox::SpawnFamily::RepositoryHelper,
                }
            ));
            assert_eq!(authorized.stdio(), pi_sandbox::StdioMode::Piped);
            assert!(authorized.timeout_ms().is_some());
            let argv = authorized.argv();
            let program = PathBuf::from(argv.first().expect("git program"));
            let mut command = Command::new(program);
            command.args(&argv[1..]);
            command.current_dir(authorized.cwd().as_path());
            command.env_clear();
            command.envs(authorized.environment());
            let output = command
                .output()
                .expect("execute governed repository fixture");
            let stdout_capped = output.stdout.len() > stdout_bytes;
            let stderr_capped = output.stderr.len() > stderr_bytes;
            Ok(GovernedProcessOutput {
                status: output.status.code(),
                stdout: output.stdout.into_iter().take(stdout_bytes).collect(),
                stderr: output.stderr.into_iter().take(stderr_bytes).collect(),
                timed_out: false,
                cancelled: false,
                stdout_capped,
                stderr_capped,
            })
        }))
    }
}

#[derive(Clone)]
pub struct GovernedProcessContext {
    generation: Arc<ApplicationGeneration>,
    auth_manager: Option<Arc<AuthManager>>,
    #[cfg(unix)]
    repository_source_root: Option<crate::fs::CapabilityRootLease>,
    #[cfg(unix)]
    repository_managed_root: Option<crate::fs::CapabilityRootLease>,
    repository_test_executor: Option<HermeticRepositoryExecutor>,
}

impl std::fmt::Debug for GovernedProcessContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GovernedProcessContext")
            .field("generation_revision", &self.generation.revision().get())
            .field("has_auth_manager", &self.auth_manager.is_some())
            .finish()
    }
}

impl GovernedProcessContext {
    pub(crate) fn new(
        generation: Arc<ApplicationGeneration>,
        auth_manager: Option<Arc<AuthManager>>,
    ) -> Self {
        Self {
            generation,
            auth_manager,
            #[cfg(unix)]
            repository_source_root: None,
            #[cfg(unix)]
            repository_managed_root: None,
            repository_test_executor: None,
        }
    }

    #[cfg(unix)]
    pub(crate) fn with_repository_authorities(
        mut self,
        source_root: crate::fs::CapabilityRootLease,
        managed_root: crate::fs::CapabilityRootLease,
    ) -> Self {
        let capabilities = self.generation.sandbox().capabilities();
        let valid_source = source_root.belongs_to(capabilities)
            && source_root.purpose() == crate::fs::CapabilityRootPurpose::ProjectResources
            && matches!(
                source_root.access(),
                crate::fs::CapabilityAccess::ReadOnly
                    | crate::fs::CapabilityAccess::ReadWrite
                    | crate::fs::CapabilityAccess::PrivateReadWrite
            );
        let valid_managed = managed_root.belongs_to(capabilities)
            && managed_root.purpose() == crate::fs::CapabilityRootPurpose::WorkflowWorktrees
            && managed_root.access() == crate::fs::CapabilityAccess::PrivateReadWrite;
        if valid_source && valid_managed {
            self.repository_source_root = Some(source_root);
            self.repository_managed_root = Some(managed_root);
        } else {
            self.repository_source_root = None;
            self.repository_managed_root = None;
        }
        self
    }
    #[cfg(test)]
    pub(crate) fn with_repository_test_executor(
        mut self,
        executor: RepositoryTestExecutor,
    ) -> Self {
        self.repository_test_executor = Some(HermeticRepositoryExecutor(executor));
        self
    }
    /// Install a hermetic repository helper executor (external test seam).
    /// The governed bridge still validates the retained repository authority
    /// and cwd provenance before the executor runs; production never calls
    /// this and the governed launch path is unchanged.
    #[must_use]
    pub fn with_hermetic_repository_executor(
        mut self,
        executor: HermeticRepositoryExecutor,
    ) -> Self {
        self.repository_test_executor = Some(executor);
        self
    }

    #[cfg(unix)]
    fn prepare_repository_spawn(
        &self,
        spec: &mut GovernedProcessSpec,
        bind_retained: bool,
    ) -> Result<()> {
        let source_root = self.repository_source_root.as_ref().ok_or_else(|| {
            anyhow!("repository source capability does not cover this generation")
        })?;
        let managed_root = self.repository_managed_root.as_ref().ok_or_else(|| {
            anyhow!("repository managed capability does not cover this generation")
        })?;
        prepare_repository_spawn(source_root, managed_root, spec, bind_retained)
    }

    /// Open the video converter through ToolExecutable authority and retain
    /// that handle for spawn. A missing or unopenable converter is reported as
    /// [`pi_sandbox::SandboxIoOperation::OpenExecutable`] so callers can map it
    /// to the actionable ffmpeg-missing error without weakening the probe.
    #[cfg(unix)]
    fn prepare_video_converter_spawn(&self, spec: &mut GovernedProcessSpec) -> Result<()> {
        let path = pi_sandbox::AbsolutePath::parse(spec.program.clone())
            .map_err(|_| video_open_executable_error(None))?;
        let opened = self
            .generation
            .sandbox()
            .capabilities()
            .open_managed_read_file(crate::fs::CapabilityRootPurpose::ToolExecutable, path)
            .map_err(|error| video_open_executable_error(Some(&error)))?;
        let resolved = pi_sandbox::AbsolutePath::parse(opened.resolved_path().to_path_buf())
            .map_err(|_| video_open_executable_error(None))?;
        let source = opened
            .into_file()
            .duplicate_for_spawn_executable()
            .map_err(|error| video_open_executable_error(Some(&error)))?;
        if source.canonical_executable().0 != &resolved {
            return Err(video_open_executable_error(None));
        }
        spec.program = resolved.as_path().to_path_buf();
        let is_danger = self.generation.sandbox().policy_snapshot().policy().mode()
            == pi_sandbox::SandboxMode::DangerFullAccess;
        if !is_danger {
            spec.retained_executable = Some(source);
        }
        Ok(())
    }
    pub(crate) fn git_program(&self) -> Result<PathBuf> {
        Ok(
            super::run_trusted_tool_probe(&self.generation, super::TrustedToolProbe::Git)?
                .as_path()
                .to_path_buf(),
        )
    }

    pub fn ffmpeg_program(&self) -> Result<PathBuf> {
        Ok(
            super::run_trusted_tool_probe(&self.generation, super::TrustedToolProbe::Ffmpeg)?
                .as_path()
                .to_path_buf(),
        )
    }
    pub fn pdftotext_program(&self) -> Result<PathBuf> {
        Ok(
            super::run_trusted_tool_probe(&self.generation, super::TrustedToolProbe::PdfToText)?
                .as_path()
                .to_path_buf(),
        )
    }

    pub fn run_blocking_document_converter(
        &self,
        program: impl AsRef<Path>,
        args: Vec<String>,
        cwd: impl AsRef<Path>,
        timeout: Duration,
        stdout_bytes: usize,
        stderr_bytes: usize,
    ) -> Result<GovernedProcessOutput> {
        self.run_blocking(
            pi_sandbox::SpawnFamily::DocumentConverter,
            GovernedProcessSpec {
                program: program.as_ref().to_path_buf(),
                args,
                cwd: cwd.as_ref().to_path_buf(),
                environment: BTreeMap::new(),
                timeout,
                stdout_bytes,
                stderr_bytes,
                #[cfg(unix)]
                mount_sources: Vec::new(),
                #[cfg(unix)]
                retained_executable: None,
                #[cfg(unix)]
                retained_working_directory: None,
            },
        )
    }


    pub async fn run_video_converter(
        &self,
        program: impl AsRef<Path>,
        args: Vec<String>,
        cwd: impl AsRef<Path>,
        timeout: Duration,
        stdout_bytes: usize,
        stderr_bytes: usize,
    ) -> Result<GovernedProcessOutput> {
        self.run(
            pi_sandbox::SpawnFamily::VideoConverter,
            GovernedProcessSpec {
                program: program.as_ref().to_path_buf(),
                args,
                cwd: cwd.as_ref().to_path_buf(),
                environment: BTreeMap::new(),
                timeout,
                stdout_bytes,
                stderr_bytes,
                #[cfg(unix)]
                mount_sources: Vec::new(),
                #[cfg(unix)]
                retained_executable: None,
                #[cfg(unix)]
                retained_working_directory: None,
            },
        )
        .await
    }
    pub fn run_blocking_video_converter(
        &self,
        program: impl AsRef<Path>,
        args: Vec<String>,
        cwd: impl AsRef<Path>,
        timeout: Duration,
        stdout_bytes: usize,
        stderr_bytes: usize,
    ) -> Result<GovernedProcessOutput> {
        self.run_blocking(
            pi_sandbox::SpawnFamily::VideoConverter,
            GovernedProcessSpec {
                program: program.as_ref().to_path_buf(),
                args,
                cwd: cwd.as_ref().to_path_buf(),
                environment: BTreeMap::new(),
                timeout,
                stdout_bytes,
                stderr_bytes,
                #[cfg(unix)]
                mount_sources: Vec::new(),
                #[cfg(unix)]
                retained_executable: None,
                #[cfg(unix)]
                retained_working_directory: None,
            },
        )
    }

    pub(crate) async fn run(
        &self,
        family: pi_sandbox::SpawnFamily,
        spec: GovernedProcessSpec,
    ) -> Result<GovernedProcessOutput> {
        self.run_with_abort(family, spec, None).await
    }

    pub(crate) async fn run_with_abort(
        &self,
        family: pi_sandbox::SpawnFamily,
        mut spec: GovernedProcessSpec,
        abort: Option<pi_agent::AbortSignal>,
    ) -> Result<GovernedProcessOutput> {
        if spec.stdout_bytes == 0 || spec.stderr_bytes == 0 {
            bail!("governed process output limits must be nonzero");
        }
        #[cfg(unix)]
        let is_danger = self.generation.sandbox().policy_snapshot().policy().mode()
            == pi_sandbox::SandboxMode::DangerFullAccess;
        // The retained working directory + mount-source binding is required
        // for confined sandboxes (the cwd must come from retained repository
        // authority). Under DangerFullAccess the RawHost backend rejects any
        // retained working directory, so the binding is skipped — but the
        // cwd-provenance + staleness check inside `prepare_repository_spawn`
        // ALWAYS runs (Unix), preserving the retained-authority invariant (a
        // foreign/stale cwd is rejected before execution, including before the
        // hermetic executor). Without this guard the workflow worktree git
        // spawn fails with `InvalidRetainedWorkingDirectory` under the default
        // sandbox, breaking `/workflow create` in production.
        //
        // `executor_authorized` gates the hermetic repository executor:
        // - Unix: the executor runs ONLY for the confined repository helper
        //   (`!is_danger`), so DangerFullAccess is fail-closed (no
        //   retained-authority binding → executor not consulted → the spawn
        //   proceeds through the governed RawHost launch path).
        // - Non-Unix: the confined `prepare_repository_spawn` path is
        //   unavailable, so prior cross-platform executor-selection semantics
        //   are retained — the executor is selected for the repository helper
        //   family (foreign/stale cwd checks are Unix-only, "wherever
        //   supported"). See `non_unix_executor_authorized` for the
        //   compile-time proof that this is family-based selection, not a hard
        //   disable.
        #[cfg(unix)]
        let executor_authorized = family == pi_sandbox::SpawnFamily::RepositoryHelper && !is_danger;
        #[cfg(not(unix))]
        let executor_authorized = non_unix_executor_authorized(family);
        #[cfg(unix)]
        if family == pi_sandbox::SpawnFamily::RepositoryHelper {
            self.prepare_repository_spawn(&mut spec, !is_danger)?;
        }
        #[cfg(unix)]
        if family == pi_sandbox::SpawnFamily::VideoConverter {
            self.prepare_video_converter_spawn(&mut spec)?;
        }
        if spec.timeout.is_zero() {
            return Ok(GovernedProcessOutput {
                status: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                timed_out: true,
                cancelled: false,
                stdout_capped: false,
                stderr_capped: false,
            });
        }
        let mut argv = Vec::with_capacity(spec.args.len() + 1);
        argv.push(OsString::from(&spec.program));
        argv.extend(spec.args.into_iter().map(OsString::from));
        let authorized = authorize_spawn_with_auth(
            self.generation.clone(),
            self.auth_manager.as_deref(),
            RawSpawnRequest {
                authorization: SpawnAuthorization::Captured(self.generation.sandbox().clone()),
                family,
                audience: CredentialAudience::new_spawn_family(family)?,
                argv,
                cwd: spec.cwd,
                requested_env: spec
                    .environment
                    .into_iter()
                    .map(|(name, value)| (OsString::from(name), value.map(OsString::from)))
                    .collect(),
                requested_grants: BTreeSet::new(),
                stdio: pi_sandbox::StdioMode::Piped,
                terminal_size: None,
                timeout_ms: Some(duration_millis(spec.timeout)),
                target_fd_mappings: Vec::new(),
                #[cfg(unix)]
                owned_target_fd_mappings: Vec::new(),
                #[cfg(unix)]
                mount_sources: spec.mount_sources,
                #[cfg(unix)]
                retained_executable: spec.retained_executable,
                #[cfg(unix)]
                retained_working_directory: spec.retained_working_directory,
            },
        )
        .await?;
        // The hermetic repository executor is consulted ONLY when
        // `executor_authorized`: Unix confines it to the retained-authority
        // binding (DangerFullAccess is fail-closed); non-Unix retains prior
        // family-based selection. A foreign/stale cwd has already been rejected
        // by the provenance check above (Unix) before the executor is reached.
        if family == pi_sandbox::SpawnFamily::RepositoryHelper
            && executor_authorized
            && let Some(executor) = &self.repository_test_executor
        {
            return (executor.0)(&authorized, spec.stdout_bytes, spec.stderr_bytes);
        }
        run_authorized_pipe_bounded(
            authorized,
            spec.timeout,
            spec.stdout_bytes,
            spec.stderr_bytes,
            abort,
        )
        .await
    }
    pub(crate) async fn run_foreground_shell(
        &self,
        spec: ForegroundShellSpec,
    ) -> Result<ForegroundShellOutput> {
        if spec.abort.is_aborted() {
            return Ok(ForegroundShellOutput::cancelled());
        }
        let family = pi_sandbox::SpawnFamily::ForegroundShell;
        let mut argv = Vec::with_capacity(spec.args.len() + 1);
        argv.push(OsString::from(&spec.program));
        argv.extend(spec.args.into_iter().map(OsString::from));
        let is_danger = self.generation.sandbox().policy_snapshot().policy().mode()
            == pi_sandbox::SandboxMode::DangerFullAccess;
        let retained_working_directory = (!is_danger)
            .then(|| {
                self.generation
                    .sandbox()
                    .fs()
                    .confined()?
                    .duplicate_for_spawn_working_directory(&spec.cwd)
            })
            .transpose()?;
        let executable = self
            .generation
            .sandbox()
            .capabilities()
            .open_managed_read_file(
                crate::fs::CapabilityRootPurpose::ToolExecutable,
                pi_sandbox::AbsolutePath::parse(spec.program.clone())?,
            )?;
        let executable_path = executable.resolved_path().to_path_buf();
        let retained_executable = (!is_danger)
            .then(|| executable.into_file().duplicate_for_spawn_executable())
            .transpose()?;
        let authorized = authorize_spawn_with_auth(
            self.generation.clone(),
            self.auth_manager.as_deref(),
            RawSpawnRequest {
                authorization: SpawnAuthorization::Captured(self.generation.sandbox().clone()),
                family,
                audience: CredentialAudience::new_spawn_family(family)?,
                argv: {
                    argv[0] = OsString::from(&executable_path);
                    argv
                },
                cwd: spec.cwd,
                requested_env: spec
                    .environment
                    .into_iter()
                    .map(|(name, value)| (OsString::from(name), value.map(OsString::from)))
                    .collect(),
                requested_grants: BTreeSet::new(),
                stdio: pi_sandbox::StdioMode::Pty,
                terminal_size: Some(super::backend::sandbox_terminal_size(spec.terminal_size)?),
                timeout_ms: spec.timeout.map(duration_millis),
                target_fd_mappings: Vec::new(),
                #[cfg(unix)]
                owned_target_fd_mappings: Vec::new(),
                #[cfg(unix)]
                mount_sources: Vec::new(),
                #[cfg(unix)]
                retained_executable,
                #[cfg(unix)]
                retained_working_directory,
            },
        )
        .await?;
        let mut process = super::backend::spawn(authorized).await?;
        // Timing starts at the successful PTY spawn boundary: everything after
        // — the optional input write, the command run, and cleanup — shares
        // one timeout/abort lifecycle, so a blocked input write can never
        // escape the configured timeout. The caller's queue wait is never on
        // this path (PTY runs do not take the brush lock).
        let execution_started = Instant::now();
        run_foreground_pty_lifecycle(
            &mut process,
            spec.input,
            spec.timeout,
            &spec.abort,
            &spec.stream,
            execution_started,
        )
        .await
    }

    pub(crate) fn run_blocking(
        &self,
        family: pi_sandbox::SpawnFamily,
        spec: GovernedProcessSpec,
    ) -> Result<GovernedProcessOutput> {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                // The caller is on a worker of a multi-thread runtime: yield this
                // executor thread to the runtime's blocking pool, then drive the
                // governed future on its own worker so the caller thread is never
                // the one blocked awaiting child I/O.
                tokio::task::block_in_place(|| handle.block_on(self.run(family, spec)))
            }
            Ok(_) => {
                // The caller is on the sole executor thread of a single-threaded
                // runtime. `block_in_place` is unavailable (and would panic outside
                // a multi-thread runtime), and `block_on` here would monopolize the
                // only thread that must also drive the child's I/O — a deadlock.
                // Bridge the governed future onto a dedicated scoped OS thread with
                // its own current-thread runtime so the outer executor is never
                // blocked while awaiting the child, while still running inside the
                // governed/authorized execution path (no raw spawn).
                self.run_blocking_bridged(family, spec)
            }
            Err(_) => tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(self.run(family, spec)),
        }
    }

    /// Runs a governed future on a fresh, dedicated current-thread Tokio runtime
    /// hosted on a scoped OS thread.
    ///
    /// The future borrows `self` only for the duration of the scope, so the
    /// governed context (generation, auth manager, retained repository authority)
    /// is preserved exactly as on the calling thread. Spawning a fresh runtime on
    /// its own thread — rather than re-entering the ambient runtime — guarantees
    /// there is no nested `block_on` against an executor thread that is already
    /// blocked, and therefore no deadlock on either a current-thread or
    /// multi-thread runtime.
    fn run_blocking_bridged(
        &self,
        family: pi_sandbox::SpawnFamily,
        spec: GovernedProcessSpec,
    ) -> Result<GovernedProcessOutput> {
        std::thread::scope(|scope| {
            let handle = scope.spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| {
                        anyhow!("failed to build governed process bridge runtime: {error}")
                    })?;
                runtime.block_on(self.run(family, spec))
            });
            match handle.join() {
                Ok(result) => result,
                Err(_) => Err(anyhow!(
                    "governed process bridge thread panicked while awaiting child"
                )),
            }
        })
    }
}

#[cfg(unix)]
fn video_open_executable_error(error: Option<&crate::fs::FsError>) -> anyhow::Error {
    let kind = match error {
        Some(crate::fs::FsError::System {
            kind: std::io::ErrorKind::NotFound,
            ..
        }) => pi_sandbox::SandboxIoKind::NotFound,
        Some(crate::fs::FsError::System {
            kind: std::io::ErrorKind::PermissionDenied,
            ..
        }) => pi_sandbox::SandboxIoKind::PermissionDenied,
        _ => pi_sandbox::SandboxIoKind::Other,
    };
    anyhow::Error::from(pi_sandbox::SandboxError::System {
        operation: pi_sandbox::SandboxIoOperation::OpenExecutable,
        kind,
        raw_os_error: None,
    })
}

#[cfg(unix)]
fn prepare_repository_spawn(
    source_root: &crate::fs::CapabilityRootLease,
    managed_root: &crate::fs::CapabilityRootLease,
    spec: &mut GovernedProcessSpec,
    bind_retained: bool,
) -> Result<()> {
    if spec.retained_working_directory.is_some() {
        bail!("repository helper working directory must come from retained repository authority");
    }
    // The cwd-provenance + staleness check ALWAYS runs: a foreign cwd is
    // rejected ("outside retained authority") and a stale (removed) cwd fails
    // at the descendant-lease open, before any execution path is reached —
    // including the hermetic executor. This preserves the retained-authority
    // invariant under DangerFullAccess, where the retained working directory
    // binding below is skipped (the RawHost backend rejects it).
    let working_directory = if spec.cwd.starts_with(source_root.display_root()) {
        source_root.duplicate_descendant_for_spawn_working_directory(&spec.cwd)
    } else if spec.cwd.starts_with(managed_root.display_root()) {
        managed_root.duplicate_descendant_for_spawn_working_directory(&spec.cwd)
    } else {
        return Err(anyhow!(
            "repository helper working directory is outside retained authority"
        ));
    }?;
    if bind_retained {
        // Confined sandboxes bind the cwd to the retained source/managed lease
        // and mount the managed root so the hermetic executor / confined spawn
        // reads it. Under DangerFullAccess (`bind_retained == false`) the
        // RawHost backend rejects any retained working directory, and full
        // access makes the binding unnecessary — so only the provenance check
        // above runs and the retained directory is dropped (fd closed).
        spec.retained_working_directory = Some(working_directory);
        spec.mount_sources.push(managed_root.duplicate_for_spawn()?);
    }
    Ok(())
}

/// Non-Unix executor-selection predicate (prior cross-platform seam). The
/// confined `prepare_repository_spawn` path is Unix-only, so on non-Unix the
/// hermetic repository executor is selected for the repository helper family
/// exactly as before the DangerFullAccess Unix guard — foreign/stale cwd
/// provenance checks are Unix-only ("wherever supported"). This is a `const
/// fn` so the compile-time assertions below prove it is family-based
/// selection, not a hard `false` disable.
#[cfg(not(unix))]
const fn non_unix_executor_authorized(family: pi_sandbox::SpawnFamily) -> bool {
    matches!(family, pi_sandbox::SpawnFamily::RepositoryHelper)
}

/// Compile-time proof that the non-Unix executor gate is family-based
/// selection (not a hard disable): the repository helper authorizes the
/// executor and a non-helper family does not. Only compiled on non-Unix
/// targets, so the cross-platform seam is verified where it applies.
#[cfg(not(unix))]
const _: () = {
    assert!(non_unix_executor_authorized(
        pi_sandbox::SpawnFamily::RepositoryHelper
    ));
    assert!(!non_unix_executor_authorized(
        pi_sandbox::SpawnFamily::ForegroundShell
    ));
};

pub(crate) async fn run_authorized_pipe_bounded(
    authorized: super::AuthorizedSpawn,
    timeout: Duration,
    stdout_bytes: usize,
    stderr_bytes: usize,
    abort: Option<pi_agent::AbortSignal>,
) -> Result<GovernedProcessOutput> {
    if stdout_bytes == 0 || stderr_bytes == 0 {
        bail!("governed process output limits must be nonzero");
    }
    if abort
        .as_ref()
        .is_some_and(pi_agent::AbortSignal::is_aborted)
    {
        return Ok(GovernedProcessOutput::cancelled());
    }
    let mut child = launch_authorized_pipe(authorized)?;
    child.take_stdin();
    let stdout = child
        .take_stdout()
        .ok_or_else(|| anyhow!("governed process stdout was unavailable"))?;
    let stderr = child
        .take_stderr()
        .ok_or_else(|| anyhow!("governed process stderr was unavailable"))?;
    let (cap_tx, mut cap_rx) = tokio::sync::mpsc::channel(2);
    let stdout_task = tokio::spawn(read_bounded(stdout, stdout_bytes, cap_tx.clone()));
    let stderr_task = tokio::spawn(read_bounded(stderr, stderr_bytes, cap_tx));
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let abort = abort.unwrap_or_else(pi_agent::AbortSignal::none);

    let (status, timed_out, cancelled) = tokio::select! {
        outcome = child.wait() => (Some(outcome?), false, false),
        () = &mut deadline => {
            terminate_and_reap(&mut child).await;
            (None, true, false)
        }
        () = abort.cancelled() => {
            terminate_and_reap(&mut child).await;
            (None, false, true)
        }
        Some(()) = cap_rx.recv() => {
            terminate_and_reap(&mut child).await;
            (None, false, false)
        }
    };
    let stdout = stdout_task
        .await
        .map_err(|_| anyhow!("governed stdout reader failed"))??;
    let stderr = stderr_task
        .await
        .map_err(|_| anyhow!("governed stderr reader failed"))??;
    Ok(GovernedProcessOutput {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        timed_out,
        cancelled,
        stdout_capped: stdout.capped,
        stderr_capped: stderr.capped,
    })
}

async fn terminate_and_reap(child: &mut super::GovernedChild) {
    let _ = child.signal(ProcessSignal::Sigkill);
    if tokio::time::timeout(PROCESS_SHUTDOWN_GRACE, child.wait())
        .await
        .is_err()
    {
        let _ = child.wait().await;
    }
}
async fn terminate_backend(controller: &super::backend::BackendController) {
    let _ = controller.signal(ProcessSignal::Sigterm);
    tokio::time::sleep(PROCESS_SHUTDOWN_GRACE).await;
    let _ = controller.signal(ProcessSignal::Sigkill);
}
/// Deterministic seam over the spawned PTY backend. Production delegates to
/// the real backend process (`super::backend`); same-file tests install a
/// fake so the lifecycle is exercisable without a governed spawn.
trait ForegroundPty {
    async fn write_input(&mut self, bytes: Vec<u8>) -> Result<()>;
    fn close_stdin(&mut self);
    /// Next lifecycle event: an output chunk, output-channel closure
    /// (`Output(None)`), or the observed exit code. `output_open` and
    /// `exit_pending` mirror the drain-loop channel conditions so a closed
    /// branch is never polled again.
    async fn next_event(&mut self, output_open: bool, exit_pending: bool) -> ForegroundEvent;
    async fn terminate(&mut self);
}

/// One observed foreground PTY event during the drain phase.
enum ForegroundEvent {
    Output(Option<(ProcessStream, Vec<u8>)>),
    Exit(i32),
}

impl ForegroundPty for super::backend::BackendProcess {
    async fn write_input(&mut self, bytes: Vec<u8>) -> Result<()> {
        self.controller.write(bytes).await
    }
    fn close_stdin(&mut self) {
        self.controller.close_stdin();
    }
    async fn next_event(&mut self, output_open: bool, exit_pending: bool) -> ForegroundEvent {
        tokio::select! {
            output = self.output_rx.recv(), if output_open => ForegroundEvent::Output(output),
            exit = &mut self.exit_rx, if exit_pending => ForegroundEvent::Exit(exit.unwrap_or(-1)),
        }
    }
    async fn terminate(&mut self) {
        terminate_backend(&self.controller).await;
    }
}

/// Terminal classification of the foreground PTY lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ForegroundTerminal {
    Exit(i32),
    TimedOut,
    Cancelled,
}

impl ForegroundTerminal {
    /// Final output classification. A timeout decided while abort fired
    /// during termination cleanup classifies as cancelled, with the measured
    /// elapsed staying frozen at the timeout decision.
    fn classify(self, aborted: bool) -> (Option<i32>, bool, bool) {
        match self {
            Self::Exit(code) => (Some(code), false, false),
            Self::TimedOut if aborted => (None, false, true),
            Self::TimedOut => (None, true, false),
            Self::Cancelled => (None, false, true),
        }
    }
}

/// Terminal decision state accumulated across the lifecycle phases: the
/// first terminal decision freezes `execution_elapsed` before termination
/// cleanup and output drain.
struct ForegroundDecision {
    execution_started: Instant,
    terminal: Option<ForegroundTerminal>,
    frozen_elapsed: Option<Duration>,
}

impl ForegroundDecision {
    fn new(execution_started: Instant) -> Self {
        Self {
            execution_started,
            terminal: None,
            frozen_elapsed: None,
        }
    }

    /// Freezes the elapsed measurement at the decision instant.
    fn freeze(&mut self, terminal: ForegroundTerminal) {
        self.frozen_elapsed = Some(self.execution_started.elapsed());
        self.terminal = Some(terminal);
    }

    fn elapsed(&self) -> Duration {
        self.frozen_elapsed
            .unwrap_or_else(|| self.execution_started.elapsed())
    }
}

/// Input phase of the post-spawn PTY lifecycle: the optional input write
/// joins the same timeout/abort select as the command run. A write error
/// terminates and propagates; a timeout/abort decision freezes the elapsed
/// measurement and terminates the process before the drain phase.
async fn run_input_phase(
    process: &mut impl ForegroundPty,
    input: Option<Vec<u8>>,
    decision: &mut ForegroundDecision,
    abort: &pi_agent::AbortSignal,
    timeout: &mut (impl Future<Output = ()> + Unpin),
) -> Result<()> {
    match input {
        Some(input) => {
            tokio::select! {
                result = process.write_input(input) => match result {
                    Ok(()) => process.close_stdin(),
                    Err(error) => {
                        process.terminate().await;
                        return Err(error);
                    }
                },
                _ = abort.cancelled() => {
                    decision.freeze(ForegroundTerminal::Cancelled);
                    process.terminate().await;
                }
                _ = &mut *timeout => {
                    decision.freeze(ForegroundTerminal::TimedOut);
                    process.terminate().await;
                }
            }
        }
        None => process.close_stdin(),
    }
    Ok(())
}

/// Drain phase of the post-spawn PTY lifecycle: polls output and exit
/// events until the exit code is observed and the output channel closes,
/// streaming every chunk. Until a terminal decision exists, the shared
/// timeout/abort select can decide the outcome — freezing elapsed at the
/// decision and terminating before the remaining output drains.
async fn run_drain_phase(
    process: &mut impl ForegroundPty,
    decision: &mut ForegroundDecision,
    abort: &pi_agent::AbortSignal,
    timeout: &mut (impl Future<Output = ()> + Unpin),
    stream: &Arc<dyn Fn(&[u8]) + Send + Sync>,
) {
    let mut exit_code: Option<i32> = None;
    let mut output_open = true;
    while decision.terminal.is_none() || exit_code.is_none() || output_open {
        let event = tokio::select! {
            event = process.next_event(output_open, exit_code.is_none()) => event,
            _ = &mut *timeout, if decision.terminal.is_none() => {
                decision.freeze(ForegroundTerminal::TimedOut);
                process.terminate().await;
                continue;
            }
            _ = abort.cancelled(), if decision.terminal.is_none() => {
                decision.freeze(ForegroundTerminal::Cancelled);
                process.terminate().await;
                continue;
            }
        };
        match event {
            ForegroundEvent::Output(Some((_, bytes))) => stream(&bytes),
            ForegroundEvent::Output(None) => output_open = false,
            ForegroundEvent::Exit(code) => {
                exit_code = Some(code);
                if decision.terminal.is_none() {
                    decision.freeze(ForegroundTerminal::Exit(code));
                }
            }
        }
    }
    debug_assert!(exit_code.is_some());
}

/// Drives the post-spawn PTY lifecycle: the optional input write joins the
/// same timeout/abort select as the command run, `execution_elapsed` freezes
/// at the first terminal decision (before termination cleanup and output
/// drain), and output is preserved until exit + channel closure. The pinned
/// timeout future spans both phases, so a deadline not consumed by the
/// input write keeps bounding the drain phase.
async fn run_foreground_pty_lifecycle(
    process: &mut impl ForegroundPty,
    input: Option<Vec<u8>>,
    timeout: Option<Duration>,
    abort: &pi_agent::AbortSignal,
    stream: &Arc<dyn Fn(&[u8]) + Send + Sync>,
    execution_started: Instant,
) -> Result<ForegroundShellOutput> {
    let timeout = async {
        match timeout {
            Some(duration) => tokio::time::sleep(duration).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(timeout);
    let mut decision = ForegroundDecision::new(execution_started);
    run_input_phase(process, input, &mut decision, abort, &mut timeout).await?;
    run_drain_phase(process, &mut decision, abort, &mut timeout, stream).await;
    let terminal = decision
        .terminal
        .expect("drain loop exits only after a terminal decision");
    let (exit_code, timed_out, cancelled) = terminal.classify(abort.is_aborted());
    Ok(ForegroundShellOutput {
        exit_code,
        timed_out,
        cancelled,
        execution_elapsed: Some(decision.elapsed()),
    })
}

pub(crate) struct GovernedProcessSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub environment: BTreeMap<String, Option<String>>,
    pub timeout: Duration,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    #[cfg(unix)]
    pub mount_sources: Vec<crate::fs::CapabilitySpawnSource>,
    #[cfg(unix)]
    pub retained_executable: Option<crate::fs::CapabilityExecutableSource>,
    #[cfg(unix)]
    pub retained_working_directory: Option<crate::fs::CapabilityWorkingDirectorySource>,
}
pub(crate) struct ForegroundShellSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub environment: BTreeMap<String, Option<String>>,
    pub terminal_size: ProcessTerminalSize,
    pub input: Option<Vec<u8>>,
    pub timeout: Option<Duration>,
    pub abort: pi_agent::AbortSignal,
    pub stream: Arc<dyn Fn(&[u8]) + Send + Sync>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ForegroundShellOutput {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    /// Monotonic measured execution duration from the spawn boundary;
    /// `None` only when the launch never happened (pre-spawn abort).
    pub execution_elapsed: Option<Duration>,
}

impl ForegroundShellOutput {
    fn cancelled() -> Self {
        Self {
            exit_code: None,
            timed_out: false,
            cancelled: true,
            execution_elapsed: None,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernedProcessOutput {
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub stdout_capped: bool,
    pub stderr_capped: bool,
}

impl GovernedProcessOutput {
    fn cancelled() -> Self {
        Self {
            status: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: false,
            cancelled: true,
            stdout_capped: false,
            stderr_capped: false,
        }
    }
}

struct BoundedRead {
    bytes: Vec<u8>,
    capped: bool,
}

async fn read_bounded(
    mut reader: tokio::fs::File,
    limit: usize,
    capped_tx: tokio::sync::mpsc::Sender<()>,
) -> Result<BoundedRead> {
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut chunk = [0_u8; 8192];
    let mut capped = false;
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            return Ok(BoundedRead { bytes, capped });
        }
        if capped {
            continue;
        }
        let room = limit.saturating_sub(bytes.len());
        bytes.extend_from_slice(&chunk[..count.min(room)]);
        if count > room {
            capped = true;
            let _ = capped_tx.send(()).await;
        }
    }
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}
#[cfg(test)]
mod foreground_pty_lifecycle_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use parking_lot::Mutex;

    use super::{ForegroundEvent, ForegroundPty, run_foreground_pty_lifecycle};
    use crate::process::ProcessStream;

    /// Deterministic fake: no huge input, no real backend, only small real
    /// timer windows so every assertion stays under a few hundred
    /// milliseconds.
    struct FakePty {
        write_hangs: bool,
        write_fails: bool,
        aborts_on_terminate: Option<Box<dyn Fn() + Send>>,
        drain_block: Option<Duration>,
        chunk: Option<(ProcessStream, Vec<u8>)>,
        exit_code: Option<i32>,
        written: Vec<u8>,
        close_events: usize,
        terminate_events: usize,
    }

    impl FakePty {
        fn exiting(exit_code: i32, chunk: Vec<u8>) -> Self {
            Self {
                exit_code: Some(exit_code),
                chunk: Some((ProcessStream::Stdout, chunk)),
                ..Self::default()
            }
        }
    }

    impl Default for FakePty {
        fn default() -> Self {
            Self {
                write_hangs: false,
                write_fails: false,
                aborts_on_terminate: None,
                drain_block: None,
                chunk: None,
                exit_code: None,
                written: Vec::new(),
                close_events: 0,
                terminate_events: 0,
            }
        }
    }

    impl ForegroundPty for FakePty {
        async fn write_input(&mut self, bytes: Vec<u8>) -> anyhow::Result<()> {
            if self.write_fails {
                anyhow::bail!("fake stdin write failed");
            }
            if self.write_hangs {
                std::future::pending::<()>().await;
            }
            self.written.extend_from_slice(&bytes);
            Ok(())
        }

        fn close_stdin(&mut self) {
            self.close_events += 1;
        }

        async fn next_event(
            &mut self,
            output_open: bool,
            exit_pending: bool,
        ) -> ForegroundEvent {
            if output_open {
                if let Some(chunk) = self.chunk.take() {
                    return ForegroundEvent::Output(Some(chunk));
                }
                if let Some(drain_block) = self.drain_block.take() {
                    tokio::time::sleep(drain_block).await;
                }
                return ForegroundEvent::Output(None);
            }
            assert!(
                exit_pending,
                "drain loop requests an event only with a pending branch"
            );
            ForegroundEvent::Exit(self.exit_code.unwrap_or(-1))
        }

        async fn terminate(&mut self) {
            self.terminate_events += 1;
            if let Some(abort) = &self.aborts_on_terminate {
                abort();
            }
        }
    }

    fn capturing_stream() -> (Arc<dyn Fn(&[u8]) + Send + Sync>, Arc<Mutex<Vec<Vec<u8>>>>) {
        let stream_captures = Arc::new(Mutex::new(Vec::new()));
        let captures = stream_captures.clone();
        (
            Arc::new(move |bytes: &[u8]| captures.lock().push(bytes.to_vec())),
            stream_captures,
        )
    }

    #[tokio::test]
    async fn natural_run_closes_stdin_once_preserves_output_and_freezes_at_exit() {
        let (stream, captures) = capturing_stream();
        let mut fake = FakePty::exiting(0, b"hello".to_vec());
        let process = run_foreground_pty_lifecycle(
            &mut fake,
            Some(b"hi\n".to_vec()),
            None,
            &pi_agent::AbortSignal::none(),
            &stream,
            std::time::Instant::now(),
        )
        .await
        .expect("natural run succeeds");
        assert_eq!(fake.written, b"hi\n");
        assert_eq!(fake.close_events, 1);
        assert_eq!(fake.terminate_events, 0);
        assert_eq!(process.exit_code, Some(0));
        assert!(!process.timed_out && !process.cancelled);
        assert!(process.execution_elapsed.is_some());
        assert_eq!(captures.lock().as_slice(), [b"hello".to_vec()]);
    }

    #[tokio::test]
    async fn timeout_bounds_blocked_input_write_and_freezes_elapsed() {
        let (stream, captures) = capturing_stream();
        let timeout = Duration::from_millis(50);
        let mut fake = FakePty::exiting(143, b"partial".to_vec());
        fake.write_hangs = true;
        fake.drain_block = Some(Duration::from_millis(200));
        let process = run_foreground_pty_lifecycle(
            &mut fake,
            Some(vec![1]),
            Some(timeout),
            &pi_agent::AbortSignal::none(),
            &stream,
            std::time::Instant::now(),
        )
        .await
        .expect("timeout path completes");
        assert!(fake.written.is_empty());
        assert_eq!(fake.close_events, 0);
        assert_eq!(fake.terminate_events, 1);
        assert!(process.timed_out && !process.cancelled);
        assert_eq!(process.exit_code, None);
        // Elapsed freezes at the timeout decision (~50ms), NOT after the
        // 200ms drain; a non-frozen measurement would be >= 250ms.
        let elapsed = process.execution_elapsed.expect("measured");
        assert!(elapsed >= timeout - Duration::from_millis(8));
        assert!(elapsed < timeout + Duration::from_millis(50));
        // Output still streamed after the timeout decision.
        assert_eq!(captures.lock().as_slice(), [b"partial".to_vec()]);
    }

    #[tokio::test]
    async fn input_success_keeps_same_pinned_deadline_into_drain() {
        let (stream, captures) = capturing_stream();
        let timeout = Duration::from_millis(50);
        let mut fake = FakePty::exiting(143, b"late".to_vec());
        fake.drain_block = Some(Duration::from_millis(200));
        let process = run_foreground_pty_lifecycle(
            &mut fake,
            Some(b"quick\n".to_vec()),
            Some(timeout),
            &pi_agent::AbortSignal::none(),
            &stream,
            std::time::Instant::now(),
        )
        .await
        .expect("pinned-deadline path completes");
        // The input write succeeded immediately, so the SAME pinned timeout
        // future must carry into the drain phase. Elapsed freezes near the
        // original 50ms deadline even though the drain then blocked for
        // 200ms; a deadline restarted at drain entry would measure >= 250ms.
        assert_eq!(fake.written, b"quick\n");
        assert_eq!(fake.close_events, 1);
        assert_eq!(fake.terminate_events, 1);
        assert!(process.timed_out && !process.cancelled);
        assert_eq!(process.exit_code, None);
        let elapsed = process.execution_elapsed.expect("measured");
        assert!(elapsed >= timeout - Duration::from_millis(8));
        assert!(elapsed < timeout + Duration::from_millis(50));
        // Output still drained after the drain-phase timeout decision.
        assert_eq!(captures.lock().as_slice(), [b"late".to_vec()]);
    }

    #[tokio::test]
    async fn timeout_with_abort_during_cleanup_classifies_cancelled() {
        let (stream, captures) = capturing_stream();
        let timeout = Duration::from_millis(50);
        let (abort_source, abort) = pi_agent::AbortController::new();
        let mut fake = FakePty::exiting(137, b"cleanup".to_vec());
        fake.aborts_on_terminate = Some(Box::new(move || abort_source.abort()));
        fake.drain_block = Some(Duration::from_millis(200));
        let process = run_foreground_pty_lifecycle(
            &mut fake,
            Some(vec![1]),
            Some(timeout),
            &abort,
            &stream,
            std::time::Instant::now(),
        )
        .await
        .expect("timeout with cleanup abort completes");
        // Timeout selected, abort fired during termination cleanup: final
        // classification is cancelled.
        assert!(process.cancelled && !process.timed_out);
        assert_eq!(process.exit_code, None);
        assert_eq!(fake.terminate_events, 1);
        // Elapsed stays frozen at the timeout decision, before the drain.
        let elapsed = process.execution_elapsed.expect("measured");
        assert!(elapsed >= timeout - Duration::from_millis(8));
        assert!(elapsed < timeout + Duration::from_millis(50));
        assert_eq!(captures.lock().as_slice(), [b"cleanup".to_vec()]);
    }

    #[tokio::test]
    async fn abort_bounds_blocked_input_write() {
        let (abort_source, abort) = pi_agent::AbortController::new();
        let (stream, _captures) = capturing_stream();
        let mut fake = FakePty::default();
        fake.write_hangs = true;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            abort_source.abort();
        });
        let process = run_foreground_pty_lifecycle(
            &mut fake,
            Some(vec![1]),
            None,
            &abort,
            &stream,
            std::time::Instant::now(),
        )
        .await
        .expect("abort path completes");
        assert!(process.cancelled && !process.timed_out);
        assert_eq!(process.exit_code, None);
        assert!(fake.written.is_empty());
        assert_eq!(fake.close_events, 0);
        assert_eq!(fake.terminate_events, 1);
    }

    #[tokio::test]
    async fn write_error_terminates_and_propagates() {
        let (stream, _captures) = capturing_stream();
        let mut fake = FakePty::default();
        fake.write_fails = true;
        let outcome = run_foreground_pty_lifecycle(
            &mut fake,
            Some(vec![1]),
            None,
            &pi_agent::AbortSignal::none(),
            &stream,
            std::time::Instant::now(),
        )
        .await;
        assert!(outcome.is_err());
        assert!(fake.written.is_empty());
        assert_eq!(fake.close_events, 0);
        assert_eq!(fake.terminate_events, 1);
    }
}
