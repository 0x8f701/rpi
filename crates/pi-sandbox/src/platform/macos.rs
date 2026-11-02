use std::ffi::CStr;
use std::path::PathBuf;

use crate::{AbsolutePath, SandboxError, SandboxIoOperation};

use super::{
    MacosPlatformFacts, PlatformFactInputs, PlatformFacts, PlatformFactsInner, UnixAccountFacts,
    derive_mandatory_paths,
};

const REQUIRED_RUNTIME: [&str; 11] = [
    "/System/Library/Frameworks",
    "/System/Library/PrivateFrameworks",
    "/System/Library/CoreServices",
    "/usr/lib",
    "/usr/libexec",
    "/private/etc/passwd",
    "/usr/bin/sandbox-exec",
    "/dev/null",
    "/dev/random",
    "/dev/urandom",
    "/dev/tty",
];
const OPTIONAL_RUNTIME: [&str; 8] = [
    "/usr/share/icu",
    "/usr/share/locale",
    "/usr/share/zoneinfo",
    "/private/var/db/timezone",
    "/System/Cryptexes/OS",
    "/System/Cryptexes/App",
    "/System/Volumes/Preboot/Cryptexes/OS",
    "/System/Volumes/Preboot/Cryptexes/App/System",
];

pub(super) fn derive_macos_platform_facts(
    inputs: PlatformFactInputs,
) -> Result<PlatformFacts, SandboxError> {
    let account = account()?;
    let mut runtime_roots = REQUIRED_RUNTIME
        .iter()
        .map(|path| {
            std::fs::symlink_metadata(path).map_err(|error| {
                SandboxError::from_io(SandboxIoOperation::OpenPolicyPath, &error)
            })?;
            AbsolutePath::parse(PathBuf::from(path)).map_err(Into::into)
        })
        .collect::<Result<Vec<_>, SandboxError>>()?;
    runtime_roots.extend(
        OPTIONAL_RUNTIME
            .iter()
            .filter(|path| std::fs::symlink_metadata(path).is_ok())
            .map(|path| AbsolutePath::parse(PathBuf::from(path)))
            .collect::<Result<Vec<_>, _>>()?,
    );
    runtime_roots.sort_unstable();
    runtime_roots.dedup();
    let darwin_temp_root = AbsolutePath::parse(PathBuf::from("/private/tmp"))?;
    let (mandatory_denied_paths, mandatory_protected_roots) =
        derive_mandatory_paths(&inputs, account.home())?;
    Ok(PlatformFacts {
        inner: PlatformFactsInner::Macos(MacosPlatformFacts::new(
            account,
            runtime_roots,
            darwin_temp_root,
        )),
        mandatory_denied_paths,
        mandatory_protected_roots,
    })
}

fn account() -> Result<UnixAccountFacts, SandboxError> {
    let effective_uid = unsafe_geteuid();
    let effective_gid = unsafe_getegid();
    let mut buffer = vec![0_u8; 16 * 1024];
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
    UnixAccountFacts::new(
        effective_uid,
        effective_gid,
        AbsolutePath::parse(PathBuf::from(unsafe_c_string(password.pw_dir)?))?,
        unsafe_c_string(password.pw_name)?,
    )
    .map_err(Into::into)
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
