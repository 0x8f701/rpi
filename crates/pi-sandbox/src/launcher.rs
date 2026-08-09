use std::ffi::OsStr;
use std::fs::Metadata;
use std::io::{ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::SandboxError;

const BWRAP_PROGRAM: &str = "bwrap";
const PROBE_POLL_INTERVAL: Duration = Duration::from_millis(20);
const PROBE_STDERR_LIMIT: usize = 64 * 1024;
const SYSTEM_TRUST_ROOT: &str = "/";
const SYSTEM_TRUST_UID: u32 = 0;
const USER_NAMESPACE_FAILURES: [&str; 4] = [
    "loopback: Failed RTM_NEWADDR",
    "loopback: Failed RTM_NEWLINK",
    "setting up uid map: Permission denied",
    "No permissions to create a new namespace",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    Available,
    UntrustedExecutable { message: String },
    UserNamespacesUnavailable { stderr: String },
    Exited { code: Option<i32>, stderr: String },
    TimedOut { stderr: String },
    SpawnFailed { message: String },
    WaitFailed { message: String, stderr: String },
}

impl ProbeOutcome {
    #[must_use]
    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TrustedExecutable {
    path: PathBuf,
    trust: TrustPolicy,
    identity: ExecutableIdentity,
}

impl TrustedExecutable {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn revalidate(&self) -> bool {
        trusted_executable(&self.path, &self.trust, None).is_some_and(|current| {
            current.path == self.path && current.identity == self.identity
        })
    }
}

#[derive(Clone, Debug)]
struct TrustPolicy {
    root: PathBuf,
    owner_uid: u32,
}

impl TrustPolicy {
    fn system() -> Option<Self> {
        Self::new(Path::new(SYSTEM_TRUST_ROOT), SYSTEM_TRUST_UID)
    }

    fn new(root: &Path, owner_uid: u32) -> Option<Self> {
        if !root.is_absolute() {
            return None;
        }
        let root = std::fs::canonicalize(root).ok()?;
        let metadata = std::fs::symlink_metadata(&root).ok()?;
        (metadata.is_dir() && metadata_is_secure(&metadata, owner_uid)).then_some(Self {
            root,
            owner_uid,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExecutableIdentity {
    device: u64,
    inode: u64,
    mode: u32,
    owner_uid: u32,
    owner_gid: u32,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl From<&Metadata> for ExecutableIdentity {
    fn from(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            owner_uid: metadata.uid(),
            owner_gid: metadata.gid(),
            size: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }
}

pub fn find_system_bwrap(search_path: Option<&OsStr>, cwd: &Path) -> Option<PathBuf> {
    discover_system_bwrap(search_path, cwd).map(|executable| executable.path)
}

pub(crate) fn discover_system_bwrap(
    search_path: Option<&OsStr>,
    cwd: &Path,
) -> Option<TrustedExecutable> {
    discover_bwrap_with_trust(search_path, cwd, TrustPolicy::system()?)
}

fn discover_bwrap_with_trust(
    search_path: Option<&OsStr>,
    cwd: &Path,
    trust: TrustPolicy,
) -> Option<TrustedExecutable> {
    let search_path = search_path?;
    let cwd = std::fs::canonicalize(cwd).ok()?;
    std::env::split_paths(search_path).find_map(|directory| {
        if !source_path_is_trusted(&directory, &trust) {
            return None;
        }
        trusted_executable(&directory.join(BWRAP_PROGRAM), &trust, Some(&cwd))
    })
}

pub fn probe_bwrap(path: &Path, timeout: Duration) -> ProbeOutcome {
    let Some(trust) = TrustPolicy::system() else {
        return untrusted_outcome(path);
    };
    let Some(executable) = trusted_executable(path, &trust, None) else {
        return untrusted_outcome(path);
    };
    probe_trusted_bwrap(&executable, timeout)
}

pub(crate) fn probe_trusted_bwrap(
    executable: &TrustedExecutable,
    timeout: Duration,
) -> ProbeOutcome {
    if !executable.revalidate() {
        return untrusted_outcome(executable.path());
    }
    let mut child = match Command::new(executable.path())
        .args([
            "--unshare-user",
            "--unshare-net",
            "--ro-bind",
            "/",
            "/",
            "--perms",
            "000",
            "--tmpfs",
            "/tmp",
            "/bin/true",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return ProbeOutcome::SpawnFailed {
                message: error.to_string(),
            };
        }
    };
    let mut stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return ProbeOutcome::WaitFailed {
                message: "probe stderr pipe was unavailable".to_owned(),
                stderr: String::new(),
            };
        }
    };
    if let Err(error) = set_nonblocking(&stderr) {
        let _ = child.kill();
        let _ = child.wait();
        return ProbeOutcome::WaitFailed {
            message: error.to_string(),
            stderr: String::new(),
        };
    }

    let deadline = Instant::now() + timeout;
    let mut stderr_bytes = Vec::new();
    loop {
        drain_stderr(&mut stderr, &mut stderr_bytes);
        match child.try_wait() {
            Ok(Some(status)) => {
                drain_stderr(&mut stderr, &mut stderr_bytes);
                let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();
                if status.success() {
                    return ProbeOutcome::Available;
                }
                if USER_NAMESPACE_FAILURES
                    .iter()
                    .any(|failure| stderr.contains(failure))
                {
                    return ProbeOutcome::UserNamespacesUnavailable { stderr };
                }
                return ProbeOutcome::Exited {
                    code: status.code(),
                    stderr,
                };
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(PROBE_POLL_INTERVAL);
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                drain_stderr(&mut stderr, &mut stderr_bytes);
                return ProbeOutcome::TimedOut {
                    stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
                };
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                drain_stderr(&mut stderr, &mut stderr_bytes);
                return ProbeOutcome::WaitFailed {
                    message: error.to_string(),
                    stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
                };
            }
        }
    }
}

fn trusted_executable(
    path: &Path,
    trust: &TrustPolicy,
    rejected_tree: Option<&Path>,
) -> Option<TrustedExecutable> {
    if !source_path_is_trusted(path, trust) {
        return None;
    }
    let path = std::fs::canonicalize(path).ok()?;
    if !path.is_absolute()
        || !path.starts_with(&trust.root)
        || rejected_tree.is_some_and(|tree| path.starts_with(tree))
        || !canonical_path_is_trusted(&path, trust)
    {
        return None;
    }
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file()
        || metadata.mode() & 0o111 == 0
        || !metadata_is_secure(&metadata, trust.owner_uid)
    {
        return None;
    }
    let identity = ExecutableIdentity::from(&metadata);
    Some(TrustedExecutable {
        path,
        trust: trust.clone(),
        identity,
    })
}

fn source_path_is_trusted(path: &Path, trust: &TrustPolicy) -> bool {
    if !path.is_absolute() || !path.starts_with(&trust.root) {
        return false;
    }
    path_chain_is_trusted(path, trust, true)
}

fn canonical_path_is_trusted(path: &Path, trust: &TrustPolicy) -> bool {
    path_chain_is_trusted(path, trust, false)
}

fn path_chain_is_trusted(path: &Path, trust: &TrustPolicy, allow_symlinks: bool) -> bool {
    let mut current = path;
    loop {
        let Ok(metadata) = std::fs::symlink_metadata(current) else {
            return false;
        };
        if (!allow_symlinks && metadata.file_type().is_symlink())
            || !metadata_is_secure(&metadata, trust.owner_uid)
        {
            return false;
        }
        // The configured root is the trust anchor: validate the root itself,
        // but not ancestors outside its boundary. The system root is `/`, so
        // production policy still validates the complete filesystem chain.
        if current == trust.root {
            return true;
        }
        let Some(parent) = current.parent() else {
            return false;
        };
        if !parent.starts_with(&trust.root) {
            return false;
        }
        current = parent;
    }
}

fn metadata_is_secure(metadata: &Metadata, owner_uid: u32) -> bool {
    metadata.uid() == owner_uid
        && (metadata.file_type().is_symlink() || metadata.mode() & 0o022 == 0)
}

fn untrusted_outcome(path: &Path) -> ProbeOutcome {
    ProbeOutcome::UntrustedExecutable {
        message: format!(
            "bubblewrap executable is not a canonical, securely owned executable in a trusted path: {}",
            path.display()
        ),
    }
}

#[cfg(test)]
pub(crate) fn find_system_bwrap_with_trust(
    search_path: Option<&OsStr>,
    cwd: &Path,
    trusted_root: &Path,
    owner_uid: u32,
) -> Option<PathBuf> {
    discover_bwrap_with_trust(
        search_path,
        cwd,
        TrustPolicy::new(trusted_root, owner_uid)?,
    )
    .map(|executable| executable.path)
}

#[cfg(test)]
pub(crate) fn discover_bwrap_with_trust_for_test(
    search_path: Option<&OsStr>,
    cwd: &Path,
    trusted_root: &Path,
    owner_uid: u32,
) -> Option<TrustedExecutable> {
    discover_bwrap_with_trust(
        search_path,
        cwd,
        TrustPolicy::new(trusted_root, owner_uid)?,
    )
}

#[cfg(test)]
pub(crate) fn probe_bwrap_with_trust(
    path: &Path,
    trusted_root: &Path,
    owner_uid: u32,
    timeout: Duration,
) -> ProbeOutcome {
    let Some(trust) = TrustPolicy::new(trusted_root, owner_uid) else {
        return untrusted_outcome(path);
    };
    let Some(executable) = trusted_executable(path, &trust, None) else {
        return untrusted_outcome(path);
    };
    probe_trusted_bwrap(&executable, timeout)
}

#[must_use]
pub fn proc_version_indicates_wsl1(proc_version: &str) -> bool {
    let proc_version = proc_version.to_ascii_lowercase();
    let mut remaining = proc_version.as_str();
    while let Some(marker) = remaining.find("wsl") {
        let version_start = marker + "wsl".len();
        let version_digits = remaining[version_start..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>();
        if let Ok(version) = version_digits.parse::<u32>() {
            return version == 1;
        }
        remaining = &remaining[version_start..];
    }
    proc_version.contains("microsoft") && !proc_version.contains("microsoft-standard")
}

fn drain_stderr(stderr: &mut ChildStderr, bytes: &mut Vec<u8>) {
    let mut buffer = [0_u8; 4096];
    loop {
        match stderr.read(&mut buffer) {
            Ok(0) => return,
            Ok(read) => {
                let remaining = PROBE_STDERR_LIMIT.saturating_sub(bytes.len());
                bytes.extend_from_slice(&buffer[..read.min(remaining)]);
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => return,
            Err(_) => return,
        }
    }
}

#[allow(unsafe_code)]
fn set_nonblocking(stderr: &ChildStderr) -> Result<(), SandboxError> {
    // SAFETY: fcntl reads and updates flags for a live owned pipe descriptor.
    // No pointer is passed and the descriptor remains owned by ChildStderr.
    let flags = unsafe { libc::fcntl(stderr.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(SandboxError::System(std::io::Error::last_os_error()));
    }
    // SAFETY: flags came from F_GETFL for the same live descriptor; O_NONBLOCK
    // is the only bit added.
    let result = unsafe {
        libc::fcntl(
            stderr.as_raw_fd(),
            libc::F_SETFL,
            flags | libc::O_NONBLOCK,
        )
    };
    if result < 0 {
        return Err(SandboxError::System(std::io::Error::last_os_error()));
    }
    Ok(())
}
