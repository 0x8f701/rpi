#![allow(unsafe_code)]

use std::collections::BTreeSet;
use std::ffi::{CString, OsString};
use std::fs::File;
use std::io;
use std::num::{NonZeroI32, NonZeroU32, NonZeroU64};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::sync::mpsc;
use std::thread;

use crate::child::{
    CHILD_STATUS_FRAME_BYTES, ChildStatusFrame, SANDBOX_CHILD_PROTOCOL_VERSION,
    SANDBOX_CHILD_REQUEST_FD, SANDBOX_CHILD_STATUS_FD, SANDBOX_SELF_EXE_FD,
    SANDBOX_WRAPPER_ERROR_FD, WRAPPER_ERROR_FRAME_BYTES, WrapperErrorFrame, WrapperStage,
};
use crate::launcher::ExecutableIdentity;
use crate::prepare::{PreparedSpawn, PreparedUnixPlan, UnixTransportFd, UnixTransportPurpose};
use crate::spawn::{PreparedSpawnRequestParts, SpawnStdioParts};
use crate::{ChildProtocolError, SandboxError, SandboxIoOperation, TerminalSize};

const FIRST_UNRESERVED_FD: RawFd = 7;
pub(crate) const MINIMUM_TRANSPORT_FD: RawFd = 64;
const WRAPPER_FAILURE_EXIT_CODE: i32 = 125;
const RAW_HOST_ERROR_FRAME_BYTES: usize = 12;
const RAW_HOST_ERROR_MAGIC: [u8; 4] = *b"RPRH";

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RawHostStage {
    PrepareProcess = 1,
    MapStdio = 2,
    MapDescriptors = 3,
    ChangeDirectory = 4,
    PrepareExecutable = 5,
    ExecuteTarget = 6,
}

impl RawHostStage {
    fn from_byte(value: u8) -> Result<Self, SandboxError> {
        match value {
            1 => Ok(Self::PrepareProcess),
            2 => Ok(Self::MapStdio),
            3 => Ok(Self::MapDescriptors),
            4 => Ok(Self::ChangeDirectory),
            5 => Ok(Self::PrepareExecutable),
            6 => Ok(Self::ExecuteTarget),
            _ => Err(invalid_protocol(ChildProtocolError::InvalidStatusFrame)),
        }
    }

    const fn operation(self) -> SandboxIoOperation {
        match self {
            Self::PrepareProcess => SandboxIoOperation::Fork,
            Self::MapStdio | Self::MapDescriptors => SandboxIoOperation::MapDescriptor,
            Self::ChangeDirectory => SandboxIoOperation::ChangeDirectory,
            Self::PrepareExecutable | Self::ExecuteTarget => SandboxIoOperation::ExecuteTarget,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RawHostErrorFrame {
    stage: RawHostStage,
    errno: i32,
}

impl RawHostErrorFrame {
    fn encode(self) -> [u8; RAW_HOST_ERROR_FRAME_BYTES] {
        let mut bytes = [0; RAW_HOST_ERROR_FRAME_BYTES];
        bytes[..4].copy_from_slice(&RAW_HOST_ERROR_MAGIC);
        bytes[4] = SANDBOX_CHILD_PROTOCOL_VERSION as u8;
        bytes[5] = self.stage as u8;
        bytes[8..].copy_from_slice(&self.errno.to_be_bytes());
        bytes
    }

    fn decode(bytes: [u8; RAW_HOST_ERROR_FRAME_BYTES]) -> Result<Self, SandboxError> {
        if bytes[..4] != RAW_HOST_ERROR_MAGIC
            || bytes[4] != SANDBOX_CHILD_PROTOCOL_VERSION as u8
            || bytes[6] != 0
            || bytes[7] != 0
        {
            return Err(invalid_protocol(ChildProtocolError::InvalidStatusFrame));
        }
        let stage = RawHostStage::from_byte(bytes[5])?;
        let errno = i32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        Ok(Self { stage, errno })
    }
}

pub(crate) struct UnixDescriptorPlan {
    wrapper_keep_fds: Box<[RawFd]>,
    complement_close_ranges: Box<[(RawFd, RawFd)]>,
    transports: Box<[UnixTransportFd]>,
    captured_open_file_limit: NonZeroU64,
}

impl UnixDescriptorPlan {
    pub(crate) fn new(
        transports: Box<[UnixTransportFd]>,
        captured_open_file_limit: NonZeroU64,
        additionally_protected: &[RawFd],
    ) -> Result<Self, SandboxError> {
        let mut wrapper_keep_fds = vec![
            SANDBOX_CHILD_REQUEST_FD,
            SANDBOX_CHILD_STATUS_FD,
            SANDBOX_SELF_EXE_FD,
        ];
        wrapper_keep_fds.extend(transports.iter().map(UnixTransportFd::raw_fd));
        wrapper_keep_fds.sort_unstable();
        wrapper_keep_fds.dedup();
        if wrapper_keep_fds
            .iter()
            .any(|fd| *fd <= 0 || *fd == SANDBOX_WRAPPER_ERROR_FD)
        {
            return Err(invalid_protocol(ChildProtocolError::InvalidDescriptor));
        }

        let ceiling = RawFd::try_from(captured_open_file_limit.get().saturating_sub(1))
            .map_err(|_| invalid_protocol(ChildProtocolError::InvalidResourceLimit))?;
        let mut protected = wrapper_keep_fds.iter().copied().collect::<BTreeSet<_>>();
        protected.extend(additionally_protected.iter().copied());
        let complement_close_ranges = complement_ranges(FIRST_UNRESERVED_FD, ceiling, &protected);
        Ok(Self {
            wrapper_keep_fds: wrapper_keep_fds.into_boxed_slice(),
            complement_close_ranges: complement_close_ranges.into_boxed_slice(),
            transports,
            captured_open_file_limit,
        })
    }

    pub(crate) fn wrapper_keep_fds(&self) -> &[RawFd] {
        &self.wrapper_keep_fds
    }

    pub(crate) fn complement_close_ranges(&self) -> &[(RawFd, RawFd)] {
        &self.complement_close_ranges
    }

    pub(crate) fn transports(&self) -> &[UnixTransportFd] {
        &self.transports
    }

    pub(crate) const fn captured_open_file_limit(&self) -> NonZeroU64 {
        self.captured_open_file_limit
    }
}

pub(crate) struct PreparedWrapper {
    pub(crate) executable: OwnedFd,
    pub(crate) executable_identity: ExecutableIdentity,
    pub(crate) argv: Box<[CString]>,
    argv_pointers: Box<[*const libc::c_char]>,
    pub(crate) environment: Box<[CString]>,
    environment_pointers: Box<[*const libc::c_char]>,
    pub(crate) stdio: SpawnStdioParts,
    pub(crate) request_frame: Box<[u8]>,
    pub(crate) current_executable: OwnedFd,
    pub(crate) current_identity: ExecutableIdentity,
    pub(crate) target_identity: ExecutableIdentity,
    pub(crate) descriptor_plan: UnixDescriptorPlan,
    pub(crate) synthetics: crate::synthetic::SyntheticDirs,
}

impl PreparedWrapper {
    pub(crate) fn new(
        executable: OwnedFd,
        executable_identity: ExecutableIdentity,
        argv: Vec<CString>,
        environment: Vec<CString>,
        stdio: SpawnStdioParts,
        request_frame: Vec<u8>,
        current_executable: OwnedFd,
        current_identity: ExecutableIdentity,
        target_identity: ExecutableIdentity,
        transports: Box<[UnixTransportFd]>,
        captured_open_file_limit: NonZeroU64,
    ) -> Result<Self, SandboxError> {
        let additionally_protected = [executable.as_raw_fd(), SANDBOX_WRAPPER_ERROR_FD];
        let descriptor_plan = UnixDescriptorPlan::new(
            transports,
            captured_open_file_limit,
            &additionally_protected,
        )?;
        let argv = argv.into_boxed_slice();
        let argv_pointers = cstring_pointers(&argv).into_boxed_slice();
        let environment = environment.into_boxed_slice();
        let environment_pointers = cstring_pointers(&environment).into_boxed_slice();
        Ok(Self {
            executable,
            executable_identity,
            argv,
            argv_pointers,
            environment,
            environment_pointers,
            stdio,
            request_frame: request_frame.into_boxed_slice(),
            current_executable,
            current_identity,
            target_identity,
            descriptor_plan,
            synthetics: crate::synthetic::SyntheticDirs::default(),
        })
    }
}
#[derive(Debug)]
struct UnixChildWaiter {
    receiver: mpsc::Receiver<Result<UnixExitStatus, SandboxError>>,
}

impl UnixChildWaiter {
    fn new(pid: NonZeroU32) -> Result<Self, SandboxError> {
        Self::new_with(pid, |task| {
            thread::Builder::new()
                .name("pi-sandbox-reaper".to_owned())
                .spawn(task)
                .map(|_| ())
        })
    }
    fn new_with(
        pid: NonZeroU32,
        spawn_reaper: impl FnOnce(Box<dyn FnOnce() + Send>) -> io::Result<()>,
    ) -> Result<Self, SandboxError> {
        let (sender, receiver) = mpsc::sync_channel(1);
        spawn_reaper(Box::new(move || {
            let outcome = waitpid_exact(pid);
            let _ = sender.send(outcome);
        }))
        .map_err(|error| SandboxError::from_io(SandboxIoOperation::WaitChild, &error))?;
        Ok(Self { receiver })
    }

    #[cfg(test)]
    fn new_with_cleanup(
        pid: NonZeroU32,
        process_group: NonZeroI32,
        spawn_reaper: impl FnOnce(Box<dyn FnOnce() + Send>) -> io::Result<()>,
    ) -> Result<Self, SandboxError> {
        match Self::new_with(pid, spawn_reaper) {
            Ok(waiter) => Ok(waiter),
            Err(error) => {
                terminate_and_reap_after_registration_failure(pid, process_group);
                Err(error)
            }
        }
    }

    fn wait(self) -> Result<UnixExitStatus, SandboxError> {
        self.receiver.recv().unwrap_or_else(|_| {
            Err(SandboxError::from_io(
                SandboxIoOperation::WaitChild,
                &io::Error::from(io::ErrorKind::BrokenPipe),
            ))
        })
    }
}

pub struct LaunchedUnixProcess {
    pid: NonZeroU32,
    process_group: NonZeroI32,
    waiter: UnixChildWaiter,
}

impl LaunchedUnixProcess {
    pub const fn pid(&self) -> u32 {
        self.pid.get()
    }

    pub const fn process_group(&self) -> i32 {
        self.process_group.get()
    }

    pub fn wait(self) -> Result<UnixExitStatus, SandboxError> {
        self.waiter.wait()
    }

    pub(crate) fn signal_process_group(&self, signal: i32) -> Result<(), SandboxError> {
        signal_process_group(self.process_group, signal)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnixExitStatus {
    code: Option<i32>,
    signal: Option<NonZeroI32>,
}

impl UnixExitStatus {
    pub const fn code(self) -> Option<i32> {
        self.code
    }

    pub const fn signal(self) -> Option<i32> {
        match self.signal {
            Some(signal) => Some(signal.get()),
            None => None,
        }
    }

    pub const fn shell_code(self) -> i32 {
        match (self.code, self.signal) {
            (Some(code), _) => code,
            (_, Some(signal)) => 128 + signal.get(),
            _ => 1,
        }
    }
}

pub struct UnixPtyMaster {
    fd: OwnedFd,
}

impl UnixPtyMaster {
    pub fn try_clone_reader(&self) -> Result<File, SandboxError> {
        duplicate_file(&self.fd)
    }

    pub fn try_clone_writer(&self) -> Result<File, SandboxError> {
        duplicate_file(&self.fd)
    }

    pub fn resize(&self, size: TerminalSize) -> Result<(), SandboxError> {
        let dimensions = libc::winsize {
            ws_row: size.rows(),
            ws_col: size.columns(),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TIOCSWINSZ, &dimensions) } < 0 {
            return Err(SandboxError::from_io(
                SandboxIoOperation::MapDescriptor,
                &io::Error::last_os_error(),
            ));
        }
        Ok(())
    }
}

pub struct LaunchedUnixPty {
    process: LaunchedUnixProcess,
    master: UnixPtyMaster,
}

impl LaunchedUnixPty {
    pub fn into_parts(self) -> (LaunchedUnixProcess, UnixPtyMaster) {
        (self.process, self.master)
    }
}

pub enum LaunchedUnixHandle {
    Pipe(LaunchedUnixProcess),
    Pty(LaunchedUnixPty),
}

pub fn launch_prepared_unix(prepared: PreparedSpawn) -> Result<LaunchedUnixHandle, SandboxError> {
    match prepared.into_unix_plan() {
        #[cfg(target_os = "linux")]
        PreparedUnixPlan::Linux(plan) => launch_confined(plan.stage_wrapper()?),
        #[cfg(target_os = "macos")]
        PreparedUnixPlan::Macos(plan) => launch_confined(plan.stage_wrapper()?),
        PreparedUnixPlan::RawHost(plan) => {
            if matches!(&plan.request.stdio, SpawnStdioParts::Pty(_)) {
                launch_raw_host_pty(plan.request)
            } else {
                launch_raw_host_pipe(plan.request)
            }
        }
    }
}

fn launch_confined(mut wrapper: PreparedWrapper) -> Result<LaunchedUnixHandle, SandboxError> {
    let mut synthetics = std::mem::take(&mut wrapper.synthetics);
    let request = atomic_cloexec_pipe()?;
    let status = atomic_cloexec_pipe()?;
    let wrapper_error = atomic_cloexec_pipe()?;
    let pty = take_prepared_pty(&mut wrapper.stdio)?;
    let pid = fork_sandbox_child(
        &wrapper,
        request.read.as_raw_fd(),
        status.write.as_raw_fd(),
        wrapper_error.write.as_raw_fd(),
        pty.as_ref().map(|pair| pair.slave.as_raw_fd()),
    )?;
    let process_group = NonZeroI32::new(pid.get() as i32)
        .ok_or_else(|| invalid_protocol(ChildProtocolError::InvalidDescriptor))?;

    drop(request.read);
    drop(status.write);
    drop(wrapper_error.write);
    let pty_master = pty.map(|PtyPair { master, slave }| {
        drop(slave);
        master
    });
    let request_frame = std::mem::take(&mut wrapper.request_frame);
    drop(wrapper);

    if let Err(error) = write_all_fd(request.write.as_raw_fd(), &request_frame) {
        drop(request.write);
        drop(status.read);
        drop(wrapper_error.read);
        let _ = synthetics.cleanup();
        terminate_and_reap_after_registration_failure(pid, process_group);
        return Err(error);
    }
    drop(request.write);

    let waiter = match UnixChildWaiter::new(pid) {
        Ok(waiter) => waiter,
        Err(error) => {
            drop(status.read);
            drop(wrapper_error.read);
            let _ = synthetics.cleanup();
            terminate_and_reap_after_registration_failure(pid, process_group);
            return Err(error);
        }
    };
    if let Err(error) = await_dual_exec_eof(wrapper_error.read, status.read) {
        let _ = synthetics.cleanup();
        let _ = signal_process_group(process_group, libc::SIGKILL);
        let _ = waiter.wait();
        return Err(error);
    }
    if let Err(error) = synthetics.cleanup() {
        let _ = signal_process_group(process_group, libc::SIGKILL);
        let _ = waiter.wait();
        return Err(error);
    }
    let process = LaunchedUnixProcess {
        pid,
        process_group,
        waiter,
    };
    match pty_master {
        Some(master) => Ok(LaunchedUnixHandle::Pty(LaunchedUnixPty {
            process,
            master: UnixPtyMaster { fd: master },
        })),
        None => Ok(LaunchedUnixHandle::Pipe(process)),
    }
}

pub(crate) fn fork_sandbox_child(
    wrapper: &PreparedWrapper,
    request_fd: RawFd,
    status_fd: RawFd,
    wrapper_error_fd: RawFd,
    pty_slave_fd: Option<RawFd>,
) -> Result<NonZeroU32, SandboxError> {
    let forked = unsafe { libc::fork() };
    if forked < 0 {
        return Err(SandboxError::from_io(
            SandboxIoOperation::Fork,
            &io::Error::last_os_error(),
        ));
    }
    if forked > 0 {
        return NonZeroU32::new(forked as u32)
            .ok_or_else(|| invalid_protocol(ChildProtocolError::InvalidDescriptor));
    }

    let stage = match prepare_wrapper_child(
        wrapper,
        request_fd,
        status_fd,
        wrapper_error_fd,
        pty_slave_fd,
    ) {
        Ok(()) => WrapperStage::ExecuteWrapper,
        Err(stage) => stage,
    };
    if stage != WrapperStage::ExecuteWrapper {
        write_wrapper_failure(wrapper_error_fd, stage);
        unsafe { libc::_exit(WRAPPER_FAILURE_EXIT_CODE) };
    }
    exec_prepared_wrapper(wrapper, wrapper_error_fd)
}

fn prepare_wrapper_child(
    wrapper: &PreparedWrapper,
    request_fd: RawFd,
    status_fd: RawFd,
    wrapper_error_fd: RawFd,
    pty_slave_fd: Option<RawFd>,
) -> Result<(), WrapperStage> {
    reset_child_signals().map_err(|_| WrapperStage::MapControl)?;
    match pty_slave_fd {
        Some(slave) => prepare_pty_process(slave).map_err(|_| WrapperStage::MapStdio)?,
        None => {
            if unsafe { libc::setpgid(0, 0) } < 0 {
                return Err(WrapperStage::MapStdio);
            }
            map_spawn_stdio(&wrapper.stdio).map_err(|_| WrapperStage::MapStdio)?;
        }
    }
    map_descriptors_cycle_safe(&[
        (request_fd, SANDBOX_CHILD_REQUEST_FD),
        (status_fd, SANDBOX_CHILD_STATUS_FD),
        (wrapper.current_executable.as_raw_fd(), SANDBOX_SELF_EXE_FD),
        (wrapper_error_fd, SANDBOX_WRAPPER_ERROR_FD),
    ])
    .map_err(|_| WrapperStage::MapControl)?;
    for fd in wrapper.descriptor_plan.wrapper_keep_fds() {
        clear_cloexec(*fd).map_err(|_| WrapperStage::MapControl)?;
    }
    set_cloexec(SANDBOX_WRAPPER_ERROR_FD).map_err(|_| WrapperStage::MapControl)?;
    close_complement(wrapper.descriptor_plan.complement_close_ranges())
        .map_err(|_| WrapperStage::CloseDescriptors)?;
    revalidate_wrapper_descriptors(wrapper).map_err(|_| WrapperStage::CloseDescriptors)?;
    Ok(())
}

pub(crate) fn exec_prepared_wrapper(wrapper: &PreparedWrapper, wrapper_error_fd: RawFd) -> ! {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::syscall(
            libc::SYS_execveat,
            wrapper.executable.as_raw_fd(),
            c"".as_ptr(),
            wrapper.argv_pointers.as_ptr(),
            wrapper.environment_pointers.as_ptr(),
            libc::AT_EMPTY_PATH,
        );
    }
    #[cfg(target_os = "macos")]
    unsafe {
        libc::execve(
            wrapper.argv[0].as_ptr(),
            wrapper.argv_pointers.as_ptr(),
            wrapper.environment_pointers.as_ptr(),
        );
    }
    write_wrapper_failure(wrapper_error_fd, WrapperStage::ExecuteWrapper);
    unsafe { libc::_exit(WRAPPER_FAILURE_EXIT_CODE) }
}

pub(crate) fn launch_raw_host_pipe(
    request: PreparedSpawnRequestParts,
) -> Result<LaunchedUnixHandle, SandboxError> {
    let PreparedSpawnRequestParts {
        mut argv,
        cwd,
        environment,
        stdio,
        target_fd_mappings,
        ..
    } = request;
    let executable = resolve_raw_host_executable(&mut argv, &environment)?;
    let argv = os_cstring_sequence(&argv)?;
    let environment = environment_cstrings(&environment)?;
    let cwd = CString::new(cwd.as_path().as_os_str().as_bytes())
        .map_err(|_| invalid_protocol(ChildProtocolError::EmbeddedNul))?;
    let mappings = raw_mapping_rows(target_fd_mappings);
    let error_channel = raw_host_error_channel(&stdio, &mappings, executable.as_raw_fd())?;
    let forked = unsafe { libc::fork() };
    if forked < 0 {
        return Err(SandboxError::from_io(
            SandboxIoOperation::Fork,
            &io::Error::last_os_error(),
        ));
    }
    if forked == 0 {
        raw_host_child_exec(
            error_channel.read.as_raw_fd(),
            error_channel.write.as_raw_fd(),
            RawHostChildStdio::Pipe(&stdio),
            &executable,
            &argv,
            &environment,
            &cwd,
            &mappings,
        );
    }
    let pid = NonZeroU32::new(forked as u32)
        .ok_or_else(|| invalid_protocol(ChildProtocolError::InvalidDescriptor))?;
    let process_group = NonZeroI32::new(forked)
        .ok_or_else(|| invalid_protocol(ChildProtocolError::InvalidDescriptor))?;
    drop(error_channel.write);
    if let Err(error) = await_raw_host_exec(error_channel.read) {
        terminate_and_reap_after_registration_failure(pid, process_group);
        return Err(error);
    }
    let waiter = match UnixChildWaiter::new(pid) {
        Ok(waiter) => waiter,
        Err(error) => {
            terminate_and_reap_after_registration_failure(pid, process_group);
            return Err(error);
        }
    };
    Ok(LaunchedUnixHandle::Pipe(LaunchedUnixProcess {
        pid,
        process_group,
        waiter,
    }))
}

pub(crate) fn launch_raw_host_pty(
    request: PreparedSpawnRequestParts,
) -> Result<LaunchedUnixHandle, SandboxError> {
    let PreparedSpawnRequestParts {
        mut argv,
        cwd,
        environment,
        stdio,
        target_fd_mappings,
        ..
    } = request;
    let size = match stdio {
        SpawnStdioParts::Pty(size) => size,
        _ => return Err(invalid_protocol(ChildProtocolError::InvalidTargetMapping)),
    };
    let pty = open_cloexec_pty(size)?;
    let executable = resolve_raw_host_executable(&mut argv, &environment)?;
    let argv = os_cstring_sequence(&argv)?;
    let environment = environment_cstrings(&environment)?;
    let cwd = CString::new(cwd.as_path().as_os_str().as_bytes())
        .map_err(|_| invalid_protocol(ChildProtocolError::EmbeddedNul))?;
    let mappings = raw_mapping_rows(target_fd_mappings);
    let pty_stdio = SpawnStdioParts::Pty(size);
    let error_channel = raw_host_error_channel(&pty_stdio, &mappings, executable.as_raw_fd())?;
    let forked = unsafe { libc::fork() };
    if forked < 0 {
        return Err(SandboxError::from_io(
            SandboxIoOperation::Fork,
            &io::Error::last_os_error(),
        ));
    }
    if forked == 0 {
        raw_host_child_exec(
            error_channel.read.as_raw_fd(),
            error_channel.write.as_raw_fd(),
            RawHostChildStdio::Pty(pty.slave.as_raw_fd()),
            &executable,
            &argv,
            &environment,
            &cwd,
            &mappings,
        );
    }
    let pid = NonZeroU32::new(forked as u32)
        .ok_or_else(|| invalid_protocol(ChildProtocolError::InvalidDescriptor))?;
    let process_group = NonZeroI32::new(forked)
        .ok_or_else(|| invalid_protocol(ChildProtocolError::InvalidDescriptor))?;
    let PtyPair { master, slave } = pty;
    drop(slave);
    drop(error_channel.write);
    if let Err(error) = await_raw_host_exec(error_channel.read) {
        drop(master);
        terminate_and_reap_after_registration_failure(pid, process_group);
        return Err(error);
    }
    let waiter = match UnixChildWaiter::new(pid) {
        Ok(waiter) => waiter,
        Err(error) => {
            drop(master);
            terminate_and_reap_after_registration_failure(pid, process_group);
            return Err(error);
        }
    };
    Ok(LaunchedUnixHandle::Pty(LaunchedUnixPty {
        process: LaunchedUnixProcess {
            pid,
            process_group,
            waiter,
        },
        master: UnixPtyMaster { fd: master },
    }))
}

enum RawHostChildStdio<'a> {
    Pipe(&'a SpawnStdioParts),
    Pty(RawFd),
}

fn raw_host_child_exec(
    error_read_fd: RawFd,
    error_write_fd: RawFd,
    stdio: RawHostChildStdio<'_>,
    executable: &OwnedFd,
    argv: &[CString],
    environment: &[CString],
    cwd: &CString,
    mappings: &[(OwnedFd, RawFd)],
) -> ! {
    close_raw(error_read_fd);
    let error_fd = error_write_fd;
    if matches!(stdio, RawHostChildStdio::Pipe(_)) && unsafe { libc::setpgid(0, 0) } < 0 {
        raw_host_child_failure(error_fd, RawHostStage::PrepareProcess, None);
    }
    if let Err(error) = reset_child_signals() {
        raw_host_child_failure(error_fd, RawHostStage::PrepareProcess, Some(&error));
    }
    let stdio_outcome = match stdio {
        RawHostChildStdio::Pipe(stdio) => map_spawn_stdio(stdio),
        RawHostChildStdio::Pty(slave) => prepare_pty_process(slave),
    };
    if let Err(error) = stdio_outcome {
        raw_host_child_failure(error_fd, RawHostStage::MapStdio, Some(&error));
    }
    if let Err(error) = map_descriptors_cycle_safe(&mapping_pairs(mappings)) {
        raw_host_child_failure(error_fd, RawHostStage::MapDescriptors, Some(&error));
    }
    if unsafe { libc::chdir(cwd.as_ptr()) } < 0 {
        raw_host_child_failure(error_fd, RawHostStage::ChangeDirectory, None);
    }
    if let Err(error) = clear_cloexec(executable.as_raw_fd()) {
        raw_host_child_failure(error_fd, RawHostStage::PrepareExecutable, Some(&error));
    }
    let argv_ptrs = cstring_pointers(argv);
    let env_ptrs = cstring_pointers(environment);
    unsafe {
        libc::fexecve(
            executable.as_raw_fd(),
            argv_ptrs.as_ptr(),
            env_ptrs.as_ptr(),
        );
    }
    raw_host_child_failure(error_fd, RawHostStage::ExecuteTarget, None);
}

fn raw_host_error_channel(
    stdio: &SpawnStdioParts,
    mappings: &[(OwnedFd, RawFd)],
    executable_fd: RawFd,
) -> Result<CloexecPipe, SandboxError> {
    let channel = atomic_cloexec_pipe()?;
    let mut maximum = executable_fd
        .max(channel.read.as_raw_fd())
        .max(channel.write.as_raw_fd())
        .max(2);
    match stdio {
        SpawnStdioParts::Piped(stdin, stdout, stderr)
        | SpawnStdioParts::Merged(stdin, stdout, stderr) => {
            maximum = maximum
                .max(stdin.as_raw_fd())
                .max(stdout.as_raw_fd())
                .max(stderr.as_raw_fd());
        }
        SpawnStdioParts::Null | SpawnStdioParts::Pty(_) | SpawnStdioParts::Inherit => {}
    }
    for (source, target) in mappings {
        maximum = maximum.max(source.as_raw_fd()).max(*target);
    }
    let write = duplicate_cloexec(
        channel.write.as_raw_fd(),
        maximum.saturating_add(1).max(MINIMUM_TRANSPORT_FD),
    )?;
    Ok(CloexecPipe {
        read: channel.read,
        write: unsafe { OwnedFd::from_raw_fd(write) },
    })
}

fn raw_host_child_failure(error_fd: RawFd, stage: RawHostStage, error: Option<&SandboxError>) -> ! {
    let errno = error
        .and_then(|error| match error {
            SandboxError::System { raw_os_error, .. } => *raw_os_error,
            _ => None,
        })
        .or_else(|| io::Error::last_os_error().raw_os_error())
        .unwrap_or(libc::EINVAL);
    let _ = write_all_raw(error_fd, &RawHostErrorFrame { stage, errno }.encode());
    unsafe { libc::_exit(WRAPPER_FAILURE_EXIT_CODE) }
}

fn await_raw_host_exec(error: OwnedFd) -> Result<(), SandboxError> {
    let mut channel = HandshakeChannel::new(error, RAW_HOST_ERROR_FRAME_BYTES);
    while !channel.is_eof {
        channel.drain()?;
    }
    if channel.length == 0 {
        return Ok(());
    }
    let bytes: [u8; RAW_HOST_ERROR_FRAME_BYTES] = channel.bytes[..RAW_HOST_ERROR_FRAME_BYTES]
        .try_into()
        .map_err(|_| invalid_protocol(ChildProtocolError::InvalidStatusFrame))?;
    let frame = RawHostErrorFrame::decode(bytes)?;
    Err(SandboxError::from_io(
        frame.stage.operation(),
        &io::Error::from_raw_os_error(frame.errno),
    ))
}

struct CloexecPipe {
    read: OwnedFd,
    write: OwnedFd,
}

fn atomic_cloexec_pipe() -> Result<CloexecPipe, SandboxError> {
    let mut fds = [-1; 2];
    #[cfg(target_os = "linux")]
    let result = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(libc::EINVAL) | Some(libc::ENOSYS)
        ) {
            return Err(SandboxError::AtomicCloexecUnavailable);
        }
        return Err(SandboxError::from_io(
            SandboxIoOperation::CreateChannel,
            &error,
        ));
    }
    Ok(CloexecPipe {
        read: unsafe { OwnedFd::from_raw_fd(fds[0]) },
        write: unsafe { OwnedFd::from_raw_fd(fds[1]) },
    })
}

struct PtyPair {
    master: OwnedFd,
    slave: OwnedFd,
}

fn open_cloexec_pty(size: TerminalSize) -> Result<PtyPair, SandboxError> {
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if master < 0 {
        return Err(SandboxError::from_io(
            SandboxIoOperation::CreateChannel,
            &io::Error::last_os_error(),
        ));
    }
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    if unsafe { libc::grantpt(master.as_raw_fd()) } < 0
        || unsafe { libc::unlockpt(master.as_raw_fd()) } < 0
    {
        return Err(SandboxError::from_io(
            SandboxIoOperation::CreateChannel,
            &io::Error::last_os_error(),
        ));
    }
    let mut name = [0_i8; 256];
    if unsafe { libc::ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()) } != 0 {
        return Err(SandboxError::from_io(
            SandboxIoOperation::CreateChannel,
            &io::Error::last_os_error(),
        ));
    }
    let slave_raw = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if slave_raw < 0 {
        return Err(SandboxError::from_io(
            SandboxIoOperation::CreateChannel,
            &io::Error::last_os_error(),
        ));
    }
    let pair = PtyPair {
        master,
        slave: unsafe { OwnedFd::from_raw_fd(slave_raw) },
    };
    let master = UnixPtyMaster {
        fd: duplicate_owned(&pair.master)?,
    };
    master.resize(size)?;
    Ok(pair)
}

fn take_prepared_pty(stdio: &mut SpawnStdioParts) -> Result<Option<PtyPair>, SandboxError> {
    match stdio {
        SpawnStdioParts::Pty(size) => open_cloexec_pty(*size).map(Some),
        _ => Ok(None),
    }
}

fn map_spawn_stdio(stdio: &SpawnStdioParts) -> Result<(), SandboxError> {
    match stdio {
        SpawnStdioParts::Null => {
            let null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
            if null < 0 {
                return Err(SandboxError::from_io(
                    SandboxIoOperation::MapDescriptor,
                    &io::Error::last_os_error(),
                ));
            }
            let mapped = map_descriptors_cycle_safe(&[(null, 0), (null, 1), (null, 2)]);
            close_raw(null);
            mapped
        }
        SpawnStdioParts::Piped(stdin, stdout, stderr)
        | SpawnStdioParts::Merged(stdin, stdout, stderr) => map_descriptors_cycle_safe(&[
            (stdin.as_raw_fd(), 0),
            (stdout.as_raw_fd(), 1),
            (stderr.as_raw_fd(), 2),
        ]),
        SpawnStdioParts::Inherit => Ok(()),
        SpawnStdioParts::Pty(_) => Err(invalid_protocol(ChildProtocolError::InvalidDescriptor)),
    }
}

fn prepare_pty_process(slave: RawFd) -> Result<(), SandboxError> {
    if unsafe { libc::setsid() } < 0 {
        return Err(SandboxError::from_io(
            SandboxIoOperation::MapDescriptor,
            &io::Error::last_os_error(),
        ));
    }
    map_descriptors_cycle_safe(&[(slave, 0), (slave, 1), (slave, 2)])?;
    if unsafe { libc::ioctl(0, libc::TIOCSCTTY, 0) } < 0 {
        return Err(SandboxError::from_io(
            SandboxIoOperation::MapDescriptor,
            &io::Error::last_os_error(),
        ));
    }
    Ok(())
}

fn map_descriptors_cycle_safe(mappings: &[(RawFd, RawFd)]) -> Result<(), SandboxError> {
    if mappings.is_empty() {
        return Ok(());
    }
    let minimum = mappings
        .iter()
        .flat_map(|(source, target)| [*source, *target])
        .max()
        .unwrap_or(MINIMUM_TRANSPORT_FD)
        .saturating_add(1)
        .max(MINIMUM_TRANSPORT_FD);
    let mut duplicates = Vec::with_capacity(mappings.len());
    for (source, target) in mappings {
        duplicates.push((duplicate_cloexec(*source, minimum)?, *target));
    }
    for (source, target) in &duplicates {
        if unsafe { libc::dup2(*source, *target) } < 0 {
            return Err(SandboxError::from_io(
                SandboxIoOperation::MapDescriptor,
                &io::Error::last_os_error(),
            ));
        }
    }
    for (source, target) in duplicates {
        close_raw(source);
        clear_cloexec(target)?;
    }
    Ok(())
}

fn close_complement(ranges: &[(RawFd, RawFd)]) -> Result<(), SandboxError> {
    for &(start, end) in ranges {
        #[cfg(target_os = "linux")]
        {
            let result =
                unsafe { libc::syscall(libc::SYS_close_range, start as u32, end as u32, 0) };
            if result == 0 {
                continue;
            }
            let error = io::Error::last_os_error();
            if !matches!(
                error.raw_os_error(),
                Some(libc::ENOSYS) | Some(libc::EINVAL)
            ) {
                return Err(SandboxError::from_io(
                    SandboxIoOperation::MapDescriptor,
                    &error,
                ));
            }
        }
        for fd in start..=end {
            close_raw(fd);
        }
    }
    Ok(())
}

fn complement_ranges(start: RawFd, end: RawFd, keep: &BTreeSet<RawFd>) -> Vec<(RawFd, RawFd)> {
    if start > end {
        return Vec::new();
    }
    let mut ranges = Vec::new();
    let mut range_start = start;
    for fd in keep.range(start..=end).copied() {
        if range_start < fd {
            ranges.push((range_start, fd - 1));
        }
        range_start = fd.saturating_add(1);
    }
    if range_start <= end {
        ranges.push((range_start, end));
    }
    ranges
}

fn revalidate_wrapper_descriptors(wrapper: &PreparedWrapper) -> Result<(), SandboxError> {
    if !wrapper
        .executable_identity
        .matches_fd(wrapper.executable.as_raw_fd())?
        || !wrapper.current_identity.matches_fd(SANDBOX_SELF_EXE_FD)?
    {
        return Err(invalid_protocol(
            ChildProtocolError::DescriptorIdentityMismatch,
        ));
    }
    let target_fd = wrapper
        .descriptor_plan
        .transports()
        .iter()
        .find(|transport| transport.purpose() == UnixTransportPurpose::TargetExecutable)
        .ok_or_else(|| invalid_protocol(ChildProtocolError::InvalidDescriptor))?
        .raw_fd();
    if !wrapper.target_identity.matches_fd(target_fd)? {
        return Err(invalid_protocol(
            ChildProtocolError::DescriptorIdentityMismatch,
        ));
    }
    for transport in wrapper.descriptor_plan.transports() {
        ExecutableIdentity::from_fd(transport.raw_fd())?;
    }
    Ok(())
}

fn await_dual_exec_eof(wrapper_error: OwnedFd, child_status: OwnedFd) -> Result<(), SandboxError> {
    let mut channels = [
        HandshakeChannel::new(wrapper_error, WRAPPER_ERROR_FRAME_BYTES),
        HandshakeChannel::new(child_status, CHILD_STATUS_FRAME_BYTES),
    ];
    while channels.iter().any(|channel| !channel.is_eof) {
        let mut poll_fds = [
            libc::pollfd {
                fd: channels[0].fd.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP,
                revents: 0,
            },
            libc::pollfd {
                fd: channels[1].fd.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP,
                revents: 0,
            },
        ];
        let result = unsafe { libc::poll(poll_fds.as_mut_ptr(), poll_fds.len() as _, -1) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(SandboxError::from_io(
                SandboxIoOperation::CreateChannel,
                &error,
            ));
        }
        for (channel, poll_fd) in channels.iter_mut().zip(poll_fds) {
            if poll_fd.revents != 0 {
                channel.drain()?;
            }
        }
    }
    if channels[0].length != 0 {
        let bytes: [u8; WRAPPER_ERROR_FRAME_BYTES] = channels[0].bytes[..WRAPPER_ERROR_FRAME_BYTES]
            .try_into()
            .map_err(|_| invalid_protocol(ChildProtocolError::InvalidStatusFrame))?;
        let frame = WrapperErrorFrame::decode(bytes)?;
        return Err(SandboxError::from_io(
            SandboxIoOperation::ExecuteWrapper,
            &io::Error::from_raw_os_error(frame.errno),
        ));
    }
    if channels[1].length != 0 {
        let bytes: [u8; CHILD_STATUS_FRAME_BYTES] = channels[1].bytes[..CHILD_STATUS_FRAME_BYTES]
            .try_into()
            .map_err(|_| invalid_protocol(ChildProtocolError::InvalidStatusFrame))?;
        let frame = ChildStatusFrame::decode(bytes)?;
        return Err(SandboxError::from_io(
            SandboxIoOperation::ExecuteTarget,
            &io::Error::from_raw_os_error(frame.errno.unwrap_or(libc::EINVAL)),
        ));
    }
    Ok(())
}

struct HandshakeChannel {
    fd: OwnedFd,
    bytes: [u8; WRAPPER_ERROR_FRAME_BYTES],
    length: usize,
    expected: usize,
    is_eof: bool,
}

impl HandshakeChannel {
    fn new(fd: OwnedFd, expected: usize) -> Self {
        Self {
            fd,
            bytes: [0; WRAPPER_ERROR_FRAME_BYTES],
            length: 0,
            expected,
            is_eof: false,
        }
    }

    fn drain(&mut self) -> Result<(), SandboxError> {
        loop {
            if self.length == self.expected {
                let mut trailing = [0_u8; 1];
                let read =
                    unsafe { libc::read(self.fd.as_raw_fd(), trailing.as_mut_ptr().cast(), 1) };
                if read == 0 {
                    self.is_eof = true;
                    return Ok(());
                }
                if read > 0 {
                    return Err(invalid_protocol(ChildProtocolError::InvalidStatusFrame));
                }
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Ok(());
                }
                return Err(SandboxError::from_io(
                    SandboxIoOperation::CreateChannel,
                    &error,
                ));
            }
            let read = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    self.bytes[self.length..self.expected].as_mut_ptr().cast(),
                    self.expected - self.length,
                )
            };
            if read == 0 {
                self.is_eof = true;
                if self.length != 0 && self.length != self.expected {
                    return Err(invalid_protocol(ChildProtocolError::InvalidStatusFrame));
                }
                return Ok(());
            }
            if read > 0 {
                self.length += read as usize;
                continue;
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(());
            }
            return Err(SandboxError::from_io(
                SandboxIoOperation::CreateChannel,
                &error,
            ));
        }
    }
}

fn write_wrapper_failure(fd: RawFd, stage: WrapperStage) {
    let errno = io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EINVAL);
    let bytes = WrapperErrorFrame { stage, errno }.encode();
    let _ = write_all_raw(fd, &bytes);
}

fn terminate_and_reap_after_registration_failure(pid: NonZeroU32, process_group: NonZeroI32) {
    let _ = signal_process_group(process_group, libc::SIGKILL);
    unsafe {
        libc::kill(pid.get() as libc::pid_t, libc::SIGKILL);
    }
    let _ = waitpid_exact(pid);
}

fn waitpid_exact(pid: NonZeroU32) -> Result<UnixExitStatus, SandboxError> {
    let expected = pid.get() as libc::pid_t;
    loop {
        let mut status = 0;
        let waited = unsafe { libc::waitpid(expected, &mut status, 0) };
        if waited == expected {
            return Ok(decode_wait_status(status));
        }
        if waited < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(SandboxError::from_io(
            SandboxIoOperation::WaitChild,
            &io::Error::last_os_error(),
        ));
    }
}

fn decode_wait_status(status: i32) -> UnixExitStatus {
    if libc::WIFEXITED(status) {
        UnixExitStatus {
            code: Some(libc::WEXITSTATUS(status)),
            signal: None,
        }
    } else if libc::WIFSIGNALED(status) {
        UnixExitStatus {
            code: None,
            signal: NonZeroI32::new(libc::WTERMSIG(status)),
        }
    } else {
        UnixExitStatus {
            code: None,
            signal: None,
        }
    }
}

fn signal_process_group(group: NonZeroI32, signal: i32) -> Result<(), SandboxError> {
    if unsafe { libc::kill(-group.get(), signal) } < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(SandboxError::from_io(SandboxIoOperation::WaitChild, &error));
    }
    Ok(())
}

fn reset_child_signals() -> Result<(), SandboxError> {
    for signal in [
        libc::SIGCHLD,
        libc::SIGHUP,
        libc::SIGINT,
        libc::SIGQUIT,
        libc::SIGTERM,
        libc::SIGALRM,
    ] {
        if unsafe { libc::signal(signal, libc::SIG_DFL) } == libc::SIG_ERR {
            return Err(SandboxError::from_io(
                SandboxIoOperation::Fork,
                &io::Error::last_os_error(),
            ));
        }
    }
    let mut mask = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    if unsafe { libc::sigemptyset(mask.as_mut_ptr()) } < 0
        || unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, mask.as_ptr(), std::ptr::null_mut()) }
            != 0
    {
        return Err(SandboxError::from_io(
            SandboxIoOperation::Fork,
            &io::Error::last_os_error(),
        ));
    }
    Ok(())
}

fn raw_mapping_rows(mappings: Box<[crate::TargetFdMapping]>) -> Vec<(OwnedFd, RawFd)> {
    mappings
        .into_vec()
        .into_iter()
        .map(|mapping| {
            let (source, canonical) = mapping.into_parts();
            (source, canonical.target() as RawFd)
        })
        .collect()
}

fn mapping_pairs(mappings: &[(OwnedFd, RawFd)]) -> Vec<(RawFd, RawFd)> {
    mappings
        .iter()
        .map(|(source, target)| (source.as_raw_fd(), *target))
        .collect()
}

fn resolve_raw_host_executable(
    argv: &mut [OsString],
    environment: &std::collections::BTreeMap<OsString, OsString>,
) -> Result<OwnedFd, SandboxError> {
    let Some(program) = argv.first_mut() else {
        return Err(invalid_protocol(ChildProtocolError::EmptyArgv));
    };
    let candidates = if Path::new(program.as_os_str()).components().count() > 1 {
        vec![std::path::PathBuf::from(program.as_os_str())]
    } else {
        let path = environment
            .get(std::ffi::OsStr::new("PATH"))
            .ok_or_else(|| {
                SandboxError::from_io(
                    SandboxIoOperation::OpenExecutable,
                    &io::Error::from(io::ErrorKind::NotFound),
                )
            })?;
        std::env::split_paths(path)
            .map(|directory| directory.join(program.as_os_str()))
            .collect()
    };
    for candidate in candidates {
        let canonical = match std::fs::canonicalize(&candidate) {
            Ok(canonical) => canonical,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                ) =>
            {
                continue;
            }
            Err(error) => {
                return Err(SandboxError::from_io(
                    SandboxIoOperation::OpenExecutable,
                    &error,
                ));
            }
        };
        match crate::launcher::open_pinned_executable(&canonical) {
            Ok((executable, _)) => {
                *program = canonical.into_os_string();
                return Ok(executable);
            }
            Err(SandboxError::System {
                kind: crate::SandboxIoKind::NotFound,
                ..
            }) => {}
            Err(SandboxError::System {
                kind: crate::SandboxIoKind::PermissionDenied,
                ..
            }) => {}
            Err(error) => return Err(error),
        }
    }
    Err(SandboxError::from_io(
        SandboxIoOperation::OpenExecutable,
        &io::Error::from(io::ErrorKind::NotFound),
    ))
}

fn os_cstring_sequence(values: &[OsString]) -> Result<Vec<CString>, SandboxError> {
    values
        .iter()
        .map(|value| {
            CString::new(value.as_os_str().as_bytes())
                .map_err(|_| invalid_protocol(ChildProtocolError::EmbeddedNul))
        })
        .collect()
}

fn environment_cstrings(
    environment: &std::collections::BTreeMap<OsString, OsString>,
) -> Result<Vec<CString>, SandboxError> {
    environment
        .iter()
        .map(|(name, value)| {
            let mut row = Vec::with_capacity(name.len() + value.len() + 1);
            row.extend_from_slice(name.as_os_str().as_bytes());
            row.push(b'=');
            row.extend_from_slice(value.as_os_str().as_bytes());
            CString::new(row).map_err(|_| invalid_protocol(ChildProtocolError::EmbeddedNul))
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

fn write_all_fd(fd: RawFd, bytes: &[u8]) -> Result<(), SandboxError> {
    write_all_raw(fd, bytes)
        .map_err(|error| SandboxError::from_io(SandboxIoOperation::CreateChannel, &error))
}

fn write_all_raw(fd: RawFd, mut bytes: &[u8]) -> Result<(), io::Error> {
    while !bytes.is_empty() {
        let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if written > 0 {
            bytes = &bytes[written as usize..];
            continue;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    Ok(())
}

fn duplicate_owned(fd: &OwnedFd) -> Result<OwnedFd, SandboxError> {
    let duplicate = duplicate_cloexec(fd.as_raw_fd(), 0)?;
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}

fn duplicate_file(fd: &OwnedFd) -> Result<File, SandboxError> {
    duplicate_owned(fd).map(File::from)
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
    update_cloexec(fd, false)
}

fn set_cloexec(fd: RawFd) -> Result<(), SandboxError> {
    update_cloexec(fd, true)
}

fn update_cloexec(fd: RawFd, should_set: bool) -> Result<(), SandboxError> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(SandboxError::from_io(
            SandboxIoOperation::MapDescriptor,
            &io::Error::last_os_error(),
        ));
    }
    let updated = if should_set {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, updated) } < 0 {
        return Err(SandboxError::from_io(
            SandboxIoOperation::MapDescriptor,
            &io::Error::last_os_error(),
        ));
    }
    Ok(())
}

fn close_raw(fd: RawFd) {
    unsafe {
        libc::close(fd);
    }
}

fn invalid_protocol(failure: ChildProtocolError) -> SandboxError {
    SandboxError::InvalidChildProtocol { failure }
}

pub(crate) fn harden_current_process() {
    #[cfg(target_os = "linux")]
    {
        let _ = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
    }
    let limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let _ = unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limit) };
}

#[used]
#[cfg_attr(target_os = "linux", unsafe(link_section = ".init_array"))]
#[cfg_attr(
    target_os = "macos",
    unsafe(link_section = "__DATA,__mod_init_func")
)]
static HARDEN_AT_LOAD: extern "C" fn() = {
    extern "C" fn harden() {
        crate::harden_current_process();
    }
    harden
};

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn process_is_not_dumpable_after_hardening() {
        crate::harden_current_process();
        let status = std::fs::read_to_string("/proc/self/status").expect("status");
        let dumpable = status.lines().find(|line| line.contains("Dumpable"));
        if let Some(dumpable) = dumpable {
            assert!(
                dumpable.rsplit_once(':').is_some_and(|(_, value)| value.trim() == "0"),
                "hardening must make the process non-dumpable: {dumpable}"
            );
        }
        let mut limit = libc::rlimit {
            rlim_cur: 1,
            rlim_max: 1,
        };
        let got = unsafe { libc::getrlimit(libc::RLIMIT_CORE, &mut limit) };
        assert_eq!(got, 0, "getrlimit");
        assert_eq!(limit.rlim_cur, 0, "soft RLIMIT_CORE");
        assert_eq!(limit.rlim_max, 0, "hard RLIMIT_CORE");
    }

    fn raw_parts(
        program: &Path,
        arguments: &[&str],
        stdio: SpawnStdioParts,
    ) -> PreparedSpawnRequestParts {
        let mut argv = vec![program.as_os_str().to_owned()];
        argv.extend(arguments.iter().map(OsString::from));
        PreparedSpawnRequestParts {
            family: if matches!(stdio, SpawnStdioParts::Pty(_)) {
                crate::SpawnFamily::ManagedPtyProcess
            } else {
                crate::SpawnFamily::Hook
            },
            argv: argv.into_boxed_slice(),
            cwd: crate::AbsolutePath::parse(
                program.parent().expect("program parent").to_path_buf(),
            )
            .expect("absolute program parent"),
            environment: std::collections::BTreeMap::new(),
            stdio,
            target_fd_mappings: Vec::new().into_boxed_slice(),
            mount_sources: Vec::new().into_boxed_slice(),
            retained_executable: None,
            retained_working_directory: None,
        }
    }

    fn executable_script(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().expect("script root");
        let path = root.path().join("target-script");
        std::fs::write(&path, body).expect("write target script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("make target script executable");
        (root, path)
    }

    fn pipe_stdio() -> (SpawnStdioParts, File) {
        let output = atomic_cloexec_pipe().expect("stdout pipe");
        let stdin: OwnedFd = File::open("/dev/null").expect("open stdin null").into();
        let stderr: OwnedFd = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .expect("open stderr null")
            .into();
        (
            SpawnStdioParts::Piped(stdin, output.write, stderr),
            File::from(output.read),
        )
    }

    #[test]
    fn raw_host_direct_script_executes_with_pipe_and_pty() {
        use std::io::Read as _;

        let (_root, script) =
            executable_script("#!/bin/sh\n[ \"$1\" = marker ] || exit 7\nprintf script-ok\n");
        let (stdio, mut output) = pipe_stdio();
        let process = match launch_raw_host_pipe(raw_parts(&script, &["marker"], stdio))
            .expect("launch direct script with pipe")
        {
            LaunchedUnixHandle::Pipe(process) => process,
            LaunchedUnixHandle::Pty(_) => panic!("pipe launch returned PTY"),
        };
        let mut text = String::new();
        output
            .read_to_string(&mut text)
            .expect("read direct script output");
        assert_eq!(process.wait().expect("wait direct script").code(), Some(0));
        assert_eq!(text, "script-ok");

        let pty = match launch_raw_host_pty(raw_parts(
            &script,
            &["marker"],
            SpawnStdioParts::Pty(TerminalSize::default()),
        ))
        .expect("launch direct script with PTY")
        {
            LaunchedUnixHandle::Pty(pty) => pty,
            LaunchedUnixHandle::Pipe(_) => panic!("PTY launch returned pipe"),
        };
        let (process, _master) = pty.into_parts();
        assert_eq!(
            process.wait().expect("wait PTY direct script").code(),
            Some(0)
        );
    }

    #[test]
    fn raw_host_binary_control_executes_normally() {
        let process = match launch_raw_host_pipe(raw_parts(
            Path::new("/bin/true"),
            &[],
            SpawnStdioParts::Null,
        ))
        .expect("launch binary control")
        {
            LaunchedUnixHandle::Pipe(process) => process,
            LaunchedUnixHandle::Pty(_) => panic!("binary pipe launch returned PTY"),
        };
        assert_eq!(process.wait().expect("wait binary control").code(), Some(0));
    }
    #[test]
    fn raw_host_real_exit_125_remains_process_status() {
        let (_root, script) = executable_script("#!/bin/sh\nexit 125\n");
        let process = match launch_raw_host_pipe(raw_parts(&script, &[], SpawnStdioParts::Null))
            .expect("launch script that exits 125")
        {
            LaunchedUnixHandle::Pipe(process) => process,
            LaunchedUnixHandle::Pty(_) => panic!("pipe launch returned PTY"),
        };
        assert_eq!(
            process.wait().expect("wait exit-125 script").code(),
            Some(125)
        );
    }

    #[test]
    fn raw_host_exec_failure_is_reported_before_handle_return() {
        let (_root, script) = executable_script("#!/definitely-missing-interpreter\n");
        let error = match launch_raw_host_pipe(raw_parts(&script, &[], SpawnStdioParts::Null)) {
            Ok(_) => panic!("missing interpreter must fail during launch"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            SandboxError::System {
                operation: SandboxIoOperation::ExecuteTarget,
                kind: crate::SandboxIoKind::NotFound,
                raw_os_error: Some(libc::ENOENT),
            }
        ));
    }

    #[test]
    fn raw_host_executable_resolves_bare_name_from_path() {
        let root = tempfile::tempdir().expect("fixture root");
        let executable = root.path().join("fixture-command");
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").expect("write executable");
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
            .expect("chmod executable");
        let mut argv = vec![OsString::from("fixture-command")];
        let environment = std::collections::BTreeMap::from([(
            OsString::from("PATH"),
            root.path().as_os_str().to_os_string(),
        )]);

        let pinned =
            resolve_raw_host_executable(&mut argv, &environment).expect("resolve bare executable");

        assert_eq!(argv[0], executable.as_os_str());
        assert!(crate::launcher::ExecutableIdentity::from_fd(pinned.as_raw_fd()).is_ok());
    }

    #[test]
    fn raw_host_executable_preserves_explicit_path() {
        let explicit = OsString::from("/usr/bin/printf");
        let mut argv = vec![explicit.clone()];
        let environment = std::collections::BTreeMap::new();

        let pinned =
            resolve_raw_host_executable(&mut argv, &environment).expect("preserve explicit path");

        assert_eq!(argv[0], explicit);
        assert!(crate::launcher::ExecutableIdentity::from_fd(pinned.as_raw_fd()).is_ok());
    }

    #[test]
    fn wrapper_exec_preserves_only_declared_transport_fds() {
        let keep = BTreeSet::from([7, 9, 12]);
        assert_eq!(complement_ranges(7, 12, &keep), vec![(8, 8), (10, 11)]);
    }

    #[test]
    fn wrapper_and_child_error_channels_have_distinct_close_owners() {
        assert_eq!(SANDBOX_CHILD_STATUS_FD, 4);
        assert_eq!(SANDBOX_WRAPPER_ERROR_FD, 6);
        assert_ne!(SANDBOX_CHILD_STATUS_FD, SANDBOX_WRAPPER_ERROR_FD);
        let keep = BTreeSet::from([3, 4, 5, 7]);
        assert!(!keep.contains(&SANDBOX_WRAPPER_ERROR_FD));
    }

    #[test]
    fn unrelated_fd_does_not_cross_wrapper_or_target_exec() {
        let keep = BTreeSet::from([3, 4, 5, 64, 65]);
        let ranges = complement_ranges(7, 70, &keep);
        assert!(ranges.iter().any(|(start, end)| *start <= 63 && 63 <= *end));
        assert!(!ranges.iter().any(|(start, end)| *start <= 64 && 64 <= *end));
    }

    #[test]
    fn reaper_creation_failure_kills_and_reaps() {
        let forked = unsafe { libc::fork() };
        assert!(forked >= 0, "fork test child");
        if forked == 0 {
            unsafe {
                libc::setpgid(0, 0);
                libc::pause();
                libc::_exit(0);
            }
        }
        let pid = NonZeroU32::new(forked as u32).expect("nonzero pid");
        let group = NonZeroI32::new(forked).expect("nonzero group");
        let error = UnixChildWaiter::new_with_cleanup(pid, group, |_| {
            Err(io::Error::other("injected reaper failure"))
        })
        .expect_err("registration must fail");
        assert!(matches!(
            error,
            SandboxError::System {
                operation: SandboxIoOperation::WaitChild,
                ..
            }
        ));
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(forked, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn complement_is_exact_keep_set_inverse() {
        let keep = BTreeSet::from([7, 8, 11, 15]);
        let ranges = complement_ranges(7, 15, &keep);
        for fd in 7..=15 {
            let is_closed = ranges.iter().any(|(start, end)| *start <= fd && fd <= *end);
            assert_ne!(keep.contains(&fd), is_closed);
        }
    }
}
