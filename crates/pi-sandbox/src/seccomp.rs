use std::collections::BTreeMap;

use seccompiler::{
    BpfProgram, SeccompAction, SeccompFilter, SeccompRule, TargetArch, apply_filter,
};

use crate::child::ChildPlatformEnforcement;
use crate::{ArchitectureKind, HostPlatform, NetworkPolicy, SandboxError, SandboxIoOperation};

pub(crate) struct CompiledChildFilter(BpfProgram);

pub fn apply_current_thread_restrictions(network: NetworkPolicy) -> Result<(), SandboxError> {
    install_seccomp_current_thread(network)
}

pub fn install_seccomp_current_thread(network: NetworkPolicy) -> Result<(), SandboxError> {
    let deny_network = match network {
        NetworkPolicy::None => true,
        NetworkPolicy::FullAccess => false,
        NetworkPolicy::ProxyRouted => return Err(SandboxError::ProxyRoutedUnavailable),
    };
    apply_compiled_child_filter(&CompiledChildFilter(compile_filter(deny_network)?))
}

pub(crate) fn compile_child_filter(
    enforcement: ChildPlatformEnforcement,
) -> Result<Option<CompiledChildFilter>, SandboxError> {
    let deny_network = match enforcement {
        ChildPlatformEnforcement::LinuxNoNetwork => true,
        ChildPlatformEnforcement::LinuxFullNetwork => false,
        ChildPlatformEnforcement::MacosSeatbelt => return Ok(None),
    };
    Ok(Some(CompiledChildFilter(compile_filter(deny_network)?)))
}

pub(crate) fn apply_compiled_child_filter(
    filter: &CompiledChildFilter,
) -> Result<(), SandboxError> {
    apply_filter(&filter.0).map_err(|_| {
        SandboxError::from_io(
            SandboxIoOperation::ApplySeccomp,
            &std::io::Error::from_raw_os_error(libc::EPERM),
        )
    })
}

#[allow(unsafe_code)]
pub fn set_no_new_privs_current_thread() -> Result<(), SandboxError> {
    let result = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if result != 0 {
        return Err(SandboxError::from_io(
            SandboxIoOperation::ApplyNoNewPrivileges,
            &std::io::Error::last_os_error(),
        ));
    }
    Ok(())
}

fn compile_filter(deny_network: bool) -> Result<BpfProgram, SandboxError> {
    build_filter(deny_network, target_architecture()?)?
        .try_into()
        .map_err(|_| {
            SandboxError::from_io(
                SandboxIoOperation::ApplySeccomp,
                &std::io::Error::from_raw_os_error(libc::EINVAL),
            )
        })
}

fn build_filter(
    deny_network: bool,
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
        rules.insert(syscall, Vec::<SeccompRule>::new());
    }
    if deny_network {
        for syscall in [
            libc::SYS_socket,
            libc::SYS_socketpair,
            libc::SYS_connect,
            libc::SYS_accept,
            libc::SYS_accept4,
            libc::SYS_bind,
            libc::SYS_listen,
            libc::SYS_getpeername,
            libc::SYS_getsockname,
            libc::SYS_shutdown,
            libc::SYS_sendto,
            libc::SYS_sendmsg,
            libc::SYS_sendmmsg,
            libc::SYS_recvfrom,
            libc::SYS_recvmsg,
            libc::SYS_recvmmsg,
            libc::SYS_getsockopt,
            libc::SYS_setsockopt,
        ] {
            rules.insert(syscall, Vec::<SeccompRule>::new());
        }
    }
    SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        architecture,
    )
    .map_err(|_| {
        SandboxError::from_io(
            SandboxIoOperation::ApplySeccomp,
            &std::io::Error::from_raw_os_error(libc::EINVAL),
        )
    })
}

fn target_architecture() -> Result<TargetArch, SandboxError> {
    if cfg!(target_arch = "x86_64") {
        Ok(TargetArch::x86_64)
    } else if cfg!(target_arch = "aarch64") {
        Ok(TargetArch::aarch64)
    } else {
        Err(SandboxError::UnsupportedArchitecture {
            platform: HostPlatform::Linux,
            architecture: ArchitectureKind::Other,
        })
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::NetworkPolicy;

    #[test]
    fn seccomp_filter_compiles_for_both_network_policies_and_rejects_proxy_routed() {
        // Both enforceable policies must compile to a valid BPF program on
        // the running architecture (the hosted E2E only exercises one).
        let deny = compile_filter(true).expect("deny-network filter compiles");
        let allow = compile_filter(false).expect("full-access filter compiles");
        assert!(!deny.is_empty(), "deny program must contain instructions");
        assert!(!allow.is_empty(), "allow program must contain instructions");
        // The deny program is strictly larger: it carries the network-syscall
        // rejection rules on top of the shared baseline.
        assert!(
            deny.len() > allow.len(),
            "deny-network filter ({}) must exceed full-access ({})",
            deny.len(),
            allow.len()
        );

        // ProxyRouted is not enforceable via raw seccomp: fail closed.
        let error = install_seccomp_current_thread(NetworkPolicy::ProxyRouted)
            .expect_err("proxy-routed must refuse seccomp enforcement");
        assert!(
            matches!(error, SandboxError::ProxyRoutedUnavailable),
            "unexpected error: {error:?}"
        );
    }
}
