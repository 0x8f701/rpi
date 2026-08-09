// Derived from OpenAI Codex under Apache-2.0 and modified for pi-rs.
// Upstream commit 646f7c0a91b8e327d263335da68ae8ef212895ce; see `../NOTICE`.

use std::collections::BTreeMap;

use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition,
    SeccompFilter, SeccompRule, TargetArch, apply_filter,
};

use crate::{NetworkMode, SandboxError};

pub fn apply_current_thread_restrictions(network: NetworkMode) -> Result<(), SandboxError> {
    install_seccomp_current_thread(network)
}

#[allow(unsafe_code)]
pub fn set_no_new_privs_current_thread() -> Result<(), SandboxError> {
    // SAFETY: PR_SET_NO_NEW_PRIVS takes integer arguments only. Setting it to 1
    // is irreversible for this thread and is the required prerequisite before
    // installing the inherited seccomp filter.
    let result = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if result != 0 {
        return Err(SandboxError::System(std::io::Error::last_os_error()));
    }
    Ok(())
}

// `apply_filter` sets no_new_privs itself, so finish every fallible build step first.
pub fn install_seccomp_current_thread(network: NetworkMode) -> Result<(), SandboxError> {
    install_seccomp_current_thread_with(network, target_architecture(), |program| {
        apply_filter(program).map_err(|error| SandboxError::Seccomp(error.to_string()))
    })
}

fn install_seccomp_current_thread_with(
    network: NetworkMode,
    architecture: Result<TargetArch, SandboxError>,
    install: impl FnOnce(&BpfProgram) -> Result<(), SandboxError>,
) -> Result<(), SandboxError> {
    let program = compile_filter(network, architecture?)?;
    install(&program)
}

fn build_filter(
    network: NetworkMode,
    architecture: TargetArch,
) -> Result<SeccompFilter, SandboxError> {
    let mut rules = BTreeMap::new();
    for syscall in [
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ] {
        deny_syscall(&mut rules, syscall);
    }
    match network {
        NetworkMode::FullAccess => {}
        NetworkMode::Restricted => append_restricted_network_rules(&mut rules)?,
        NetworkMode::ProxyRouted => append_proxy_network_rules(&mut rules)?,
    }
    SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        architecture,
    )
    .map_err(|error| SandboxError::Seccomp(error.to_string()))
}

fn compile_filter(
    network: NetworkMode,
    architecture: TargetArch,
) -> Result<BpfProgram, SandboxError> {
    build_filter(network, architecture)?
        .try_into()
        .map_err(|error: seccompiler::BackendError| SandboxError::Seccomp(error.to_string()))
}

fn deny_syscall(rules: &mut BTreeMap<i64, Vec<SeccompRule>>, syscall: i64) {
    rules.insert(syscall, Vec::new());
}

fn append_restricted_network_rules(
    rules: &mut BTreeMap<i64, Vec<SeccompRule>>,
) -> Result<(), SandboxError> {
    for syscall in [
        libc::SYS_connect,
        libc::SYS_accept,
        libc::SYS_accept4,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_getpeername,
        libc::SYS_getsockname,
        libc::SYS_shutdown,
        libc::SYS_sendto,
        libc::SYS_sendmmsg,
        libc::SYS_recvmmsg,
        libc::SYS_getsockopt,
        libc::SYS_setsockopt,
    ] {
        deny_syscall(rules, syscall);
    }
    let deny_non_unix = SeccompRule::new(vec![condition_not_equal(libc::AF_UNIX)?])
        .map_err(|error| SandboxError::Seccomp(error.to_string()))?;
    rules.insert(libc::SYS_socket, vec![deny_non_unix.clone()]);
    rules.insert(libc::SYS_socketpair, vec![deny_non_unix]);
    Ok(())
}

fn append_proxy_network_rules(
    rules: &mut BTreeMap<i64, Vec<SeccompRule>>,
) -> Result<(), SandboxError> {
    let deny_non_ip = SeccompRule::new(vec![
        condition_not_equal(libc::AF_INET)?,
        condition_not_equal(libc::AF_INET6)?,
    ])
    .map_err(|error| SandboxError::Seccomp(error.to_string()))?;
    let deny_non_unix = SeccompRule::new(vec![condition_not_equal(libc::AF_UNIX)?])
        .map_err(|error| SandboxError::Seccomp(error.to_string()))?;
    rules.insert(libc::SYS_socket, vec![deny_non_ip]);
    rules.insert(libc::SYS_socketpair, vec![deny_non_unix]);
    Ok(())
}

fn condition_not_equal(value: i32) -> Result<SeccompCondition, SandboxError> {
    SeccompCondition::new(
        0,
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::Ne,
        value as u64,
    )
    .map_err(|error| SandboxError::Seccomp(error.to_string()))
}

fn target_architecture() -> Result<TargetArch, SandboxError> {
    if cfg!(target_arch = "x86_64") {
        Ok(TargetArch::x86_64)
    } else if cfg!(target_arch = "aarch64") {
        Ok(TargetArch::aarch64)
    } else {
        Err(SandboxError::UnsupportedArchitecture)
    }
}

#[cfg(test)]
pub(crate) fn compiled_filter(network: NetworkMode) -> Result<BpfProgram, SandboxError> {
    compile_filter(network, target_architecture()?)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn unsupported_architecture_fails_before_irreversible_install() {
        let installer_called = Cell::new(false);
        let result = install_seccomp_current_thread_with(
            NetworkMode::Restricted,
            Err(SandboxError::UnsupportedArchitecture),
            |_| {
                installer_called.set(true);
                Ok(())
            },
        );

        assert!(matches!(result, Err(SandboxError::UnsupportedArchitecture)));
        assert!(!installer_called.get());
    }
}
