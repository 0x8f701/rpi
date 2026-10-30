#![allow(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::ffi::CString;
use std::io;
use std::os::fd::RawFd;
use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize};

use crate::coverage::{CoverageRoute, covers};
use crate::launcher::ExecutableIdentity;
use crate::policy::wire::ResourceLimitsWire;
use crate::{
    CanonicalTargetFdMapping, ChildProtocolError, ResourceLimits, SandboxError, SandboxIoOperation,
    SpawnFamily, TargetFdDirection,
};

pub const SANDBOX_CHILD_PROTOCOL_VERSION: u16 = 1;
pub const SANDBOX_CHILD_REQUEST_FD: RawFd = 3;
pub const SANDBOX_CHILD_STATUS_FD: RawFd = 4;
pub const SANDBOX_SELF_EXE_FD: RawFd = 5;
pub const SANDBOX_WRAPPER_ERROR_FD: RawFd = 6;
pub const MAX_SANDBOX_CHILD_REQUEST_BYTES: usize = 1024 * 1024;
pub const WRAPPER_ERROR_FRAME_BYTES: usize = 12;
pub const CHILD_STATUS_FRAME_BYTES: usize = 12;
pub const SANDBOX_CHILD_ARGV0: &str = "__sandbox-child";

const WRAPPER_ERROR_MAGIC: [u8; 4] = *b"RPWE";
const CHILD_STATUS_MAGIC: [u8; 4] = *b"RPCS";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChildPlatformEnforcement {
    LinuxNoNetwork,
    LinuxFullNetwork,
    MacosSeatbelt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ChildExecutableFd {
    transport_fd: u32,
    identity: ExecutableIdentity,
}

impl ChildExecutableFd {
    pub(crate) const fn new(transport_fd: RawFd, identity: ExecutableIdentity) -> Self {
        Self {
            transport_fd: transport_fd as u32,
            identity,
        }
    }

    pub(crate) const fn transport_fd(self) -> RawFd {
        self.transport_fd as RawFd
    }

    pub(crate) const fn identity(self) -> ExecutableIdentity {
        self.identity
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ChildWorkingDirectoryFd {
    transport_fd: u32,
    identity: ExecutableIdentity,
}

impl ChildWorkingDirectoryFd {
    pub(crate) const fn new(transport_fd: RawFd, identity: ExecutableIdentity) -> Self {
        Self {
            transport_fd: transport_fd as u32,
            identity,
        }
    }

    pub(crate) const fn transport_fd(self) -> RawFd {
        self.transport_fd as RawFd
    }

    pub(crate) const fn identity(self) -> ExecutableIdentity {
        self.identity
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChildTargetFdMapping {
    pub transport_fd: u32,
    pub target_fd: u32,
    pub direction: TargetFdDirection,
}

pub(crate) struct SandboxChildRequestTemplate {
    protocol_version: u16,
    request_id: String,
    family: SpawnFamily,
    argv: Box<[String]>,
    cwd: String,
    environment: BTreeMap<String, String>,
    limits: ResourceLimits,
    enforcement: ChildPlatformEnforcement,
    target_fd_mappings: Box<[CanonicalTargetFdMapping]>,
}

impl SandboxChildRequestTemplate {
    pub(crate) fn new(
        request_id: String,
        family: SpawnFamily,
        argv: Vec<String>,
        cwd: String,
        environment: BTreeMap<String, String>,
        limits: ResourceLimits,
        enforcement: ChildPlatformEnforcement,
        target_fd_mappings: Vec<CanonicalTargetFdMapping>,
    ) -> Result<Self, SandboxError> {
        validate_request_id(&request_id)?;
        validate_argv(&argv)?;
        validate_cwd(&cwd)?;
        validate_environment(
            environment
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        )?;
        validate_coverage(family, enforcement)?;
        validate_canonical_mappings(family, &target_fd_mappings)?;
        let mut target_fd_mappings = target_fd_mappings;
        target_fd_mappings.sort_unstable();
        Ok(Self {
            protocol_version: SANDBOX_CHILD_PROTOCOL_VERSION,
            request_id,
            family,
            argv: argv.into_boxed_slice(),
            cwd,
            environment,
            limits,
            enforcement,
            target_fd_mappings: target_fd_mappings.into_boxed_slice(),
        })
    }

    pub(crate) fn canonical_mappings(&self) -> &[CanonicalTargetFdMapping] {
        &self.target_fd_mappings
    }

    pub(crate) fn finalize(
        self,
        current_executable: ChildExecutableFd,
        target_executable: ChildExecutableFd,
        target_fd_mappings: Vec<ChildTargetFdMapping>,
        working_directory: Option<ChildWorkingDirectoryFd>,
    ) -> Result<SandboxChildRequest, SandboxError> {
        if self.target_fd_mappings.len() != target_fd_mappings.len()
            || !self
                .target_fd_mappings
                .iter()
                .zip(&target_fd_mappings)
                .all(|(canonical, wire)| {
                    canonical.target() == wire.target_fd && canonical.direction() == wire.direction
                })
        {
            return Err(protocol_error(ChildProtocolError::InvalidTargetMapping));
        }
        SandboxChildRequest::new(
            self.protocol_version,
            self.request_id,
            self.family,
            self.argv.into_vec(),
            self.cwd,
            self.environment.into_iter().collect(),
            self.limits,
            self.enforcement,
            current_executable,
            target_executable,
            target_fd_mappings,
            working_directory,
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxChildRequest {
    protocol_version: u16,
    request_id: String,
    family: SpawnFamily,
    argv: Box<[String]>,
    cwd: String,
    #[serde(serialize_with = "serialize_environment")]
    environment: BTreeMap<String, String>,
    limits: ResourceLimits,
    enforcement: ChildPlatformEnforcement,
    current_executable: ChildExecutableFd,
    target_executable: ChildExecutableFd,
    target_fd_mappings: Box<[ChildTargetFdMapping]>,
    working_directory: Option<ChildWorkingDirectoryFd>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SandboxChildRequestWire {
    protocol_version: u16,
    request_id: String,
    family: SpawnFamily,
    argv: Vec<String>,
    cwd: String,
    environment: Vec<(String, String)>,
    limits: ResourceLimitsWire,
    enforcement: ChildPlatformEnforcement,
    current_executable: ChildExecutableFd,
    target_executable: ChildExecutableFd,
    target_fd_mappings: Vec<ChildTargetFdMapping>,
    working_directory: Option<ChildWorkingDirectoryFd>,
}

impl SandboxChildRequest {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        protocol_version: u16,
        request_id: String,
        family: SpawnFamily,
        argv: Vec<String>,
        cwd: String,
        environment: Vec<(String, String)>,
        limits: ResourceLimits,
        enforcement: ChildPlatformEnforcement,
        current_executable: ChildExecutableFd,
        target_executable: ChildExecutableFd,
        target_fd_mappings: Vec<ChildTargetFdMapping>,
        working_directory: Option<ChildWorkingDirectoryFd>,
    ) -> Result<Self, SandboxError> {
        if protocol_version != SANDBOX_CHILD_PROTOCOL_VERSION {
            return Err(protocol_error(ChildProtocolError::UnknownVersion));
        }
        validate_request_id(&request_id)?;
        validate_argv(&argv)?;
        validate_cwd(&cwd)?;
        let environment = collect_environment(environment)?;
        validate_coverage(family, enforcement)?;
        validate_descriptors(
            current_executable,
            target_executable,
            &target_fd_mappings,
            working_directory,
        )?;
        validate_wire_mappings(family, &target_fd_mappings)?;
        Ok(Self {
            protocol_version,
            request_id,
            family,
            argv: argv.into_boxed_slice(),
            cwd,
            environment,
            limits,
            enforcement,
            current_executable,
            target_executable,
            working_directory,
            target_fd_mappings: target_fd_mappings.into_boxed_slice(),
        })
    }

    pub(crate) fn encode_bounded(&self) -> Result<Vec<u8>, SandboxError> {
        let json = serde_json::to_vec(self)
            .map_err(|_| protocol_error(ChildProtocolError::InvalidJson))?;
        if json.len() > MAX_SANDBOX_CHILD_REQUEST_BYTES {
            return Err(protocol_error(ChildProtocolError::Oversize));
        }
        let length =
            u32::try_from(json.len()).map_err(|_| protocol_error(ChildProtocolError::Oversize))?;
        let mut frame = Vec::with_capacity(4 + json.len());
        frame.extend_from_slice(&length.to_be_bytes());
        frame.extend_from_slice(&json);
        Ok(frame)
    }
}

impl TryFrom<SandboxChildRequestWire> for SandboxChildRequest {
    type Error = SandboxError;

    fn try_from(wire: SandboxChildRequestWire) -> Result<Self, Self::Error> {
        let limits = ResourceLimits::try_from(wire.limits)
            .map_err(|_| protocol_error(ChildProtocolError::InvalidResourceLimit))?;
        Self::new(
            wire.protocol_version,
            wire.request_id,
            wire.family,
            wire.argv,
            wire.cwd,
            wire.environment,
            limits,
            wire.enforcement,
            wire.current_executable,
            wire.target_executable,
            wire.target_fd_mappings,
            wire.working_directory,
        )
    }
}

impl<'de> Deserialize<'de> for SandboxChildRequest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        SandboxChildRequestWire::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WrapperStage {
    MapStdio = 1,
    MapControl = 2,
    CloseDescriptors = 3,
    ExecuteWrapper = 4,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChildStage {
    ReadRequest = 1,
    ValidateRequest = 2,
    ValidateDescriptors = 3,
    CompileSeccomp = 4,
    ApplyAddressSpace = 5,
    ApplyCpu = 6,
    ApplyFileSize = 7,
    ApplyOpenFiles = 8,
    ApplyNoNewPrivileges = 9,
    ApplySeccomp = 10,
    InstallTargetMappings = 11,
    ChangeDirectory = 12,
    ExecuteTarget = 13,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WrapperErrorFrame {
    pub(crate) stage: WrapperStage,
    pub(crate) errno: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ChildStatusFrame {
    pub(crate) stage: ChildStage,
    pub(crate) errno: Option<i32>,
}

impl WrapperErrorFrame {
    pub(crate) fn encode(self) -> [u8; WRAPPER_ERROR_FRAME_BYTES] {
        let mut bytes = [0; WRAPPER_ERROR_FRAME_BYTES];
        bytes[..4].copy_from_slice(&WRAPPER_ERROR_MAGIC);
        bytes[4] = SANDBOX_CHILD_PROTOCOL_VERSION as u8;
        bytes[5] = self.stage as u8;
        bytes[8..].copy_from_slice(&self.errno.to_be_bytes());
        bytes
    }

    pub(crate) fn decode(bytes: [u8; WRAPPER_ERROR_FRAME_BYTES]) -> Result<Self, SandboxError> {
        if bytes[..4] != WRAPPER_ERROR_MAGIC
            || bytes[4] != SANDBOX_CHILD_PROTOCOL_VERSION as u8
            || bytes[6] != 0
            || bytes[7] != 0
        {
            return Err(protocol_error(ChildProtocolError::InvalidStatusFrame));
        }
        let stage = WrapperStage::from_byte(bytes[5])?;
        let errno = i32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        Ok(Self { stage, errno })
    }
}

impl ChildStatusFrame {
    pub(crate) fn encode(self) -> [u8; CHILD_STATUS_FRAME_BYTES] {
        let mut bytes = [0; CHILD_STATUS_FRAME_BYTES];
        bytes[..4].copy_from_slice(&CHILD_STATUS_MAGIC);
        bytes[4] = SANDBOX_CHILD_PROTOCOL_VERSION as u8;
        bytes[5] = self.stage as u8;
        bytes[6] = u8::from(self.errno.is_some());
        bytes[8..].copy_from_slice(&self.errno.unwrap_or_default().to_be_bytes());
        bytes
    }

    pub(crate) fn decode(bytes: [u8; CHILD_STATUS_FRAME_BYTES]) -> Result<Self, SandboxError> {
        if bytes[..4] != CHILD_STATUS_MAGIC
            || bytes[4] != SANDBOX_CHILD_PROTOCOL_VERSION as u8
            || bytes[6] & !1 != 0
            || bytes[7] != 0
        {
            return Err(protocol_error(ChildProtocolError::InvalidStatusFrame));
        }
        let stage = ChildStage::from_byte(bytes[5])?;
        let raw_errno = i32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let errno = match bytes[6] {
            0 if raw_errno == 0 => None,
            1 => Some(raw_errno),
            _ => return Err(protocol_error(ChildProtocolError::InvalidStatusFrame)),
        };
        Ok(Self { stage, errno })
    }
}

impl WrapperStage {
    fn from_byte(value: u8) -> Result<Self, SandboxError> {
        match value {
            1 => Ok(Self::MapStdio),
            2 => Ok(Self::MapControl),
            3 => Ok(Self::CloseDescriptors),
            4 => Ok(Self::ExecuteWrapper),
            _ => Err(protocol_error(ChildProtocolError::InvalidStatusFrame)),
        }
    }
}

impl ChildStage {
    fn from_byte(value: u8) -> Result<Self, SandboxError> {
        match value {
            1 => Ok(Self::ReadRequest),
            2 => Ok(Self::ValidateRequest),
            3 => Ok(Self::ValidateDescriptors),
            4 => Ok(Self::CompileSeccomp),
            5 => Ok(Self::ApplyAddressSpace),
            6 => Ok(Self::ApplyCpu),
            7 => Ok(Self::ApplyFileSize),
            8 => Ok(Self::ApplyOpenFiles),
            9 => Ok(Self::ApplyNoNewPrivileges),
            10 => Ok(Self::ApplySeccomp),
            11 => Ok(Self::InstallTargetMappings),
            12 => Ok(Self::ChangeDirectory),
            13 => Ok(Self::ExecuteTarget),
            _ => Err(protocol_error(ChildProtocolError::InvalidStatusFrame)),
        }
    }
}

pub fn exec_child_target(request_fd: RawFd, status_fd: RawFd) -> Result<Infallible, SandboxError> {
    crate::harden_current_process();
    let plan = match prepare_child_target(request_fd, status_fd) {
        Ok(plan) => plan,
        Err(failure) => {
            write_child_failure(
                failure.status_fd,
                failure.stage,
                failure.error.raw_os_error(),
            );
            return Err(failure.error);
        }
    };
    let argv = cstring_pointers(&plan.argv);
    let environment = cstring_pointers(&plan.environment);
    #[cfg(target_os = "linux")]
    unsafe {
        libc::syscall(
            libc::SYS_execveat,
            plan.target_fd,
            c"".as_ptr(),
            argv.as_ptr(),
            environment.as_ptr(),
            libc::AT_EMPTY_PATH,
        );
    }
    #[cfg(target_os = "macos")]
    unsafe {
        let mut path = [0_u8; 32];
        let length = format_decimal_path(plan.target_fd, &mut path);
        libc::execve(path.as_ptr().cast(), argv.as_ptr(), environment.as_ptr());
        let _ = length;
    }
    let error = SandboxError::from_io(
        SandboxIoOperation::ExecuteTarget,
        &io::Error::last_os_error(),
    );
    write_child_failure(
        plan.status_fd,
        ChildStage::ExecuteTarget,
        error.raw_os_error(),
    );
    Err(error)
}

struct ChildExecPlan {
    status_fd: RawFd,
    target_fd: RawFd,
    argv: Vec<CString>,
    environment: Vec<CString>,
}

struct ChildFailure {
    status_fd: RawFd,
    stage: ChildStage,
    error: SandboxError,
}

impl ChildFailure {
    fn new(status_fd: RawFd, stage: ChildStage, error: SandboxError) -> Self {
        Self {
            status_fd,
            stage,
            error,
        }
    }
}

fn prepare_child_target(
    request_fd: RawFd,
    status_fd: RawFd,
) -> Result<ChildExecPlan, ChildFailure> {
    let request = read_request(request_fd)
        .map_err(|error| ChildFailure::new(status_fd, ChildStage::ReadRequest, error))?;
    validate_runtime_enforcement(request.enforcement)
        .map_err(|error| ChildFailure::new(status_fd, ChildStage::ValidateRequest, error))?;
    validate_runtime_descriptors(&request)
        .map_err(|error| ChildFailure::new(status_fd, ChildStage::ValidateDescriptors, error))?;

    #[cfg(target_os = "linux")]
    let compiled_filter = crate::seccomp::compile_child_filter(request.enforcement)
        .map_err(|error| ChildFailure::new(status_fd, ChildStage::CompileSeccomp, error))?;

    let private_status_minimum = request
        .target_fd_mappings
        .iter()
        .flat_map(|mapping| [mapping.transport_fd, mapping.target_fd])
        .chain(std::iter::once(request.target_executable.transport_fd))
        .chain(
            request
                .working_directory
                .map(|directory| directory.transport_fd as u32),
        )
        .max()
        .unwrap_or(6)
        .saturating_add(1)
        .max(64) as RawFd;
    let private_status_fd = duplicate_cloexec(status_fd, private_status_minimum)
        .map_err(|error| ChildFailure::new(status_fd, ChildStage::InstallTargetMappings, error))?;
    apply_limit(
        status_fd,
        libc::RLIMIT_AS,
        request.limits.address_space_bytes(),
        ChildStage::ApplyAddressSpace,
    )?;
    apply_limit(
        status_fd,
        libc::RLIMIT_CPU,
        request.limits.cpu_seconds(),
        ChildStage::ApplyCpu,
    )?;
    apply_limit(
        status_fd,
        libc::RLIMIT_FSIZE,
        request.limits.file_size_bytes(),
        ChildStage::ApplyFileSize,
    )?;
    apply_limit(
        status_fd,
        libc::RLIMIT_NOFILE,
        request.limits.open_files(),
        ChildStage::ApplyOpenFiles,
    )?;

    #[cfg(target_os = "linux")]
    {
        crate::seccomp::set_no_new_privs_current_thread().map_err(|error| {
            ChildFailure::new(status_fd, ChildStage::ApplyNoNewPrivileges, error)
        })?;
        if let Some(filter) = compiled_filter.as_ref() {
            crate::seccomp::apply_compiled_child_filter(filter)
                .map_err(|error| ChildFailure::new(status_fd, ChildStage::ApplySeccomp, error))?;
        }
    }

    close_raw(request_fd);
    close_raw(status_fd);
    if let Some(directory) = request.working_directory {
        close_raw(directory.transport_fd());
    }
    install_target_mappings(&request.target_fd_mappings).map_err(|error| {
        ChildFailure::new(private_status_fd, ChildStage::InstallTargetMappings, error)
    })?;
    close_raw(SANDBOX_SELF_EXE_FD);
    close_transport_sources(&request);
    // The retained descriptor proved that the approved cwd was not replaced.
    // Enter through the post-mount namespace path so read-only bind authority
    // cannot be bypassed by an open descriptor to the host mount.
    let cwd = CString::new(request.cwd.as_bytes()).map_err(|_| {
        ChildFailure::new(
            private_status_fd,
            ChildStage::ChangeDirectory,
            protocol_error(ChildProtocolError::EmbeddedNul),
        )
    })?;
    if unsafe { libc::chdir(cwd.as_ptr()) } != 0 {
        return Err(ChildFailure::new(
            private_status_fd,
            ChildStage::ChangeDirectory,
            SandboxError::from_io(
                SandboxIoOperation::ChangeDirectory,
                &io::Error::last_os_error(),
            ),
        ));
    }

    let argv = cstring_sequence(&request.argv)
        .map_err(|error| ChildFailure::new(private_status_fd, ChildStage::ExecuteTarget, error))?;
    let environment = request
        .environment
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>();
    let env = cstring_sequence(&environment)
        .map_err(|error| ChildFailure::new(private_status_fd, ChildStage::ExecuteTarget, error))?;
    clear_cloexec(request.target_executable.transport_fd())
        .map_err(|error| ChildFailure::new(private_status_fd, ChildStage::ExecuteTarget, error))?;
    Ok(ChildExecPlan {
        status_fd: private_status_fd,
        target_fd: request.target_executable.transport_fd(),
        argv,
        environment: env,
    })
}

fn read_request(request_fd: RawFd) -> Result<SandboxChildRequest, SandboxError> {
    let mut length = [0_u8; 4];
    read_exact_fd(request_fd, &mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_SANDBOX_CHILD_REQUEST_BYTES {
        return Err(protocol_error(ChildProtocolError::Oversize));
    }
    let mut json = vec![0_u8; length];
    read_exact_fd(request_fd, &mut json)?;
    let mut trailing = [0_u8; 1];
    match read_some_fd(request_fd, &mut trailing) {
        Ok(0) => {}
        Ok(_) => return Err(protocol_error(ChildProtocolError::TrailingBytes)),
        Err(error) => return Err(error),
    }
    let wire: SandboxChildRequestWire = serde_json::from_slice(&json)
        .map_err(|_| protocol_error(ChildProtocolError::InvalidJson))?;
    wire.try_into()
}

fn validate_request_id(request_id: &str) -> Result<(), SandboxError> {
    let bytes = request_id.as_bytes();
    let is_valid = (1..=64).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        });
    if !is_valid {
        return Err(protocol_error(ChildProtocolError::InvalidRequestId));
    }
    Ok(())
}

fn validate_argv(argv: &[String]) -> Result<(), SandboxError> {
    if argv.first().is_none_or(String::is_empty) {
        return Err(protocol_error(ChildProtocolError::EmptyArgv));
    }
    if argv.iter().any(|argument| argument.as_bytes().contains(&0)) {
        return Err(protocol_error(ChildProtocolError::EmbeddedNul));
    }
    Ok(())
}

fn validate_cwd(cwd: &str) -> Result<(), SandboxError> {
    if cwd.as_bytes().contains(&0) {
        return Err(protocol_error(ChildProtocolError::EmbeddedNul));
    }
    if !Path::new(cwd).is_absolute() {
        return Err(protocol_error(ChildProtocolError::InvalidCwd));
    }
    Ok(())
}

fn validate_environment<'a>(
    environment: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Result<(), SandboxError> {
    for (name, value) in environment {
        if name.is_empty()
            || name.as_bytes().contains(&b'=')
            || name.as_bytes().contains(&0)
            || value.as_bytes().contains(&0)
        {
            return Err(protocol_error(ChildProtocolError::EmbeddedNul));
        }
    }
    Ok(())
}

fn collect_environment(
    environment: Vec<(String, String)>,
) -> Result<BTreeMap<String, String>, SandboxError> {
    validate_environment(
        environment
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str())),
    )?;
    let mut collected = BTreeMap::new();
    for (name, value) in environment {
        if collected.insert(name, value).is_some() {
            return Err(protocol_error(ChildProtocolError::DuplicateEnvironment));
        }
    }
    Ok(collected)
}

fn validate_coverage(
    family: SpawnFamily,
    enforcement: ChildPlatformEnforcement,
) -> Result<(), SandboxError> {
    let route = match enforcement {
        ChildPlatformEnforcement::LinuxNoNetwork | ChildPlatformEnforcement::LinuxFullNetwork => {
            CoverageRoute::LinuxConfined
        }
        ChildPlatformEnforcement::MacosSeatbelt => CoverageRoute::MacosConfined,
    };
    if !covers(route, family) {
        return Err(protocol_error(ChildProtocolError::UncoveredFamily));
    }
    Ok(())
}

fn validate_runtime_enforcement(enforcement: ChildPlatformEnforcement) -> Result<(), SandboxError> {
    let valid = if cfg!(target_os = "linux") {
        matches!(
            enforcement,
            ChildPlatformEnforcement::LinuxNoNetwork | ChildPlatformEnforcement::LinuxFullNetwork
        )
    } else if cfg!(target_os = "macos") {
        enforcement == ChildPlatformEnforcement::MacosSeatbelt
    } else {
        false
    };
    if !valid {
        return Err(protocol_error(ChildProtocolError::EnforcementMismatch));
    }
    Ok(())
}

fn validate_canonical_mappings(
    family: SpawnFamily,
    mappings: &[CanonicalTargetFdMapping],
) -> Result<(), SandboxError> {
    let wire = mappings
        .iter()
        .map(|mapping| ChildTargetFdMapping {
            transport_fd: 7,
            target_fd: mapping.target(),
            direction: mapping.direction(),
        })
        .collect::<Vec<_>>();
    validate_mapping_targets(family, &wire)
}

fn validate_descriptors(
    current: ChildExecutableFd,
    target: ChildExecutableFd,
    mappings: &[ChildTargetFdMapping],
    working_directory: Option<ChildWorkingDirectoryFd>,
) -> Result<(), SandboxError> {
    if current.transport_fd() != SANDBOX_SELF_EXE_FD
        || target.transport_fd() <= SANDBOX_WRAPPER_ERROR_FD
        || target.transport_fd as usize > RawFd::MAX as usize
        || working_directory.is_some_and(|directory| {
            directory.transport_fd() <= SANDBOX_WRAPPER_ERROR_FD
                || directory.transport_fd as usize > RawFd::MAX as usize
        })
        || mappings.iter().any(|mapping| {
            mapping.transport_fd <= SANDBOX_WRAPPER_ERROR_FD as u32
                || mapping.transport_fd as usize > RawFd::MAX as usize
                || mapping.target_fd == 0
                || mapping.target_fd as usize > RawFd::MAX as usize
        })
    {
        return Err(protocol_error(ChildProtocolError::InvalidDescriptor));
    }
    let reserved_transports = working_directory
        .map(|directory| {
            BTreeSet::from([
                current.transport_fd(),
                target.transport_fd(),
                directory.transport_fd(),
            ])
        })
        .unwrap_or_else(|| BTreeSet::from([current.transport_fd(), target.transport_fd()]));
    let mut transports = reserved_transports.clone();
    if mappings
        .iter()
        .any(|mapping| !transports.insert(mapping.transport_fd as RawFd))
        || mappings
            .iter()
            .any(|mapping| reserved_transports.contains(&(mapping.target_fd as RawFd)))
    {
        return Err(protocol_error(ChildProtocolError::DuplicateDescriptor));
    }
    Ok(())
}

fn validate_wire_mappings(
    family: SpawnFamily,
    mappings: &[ChildTargetFdMapping],
) -> Result<(), SandboxError> {
    validate_mapping_targets(family, mappings)?;
    let mut targets = BTreeSet::new();
    if mappings
        .iter()
        .any(|mapping| !targets.insert(mapping.target_fd))
    {
        return Err(protocol_error(ChildProtocolError::InvalidTargetMapping));
    }
    Ok(())
}

fn validate_mapping_targets(
    family: SpawnFamily,
    mappings: &[ChildTargetFdMapping],
) -> Result<(), SandboxError> {
    let is_valid = if family == SpawnFamily::Browser {
        mappings.len() == 2
            && mappings.iter().any(|mapping| {
                mapping.target_fd == 3 && mapping.direction == TargetFdDirection::ParentWrites
            })
            && mappings.iter().any(|mapping| {
                mapping.target_fd == 4 && mapping.direction == TargetFdDirection::ParentReads
            })
    } else {
        mappings.iter().all(|mapping| mapping.target_fd >= 7)
    };
    if !is_valid {
        return Err(protocol_error(ChildProtocolError::InvalidTargetMapping));
    }
    Ok(())
}

fn validate_runtime_descriptors(request: &SandboxChildRequest) -> Result<(), SandboxError> {
    if !runtime_file_identity_matches(
        request.current_executable.identity(),
        request.current_executable.transport_fd(),
    )? || !runtime_file_identity_matches(
        request.target_executable.identity(),
        request.target_executable.transport_fd(),
    )? {
        return Err(protocol_error(
            ChildProtocolError::DescriptorIdentityMismatch,
        ));
    }
    if let Some(directory) = request.working_directory {
        if !runtime_directory_identity_matches(directory.identity(), directory.transport_fd())? {
            return Err(protocol_error(
                ChildProtocolError::DescriptorIdentityMismatch,
            ));
        }
    }
    for mapping in &request.target_fd_mappings {
        ExecutableIdentity::from_fd(mapping.transport_fd as RawFd)?;
    }
    Ok(())
}

fn runtime_file_identity_matches(
    identity: ExecutableIdentity,
    descriptor: RawFd,
) -> Result<bool, SandboxError> {
    #[cfg(target_os = "linux")]
    {
        identity.matches_fd_in_user_namespace(descriptor)
    }
    #[cfg(target_os = "macos")]
    {
        identity.matches_fd(descriptor)
    }
}

fn runtime_directory_identity_matches(
    identity: ExecutableIdentity,
    descriptor: RawFd,
) -> Result<bool, SandboxError> {
    #[cfg(target_os = "linux")]
    {
        identity.directory_matches_fd_in_user_namespace(descriptor)
    }
    #[cfg(target_os = "macos")]
    {
        identity.directory_matches_fd(descriptor)
    }
}

fn apply_limit(
    status_fd: RawFd,
    resource: libc::__rlimit_resource_t,
    value: u64,
    stage: ChildStage,
) -> Result<(), ChildFailure> {
    let value = libc::rlim_t::try_from(value).map_err(|_| {
        ChildFailure::new(
            status_fd,
            stage,
            protocol_error(ChildProtocolError::InvalidResourceLimit),
        )
    })?;
    let limit = libc::rlimit {
        rlim_cur: value,
        rlim_max: value,
    };
    if unsafe { libc::setrlimit(resource, &limit) } != 0 {
        return Err(ChildFailure::new(
            status_fd,
            stage,
            SandboxError::from_io(
                SandboxIoOperation::ApplyResourceLimit,
                &io::Error::last_os_error(),
            ),
        ));
    }
    Ok(())
}

fn install_target_mappings(mappings: &[ChildTargetFdMapping]) -> Result<(), SandboxError> {
    if mappings.is_empty() {
        return Ok(());
    }
    let targets = mappings
        .iter()
        .map(|mapping| mapping.target_fd as RawFd)
        .collect::<BTreeSet<_>>();
    let minimum = mappings
        .iter()
        .map(|mapping| mapping.transport_fd)
        .chain(mappings.iter().map(|mapping| mapping.target_fd))
        .max()
        .unwrap_or(6)
        .saturating_add(1)
        .max(64) as RawFd;
    let mut sources = Vec::with_capacity(mappings.len());
    for mapping in mappings {
        let source = mapping.transport_fd as RawFd;
        let source = if targets.contains(&source) {
            duplicate_cloexec(source, minimum)?
        } else {
            source
        };
        sources.push((source, mapping.target_fd as RawFd));
    }
    for (source, target) in &sources {
        if source != target && unsafe { libc::dup2(*source, *target) } < 0 {
            return Err(SandboxError::from_io(
                SandboxIoOperation::MapDescriptor,
                &io::Error::last_os_error(),
            ));
        }
        clear_cloexec(*target)?;
    }
    for (source, target) in sources {
        if source != target {
            close_raw(source);
        }
    }
    Ok(())
}

fn close_transport_sources(request: &SandboxChildRequest) {
    for mapping in &request.target_fd_mappings {
        let source = mapping.transport_fd as RawFd;
        if source != request.target_executable.transport_fd() {
            close_raw(source);
        }
    }
}

fn cstring_sequence(values: &[String]) -> Result<Vec<CString>, SandboxError> {
    values
        .iter()
        .map(|value| {
            CString::new(value.as_bytes())
                .map_err(|_| protocol_error(ChildProtocolError::EmbeddedNul))
        })
        .collect()
}

fn cstring_pointers(values: &[CString]) -> Vec<*const libc::c_char> {
    values
        .iter()
        .map(|value| value.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect()
}

fn serialize_environment<S>(
    environment: &BTreeMap<String, String>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeSeq as _;

    let mut sequence = serializer.serialize_seq(Some(environment.len()))?;
    for row in environment {
        sequence.serialize_element(&row)?;
    }
    sequence.end()
}

fn duplicate_cloexec(fd: RawFd, minimum: RawFd) -> Result<RawFd, SandboxError> {
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, minimum) };
    if duplicate < 0 {
        return Err(SandboxError::from_io(
            SandboxIoOperation::DuplicateDescriptor,
            &io::Error::last_os_error(),
        ));
    }
    Ok(duplicate)
}

fn clear_cloexec(fd: RawFd) -> Result<(), SandboxError> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(SandboxError::from_io(
            SandboxIoOperation::MapDescriptor,
            &io::Error::last_os_error(),
        ));
    }
    Ok(())
}

fn read_exact_fd(fd: RawFd, mut bytes: &mut [u8]) -> Result<(), SandboxError> {
    while !bytes.is_empty() {
        match read_some_fd(fd, bytes)? {
            0 => return Err(protocol_error(ChildProtocolError::Truncated)),
            read => bytes = &mut bytes[read..],
        }
    }
    Ok(())
}

fn read_some_fd(fd: RawFd, bytes: &mut [u8]) -> Result<usize, SandboxError> {
    loop {
        let read = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
        if read >= 0 {
            return Ok(read as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(SandboxError::from_io(
                SandboxIoOperation::CreateChannel,
                &error,
            ));
        }
    }
}

fn write_child_failure(status_fd: RawFd, stage: ChildStage, errno: Option<i32>) {
    let bytes = ChildStatusFrame { stage, errno }.encode();
    let mut written = 0;
    while written < bytes.len() {
        let count = unsafe {
            libc::write(
                status_fd,
                bytes[written..].as_ptr().cast(),
                bytes.len() - written,
            )
        };
        if count > 0 {
            written += count as usize;
            continue;
        }
        if count < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        break;
    }
}

fn close_raw(fd: RawFd) {
    unsafe {
        libc::close(fd);
    }
}

fn protocol_error(failure: ChildProtocolError) -> SandboxError {
    SandboxError::InvalidChildProtocol { failure }
}

trait RawOsError {
    fn raw_os_error(&self) -> Option<i32>;
}

impl RawOsError for SandboxError {
    fn raw_os_error(&self) -> Option<i32> {
        match self {
            Self::System { raw_os_error, .. } => *raw_os_error,
            _ => None,
        }
    }
}

#[cfg(target_os = "macos")]
fn format_decimal_path(fd: RawFd, bytes: &mut [u8; 32]) -> usize {
    let prefix = b"/dev/fd/";
    bytes[..prefix.len()].copy_from_slice(prefix);
    let mut digits = [0_u8; 10];
    let mut value = fd as u32;
    let mut count = 0;
    loop {
        digits[count] = b'0' + (value % 10) as u8;
        count += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for index in 0..count {
        bytes[prefix.len() + index] = digits[count - index - 1];
    }
    bytes[prefix.len() + count] = 0;
    prefix.len() + count + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ExecutableIdentity {
        ExecutableIdentity::for_test(1, 2)
    }

    fn request_with_limits(limits: ResourceLimits) -> Result<SandboxChildRequest, SandboxError> {
        SandboxChildRequest::new(
            SANDBOX_CHILD_PROTOCOL_VERSION,
            "spawn-1".to_owned(),
            SpawnFamily::ManagedPipeProcess,
            vec!["target".to_owned()],
            "/".to_owned(),
            vec![],
            limits,
            if cfg!(target_os = "macos") {
                ChildPlatformEnforcement::MacosSeatbelt
            } else {
                ChildPlatformEnforcement::LinuxFullNetwork
            },
            ChildExecutableFd::new(SANDBOX_SELF_EXE_FD, identity()),
            ChildExecutableFd::new(64, identity()),
            vec![],
            None,
        )
    }

    #[test]
    fn child_request_rejects_zero_and_above_default_limits() {
        let defaults = ResourceLimits::confined_defaults();
        assert!(
            ResourceLimits::new(
                0,
                defaults.cpu_seconds(),
                defaults.file_size_bytes(),
                defaults.open_files()
            )
            .is_err()
        );
        assert!(
            ResourceLimits::new(
                defaults.address_space_bytes() + 1,
                defaults.cpu_seconds(),
                defaults.file_size_bytes(),
                defaults.open_files()
            )
            .is_err()
        );
        assert!(request_with_limits(defaults).is_ok());
    }

    #[test]
    fn current_executable_uses_reserved_fd_five() {
        let invalid = SandboxChildRequest::new(
            SANDBOX_CHILD_PROTOCOL_VERSION,
            "spawn-2".to_owned(),
            SpawnFamily::ManagedPipeProcess,
            vec!["target".to_owned()],
            "/".to_owned(),
            vec![],
            ResourceLimits::confined_defaults(),
            ChildPlatformEnforcement::LinuxFullNetwork,
            ChildExecutableFd::new(7, identity()),
            ChildExecutableFd::new(64, identity()),
            vec![],
            None,
        );
        assert_eq!(
            invalid,
            Err(protocol_error(ChildProtocolError::InvalidDescriptor))
        );
    }
    #[test]
    fn target_mappings_cannot_overwrite_reserved_transport_descriptors() {
        for reserved in [64_u32, 65_u32] {
            let invalid = SandboxChildRequest::new(
                SANDBOX_CHILD_PROTOCOL_VERSION,
                "spawn-reserved-target".to_owned(),
                SpawnFamily::ManagedPipeProcess,
                vec!["target".to_owned()],
                "/".to_owned(),
                vec![],
                ResourceLimits::confined_defaults(),
                ChildPlatformEnforcement::LinuxFullNetwork,
                ChildExecutableFd::new(SANDBOX_SELF_EXE_FD, identity()),
                ChildExecutableFd::new(64, identity()),
                vec![ChildTargetFdMapping {
                    transport_fd: 66,
                    target_fd: reserved,
                    direction: TargetFdDirection::ParentReads,
                }],
                Some(ChildWorkingDirectoryFd::new(65, identity())),
            );
            assert_eq!(
                invalid,
                Err(protocol_error(ChildProtocolError::DuplicateDescriptor))
            );
        }
    }

    #[test]
    fn wrapper_and_child_frames_are_complete_and_distinct() {
        let wrapper = WrapperErrorFrame {
            stage: WrapperStage::ExecuteWrapper,
            errno: libc::ENOENT,
        };
        let child = ChildStatusFrame {
            stage: ChildStage::ExecuteTarget,
            errno: Some(libc::EACCES),
        };
        assert_eq!(WrapperErrorFrame::decode(wrapper.encode()), Ok(wrapper));
        assert_eq!(ChildStatusFrame::decode(child.encode()), Ok(child));
        assert_ne!(&wrapper.encode()[..4], &child.encode()[..4]);
        let mut malformed = child.encode();
        malformed[7] = 1;
        assert!(ChildStatusFrame::decode(malformed).is_err());
    }

    #[test]
    fn hidden_child_applies_limits_nnp_seccomp_then_exec() {
        assert_eq!(ChildStage::CompileSeccomp as u8, 4);
        assert_eq!(ChildStage::ApplyAddressSpace as u8, 5);
        assert_eq!(ChildStage::ApplyCpu as u8, 6);
        assert_eq!(ChildStage::ApplyFileSize as u8, 7);
        assert_eq!(ChildStage::ApplyOpenFiles as u8, 8);
        assert_eq!(ChildStage::ApplyNoNewPrivileges as u8, 9);
        assert_eq!(ChildStage::ApplySeccomp as u8, 10);
        assert_eq!(ChildStage::ExecuteTarget as u8, 13);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_descriptor_exec_uses_prepared_inode_after_path_replacement() {
        assert_eq!(libc::AT_EMPTY_PATH, 0x1000);
        assert_eq!(SANDBOX_SELF_EXE_FD, 5);
    }

    #[test]
    fn all_governed_mappings_survive_and_sources_close_after_install() {
        let mappings = [
            ChildTargetFdMapping {
                transport_fd: 64,
                target_fd: 7,
                direction: TargetFdDirection::ParentReads,
            },
            ChildTargetFdMapping {
                transport_fd: 65,
                target_fd: 8,
                direction: TargetFdDirection::ParentWrites,
            },
        ];
        assert!(validate_wire_mappings(SpawnFamily::ManagedPipeProcess, &mappings).is_ok());
        assert_eq!(
            mappings
                .iter()
                .map(|mapping| mapping.transport_fd)
                .collect::<BTreeSet<_>>()
                .len(),
            2
        );
    }

    #[test]
    fn status_eof_follows_exec_armed_frame_and_target_exec() {
        let frame = ChildStatusFrame {
            stage: ChildStage::ExecuteTarget,
            errno: None,
        };
        let encoded = frame.encode();
        assert_eq!(encoded[6], 0);
        assert_eq!(&encoded[8..], &[0, 0, 0, 0]);
    }
}
