#![deny(unsafe_code)]
//! Reusable Linux bubblewrap and seccomp primitives adapted from OpenAI Codex.
//!
//! This crate is library-only. It plans a read-only-by-default bubblewrap
//! filesystem, discovers and probes a trusted system bubblewrap executable, and
//! applies current-thread `no_new_privs` plus Codex-derived seccomp rules. It is
//! deliberately not wired into any pi-rs runtime crate.
//!
//! Adapted and modified from `codex-rs/linux-sandbox/src/{bwrap,landlock,launcher}.rs`
//! and `codex-rs/sandboxing/src/bwrap.rs` at commit
//! `646f7c0a91b8e327d263335da68ae8ef212895ce`. Apache-2.0; see
//! `LICENSE-APACHE` and `NOTICE`.

use std::path::{Path, PathBuf};
use std::time::Duration;

mod bwrap;
mod policy;

pub use bwrap::{BubblewrapPlan, build_bubblewrap_arguments};
pub use policy::{FileSystemPolicy, NetworkMode, WritableRoot};

pub const UPSTREAM_CODEX_COMMIT: &str = "646f7c0a91b8e327d263335da68ae8ef212895ce";
pub const DEFAULT_BWRAP_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlatformSupport {
    Linux,
    Unsupported { reason: &'static str },
}

#[must_use]
pub const fn platform_support() -> PlatformSupport {
    if !cfg!(target_os = "linux") {
        PlatformSupport::Unsupported {
            reason: "sandbox primitives require Linux",
        }
    } else if cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
        PlatformSupport::Linux
    } else {
        PlatformSupport::Unsupported {
            reason: "seccomp supports only x86_64 and aarch64 Linux targets",
        }
    }
}

#[derive(Debug)]
pub enum SandboxError {
    UnsupportedPlatform,
    UnsupportedArchitecture,
    Wsl1Unsupported,
    BubblewrapNotFound,
    BubblewrapProbeFailed(ProbeOutcome),
    EmptyCommand,
    PathNotAbsolute(PathBuf),
    PathNotNormalized(PathBuf),
    SymlinkedPolicyPath(PathBuf),
    ProtectedPathOutsideWritableRoot { root: PathBuf, protected: PathBuf },
    NonUtf8Path(PathBuf),
    System(std::io::Error),
    Seccomp(String),
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedPlatform => write!(formatter, "sandbox primitives are unsupported on this platform"),
            Self::UnsupportedArchitecture => write!(formatter, "seccomp supports only x86_64 and aarch64 Linux targets"),
            Self::Wsl1Unsupported => write!(formatter, "bubblewrap sandboxing is unsupported on WSL1; use WSL2"),
            Self::BubblewrapNotFound => write!(formatter, "bubblewrap was not found on a trusted PATH entry"),
            Self::BubblewrapProbeFailed(outcome) => write!(formatter, "bubblewrap capability probe failed: {outcome:?}"),
            Self::EmptyCommand => write!(formatter, "sandbox command must not be empty"),
            Self::PathNotAbsolute(path) => write!(formatter, "sandbox path must be absolute: {}", path.display()),
            Self::PathNotNormalized(path) => write!(formatter, "sandbox path must not contain dot components: {}", path.display()),
            Self::SymlinkedPolicyPath(path) => write!(formatter, "sandbox policy path crosses symlink: {}", path.display()),
            Self::ProtectedPathOutsideWritableRoot { root, protected } => write!(formatter, "protected path {} is not a strict descendant of writable root {}", protected.display(), root.display()),
            Self::NonUtf8Path(path) => write!(formatter, "sandbox path is not valid UTF-8: {}", path.display()),
            Self::System(error) => write!(formatter, "sandbox host operation failed: {error}"),
            Self::Seccomp(error) => write!(formatter, "seccomp enforcement failed: {error}"),
        }
    }
}

impl std::error::Error for SandboxError {}

#[cfg(target_os = "linux")]
mod launcher;
#[cfg(target_os = "linux")]
mod seccomp;

#[cfg(target_os = "linux")]
pub use launcher::{ProbeOutcome, find_system_bwrap, probe_bwrap, proc_version_indicates_wsl1};
#[cfg(target_os = "linux")]
pub use seccomp::{
    apply_current_thread_restrictions, install_seccomp_current_thread,
    set_no_new_privs_current_thread,
};

#[cfg(not(target_os = "linux"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    UnsupportedPlatform,
}

#[cfg(not(target_os = "linux"))]
pub fn find_system_bwrap(
    search_path: Option<&std::ffi::OsStr>,
    cwd: &Path,
) -> Option<PathBuf> {
    let _ = (search_path, cwd);
    None
}

#[cfg(not(target_os = "linux"))]
pub fn probe_bwrap(path: &Path, timeout: Duration) -> ProbeOutcome {
    let _ = (path, timeout);
    ProbeOutcome::UnsupportedPlatform
}

#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn proc_version_indicates_wsl1(proc_version: &str) -> bool {
    let _ = proc_version;
    false
}

pub fn plan_system_bubblewrap(
    command: &[String],
    command_cwd: &Path,
    policy: &FileSystemPolicy,
    network: NetworkMode,
) -> Result<BubblewrapPlan, SandboxError> {
    plan_system_bubblewrap_with(command, command_cwd, policy, network, None)
}

#[cfg(target_os = "linux")]
fn plan_system_bubblewrap_with(
    command: &[String],
    command_cwd: &Path,
    policy: &FileSystemPolicy,
    network: NetworkMode,
    search_path: Option<&std::ffi::OsStr>,
) -> Result<BubblewrapPlan, SandboxError> {
    let proc_version = std::fs::read_to_string("/proc/version").map_err(SandboxError::System)?;
    if proc_version_indicates_wsl1(&proc_version) {
        return Err(SandboxError::Wsl1Unsupported);
    }
    let owned_search_path;
    let search_path = match search_path {
        Some(search_path) => Some(search_path),
        None => {
            owned_search_path = std::env::var_os("PATH");
            owned_search_path.as_deref()
        }
    };
    let executable = launcher::discover_system_bwrap(search_path, command_cwd)
        .ok_or(SandboxError::BubblewrapNotFound)?;
    let outcome = launcher::probe_trusted_bwrap(&executable, DEFAULT_BWRAP_PROBE_TIMEOUT);
    if !outcome.is_available() {
        return Err(SandboxError::BubblewrapProbeFailed(outcome));
    }
    let arguments = build_bubblewrap_arguments(command, command_cwd, policy, network)?;
    if !executable.revalidate() {
        return Err(SandboxError::BubblewrapNotFound);
    }
    Ok(BubblewrapPlan {
        program: executable.path().to_path_buf(),
        arguments,
    })
}

#[cfg(not(target_os = "linux"))]
fn plan_system_bubblewrap_with(
    command: &[String],
    command_cwd: &Path,
    policy: &FileSystemPolicy,
    network: NetworkMode,
    search_path: Option<&std::ffi::OsStr>,
) -> Result<BubblewrapPlan, SandboxError> {
    let _ = (command, command_cwd, policy, network, search_path);
    Err(SandboxError::UnsupportedPlatform)
}

#[cfg(not(target_os = "linux"))]
pub fn set_no_new_privs_current_thread() -> Result<(), SandboxError> {
    Err(SandboxError::UnsupportedPlatform)
}

#[cfg(not(target_os = "linux"))]
pub fn install_seccomp_current_thread(network: NetworkMode) -> Result<(), SandboxError> {
    let _ = network;
    Err(SandboxError::UnsupportedPlatform)
}

#[cfg(not(target_os = "linux"))]
pub fn apply_current_thread_restrictions(network: NetworkMode) -> Result<(), SandboxError> {
    let _ = network;
    Err(SandboxError::UnsupportedPlatform)
}

#[cfg(test)]
mod tests;
