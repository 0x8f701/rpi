#![deny(unsafe_code)]
//! Validated sandbox policy, coverage, and sealed platform launch primitives.

mod coverage;
mod error;
mod policy;

pub use coverage::{CoverageRoute, SpawnFamily, covers};
pub use error::{
    ArchitectureKind, ChildProtocolError, LauncherKind, LauncherTrustFailure,
    PolicyRepresentabilityError, PolicyValidationError, SandboxError, SandboxIoKind,
    SandboxIoOperation, SpawnValidationError,
};
pub use policy::{
    AbsolutePath, CredentialGrantId, CredentialPolicy, DnsName, EnvironmentVariableName,
    FileSystemPolicy, HostHttpClass, HostHttpDestination, HostHttpDestinationKey, HostHttpHost,
    HostHttpPolicy, HostHttpScheme, HostPlatform, IpScope, NetworkPolicy, PolicyDigest,
    PolicyRevision, PolicySnapshot, RelativePath, RequestedPolicy, ResourceLimits, SandboxMode,
    SandboxPolicy, SessionScope, SharedPolicySnapshot, WritableRoot,
    agent_overlaps_generated_write, required_workspace_protected_descendants,
    resolve_policy_snapshot,
};

mod backend;
mod platform;
mod spawn;

#[cfg(unix)]
pub use backend::probe_backend_with_revision_roots;
pub use backend::{SandboxBackend, probe_backend};
pub use platform::{PlatformFactInputs, PlatformFacts, derive_platform_facts};
pub use spawn::{CanonicalTargetFdMapping, StdioMode, TargetFdDirection, TerminalSize};

#[cfg(unix)]
mod child;
#[cfg(unix)]
mod launcher;
#[cfg(unix)]
mod prepare;
#[cfg(unix)]
mod unix;
#[cfg(unix)]
mod synthetic;

#[cfg(unix)]
pub use child::exec_child_target;
#[cfg(unix)]
pub use prepare::{PreparedSpawn, prepare_spawn};
#[cfg(unix)]
pub use spawn::{
    MountSource, PipeStdio, PreparedSpawnRequest, RetainedExecutable, RetainedRevisionRoots,
    RetainedWorkingDirectory, SpawnStdio, TargetFd, TargetFdMapping,
};
#[cfg(unix)]
pub use unix::{
    LaunchedUnixHandle, LaunchedUnixProcess, LaunchedUnixPty, UnixExitStatus, UnixPtyMaster,
    launch_prepared_unix,
};

#[cfg(target_os = "linux")]
mod bwrap;
#[cfg(target_os = "linux")]
mod seccomp;
#[cfg(target_os = "linux")]
pub use seccomp::{
    apply_current_thread_restrictions, install_seccomp_current_thread,
    set_no_new_privs_current_thread,
};

#[cfg(not(target_os = "linux"))]
pub fn set_no_new_privs_current_thread() -> Result<(), SandboxError> {
    Err(SandboxError::UnsupportedArchitecture {
        platform: current_host_platform(),
        architecture: ArchitectureKind::Other,
    })
}

#[cfg(not(target_os = "linux"))]
pub fn install_seccomp_current_thread(network: NetworkPolicy) -> Result<(), SandboxError> {
    let _ = network;
    Err(SandboxError::UnsupportedArchitecture {
        platform: current_host_platform(),
        architecture: ArchitectureKind::Other,
    })
}

#[cfg(not(target_os = "linux"))]
pub fn apply_current_thread_restrictions(network: NetworkPolicy) -> Result<(), SandboxError> {
    install_seccomp_current_thread(network)
}

/// Best-effort process hardening. Safe to call from a constructor, the
/// sandbox-child entry, and ordinary CLI fallthrough: the syscalls run at
/// most once per process.
///
/// Linux: `PR_SET_DUMPABLE=0` and `RLIMIT_CORE=0`. Other unix: `RLIMIT_CORE=0`
/// where supported. Failures are ignored.
#[cfg(unix)]
pub fn harden_current_process() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static HARDENED: AtomicBool = AtomicBool::new(false);
    if HARDENED.swap(true, Ordering::SeqCst) {
        return;
    }
    unix::harden_current_process();
}

const fn current_host_platform() -> HostPlatform {
    if cfg!(target_os = "macos") {
        HostPlatform::Macos
    } else if cfg!(target_os = "windows") {
        HostPlatform::Windows
    } else {
        HostPlatform::Linux
    }
}

#[cfg(target_os = "macos")]
mod seatbelt;
