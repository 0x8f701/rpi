use std::ffi::CString;
use std::os::fd::RawFd;

use crate::prepare::{LinuxMountAccess, LinuxMountEntryKind};
use crate::{AbsolutePath, NetworkPolicy, SandboxError};
const PRIVATE_PROJECTION_PATHS: [&str; 3] = ["/tmp", "/run/pi/home", "/run/pi/scratch"];
const EXECUTABLE_PROJECTION_PATHS: [&str; 2] = ["/run/pi/rpi", "/run/pi/target"];

pub(crate) struct BwrapMountRow {
    pub(crate) source_fd: RawFd,
    pub(crate) destination: AbsolutePath,
    pub(crate) access: LinuxMountAccess,
    pub(crate) entry_kind: LinuxMountEntryKind,
}

pub(crate) struct BwrapArgvInput {
    pub(crate) network: NetworkPolicy,
    pub(crate) device_rows: Vec<BwrapMountRow>,
    pub(crate) destination_parents: Vec<AbsolutePath>,
    pub(crate) runtime_rows: Vec<BwrapMountRow>,
    pub(crate) read_only_rows: Vec<BwrapMountRow>,
    pub(crate) mandatory_read_only_rows: Vec<BwrapMountRow>,
    pub(crate) writable_rows: Vec<BwrapMountRow>,
    pub(crate) scratch_row: BwrapMountRow,
    pub(crate) home_row: BwrapMountRow,
    pub(crate) tmp_row: BwrapMountRow,
    pub(crate) protected_rows: Vec<BwrapMountRow>,
    pub(crate) denied_file_rows: Vec<BwrapMountRow>,
    pub(crate) denied_directories: Vec<AbsolutePath>,
    pub(crate) current_executable_row: BwrapMountRow,
    pub(crate) target_executable_row: BwrapMountRow,
    pub(crate) cwd: AbsolutePath,
}

pub(crate) fn build_bwrap_argv(input: BwrapArgvInput) -> Result<Vec<CString>, SandboxError> {
    validate_private_projection_precedence(&input)?;
    let mut arguments = Vec::new();
    push(&mut arguments, "--die-with-parent")?;
    push(&mut arguments, "--unshare-user")?;
    push(&mut arguments, "--unshare-pid")?;
    push(&mut arguments, "--unshare-ipc")?;
    push(&mut arguments, "--unshare-uts")?;
    match input.network {
        NetworkPolicy::None => push(&mut arguments, "--unshare-net")?,
        NetworkPolicy::FullAccess => {}
        NetworkPolicy::ProxyRouted => return Err(SandboxError::ProxyRoutedUnavailable),
    }
    push(&mut arguments, "--clearenv")?;
    for (permissions, option, path) in [
        ("0755", "--dir", "/proc"),
        ("0755", "--dir", "/dev"),
        ("0755", "--dir", "/run"),
        ("0700", "--tmpfs", "/run/pi"),
        ("0755", "--dir", "/run/pi/home"),
        ("0755", "--dir", "/run/pi/scratch"),
        ("0755", "--dir", "/dev/shm"),
    ] {
        push(&mut arguments, "--perms")?;
        push(&mut arguments, permissions)?;
        push(&mut arguments, option)?;
        push(&mut arguments, path)?;
    }
    push(&mut arguments, "--proc")?;
    push(&mut arguments, "/proc")?;
    push(&mut arguments, "--perms")?;
    push(&mut arguments, "1777")?;
    push(&mut arguments, "--tmpfs")?;
    push(&mut arguments, "/dev/shm")?;

    for row in input.device_rows {
        push_device_mount(&mut arguments, &row)?;
    }
    push_mount(&mut arguments, &input.scratch_row)?;
    push_mount(&mut arguments, &input.home_row)?;
    push_mount(&mut arguments, &input.tmp_row)?;
    let mut parents = input.destination_parents;
    parents.sort_unstable_by(|left, right| {
        left.as_path()
            .components()
            .count()
            .cmp(&right.as_path().components().count())
            .then_with(|| left.cmp(right))
    });
    parents.dedup();
    for parent in parents {
        push(&mut arguments, "--dir")?;
        push_path(&mut arguments, &parent)?;
    }
    for rows in [
        input.runtime_rows,
        input.read_only_rows,
        input.mandatory_read_only_rows,
        input.writable_rows,
    ] {
        push_sorted_rows(&mut arguments, rows)?;
    }
    push_sorted_rows(&mut arguments, input.protected_rows)?;
    push_sorted_rows(&mut arguments, input.denied_file_rows)?;
    let mut denied_directories = input.denied_directories;
    sort_paths(&mut denied_directories);
    for denied in denied_directories {
        push(&mut arguments, "--perms")?;
        push(&mut arguments, "000")?;
        push(&mut arguments, "--tmpfs")?;
        push_path(&mut arguments, &denied)?;
        push(&mut arguments, "--remount-ro")?;
        push_path(&mut arguments, &denied)?;
    }
    push_mount(&mut arguments, &input.current_executable_row)?;
    push_mount(&mut arguments, &input.target_executable_row)?;
    push(&mut arguments, "--chdir")?;
    push_path(&mut arguments, &input.cwd)?;
    for tail in ["--", "/run/pi/rpi", "__sandbox-child", "3", "4"] {
        push(&mut arguments, tail)?;
    }
    Ok(arguments)
}

fn validate_private_projection_precedence(input: &BwrapArgvInput) -> Result<(), SandboxError> {
    for row in input
        .runtime_rows
        .iter()
        .chain(&input.read_only_rows)
        .chain(&input.mandatory_read_only_rows)
        .chain(&input.writable_rows)
        .chain(&input.protected_rows)
        .chain(&input.denied_file_rows)
    {
        reject_private_projection_ancestor(&row.destination)?;
    }
    for denied in &input.denied_directories {
        reject_private_projection_ancestor(denied)?;
    }
    for destination in EXECUTABLE_PROJECTION_PATHS {
        let destination = std::path::Path::new(destination);
        for row in input
            .runtime_rows
            .iter()
            .chain(&input.read_only_rows)
            .chain(&input.mandatory_read_only_rows)
            .chain(&input.writable_rows)
            .chain(&input.protected_rows)
            .chain(&input.denied_file_rows)
        {
            reject_executable_projection_overlap(&row.destination, destination)?;
        }
        for denied in &input.denied_directories {
            reject_executable_projection_overlap(denied, destination)?;
        }
    }
    Ok(())
}

fn reject_private_projection_ancestor(destination: &AbsolutePath) -> Result<(), SandboxError> {
    if PRIVATE_PROJECTION_PATHS
        .iter()
        .any(|private| std::path::Path::new(private).starts_with(destination.as_path()))
    {
        return Err(SandboxError::UnrepresentablePolicy {
            reason: crate::PolicyRepresentabilityError::PrivateProjectionOverlap {
                path: destination.clone(),
            },
        });
    }
    Ok(())
}

fn reject_executable_projection_overlap(
    configured: &AbsolutePath,
    executable: &std::path::Path,
) -> Result<(), SandboxError> {
    if executable.starts_with(configured.as_path()) || configured.as_path().starts_with(executable)
    {
        return Err(SandboxError::UnrepresentablePolicy {
            reason: crate::PolicyRepresentabilityError::ReservedExecutableOverlap {
                path: configured.clone(),
            },
        });
    }
    Ok(())
}

fn push_sorted_rows(
    arguments: &mut Vec<CString>,
    mut rows: Vec<BwrapMountRow>,
) -> Result<(), SandboxError> {
    rows.sort_unstable_by(|left, right| {
        left.destination
            .as_path()
            .components()
            .count()
            .cmp(&right.destination.as_path().components().count())
            .then_with(|| left.destination.cmp(&right.destination))
    });
    for row in rows {
        push_mount(arguments, &row)?;
    }
    Ok(())
}

fn sort_paths(paths: &mut [AbsolutePath]) {
    paths.sort_unstable_by(|left, right| {
        left.as_path()
            .components()
            .count()
            .cmp(&right.as_path().components().count())
            .then_with(|| left.cmp(right))
    });
}

fn push_device_mount(
    arguments: &mut Vec<CString>,
    row: &BwrapMountRow,
) -> Result<(), SandboxError> {
    if row.entry_kind != LinuxMountEntryKind::CharacterDevice
        || row.access != LinuxMountAccess::ReadWrite
    {
        return Err(SandboxError::MountTargetCreationUnsupported);
    }
    push(arguments, "--dev-bind")?;
    push(arguments, &format!("/proc/self/fd/{}", row.source_fd))?;
    push_path(arguments, &row.destination)
}

fn push_mount(arguments: &mut Vec<CString>, row: &BwrapMountRow) -> Result<(), SandboxError> {
    if row.entry_kind == LinuxMountEntryKind::Directory
        && row.access == LinuxMountAccess::DenyFileMask
    {
        return Err(SandboxError::MountTargetCreationUnsupported);
    }
    match row.access {
        LinuxMountAccess::ReadOnly => push(arguments, "--ro-bind-fd")?,
        LinuxMountAccess::ReadWrite => push(arguments, "--bind-fd")?,
        LinuxMountAccess::DenyFileMask => {
            push(arguments, "--perms")?;
            push(arguments, "000")?;
            push(arguments, "--ro-bind-data")?;
        }
    }
    push(arguments, &row.source_fd.to_string())?;
    push_path(arguments, &row.destination)
}

fn push_path(arguments: &mut Vec<CString>, path: &AbsolutePath) -> Result<(), SandboxError> {
    let value = path
        .as_path()
        .to_str()
        .ok_or_else(|| SandboxError::InvalidChildProtocol {
            failure: crate::ChildProtocolError::EmbeddedNul,
        })?;
    push(arguments, value)
}

fn push(arguments: &mut Vec<CString>, value: &str) -> Result<(), SandboxError> {
    arguments.push(
        CString::new(value).map_err(|_| SandboxError::InvalidChildProtocol {
            failure: crate::ChildProtocolError::EmbeddedNul,
        })?,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(value: &str) -> AbsolutePath {
        AbsolutePath::parse(value.into()).expect("path")
    }

    fn row_with_kind(
        fd: RawFd,
        destination: &str,
        access: LinuxMountAccess,
        entry_kind: LinuxMountEntryKind,
    ) -> BwrapMountRow {
        BwrapMountRow {
            source_fd: fd,
            destination: path(destination),
            access,
            entry_kind,
        }
    }

    fn row(fd: RawFd, destination: &str, access: LinuxMountAccess) -> BwrapMountRow {
        row_with_kind(fd, destination, access, LinuxMountEntryKind::RegularFile)
    }

    fn input() -> BwrapArgvInput {
        BwrapArgvInput {
            network: NetworkPolicy::None,
            device_rows: vec![row_with_kind(
                64,
                "/dev/null",
                LinuxMountAccess::ReadWrite,
                LinuxMountEntryKind::CharacterDevice,
            )],
            destination_parents: vec![path("/usr/bin"), path("/tmp/workspace")],
            runtime_rows: vec![row(65, "/usr/bin/true", LinuxMountAccess::ReadOnly)],
            read_only_rows: vec![row(72, "/tmp/read-only", LinuxMountAccess::ReadOnly)],
            mandatory_read_only_rows: Vec::new(),
            writable_rows: vec![row(73, "/tmp/workspace", LinuxMountAccess::ReadWrite)],
            scratch_row: row(66, "/run/pi/scratch", LinuxMountAccess::ReadWrite),
            home_row: row(67, "/run/pi/home", LinuxMountAccess::ReadWrite),
            tmp_row: row(68, "/tmp", LinuxMountAccess::ReadWrite),
            protected_rows: vec![row(
                74,
                "/tmp/workspace/protected",
                LinuxMountAccess::ReadOnly,
            )],
            denied_file_rows: vec![row(
                69,
                "/tmp/workspace/.env",
                LinuxMountAccess::DenyFileMask,
            )],
            denied_directories: vec![path("/tmp/workspace/private")],
            current_executable_row: row(70, "/run/pi/rpi", LinuxMountAccess::ReadOnly),
            target_executable_row: row(71, "/run/pi/target", LinuxMountAccess::ReadOnly),
            cwd: path("/workspace"),
        }
    }

    fn strings(input: BwrapArgvInput) -> Vec<String> {
        build_bwrap_argv(input)
            .expect("argv")
            .into_iter()
            .map(|value| value.into_string().expect("utf8"))
            .collect()
    }

    #[test]
    fn argv_has_empty_root_and_literal_order() {
        let argv = strings(input());
        assert_eq!(
            &argv[..6],
            [
                "--die-with-parent",
                "--unshare-user",
                "--unshare-pid",
                "--unshare-ipc",
                "--unshare-uts",
                "--unshare-net",
            ]
        );
        assert!(!argv.windows(3).any(|row| row == ["--ro-bind", "/", "/"]));
        assert_eq!(
            &argv[argv.len() - 5..],
            ["--", "/run/pi/rpi", "__sandbox-child", "3", "4"]
        );
    }

    #[test]
    fn every_mount_source_is_fd_backed() {
        let argv = strings(input());
        for (index, argument) in argv.iter().enumerate() {
            if matches!(
                argument.as_str(),
                "--bind-fd" | "--ro-bind-fd" | "--ro-bind-data"
            ) {
                assert!(argv[index + 1].parse::<RawFd>().is_ok());
            }
            if argument == "--dev-bind" {
                assert_eq!(argv[index + 1], "/proc/self/fd/64");
            }
        }
        assert!(
            !argv
                .iter()
                .any(|argument| argument == "--bind" || argument == "--ro-bind")
        );
    }

    #[test]
    fn character_devices_use_device_enabled_fd_paths() {
        let argv = strings(input());
        assert!(
            argv.windows(3)
                .any(|row| { row == ["--dev-bind", "/proc/self/fd/64", "/dev/null"] })
        );
        assert!(
            !argv
                .windows(3)
                .any(|row| { row == ["--bind-fd", "64", "/dev/null"] })
        );
    }

    #[test]
    fn network_none_unshares_net_namespace() {
        assert!(
            strings(input())
                .iter()
                .any(|argument| argument == "--unshare-net")
        );
        let mut full = input();
        full.network = NetworkPolicy::FullAccess;
        assert!(
            !strings(full)
                .iter()
                .any(|argument| argument == "--unshare-net")
        );
    }

    #[test]
    fn denied_regular_file_uses_zero_mode_bind_data() {
        let argv = strings(input());
        assert!(argv.windows(5).any(|row| {
            row == [
                "--perms",
                "000",
                "--ro-bind-data",
                "69",
                "/tmp/workspace/.env",
            ]
        }));
        assert!(
            !argv
                .windows(3)
                .any(|row| { row == ["--ro-bind-fd", "69", "/tmp/workspace/.env"] })
        );
    }

    fn mount_index(argv: &[String], fd: RawFd, destination: &str) -> usize {
        let fd = fd.to_string();
        argv.windows(3)
            .position(|row| {
                matches!(
                    row[0].as_str(),
                    "--bind-fd" | "--ro-bind-fd" | "--ro-bind-data"
                ) && row[1] == fd
                    && row[2] == destination
            })
            .expect("mount row")
    }

    #[test]
    fn private_projections_precede_descendant_policy_mounts() {
        let argv = strings(input());
        let tmp = mount_index(&argv, 68, "/tmp");
        let read_only = mount_index(&argv, 72, "/tmp/read-only");
        let writable = mount_index(&argv, 73, "/tmp/workspace");
        let protected = mount_index(&argv, 74, "/tmp/workspace/protected");
        let denied = mount_index(&argv, 69, "/tmp/workspace/.env");
        assert!(tmp < read_only);
        assert!(read_only < writable);
        assert!(writable < protected);
        assert!(protected < denied);
    }

    #[test]
    fn policy_mounts_cannot_replace_private_projections() {
        for destination in [
            "/",
            "/tmp",
            "/run",
            "/run/pi",
            "/run/pi/home",
            "/run/pi/scratch",
        ] {
            let mut configured = input();
            configured
                .read_only_rows
                .push(row(75, destination, LinuxMountAccess::ReadOnly));
            assert!(matches!(
                build_bwrap_argv(configured),
                Err(SandboxError::UnrepresentablePolicy {
                    reason: crate::PolicyRepresentabilityError::PrivateProjectionOverlap { .. }
                })
            ));

            let mut denied = input();
            denied.denied_directories.push(path(destination));
            assert!(matches!(
                build_bwrap_argv(denied),
                Err(SandboxError::UnrepresentablePolicy {
                    reason: crate::PolicyRepresentabilityError::PrivateProjectionOverlap { .. }
                })
            ));
        }
    }

    #[test]
    fn strict_private_projection_descendants_remain_representable() {
        let mut configured = input();
        configured.read_only_rows.extend([
            row(75, "/run/pi/home/agent", LinuxMountAccess::ReadOnly),
            row(76, "/run/pi/scratch/revision", LinuxMountAccess::ReadOnly),
            row(77, "/run/pi/home2", LinuxMountAccess::ReadOnly),
        ]);
        build_bwrap_argv(configured).expect("strict descendants do not replace private roots");
    }

    #[test]
    fn reserved_executable_projections_reject_overlap_fail_closed() {
        // A policy row that would replace or nest under /run/pi/rpi or
        // /run/pi/target must be rejected with a reason-specific error so the
        // reserved executable projections cannot be shadowed by caller policy.
        for destination in [
            "/run/pi/rpi",
            "/run/pi/target",
            "/run/pi/rpi/sub",
            "/run/pi/target/sub",
        ] {
            let mut configured = input();
            configured
                .read_only_rows
                .push(row(78, destination, LinuxMountAccess::ReadOnly));
            assert!(
                matches!(
                    build_bwrap_argv(configured),
                    Err(SandboxError::UnrepresentablePolicy {
                        reason: crate::PolicyRepresentabilityError::ReservedExecutableOverlap { .. }
                    })
                ),
                "reserved executable overlap must reject {destination:?}"
            );
        }
        // An ancestor of the reserved projections (e.g. /run/pi) is rejected as a
        // private-projection ancestor because it would also shadow HOME/scratch.
        let mut ancestor = input();
        ancestor
            .read_only_rows
            .push(row(79, "/run/pi", LinuxMountAccess::ReadOnly));
        assert!(matches!(
            build_bwrap_argv(ancestor),
            Err(SandboxError::UnrepresentablePolicy {
                reason: crate::PolicyRepresentabilityError::PrivateProjectionOverlap { .. }
            })
        ));
    }

    #[test]
    fn reserved_executable_rows_are_emitted_after_denied_policy() {
        // Reserved executable rows must be the last mounts so policy rows can
        // never shadow /run/pi/rpi or /run/pi/target in the effective order.
        let argv = strings(input());
        let denied = mount_index(&argv, 69, "/tmp/workspace/.env");
        let current = mount_index(&argv, 70, "/run/pi/rpi");
        let target = mount_index(&argv, 71, "/run/pi/target");
        assert!(
            denied < current,
            "denied policy must precede the reserved rpi row"
        );
        assert!(
            denied < target,
            "denied policy must precede the reserved target row"
        );
        assert!(current < target);
        assert_eq!(
            &argv[argv.len() - 5..],
            ["--", "/run/pi/rpi", "__sandbox-child", "3", "4"]
        );
    }

    #[test]
    fn caller_mounts_in_writable_bucket_precede_protected_and_denied() {
        // Caller mounts are spliced into the writable bucket by the prepare
        // stage; the bwrap builder must keep every writable row ahead of the
        // protected and denied rows so a caller cannot drift behind a denial.
        let mut configured = input();
        configured.writable_rows.extend([
            row(80, "/tmp/workspace/caller-a", LinuxMountAccess::ReadWrite),
            row(81, "/tmp/workspace/caller-b", LinuxMountAccess::ReadWrite),
        ]);
        let argv = strings(configured);
        let writable_root = mount_index(&argv, 73, "/tmp/workspace");
        let caller_a = mount_index(&argv, 80, "/tmp/workspace/caller-a");
        let caller_b = mount_index(&argv, 81, "/tmp/workspace/caller-b");
        let protected = mount_index(&argv, 74, "/tmp/workspace/protected");
        let denied = mount_index(&argv, 69, "/tmp/workspace/.env");
        // Sorted by component depth then lexicographic within the writable bucket.
        assert!(writable_root < caller_a);
        assert!(caller_a < caller_b);
        assert!(
            caller_b < protected,
            "caller mounts must precede protected rows"
        );
        assert!(protected < denied);
    }

    #[test]
    fn read_only_descendants_of_private_projections_are_mounted_after_their_root() {
        // Strict RO descendants of the private HOME/scratch/tmp projections must
        // remain representable and appear after the private projection row so a
        // real bwrap mount sequence can stack them on top of the projected root.
        let mut configured = input();
        configured.read_only_rows.extend([
            row(75, "/run/pi/home/agent", LinuxMountAccess::ReadOnly),
            row(76, "/run/pi/scratch/revision", LinuxMountAccess::ReadOnly),
            row(82, "/tmp/ro-descendant", LinuxMountAccess::ReadOnly),
        ]);
        let argv = strings(configured);
        let tmp = mount_index(&argv, 68, "/tmp");
        let home = mount_index(&argv, 67, "/run/pi/home");
        let scratch = mount_index(&argv, 66, "/run/pi/scratch");
        let home_agent = mount_index(&argv, 75, "/run/pi/home/agent");
        let scratch_rev = mount_index(&argv, 76, "/run/pi/scratch/revision");
        let tmp_descendant = mount_index(&argv, 82, "/tmp/ro-descendant");
        assert!(
            home < home_agent,
            "HOME projection precedes its RO descendant"
        );
        assert!(
            scratch < scratch_rev,
            "scratch projection precedes its RO descendant"
        );
        assert!(
            tmp < tmp_descendant,
            "tmp projection precedes its RO descendant"
        );
        // RO descendants stay read-only fd mounts, not writable binds.
        let home_agent_row = argv
            .windows(3)
            .find(|row| row[1] == "75" && row[2] == "/run/pi/home/agent")
            .expect("home agent row");
        assert_eq!(home_agent_row[0], "--ro-bind-fd");
    }
}
