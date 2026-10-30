use std::io::{Error as IoError, ErrorKind};

use thiserror::Error;

use crate::policy::{AbsolutePath, HostPlatform, SandboxMode};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum SandboxIoKind {
    #[error("not found")]
    NotFound,
    #[error("permission denied")]
    PermissionDenied,
    #[error("already exists")]
    AlreadyExists,
    #[error("invalid input")]
    InvalidInput,
    #[error("interrupted")]
    Interrupted,
    #[error("timed out")]
    TimedOut,
    #[error("broken pipe")]
    BrokenPipe,
    #[error("other host error")]
    Other,
}

impl From<ErrorKind> for SandboxIoKind {
    fn from(kind: ErrorKind) -> Self {
        match kind {
            ErrorKind::NotFound => Self::NotFound,
            ErrorKind::PermissionDenied => Self::PermissionDenied,
            ErrorKind::AlreadyExists => Self::AlreadyExists,
            ErrorKind::InvalidInput
            | ErrorKind::InvalidData
            | ErrorKind::InvalidFilename
            | ErrorKind::UnexpectedEof
            | ErrorKind::Unsupported
            | ErrorKind::OutOfMemory
            | ErrorKind::WriteZero => Self::InvalidInput,
            ErrorKind::Interrupted => Self::Interrupted,
            ErrorKind::TimedOut => Self::TimedOut,
            ErrorKind::BrokenPipe => Self::BrokenPipe,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxIoOperation {
    OpenPolicyPath,
    OpenExecutable,
    DuplicateDescriptor,
    InspectDescriptor,
    ProbeLauncher,
    BuildMount,
    CreateChannel,
    Fork,
    MapDescriptor,
    ApplyResourceLimit,
    ApplyNoNewPrivileges,
    ApplySeccomp,
    ChangeDirectory,
    ExecuteWrapper,
    ExecuteTarget,
    WaitChild,
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum PolicyValidationError {
    #[error("invalid field {field}: {reason}")]
    InvalidField {
        field: &'static str,
        reason: &'static str,
    },
    #[error("duplicate normalized entry in {field}")]
    DuplicateEntry { field: &'static str },
    #[error("conflicting policy fields: {left} and {right}")]
    ConflictingFields {
        left: &'static str,
        right: &'static str,
    },
    #[error("resource limit {field} must be in 1..={maximum}")]
    ResourceLimitOutOfRange { field: &'static str, maximum: u64 },
    #[error("policy digest mismatch")]
    DigestMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LauncherKind {
    Bubblewrap,
    SandboxExec,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchitectureKind {
    X86_64,
    Aarch64,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LauncherTrustFailure {
    NotRegular,
    WrongOwner,
    WritableByGroupOrOther,
    UnsafeAncestor,
    IdentityMismatch,
    ProbeTimedOut,
    ProbeRejected,
    VersionTooOld {
        found: (u32, u32, u32),
        required: (u32, u32, u32),
    },
}

impl std::fmt::Display for LauncherTrustFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VersionTooOld { found, required } => write!(
                formatter,
                "installed version {}.{}.{} is older than the required {}.{}.{} (identity-pinned --bind-fd mounts)",
                found.0, found.1, found.2, required.0, required.1, required.2
            ),
            other => write!(formatter, "{other:?}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildProtocolError {
    UnknownVersion,
    Oversize,
    Truncated,
    TrailingBytes,
    InvalidJson,
    DuplicateEnvironment,
    EmptyArgv,
    InvalidCwd,
    EmbeddedNul,
    InvalidRequestId,
    EnforcementMismatch,
    InvalidDescriptor,
    DuplicateDescriptor,
    DescriptorIdentityMismatch,
    UncoveredFamily,
    InvalidTargetMapping,
    InvalidResourceLimit,
    InvalidStatusFrame,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpawnValidationError {
    EmptyArgv,
    EmbeddedNul,
    InvalidCwd,
    InvalidEnvironment,
    DuplicateSourceDescriptor,
    DuplicateTargetDescriptor,
    ReservedTargetDescriptor,
    FamilyStdioMismatch,
    FamilyTargetMappingMismatch,
    OpenFileLimitExceeded,
    InvalidMountSource,
    DuplicateMountDestination,
    InvalidRetainedExecutable,
    InvalidRetainedWorkingDirectory,
    InvalidRevisionRoots,
    MountDestinationOutsideWritableAuthority,
    MountDestinationDenied,
    MountDestinationProtected,
}

impl std::fmt::Display for SpawnValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRevisionRoots => formatter.write_str(
                "revision home/tmp/scratch overlap a writable workspace; launch from a project directory or set sandbox.mode danger-full-access",
            ),
            other => write!(formatter, "{other:?}"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum PolicyRepresentabilityError {
    #[error("required policy root is unavailable")]
    MissingPolicyRoot { path: AbsolutePath },
    #[error("required denied path is unavailable")]
    MissingDeniedPath { path: AbsolutePath },
    #[error("denied path is a symlink and cannot be masked: {path:?}")]
    DeniedSymlink { path: AbsolutePath },
    #[error("required protected descendant is unavailable")]
    MissingProtectedDescendant { path: AbsolutePath },
    #[error("policy mount overlaps a private sandbox projection: {path:?}")]
    PrivateProjectionOverlap { path: AbsolutePath },
    #[error("policy mount overlaps a reserved sandbox executable projection: {path:?}")]
    ReservedExecutableOverlap { path: AbsolutePath },
    #[error("policy mount source is a symlink and cannot be pinned: {path:?}")]
    PolicySymlinkRejected { path: AbsolutePath },
    #[error("target is outside readable policy authority")]
    TargetNotReadable,
    #[error("working directory is outside readable policy authority")]
    CwdNotReadable,
    #[error("home is outside readable policy authority")]
    HomeNotReadable,
    #[error("temporary directory is outside writable policy authority")]
    TmpdirNotWritable,
    #[error("device is not in the fixed confined set")]
    UnsupportedDevice,
    #[error("descriptor execution is unavailable for the fixed launcher path")]
    DescriptorExecUnsupported,
    #[error("Seatbelt profile exceeds {limit} bytes")]
    ProfileTooLarge { bytes: usize, limit: usize },
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum SandboxError {
    #[error("invalid sandbox policy: {0}")]
    Policy(#[from] PolicyValidationError),
    #[error("sandbox mode {mode:?} is unsupported on {platform:?}")]
    UnsupportedMode {
        platform: HostPlatform,
        mode: SandboxMode,
    },
    #[error("sandbox architecture {architecture:?} is unsupported on {platform:?}")]
    UnsupportedArchitecture {
        platform: HostPlatform,
        architecture: ArchitectureKind,
    },
    #[error("proxy-routed child networking is unavailable")]
    ProxyRoutedUnavailable,
    #[error("policy cannot be represented: {reason}")]
    UnrepresentablePolicy { reason: PolicyRepresentabilityError },
    #[error("trusted {launcher:?} launcher is unavailable")]
    LauncherUnavailable { launcher: LauncherKind },
    #[error("trusted {launcher:?} launcher validation failed: {failure}")]
    LauncherUntrusted {
        launcher: LauncherKind,
        failure: LauncherTrustFailure,
    },
    #[error("trusted {launcher:?} launcher identity changed")]
    LauncherChanged { launcher: LauncherKind },
    #[error("bubblewrap does not support identity-pinned mount sources")]
    PinnedMountSourceUnsupported,
    #[error("bubblewrap does not preserve required child descriptors")]
    BubblewrapFdForwardingUnsupported,
    #[error("bubblewrap cannot create the required empty-root mount target")]
    MountTargetCreationUnsupported,
    #[error("atomic close-on-exec channel creation is unavailable")]
    AtomicCloexecUnavailable,
    #[error("sandbox child protocol is invalid: {failure:?}")]
    InvalidChildProtocol { failure: ChildProtocolError },
    #[error("prepared spawn is invalid: {failure}")]
    InvalidSpawn { failure: SpawnValidationError },
    #[error("sandbox host operation {operation:?} failed: {kind}")]
    System {
        operation: SandboxIoOperation,
        kind: SandboxIoKind,
        raw_os_error: Option<i32>,
    },
}

impl SandboxError {
    #[must_use]
    pub fn from_io(operation: SandboxIoOperation, error: &IoError) -> Self {
        Self::System {
            operation,
            kind: SandboxIoKind::from(error.kind()),
            raw_os_error: error.raw_os_error(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_sandbox_error_contract_is_constructible_and_redacted() {
        let io = IoError::new(
            ErrorKind::PermissionDenied,
            "secret-path /etc/shadow token=abc",
        );
        let error = SandboxError::from_io(SandboxIoOperation::OpenExecutable, &io);
        let rendered = error.to_string();
        assert!(!rendered.contains("secret-path"));
        assert!(!rendered.contains("/etc/shadow"));
        assert!(!rendered.contains("token=abc"));
        assert!(rendered.contains("OpenExecutable"));
        assert!(matches!(
            error,
            SandboxError::System {
                operation: SandboxIoOperation::OpenExecutable,
                kind: SandboxIoKind::PermissionDenied,
                raw_os_error: _,
            }
        ));

        let policy = SandboxError::from(PolicyValidationError::DigestMismatch);
        assert_eq!(
            policy.to_string(),
            "invalid sandbox policy: policy digest mismatch"
        );
        assert!(!format!("{policy:?}").contains("secret"));

        let protocol = SandboxError::InvalidChildProtocol {
            failure: ChildProtocolError::InvalidJson,
        };
        assert!(!protocol.to_string().contains('{'));
        assert_eq!(
            protocol.to_string(),
            "sandbox child protocol is invalid: InvalidJson"
        );
    }
}
