use std::fmt::Write as _;
use std::fs;
use std::os::fd::RawFd;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use crate::{
    AbsolutePath, FileSystemPolicy, NetworkPolicy, PolicyRepresentabilityError,
    PolicyValidationError, SandboxError, SandboxMode, StdioMode,
};

pub(crate) const MAX_SEATBELT_PROFILE_BYTES: usize = 128 * 1024;

const REQUIRED_RUNTIME_DIRECTORIES: &[&str] = &[
    "/System/Library/Frameworks",
    "/System/Library/PrivateFrameworks",
    "/System/Library/CoreServices",
    "/usr/lib",
    "/usr/libexec",
];
const REQUIRED_RUNTIME_FILES: &[&str] = &["/private/etc/passwd", "/usr/bin/sandbox-exec"];
const REQUIRED_DEVICES: &[&str] = &["/dev/null", "/dev/random", "/dev/urandom", "/dev/tty"];
const OPTIONAL_RUNTIME_DIRECTORIES: &[&str] = &[
    "/usr/share/icu",
    "/usr/share/locale",
    "/usr/share/zoneinfo",
    "/private/var/db/timezone",
    "/System/Cryptexes/OS",
    "/System/Cryptexes/App",
    "/System/Volumes/Preboot/Cryptexes/OS",
    "/System/Volumes/Preboot/Cryptexes/App/System",
];

pub(crate) struct MacosSeatbeltInput<'a> {
    pub(crate) mode: SandboxMode,
    pub(crate) network: NetworkPolicy,
    pub(crate) filesystem: &'a FileSystemPolicy,
    pub(crate) current_executable: &'a AbsolutePath,
    pub(crate) current_transport_fd: RawFd,
    pub(crate) target_executable: &'a AbsolutePath,
    pub(crate) target_transport_fd: RawFd,
    pub(crate) cwd: &'a AbsolutePath,
    pub(crate) home: &'a AbsolutePath,
    pub(crate) tmpdir: &'a AbsolutePath,
    pub(crate) stdio: StdioMode,
    pub(crate) caller_writable_roots: Vec<AbsolutePath>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum SeatbeltFilter {
    Literal(String),
    Subpath(String),
}

struct RepresentedMacosPolicy {
    current_executable: String,
    current_fd_path: String,
    target_executable: String,
    target_fd_path: String,
    runtime_read_filters: Vec<SeatbeltFilter>,
    policy_read_filters: Vec<SeatbeltFilter>,
    policy_write_filters: Vec<SeatbeltFilter>,
    policy_deny_filters: Vec<SeatbeltFilter>,
    policy_protected_filters: Vec<SeatbeltFilter>,
    network: NetworkPolicy,
}

pub(crate) fn validate_macos_representability(
    input: &MacosSeatbeltInput<'_>,
) -> Result<(), SandboxError> {
    let policy = represent_macos_policy(input)?;
    emit_seatbelt_profile(&policy).map(|_| ())
}

pub(crate) fn build_seatbelt_profile(
    input: &MacosSeatbeltInput<'_>,
) -> Result<String, SandboxError> {
    emit_seatbelt_profile(&represent_macos_policy(input)?)
}

fn represent_macos_policy(
    input: &MacosSeatbeltInput<'_>,
) -> Result<RepresentedMacosPolicy, SandboxError> {
    if matches!(input.mode, SandboxMode::DangerFullAccess) {
        return Err(invalid_seatbelt_field(
            "danger-full-access does not have a Seatbelt profile",
        ));
    }
    if matches!(input.network, NetworkPolicy::ProxyRouted) {
        return Err(SandboxError::ProxyRoutedUnavailable);
    }
    if !matches!(
        input.stdio,
        StdioMode::Piped | StdioMode::Merged | StdioMode::Pty
    ) {
        return Err(unrepresentable(
            PolicyRepresentabilityError::UnsupportedDevice,
        ));
    }

    reject_overlapping_writable_roots(input.filesystem)?;

    let mut runtime_read_filters = Vec::new();
    for path in REQUIRED_RUNTIME_DIRECTORIES {
        let path = required_runtime_path(path, true)?;
        runtime_read_filters.push(SeatbeltFilter::Subpath(path));
    }
    for path in REQUIRED_RUNTIME_FILES {
        let path = required_runtime_path(path, false)?;
        runtime_read_filters.push(SeatbeltFilter::Literal(path));
    }
    for path in REQUIRED_DEVICES {
        let path = required_device_path(path)?;
        runtime_read_filters.push(SeatbeltFilter::Literal(path));
    }
    for path in OPTIONAL_RUNTIME_DIRECTORIES {
        if let Some(path) = optional_runtime_directory(path)? {
            runtime_read_filters.push(SeatbeltFilter::Subpath(path));
        }
    }
    sort_and_reject_duplicates("runtime roots", &mut runtime_read_filters)?;

    let mut readable_roots = Vec::new();
    let mut writable_roots = Vec::new();
    let mut policy_read_filters = Vec::new();
    let mut policy_write_filters = Vec::new();
    let mut policy_deny_filters = Vec::new();
    let mut policy_protected_filters = Vec::new();
    let mut protected_paths = Vec::new();

    for root in input.filesystem.read_only_roots() {
        let represented = policy_directory(root)?;
        readable_roots.push(root.as_path());
        policy_read_filters.push(SeatbeltFilter::Subpath(represented));
    }
    for writable in input.filesystem.writable_roots() {
        let root = writable.root();
        let represented = policy_directory(root)?;
        readable_roots.push(root.as_path());
        writable_roots.push(root.as_path());
        policy_read_filters.push(SeatbeltFilter::Subpath(represented.clone()));
        policy_write_filters.push(SeatbeltFilter::Subpath(represented));
        for protected in writable.protected_descendants() {
            let absolute = AbsolutePath::parse(root.as_path().join(protected.as_path()))
                .map_err(SandboxError::Policy)?;
            let represented = protected_path(&absolute)?;
            protected_paths.push(absolute.as_path().to_path_buf());
            policy_protected_filters.push(SeatbeltFilter::Literal(represented.clone()));
            policy_protected_filters.push(SeatbeltFilter::Subpath(represented));
        }
    }
    if let Some(scratch) = input.filesystem.sealed_scratch_root() {
        let represented = policy_directory(scratch)?;
        readable_roots.push(scratch.as_path());
        writable_roots.push(scratch.as_path());
        policy_read_filters.push(SeatbeltFilter::Subpath(represented.clone()));
        policy_write_filters.push(SeatbeltFilter::Subpath(represented));
    }
    for root in &input.caller_writable_roots {
        let represented = policy_directory(root)?;
        readable_roots.push(root.as_path());
        writable_roots.push(root.as_path());
        policy_read_filters.push(SeatbeltFilter::Subpath(represented.clone()));
        policy_write_filters.push(SeatbeltFilter::Subpath(represented));
    }

    let mut denied_paths = Vec::new();
    for denied in input.filesystem.denied_paths() {
        let represented = denied_path(denied)?;
        denied_paths.push(denied.as_path());
        policy_deny_filters.push(SeatbeltFilter::Literal(represented.clone()));
        policy_deny_filters.push(SeatbeltFilter::Subpath(represented));
    }

    sort_and_reject_duplicates("readable roots", &mut policy_read_filters)?;
    sort_and_reject_duplicates("writable roots", &mut policy_write_filters)?;
    sort_and_reject_duplicates("denied paths", &mut policy_deny_filters)?;
    sort_and_reject_duplicates("protected descendants", &mut policy_protected_filters)?;

    ensure_canonical_existing(input.cwd, EntryKind::Any, || {
        PolicyRepresentabilityError::CwdNotReadable
    })?;
    if !covered_by(input.cwd.as_path(), &readable_roots) {
        return Err(unrepresentable(PolicyRepresentabilityError::CwdNotReadable));
    }

    ensure_canonical_existing(input.home, EntryKind::Directory, || {
        PolicyRepresentabilityError::HomeNotReadable
    })?;
    if !covered_by(input.home.as_path(), &readable_roots) {
        return Err(unrepresentable(
            PolicyRepresentabilityError::HomeNotReadable,
        ));
    }

    ensure_canonical_existing(input.tmpdir, EntryKind::Directory, || {
        PolicyRepresentabilityError::TmpdirNotWritable
    })?;
    if !covered_by(input.tmpdir.as_path(), &writable_roots)
        || denied_paths
            .iter()
            .any(|path| paths_overlap(input.tmpdir.as_path(), path))
        || protected_paths
            .iter()
            .any(|path| paths_overlap(input.tmpdir.as_path(), path))
    {
        return Err(unrepresentable(
            PolicyRepresentabilityError::TmpdirNotWritable,
        ));
    }

    let current_executable =
        executable_path_and_fd(input.current_executable, input.current_transport_fd)?;
    let target_executable =
        executable_path_and_fd(input.target_executable, input.target_transport_fd)?;
    if !covered_by(input.target_executable.as_path(), &readable_roots)
        && !runtime_filter_covers(input.target_executable.as_path(), &runtime_read_filters)
    {
        return Err(unrepresentable(
            PolicyRepresentabilityError::TargetNotReadable,
        ));
    }

    Ok(RepresentedMacosPolicy {
        current_executable,
        current_fd_path: fd_path(input.current_transport_fd)?,
        target_executable,
        target_fd_path: fd_path(input.target_transport_fd)?,
        runtime_read_filters,
        policy_read_filters,
        policy_write_filters,
        policy_deny_filters,
        policy_protected_filters,
        network: input.network,
    })
}

pub(crate) fn revalidate_directory_identity(
    path: &AbsolutePath,
    expected: crate::launcher::ExecutableIdentity,
) -> Result<bool, SandboxError> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt as _;

    ensure_canonical_existing(path, EntryKind::Directory, || {
        PolicyRepresentabilityError::MissingPolicyRoot { path: path.clone() }
    })?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path.as_path())
        .map_err(|error| {
            SandboxError::from_io(crate::SandboxIoOperation::OpenPolicyPath, &error)
        })?;
    expected.directory_matches_fd(file.as_raw_fd())
}

fn reject_overlapping_writable_roots(filesystem: &FileSystemPolicy) -> Result<(), SandboxError> {
    for (index, left) in filesystem.writable_roots().iter().enumerate() {
        for right in &filesystem.writable_roots()[index + 1..] {
            if paths_overlap(left.root().as_path(), right.root().as_path()) {
                return Err(invalid_seatbelt_field("overlapping writable roots"));
            }
        }
    }
    Ok(())
}

fn required_runtime_path(path: &str, directory: bool) -> Result<String, SandboxError> {
    let absolute = parse_fixed_path(path)?;
    ensure_canonical_existing(
        &absolute,
        if directory {
            EntryKind::Directory
        } else {
            EntryKind::File
        },
        || PolicyRepresentabilityError::MissingPolicyRoot {
            path: absolute.clone(),
        },
    )?;
    Ok(path.to_owned())
}

fn required_device_path(path: &str) -> Result<String, SandboxError> {
    let absolute = parse_fixed_path(path)?;
    ensure_canonical_existing(&absolute, EntryKind::Any, || {
        PolicyRepresentabilityError::UnsupportedDevice
    })?;
    Ok(path.to_owned())
}

fn optional_runtime_directory(path: &str) -> Result<Option<String>, SandboxError> {
    let absolute = parse_fixed_path(path)?;
    match fs::symlink_metadata(absolute.as_path()) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(unrepresentable(
            PolicyRepresentabilityError::MissingPolicyRoot { path: absolute },
        )),
        Ok(_) => {
            ensure_canonical_existing(&absolute, EntryKind::Directory, || {
                PolicyRepresentabilityError::MissingPolicyRoot {
                    path: absolute.clone(),
                }
            })?;
            Ok(Some(path.to_owned()))
        }
    }
}

fn policy_directory(path: &AbsolutePath) -> Result<String, SandboxError> {
    ensure_canonical_existing(path, EntryKind::Directory, || {
        PolicyRepresentabilityError::MissingPolicyRoot { path: path.clone() }
    })?;
    path_to_string(path)
}

fn denied_path(path: &AbsolutePath) -> Result<String, SandboxError> {
    let metadata = fs::symlink_metadata(path.as_path()).map_err(|_| {
        unrepresentable(PolicyRepresentabilityError::MissingDeniedPath { path: path.clone() })
    })?;
    if metadata.file_type().is_symlink() {
        match fs::metadata(path.as_path()) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(path_to_string(path)?);
            }
            Err(_) => {
                return Err(unrepresentable(
                    PolicyRepresentabilityError::DeniedSymlink { path: path.clone() },
                ));
            }
            Ok(_) => {
                let resolved = fs::canonicalize(path.as_path()).map_err(|_| {
                    unrepresentable(PolicyRepresentabilityError::DeniedSymlink {
                        path: path.clone(),
                    })
                })?;
                let resolved = AbsolutePath::parse(resolved).map_err(|_| {
                    unrepresentable(PolicyRepresentabilityError::DeniedSymlink {
                        path: path.clone(),
                    })
                })?;
                return path_to_string(&resolved);
            }
        }
    }
    ensure_canonical_existing(path, EntryKind::Any, || {
        PolicyRepresentabilityError::MissingDeniedPath { path: path.clone() }
    })?;
    path_to_string(path)
}

fn protected_path(path: &AbsolutePath) -> Result<String, SandboxError> {
    ensure_canonical_existing(path, EntryKind::Any, || {
        PolicyRepresentabilityError::MissingProtectedDescendant { path: path.clone() }
    })?;
    path_to_string(path)
}

fn executable_path_and_fd(path: &AbsolutePath, fd: RawFd) -> Result<String, SandboxError> {
    ensure_canonical_existing(path, EntryKind::ExecutableFile, || {
        PolicyRepresentabilityError::TargetNotReadable
    })?;
    let descriptor_path = fd_path(fd)?;
    let path_metadata = fs::metadata(path.as_path())
        .map_err(|_| unrepresentable(PolicyRepresentabilityError::TargetNotReadable))?;
    let descriptor_metadata = fs::metadata(&descriptor_path)
        .map_err(|_| unrepresentable(PolicyRepresentabilityError::DescriptorExecUnsupported))?;
    if path_metadata.dev() != descriptor_metadata.dev()
        || path_metadata.ino() != descriptor_metadata.ino()
        || !descriptor_metadata.is_file()
    {
        return Err(unrepresentable(
            PolicyRepresentabilityError::DescriptorExecUnsupported,
        ));
    }
    path_to_string(path)
}

fn fd_path(fd: RawFd) -> Result<String, SandboxError> {
    if fd < 0 {
        return Err(unrepresentable(
            PolicyRepresentabilityError::DescriptorExecUnsupported,
        ));
    }
    Ok(format!("/dev/fd/{fd}"))
}

#[derive(Clone, Copy)]
enum EntryKind {
    Any,
    Directory,
    File,
    ExecutableFile,
}

fn ensure_canonical_existing(
    path: &AbsolutePath,
    kind: EntryKind,
    failure: impl FnOnce() -> PolicyRepresentabilityError,
) -> Result<(), SandboxError> {
    let metadata = match fs::symlink_metadata(path.as_path()) {
        Ok(metadata) => metadata,
        Err(_) => return Err(unrepresentable(failure())),
    };
    if metadata.file_type().is_symlink() {
        return Err(unrepresentable(failure()));
    }
    let canonical = match fs::canonicalize(path.as_path()) {
        Ok(canonical) => canonical,
        Err(_) => return Err(unrepresentable(failure())),
    };
    if canonical != path.as_path() {
        return Err(unrepresentable(failure()));
    }
    let valid_kind = match kind {
        EntryKind::Any => true,
        EntryKind::Directory => metadata.is_dir(),
        EntryKind::File => metadata.is_file(),
        EntryKind::ExecutableFile => {
            metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
        }
    };
    if !valid_kind {
        return Err(unrepresentable(failure()));
    }
    Ok(())
}

fn parse_fixed_path(path: &str) -> Result<AbsolutePath, SandboxError> {
    AbsolutePath::parse(PathBuf::from(path)).map_err(SandboxError::Policy)
}

fn path_to_string(path: &AbsolutePath) -> Result<String, SandboxError> {
    path.as_path()
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid_seatbelt_field("non-UTF-8 Seatbelt path"))
}

fn covered_by(path: &Path, roots: &[&Path]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn runtime_filter_covers(path: &Path, filters: &[SeatbeltFilter]) -> bool {
    filters.iter().any(|filter| match filter {
        SeatbeltFilter::Literal(candidate) => path == Path::new(candidate),
        SeatbeltFilter::Subpath(candidate) => path.starts_with(Path::new(candidate)),
    })
}

fn sort_and_reject_duplicates(
    field: &'static str,
    filters: &mut Vec<SeatbeltFilter>,
) -> Result<(), SandboxError> {
    filters.sort();
    if filters.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(SandboxError::Policy(
            PolicyValidationError::DuplicateEntry { field },
        ));
    }
    Ok(())
}

pub(crate) fn escape_sbpl_string(value: &str) -> Result<String, SandboxError> {
    if value.as_bytes().contains(&0) {
        return Err(invalid_seatbelt_field("NUL in Seatbelt string"));
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            value if (value as u32) < 0x20 || value == '\u{7f}' => {
                write!(&mut out, "\\x{:02x}", value as u32)
                    .map_err(|_| invalid_seatbelt_field("escape failed"))?;
            }
            value => out.push(value),
        }
    }
    out.push('"');
    Ok(out)
}

fn emit_filter(out: &mut String, filter: &SeatbeltFilter) -> Result<(), SandboxError> {
    match filter {
        SeatbeltFilter::Literal(path) => {
            writeln!(out, "  (literal {})", escape_sbpl_string(path)?)
        }
        SeatbeltFilter::Subpath(path) => {
            writeln!(out, "  (subpath {})", escape_sbpl_string(path)?)
        }
    }
    .map_err(|_| invalid_seatbelt_field("emission failed"))
}

fn emit_form(
    out: &mut String,
    prefix: &str,
    filters: &[SeatbeltFilter],
) -> Result<(), SandboxError> {
    if filters.is_empty() {
        return Ok(());
    }
    out.push_str(prefix);
    out.push('\n');
    for filter in filters {
        emit_filter(out, filter)?;
    }
    out.push_str(")\n");
    Ok(())
}

fn process_exec_filters(policy: &RepresentedMacosPolicy) -> Vec<SeatbeltFilter> {
    let mut filters = vec![
        SeatbeltFilter::Literal(policy.current_executable.clone()),
        SeatbeltFilter::Literal(policy.current_fd_path.clone()),
        SeatbeltFilter::Literal(policy.target_executable.clone()),
        SeatbeltFilter::Literal(policy.target_fd_path.clone()),
    ];
    filters.extend(
        policy
            .runtime_read_filters
            .iter()
            .filter(|filter| match filter {
                SeatbeltFilter::Subpath(_) => true,
                SeatbeltFilter::Literal(path) => path == "/usr/bin/sandbox-exec",
            })
            .cloned(),
    );
    filters.extend(policy.policy_read_filters.iter().cloned());
    filters.sort();
    filters.dedup();
    filters
}

fn emit_seatbelt_profile(policy: &RepresentedMacosPolicy) -> Result<String, SandboxError> {
    let mut out = String::with_capacity(16 * 1024);
    out.push_str("(version 1)\n");
    out.push_str("(deny default (with no-log))\n");
    out.push_str("(allow process-fork)\n");
    emit_form(
        &mut out,
        "(allow process-exec",
        &process_exec_filters(policy),
    )?;
    out.push_str("(allow signal (target self))\n");
    out.push_str("(allow signal (target pgrp))\n");
    out.push_str("(allow process-info-pidinfo (target self))\n");
    out.push_str("(allow process-info-setcontrol (target self))\n");
    out.push_str("(allow process-info-pidfdinfo (target self))\n");
    out.push_str("(allow file-read-metadata (subpath \"/\"))\n");
    emit_form(
        &mut out,
        "(allow file-read* file-map-executable",
        &policy.runtime_read_filters,
    )?;
    emit_form(
        &mut out,
        "(allow file-read* file-map-executable",
        &policy.policy_read_filters,
    )?;
    emit_form(&mut out, "(allow file-write*", &policy.policy_write_filters)?;
    emit_form(&mut out, "(deny file*", &policy.policy_deny_filters)?;
    emit_form(
        &mut out,
        "(deny file-write*",
        &policy.policy_protected_filters,
    )?;
    out.push_str("(allow file-read-data file-write-data (literal \"/dev/null\"))\n");
    out.push_str("(allow file-read-data (literal \"/dev/random\"))\n");
    out.push_str("(allow file-read-data (literal \"/dev/urandom\"))\n");
    out.push_str("(allow file-read-data file-write-data file-ioctl (literal \"/dev/tty\"))\n");
    out.push_str("(allow sysctl-read)\n");
    out.push_str("(allow mach-lookup\n");
    out.push_str("  (global-name \"com.apple.system.logger\")\n");
    out.push_str("  (global-name \"com.apple.system.opendirectoryd.libinfo\"))\n");
    match policy.network {
        NetworkPolicy::None => {}
        NetworkPolicy::FullAccess => out.push_str("(allow network*)\n"),
        NetworkPolicy::ProxyRouted => return Err(SandboxError::ProxyRoutedUnavailable),
    }
    if out.len() > MAX_SEATBELT_PROFILE_BYTES {
        return Err(unrepresentable(
            PolicyRepresentabilityError::ProfileTooLarge {
                bytes: out.len(),
                limit: MAX_SEATBELT_PROFILE_BYTES,
            },
        ));
    }
    Ok(out)
}

fn invalid_seatbelt_field(reason: &'static str) -> SandboxError {
    SandboxError::Policy(PolicyValidationError::InvalidField {
        field: "seatbelt",
        reason,
    })
}

fn unrepresentable(reason: PolicyRepresentabilityError) -> SandboxError {
    SandboxError::UnrepresentablePolicy { reason }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_write_policy() -> RepresentedMacosPolicy {
        RepresentedMacosPolicy {
            current_executable: "/Applications/rpi.app/Contents/MacOS/rpi".to_owned(),
            current_fd_path: "/dev/fd/5".to_owned(),
            target_executable: "/usr/bin/clang".to_owned(),
            target_fd_path: "/dev/fd/66".to_owned(),
            runtime_read_filters: vec![
                SeatbeltFilter::Literal("/dev/null".to_owned()),
                SeatbeltFilter::Literal("/dev/random".to_owned()),
                SeatbeltFilter::Literal("/dev/tty".to_owned()),
                SeatbeltFilter::Literal("/dev/urandom".to_owned()),
                SeatbeltFilter::Literal("/private/etc/passwd".to_owned()),
                SeatbeltFilter::Literal("/usr/bin/sandbox-exec".to_owned()),
                SeatbeltFilter::Subpath("/System/Library/CoreServices".to_owned()),
                SeatbeltFilter::Subpath("/System/Library/Frameworks".to_owned()),
                SeatbeltFilter::Subpath("/System/Library/PrivateFrameworks".to_owned()),
                SeatbeltFilter::Subpath("/usr/lib".to_owned()),
                SeatbeltFilter::Subpath("/usr/libexec".to_owned()),
            ],
            policy_read_filters: vec![
                SeatbeltFilter::Subpath(
                    "/Users/alice/Library/Caches/rpi/sandbox/session-a/43".to_owned(),
                ),
                SeatbeltFilter::Subpath("/Users/alice/work".to_owned()),
                SeatbeltFilter::Subpath("/usr/bin".to_owned()),
            ],
            policy_write_filters: vec![
                SeatbeltFilter::Subpath(
                    "/Users/alice/Library/Caches/rpi/sandbox/session-a/43".to_owned(),
                ),
                SeatbeltFilter::Subpath("/Users/alice/work".to_owned()),
            ],
            policy_deny_filters: vec![
                SeatbeltFilter::Literal("/Users/alice/work/private-credentials".to_owned()),
                SeatbeltFilter::Subpath("/Users/alice/work/private-credentials".to_owned()),
            ],
            policy_protected_filters: vec![
                SeatbeltFilter::Literal("/Users/alice/work/.env".to_owned()),
                SeatbeltFilter::Literal("/Users/alice/work/.git".to_owned()),
                SeatbeltFilter::Subpath("/Users/alice/work/.env".to_owned()),
                SeatbeltFilter::Subpath("/Users/alice/work/.git".to_owned()),
            ],
            network: NetworkPolicy::FullAccess,
        }
    }

    #[test]
    fn workspace_write_profile_is_literal_and_complete() {
        let profile = emit_seatbelt_profile(&workspace_write_policy()).unwrap();
        let expected = r#"(version 1)
(deny default (with no-log))
(allow process-fork)
(allow process-exec
  (literal "/Applications/rpi.app/Contents/MacOS/rpi")
  (literal "/dev/fd/5")
  (literal "/dev/fd/66")
  (literal "/usr/bin/clang")
  (literal "/usr/bin/sandbox-exec")
  (subpath "/System/Library/CoreServices")
  (subpath "/System/Library/Frameworks")
  (subpath "/System/Library/PrivateFrameworks")
  (subpath "/Users/alice/Library/Caches/rpi/sandbox/session-a/43")
  (subpath "/Users/alice/work")
  (subpath "/usr/bin")
  (subpath "/usr/lib")
  (subpath "/usr/libexec"))
(allow signal (target self))
(allow signal (target pgrp))
(allow process-info-pidinfo (target self))
(allow process-info-setcontrol (target self))
(allow process-info-pidfdinfo (target self))
(allow file-read-metadata (subpath "/"))
(allow file-read* file-map-executable
  (literal "/dev/null")
  (literal "/dev/random")
  (literal "/dev/tty")
  (literal "/dev/urandom")
  (literal "/private/etc/passwd")
  (literal "/usr/bin/sandbox-exec")
  (subpath "/System/Library/CoreServices")
  (subpath "/System/Library/Frameworks")
  (subpath "/System/Library/PrivateFrameworks")
  (subpath "/usr/lib")
  (subpath "/usr/libexec"))
(allow file-read* file-map-executable
  (subpath "/Users/alice/Library/Caches/rpi/sandbox/session-a/43")
  (subpath "/Users/alice/work")
  (subpath "/usr/bin"))
(allow file-write*
  (subpath "/Users/alice/Library/Caches/rpi/sandbox/session-a/43")
  (subpath "/Users/alice/work"))
(deny file*
  (literal "/Users/alice/work/private-credentials")
  (subpath "/Users/alice/work/private-credentials"))
(deny file-write*
  (literal "/Users/alice/work/.env")
  (literal "/Users/alice/work/.git")
  (subpath "/Users/alice/work/.env")
  (subpath "/Users/alice/work/.git"))
(allow file-read-data file-write-data (literal "/dev/null"))
(allow file-read-data (literal "/dev/random"))
(allow file-read-data (literal "/dev/urandom"))
(allow file-read-data file-write-data file-ioctl (literal "/dev/tty"))
(allow sysctl-read)
(allow mach-lookup
  (global-name "com.apple.system.logger")
  (global-name "com.apple.system.opendirectoryd.libinfo"))
(allow network*)
"#;
        assert_eq!(profile, expected);
        assert!(!profile.contains("(allow default"));
        assert!(!profile.contains("regex"));
    }

    #[test]
    fn descriptor_process_exec_representation_is_exact() {
        let filters = process_exec_filters(&workspace_write_policy());
        assert_eq!(
            filters,
            vec![
                SeatbeltFilter::Literal("/Applications/rpi.app/Contents/MacOS/rpi".to_owned(),),
                SeatbeltFilter::Literal("/dev/fd/5".to_owned()),
                SeatbeltFilter::Literal("/dev/fd/66".to_owned()),
                SeatbeltFilter::Literal("/usr/bin/clang".to_owned()),
                SeatbeltFilter::Literal("/usr/bin/sandbox-exec".to_owned()),
                SeatbeltFilter::Subpath("/System/Library/CoreServices".to_owned()),
                SeatbeltFilter::Subpath("/System/Library/Frameworks".to_owned()),
                SeatbeltFilter::Subpath("/System/Library/PrivateFrameworks".to_owned(),),
                SeatbeltFilter::Subpath(
                    "/Users/alice/Library/Caches/rpi/sandbox/session-a/43".to_owned(),
                ),
                SeatbeltFilter::Subpath("/Users/alice/work".to_owned()),
                SeatbeltFilter::Subpath("/usr/bin".to_owned()),
                SeatbeltFilter::Subpath("/usr/lib".to_owned()),
                SeatbeltFilter::Subpath("/usr/libexec".to_owned()),
            ]
        );
    }

    #[test]
    fn sbpl_escaping_is_deterministic_and_rejects_nul() {
        assert_eq!(
            escape_sbpl_string("a\"b\\c\nd\re\tf\u{1f}\u{7f}").unwrap(),
            "\"a\\\"b\\\\c\\nd\\re\\tf\\x1f\\x7f\""
        );
        assert!(escape_sbpl_string("bad\0value").is_err());
    }

    #[test]
    fn oversized_profile_is_rejected_at_the_exact_byte_cap() {
        let mut policy = workspace_write_policy();
        policy.policy_read_filters = vec![SeatbeltFilter::Subpath(format!(
            "/{}",
            "x".repeat(MAX_SEATBELT_PROFILE_BYTES)
        ))];
        assert!(matches!(
            emit_seatbelt_profile(&policy),
            Err(SandboxError::UnrepresentablePolicy {
                reason: PolicyRepresentabilityError::ProfileTooLarge {
                    limit: MAX_SEATBELT_PROFILE_BYTES,
                    ..
                }
            })
        ));
    }
    #[test]
    fn denied_symlink_resolves_to_target_path() {
        let root = tempfile::tempdir().expect("fixture root");
        let target = root.path().join("target");
        std::fs::create_dir(&target).expect("symlink target directory");
        let link = root.path().join("denied-link");
        std::os::unix::fs::symlink(&target, &link).expect("denied symlink");
        let resolved =
            denied_path(&AbsolutePath::parse(link.to_path_buf()).expect("absolute path"))
                .expect("denied symlink resolves");
        assert_eq!(
            resolved,
            AbsolutePath::parse(target.to_path_buf())
                .expect("target")
                .as_path()
                .to_string_lossy()
        );
    }
}
