use std::ffi::CStr;
use std::path::PathBuf;

use crate::{AbsolutePath, ArchitectureKind, HostPlatform, SandboxError, SandboxIoOperation};

use super::{
    LinuxArchitecture, LinuxPlatformFacts, PlatformFactInputs, PlatformFacts, PlatformFactsInner,
    UnixAccountFacts, derive_mandatory_paths,
};

const RUNTIME_CANDIDATES: [&str; 16] = [
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/etc/ld.so.cache",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    "/etc/nsswitch.conf",
    "/etc/passwd",
    "/etc/group",
    "/etc/hosts",
    "/etc/resolv.conf",
    "/etc/ssl/certs",
    "/etc/pki/tls/certs",
    "/usr/share/zoneinfo",
];

const DEVICES: [&str; 5] = [
    "/dev/null",
    "/dev/zero",
    "/dev/random",
    "/dev/urandom",
    "/dev/tty",
];

pub(super) fn derive_linux_platform_facts(
    inputs: PlatformFactInputs,
) -> Result<PlatformFacts, SandboxError> {
    let architecture = architecture()?;
    let account = account()?;
    let runtime_roots = existing_paths(&RUNTIME_CANDIDATES)?;
    let policy_runtime_roots = canonical_existing_paths(&RUNTIME_CANDIDATES)?;
    if !runtime_roots.iter().any(|root| {
        matches!(
            root.as_path().to_str(),
            Some("/lib") | Some("/lib64") | Some("/usr/lib")
        )
    }) {
        return Err(SandboxError::UnsupportedArchitecture {
            platform: HostPlatform::Linux,
            architecture: match architecture {
                LinuxArchitecture::X86_64 => ArchitectureKind::X86_64,
                LinuxArchitecture::Aarch64 => ArchitectureKind::Aarch64,
            },
        });
    }
    let device_paths = existing_paths(&DEVICES)?;
    let (mandatory_denied_paths, mandatory_protected_roots) =
        derive_mandatory_paths(&inputs, account.home())?;
    Ok(PlatformFacts {
        inner: PlatformFactsInner::Linux(LinuxPlatformFacts::new(
            architecture,
            account,
            runtime_roots,
            policy_runtime_roots,
            device_paths,
        )),
        mandatory_denied_paths,
        mandatory_protected_roots,
    })
}

fn architecture() -> Result<LinuxArchitecture, SandboxError> {
    #[cfg(target_arch = "x86_64")]
    {
        Ok(LinuxArchitecture::X86_64)
    }
    #[cfg(target_arch = "aarch64")]
    {
        Ok(LinuxArchitecture::Aarch64)
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        Err(SandboxError::UnsupportedArchitecture {
            platform: HostPlatform::Linux,
            architecture: ArchitectureKind::Other,
        })
    }
}

fn account() -> Result<UnixAccountFacts, SandboxError> {
    let effective_uid = unsafe_geteuid();
    let effective_gid = unsafe_getegid();
    let buffer_length = unsafe_sysconf_buffer_length();
    let mut buffer = vec![0_u8; buffer_length];
    let mut password = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut found = std::ptr::null_mut();
    let result = unsafe_getpwuid_r(
        effective_uid,
        password.as_mut_ptr(),
        buffer.as_mut_ptr().cast(),
        buffer.len(),
        &raw mut found,
    );
    if result != 0 || found.is_null() {
        let error =
            std::io::Error::from_raw_os_error(if result == 0 { libc::ENOENT } else { result });
        return Err(SandboxError::from_io(
            SandboxIoOperation::InspectDescriptor,
            &error,
        ));
    }
    let password = unsafe_assume_init(password);
    let home = unsafe_c_string(password.pw_dir)?;
    let user_name = unsafe_c_string(password.pw_name)?;
    UnixAccountFacts::new(
        effective_uid,
        effective_gid,
        AbsolutePath::parse(PathBuf::from(home))?,
        user_name,
    )
    .map_err(Into::into)
}

fn existing_paths(candidates: &[&str]) -> Result<Vec<AbsolutePath>, SandboxError> {
    let mut paths = candidates
        .iter()
        .filter(|candidate| std::fs::symlink_metadata(candidate).is_ok())
        .map(|candidate| AbsolutePath::parse(PathBuf::from(candidate)))
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort_unstable();
    paths.dedup();
    Ok(paths)
}

/// Policy-plane projection of the runtime candidates: symlink aliases resolve
/// to their canonical targets because policy roots are pinned symlink-free.
fn canonical_existing_paths(candidates: &[&str]) -> Result<Vec<AbsolutePath>, SandboxError> {
    let mut paths = candidates
        .iter()
        .filter_map(|candidate| std::fs::canonicalize(candidate).ok())
        .map(AbsolutePath::parse)
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort_unstable();
    paths.dedup();
    Ok(paths)
}

fn unsafe_c_string(value: *const libc::c_char) -> Result<String, SandboxError> {
    if value.is_null() {
        return Err(SandboxError::from_io(
            SandboxIoOperation::InspectDescriptor,
            &std::io::Error::from_raw_os_error(libc::EINVAL),
        ));
    }
    unsafe_c_str(value)
        .to_str()
        .map(str::to_owned)
        .map_err(|_| {
            SandboxError::from_io(
                SandboxIoOperation::InspectDescriptor,
                &std::io::Error::from_raw_os_error(libc::EILSEQ),
            )
        })
}

#[allow(unsafe_code)]
fn unsafe_geteuid() -> u32 {
    unsafe { libc::geteuid() }
}

#[allow(unsafe_code)]
fn unsafe_getegid() -> u32 {
    unsafe { libc::getegid() }
}

#[allow(unsafe_code)]
fn unsafe_sysconf_buffer_length() -> usize {
    let value = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    if value <= 0 {
        16 * 1024
    } else {
        value as usize
    }
}

#[allow(unsafe_code)]
fn unsafe_getpwuid_r(
    uid: libc::uid_t,
    password: *mut libc::passwd,
    buffer: *mut libc::c_char,
    buffer_length: usize,
    found: *mut *mut libc::passwd,
) -> i32 {
    unsafe { libc::getpwuid_r(uid, password, buffer, buffer_length, found) }
}

#[allow(unsafe_code)]
fn unsafe_assume_init(password: std::mem::MaybeUninit<libc::passwd>) -> libc::passwd {
    unsafe { password.assume_init() }
}

#[allow(unsafe_code)]
fn unsafe_c_str<'a>(value: *const libc::c_char) -> &'a CStr {
    unsafe { CStr::from_ptr(value) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_paths_keeps_symlink_aliases_for_mount_destinations() {
        let root = tempfile::tempdir().expect("tempdir");
        let real = root.path().join("real");
        std::fs::create_dir(&real).expect("create real dir");
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).expect("create alias symlink");
        let alias_str = alias.to_str().expect("alias path is utf-8");

        let mounts = existing_paths(&[alias_str]).expect("existing_paths");
        assert_eq!(
            mounts,
            vec![AbsolutePath::parse(alias.clone()).expect("alias")]
        );
    }

    #[test]
    fn canonical_existing_paths_resolves_symlink_aliases_for_policy() {
        let root = tempfile::tempdir().expect("tempdir");
        let real = root.path().join("real");
        std::fs::create_dir(&real).expect("create real dir");
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).expect("create alias symlink");
        let alias_str = alias.to_str().expect("alias path is utf-8");

        let policy = canonical_existing_paths(&[alias_str]).expect("canonical_existing_paths");
        let canonical_real = std::fs::canonicalize(&real).expect("canonical real");
        assert_eq!(
            policy,
            vec![AbsolutePath::parse(canonical_real).expect("real")]
        );
    }

    // Regression: merged-usr aliases (/bin -> /usr/bin) crashed startup because
    // raw symlink aliases entered the policy read-only roots and the capability
    // filesystem rejects symlink roots fail-closed.
    #[test]
    fn derived_policy_runtime_roots_contain_no_symlinks() {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace = AbsolutePath::parse(root.path().join("workspace")).expect("workspace");
        let agent_dir = AbsolutePath::parse(root.path().join("agent")).expect("agent");
        let session_dir = AbsolutePath::parse(root.path().join("session")).expect("session");
        let inputs = PlatformFactInputs::new(
            "policy-runtime-roots-test".to_owned(),
            vec![workspace],
            agent_dir,
            session_dir,
        )
        .expect("inputs");
        let facts = derive_linux_platform_facts(inputs).expect("platform facts");
        for root in facts.policy_runtime_roots() {
            let metadata = std::fs::symlink_metadata(root.as_path()).expect("symlink_metadata");
            assert!(
                !metadata.file_type().is_symlink(),
                "policy runtime root must not be a symlink: {root:?}"
            );
        }
    }
}
