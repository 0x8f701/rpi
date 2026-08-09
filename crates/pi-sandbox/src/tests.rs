use std::path::{Path, PathBuf};

use super::*;

fn fixture_root(name: &str) -> PathBuf {
    std::env::current_dir()
        .expect("current directory")
        .join("target")
        .join("pi-sandbox-fixtures")
        .join(name)
}

fn operation_index(arguments: &[String], option: &str, path: &Path) -> usize {
    let path = path.to_str().expect("fixture path is UTF-8");
    arguments
        .windows(3)
        .position(|window| window[0] == option && window[1] == path && window[2] == path)
        .expect("mount operation")
}

#[test]
fn argv_has_read_only_baseline_and_literal_command_tail() {
    let root = fixture_root("ordering");
    let policy = FileSystemPolicy::read_only();
    let command = vec!["sh".to_owned(), "-c".to_owned(), "echo 'a b'".to_owned()];
    let arguments = build_bubblewrap_arguments(
        &command,
        &root,
        &policy,
        NetworkMode::Restricted,
    )
    .expect("bubblewrap arguments");

    assert_eq!(
        &arguments[..7],
        ["--new-session", "--die-with-parent", "--ro-bind", "/", "/", "--dev", "/dev"]
    );
    let separator = arguments.iter().position(|argument| argument == "--").expect("separator");
    assert_eq!(&arguments[separator + 1..], command);
    assert!(arguments[..separator].contains(&"--unshare-net".to_owned()));
}

#[test]
fn narrower_mounts_override_broader_mounts_deterministically() {
    let root = fixture_root("precedence");
    let denied_parent = root.join("blocked");
    let writable_child = denied_parent.join("allowed");
    let protected_child = writable_child.join("metadata");
    let denied_grandchild = protected_child.join("secret");
    let policy = FileSystemPolicy::new(
        vec![
            WritableRoot::new(root.clone(), vec![root.join("metadata")]).expect("root"),
            WritableRoot::new(writable_child.clone(), vec![protected_child.clone()]).expect("child"),
        ],
        vec![denied_parent.clone(), denied_grandchild.clone()],
    )
    .expect("policy");
    let arguments = build_bubblewrap_arguments(
        &["true".to_owned()],
        &root,
        &policy,
        NetworkMode::FullAccess,
    )
    .expect("arguments");

    let root_bind = operation_index(&arguments, "--bind", &root);
    let parent_mask = arguments
        .windows(4)
        .position(|window| window[0] == "--perms" && window[2] == "--tmpfs" && window[3] == denied_parent.to_str().expect("UTF-8"))
        .expect("parent mask");
    let child_bind = operation_index(&arguments, "--bind", &writable_child);
    let protected_bind = arguments
        .windows(3)
        .position(|window| window[0] == "--ro-bind" && window[1] == protected_child.to_str().expect("UTF-8"))
        .unwrap_or_else(|| {
            arguments
                .windows(4)
                .position(|window| window[0] == "--perms" && window[2] == "--tmpfs" && window[3] == protected_child.to_str().expect("UTF-8"))
                .expect("protected mask")
        });
    let grandchild_mask = arguments
        .windows(4)
        .position(|window| window[0] == "--perms" && window[2] == "--tmpfs" && window[3] == denied_grandchild.to_str().expect("UTF-8"))
        .expect("grandchild mask");

    assert!(root_bind < parent_mask);
    assert!(parent_mask < child_bind);
    assert!(child_bind < protected_bind);
    assert!(protected_bind < grandchild_mask);
}

#[test]
fn denied_wins_over_protected_at_the_same_path() {
    let root = fixture_root("same-path");
    let protected = root.join("metadata");
    let policy = FileSystemPolicy::new(
        vec![WritableRoot::new(root.clone(), vec![protected.clone()]).expect("writable")],
        vec![protected.clone()],
    )
    .expect("policy");
    let arguments = build_bubblewrap_arguments(
        &["true".to_owned()],
        &root,
        &policy,
        NetworkMode::FullAccess,
    )
    .expect("arguments");
    let masks = arguments
        .windows(4)
        .filter(|window| window[2] == "--tmpfs" && window[3] == protected.to_str().expect("UTF-8"))
        .count();
    assert_eq!(masks, 2, "protected mount is followed by the same-path denied mount");
}

#[test]
fn duplicate_writable_roots_preserve_all_protected_descendants() {
    let temp = tempfile::TempDir::new().expect("temp directory");
    let root = temp.path().join("writable-root");
    let first_protected = root.join("first-protected");
    let second_protected = root.join("second-protected");
    std::fs::create_dir_all(&first_protected).expect("first protected directory");
    let policy = FileSystemPolicy::new(
        vec![
            WritableRoot::new(root.clone(), vec![second_protected.clone()]).expect("second root policy"),
            WritableRoot::new(
                root.clone(),
                vec![first_protected.clone(), second_protected.clone()],
            )
            .expect("first root policy"),
        ],
        Vec::new(),
    )
    .expect("policy");

    assert_eq!(policy.writable_roots().len(), 1);
    assert_eq!(
        policy.writable_roots()[0].protected_descendants(),
        &[first_protected.clone(), second_protected.clone()]
    );

    let arguments = build_bubblewrap_arguments(
        &["true".to_owned()],
        &root,
        &policy,
        NetworkMode::FullAccess,
    )
    .expect("arguments");
    let root_bind = operation_index(&arguments, "--bind", &root);
    let first_protected_bind = operation_index(&arguments, "--ro-bind", &first_protected);
    let second_protected_masks = arguments
        .windows(4)
        .enumerate()
        .filter(|(_, window)| {
            window[0] == "--perms"
                && window[1] == "555"
                && window[2] == "--tmpfs"
                && window[3] == second_protected.to_str().expect("UTF-8")
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();

    assert_eq!(second_protected_masks.len(), 1, "duplicate protection is mounted once");
    assert!(root_bind < first_protected_bind, "read-only bind overrides writable root");
    assert!(
        root_bind < second_protected_masks[0],
        "read-only mask overrides writable root"
    );
}

#[test]
fn network_modes_control_namespace_without_changing_command() {
    let cwd = fixture_root("network");
    for (mode, expects_unshare) in [
        (NetworkMode::FullAccess, false),
        (NetworkMode::Restricted, true),
        (NetworkMode::ProxyRouted, true),
    ] {
        let arguments = build_bubblewrap_arguments(
            &["printf".to_owned(), "%s".to_owned(), "literal value".to_owned()],
            &cwd,
            &FileSystemPolicy::read_only(),
            mode,
        )
        .expect("arguments");
        assert_eq!(arguments.iter().any(|argument| argument == "--unshare-net"), expects_unshare);
        assert_eq!(&arguments[arguments.len() - 3..], ["printf", "%s", "literal value"]);
    }
}

#[test]
fn policy_rejects_relative_parent_and_symlink_paths() {
    assert!(matches!(
        WritableRoot::new(PathBuf::from("relative"), Vec::new()),
        Err(SandboxError::PathNotAbsolute(_))
    ));
    let root = fixture_root("normalization");
    assert!(matches!(
        WritableRoot::new(root.join("child").join("..").join("escape"), Vec::new()),
        Err(SandboxError::PathNotNormalized(_))
    ));
}

#[test]
fn source_revision_and_platform_boundary_are_explicit() {
    assert_eq!(UPSTREAM_CODEX_COMMIT, "646f7c0a91b8e327d263335da68ae8ef212895ce");
    let expected = if cfg!(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )) {
        PlatformSupport::Linux
    } else if cfg!(target_os = "linux") {
        PlatformSupport::Unsupported {
            reason: "seccomp supports only x86_64 and aarch64 Linux targets",
        }
    } else {
        PlatformSupport::Unsupported {
            reason: "sandbox primitives require Linux",
        }
    };
    assert_eq!(platform_support(), expected);
}

#[cfg(target_os = "linux")]
mod linux_tests {
    use std::fs;
    use std::ffi::OsString;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::time::{Duration, Instant};

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn wsl1_detection_distinguishes_wsl_versions() {
        for wsl1 in [
            "Linux version 4.4.0-22621-Microsoft",
            "Linux version 5.15.0-microsoft-standard-WSL1",
            "Linux version 5.15.0-wsl-microsoft-standard-WSL1",
        ] {
            assert!(proc_version_indicates_wsl1(wsl1), "{wsl1}");
        }
        for supported in [
            "Linux version 6.6.87.2-microsoft-standard-WSL2",
            "Linux version 4.19.104-microsoft-standard",
            "Linux version 6.6.87.2-microsoft-standard-WSL3",
            "Linux version 6.8.0",
        ] {
            assert!(!proc_version_indicates_wsl1(supported), "{supported}");
        }
    }

    #[test]
    fn discovery_rejects_tempdir_candidates_by_default() {
        let temp = TempDir::new().expect("temp directory");
        let cwd = temp.path().join("workspace");
        let sibling = temp.path().join("sibling-bin");
        fs::create_dir_all(&cwd).expect("workspace");
        fs::create_dir_all(&sibling).expect("sibling bin");
        let sibling_bwrap = sibling.join("bwrap");
        write_executable(&sibling_bwrap, "#!/bin/sh\nexit 0\n");
        let search_path = std::env::join_paths([sibling]).expect("search path");

        assert_eq!(find_system_bwrap(Some(search_path.as_os_str()), &cwd), None);
        assert!(matches!(
            probe_bwrap(&sibling_bwrap, Duration::from_millis(200)),
            ProbeOutcome::UntrustedExecutable { .. }
        ));
    }

    #[test]
    fn discovery_rejects_candidate_inside_cwd() {
        let temp = TempDir::new().expect("temp directory");
        let trusted_root = temp.path().join("trusted-root");
        let cwd = trusted_root.join("workspace");
        let trusted_bin = trusted_root.join("trusted-bin");
        create_secure_directory(&trusted_root);
        create_secure_directory(&cwd);
        create_secure_directory(&trusted_bin);
        let cwd_bwrap = cwd.join("bwrap");
        write_executable(&cwd_bwrap, "#!/bin/sh\nexit 0\n");
        let trusted_bwrap = trusted_bin.join("bwrap");
        write_executable(&trusted_bwrap, "#!/bin/sh\nexit 0\n");
        let search_path = std::env::join_paths([cwd.clone(), trusted_bin])
            .expect("search path");
        let owner_uid = fs::metadata(&trusted_root).expect("trusted root metadata").uid();

        assert_eq!(
            crate::launcher::find_system_bwrap_with_trust(
                Some(search_path.as_os_str()),
                &cwd,
                &trusted_root,
                owner_uid,
            ),
            Some(fs::canonicalize(&trusted_bwrap).expect("canonical bwrap"))
        );
    }

    #[test]
    fn writable_path_executable_and_root_candidates_are_never_executed() {
        let temp = TempDir::new().expect("temp directory");
        let trusted_root = temp.path().join("trusted-root");
        let cwd = trusted_root.join("workspace");
        let trusted_bin = trusted_root.join("trusted-bin");
        let writable_sibling = trusted_root.join("writable-bin");
        create_secure_directory(&trusted_root);
        create_secure_directory(&cwd);
        create_secure_directory(&trusted_bin);
        create_secure_directory(&writable_sibling);
        fs::set_permissions(&writable_sibling, fs::Permissions::from_mode(0o777))
            .expect("make sibling writable");
        let sentinel = temp.path().join("candidate-ran");
        let candidate = writable_sibling.join("bwrap");
        write_executable(
            &candidate,
            &format!("#!/bin/sh\ntouch '{}'\nexit 0\n", sentinel.display()),
        );
        let trusted_bwrap = trusted_bin.join("bwrap");
        write_executable(&trusted_bwrap, "#!/bin/sh\nexit 0\n");
        let search_path = std::env::join_paths([&writable_sibling, &trusted_bin])
            .expect("search path");
        let owner_uid = fs::metadata(&trusted_root).expect("trusted root metadata").uid();

        assert_eq!(
            crate::launcher::find_system_bwrap_with_trust(
                Some(search_path.as_os_str()),
                &cwd,
                &trusted_root,
                owner_uid,
            ),
            Some(fs::canonicalize(&trusted_bwrap).expect("canonical bwrap"))
        );
        assert!(matches!(
            crate::launcher::probe_bwrap_with_trust(
                &candidate,
                &trusted_root,
                owner_uid,
                Duration::from_millis(200),
            ),
            ProbeOutcome::UntrustedExecutable { .. }
        ));
        assert!(!sentinel.exists(), "untrusted candidate must never execute");

        fs::set_permissions(&writable_sibling, fs::Permissions::from_mode(0o755))
            .expect("restore sibling permissions");
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o777))
            .expect("make executable writable");
        assert!(matches!(
            crate::launcher::probe_bwrap_with_trust(
                &candidate,
                &trusted_root,
                owner_uid,
                Duration::from_millis(200),
            ),
            ProbeOutcome::UntrustedExecutable { .. }
        ));
        assert!(!sentinel.exists(), "writable executable must never execute");

        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755))
            .expect("restore executable permissions");
        fs::set_permissions(&trusted_root, fs::Permissions::from_mode(0o777))
            .expect("make trusted root writable");
        assert!(matches!(
            crate::launcher::probe_bwrap_with_trust(
                &candidate,
                &trusted_root,
                owner_uid,
                Duration::from_millis(200),
            ),
            ProbeOutcome::UntrustedExecutable { .. }
        ));
        assert!(!sentinel.exists(), "writable trust root must never execute");
    }

    #[test]
    fn trusted_fixture_seam_discovers_and_probes_executable() {
        let temp = TempDir::new().expect("temp directory");
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o777))
            .expect("make ancestor above trust root writable");
        // This ancestor represents `/tmp`: it is intentionally outside the
        // custom root's trust boundary and must not invalidate the fixture.
        let trusted_root = temp.path().join("trusted-root");
        let cwd = trusted_root.join("workspace");
        let trusted_bin = trusted_root.join("bin");
        create_secure_directory(&trusted_root);
        create_secure_directory(&cwd);
        create_secure_directory(&trusted_bin);
        let trusted_bwrap = trusted_bin.join("bwrap");
        write_executable(&trusted_bwrap, "#!/bin/sh\nexit 0\n");
        let search_path = OsString::from(trusted_bin.as_os_str());
        let owner_uid = fs::metadata(&trusted_root).expect("trusted root metadata").uid();

        assert_eq!(
            crate::launcher::find_system_bwrap_with_trust(
                Some(search_path.as_os_str()),
                &cwd,
                &trusted_root,
                owner_uid,
            ),
            Some(fs::canonicalize(&trusted_bwrap).expect("canonical bwrap"))
        );
        assert_eq!(
            crate::launcher::probe_bwrap_with_trust(
                &trusted_bwrap,
                &trusted_root,
                owner_uid,
                Duration::from_millis(200),
            ),
            ProbeOutcome::Available
        );
    }

    #[test]
    fn trusted_probe_rejects_replaced_executable_identity() {
        let temp = TempDir::new().expect("temp directory");
        let trusted_root = temp.path().join("trusted-root");
        let cwd = trusted_root.join("workspace");
        let trusted_bin = trusted_root.join("bin");
        create_secure_directory(&trusted_root);
        create_secure_directory(&cwd);
        create_secure_directory(&trusted_bin);
        let trusted_bwrap = trusted_bin.join("bwrap");
        write_executable(&trusted_bwrap, "#!/bin/sh\nexit 0\n");
        let search_path = OsString::from(trusted_bin.as_os_str());
        let owner_uid = fs::metadata(&trusted_root).expect("trusted root metadata").uid();
        let executable = crate::launcher::discover_bwrap_with_trust_for_test(
            Some(search_path.as_os_str()),
            &cwd,
            &trusted_root,
            owner_uid,
        )
        .expect("trusted executable");
        let sentinel = temp.path().join("replacement-ran");
        fs::remove_file(&trusted_bwrap).expect("remove original");
        write_executable(
            &trusted_bwrap,
            &format!("#!/bin/sh\ntouch '{}'\nexit 0\n", sentinel.display()),
        );

        assert!(matches!(
            crate::launcher::probe_trusted_bwrap(&executable, Duration::from_millis(200)),
            ProbeOutcome::UntrustedExecutable { .. }
        ));
        assert!(!sentinel.exists(), "replaced candidate must never execute");
    }

    #[test]
    fn probe_classifies_success_userns_failure_other_exit_and_timeout() {
        let temp = TempDir::new().expect("temp directory");
        let trusted_root = temp.path().join("trusted-root");
        create_secure_directory(&trusted_root);
        let owner_uid = fs::metadata(&trusted_root).expect("trusted root metadata").uid();
        let probe = |path: &Path, timeout| {
            crate::launcher::probe_bwrap_with_trust(
                path,
                &trusted_root,
                owner_uid,
                timeout,
            )
        };
        let success = trusted_root.join("success");
        write_executable(&success, "#!/bin/sh\nexit 0\n");
        assert_eq!(probe(&success, Duration::from_millis(200)), ProbeOutcome::Available);

        let userns = trusted_root.join("userns");
        write_executable(&userns, "#!/bin/sh\necho 'No permissions to create a new namespace' >&2\nexit 1\n");
        assert!(matches!(probe(&userns, Duration::from_millis(200)), ProbeOutcome::UserNamespacesUnavailable { .. }));

        let other = trusted_root.join("other");
        write_executable(&other, "#!/bin/sh\necho 'unsupported option' >&2\nexit 42\n");
        assert!(matches!(probe(&other, Duration::from_millis(200)), ProbeOutcome::Exited { code: Some(42), .. }));

        let timeout = trusted_root.join("timeout");
        write_executable(&timeout, "#!/bin/sh\necho 'starting' >&2\nsleep 1\n");
        let started = Instant::now();
        assert!(matches!(probe(&timeout, Duration::from_millis(20)), ProbeOutcome::TimedOut { .. }));
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn probe_does_not_wait_for_descendant_holding_stderr_open() {
        let temp = TempDir::new().expect("temp directory");
        let trusted_root = temp.path().join("trusted-root");
        create_secure_directory(&trusted_root);
        let probe = trusted_root.join("descendant");
        write_executable(&probe, "#!/bin/sh\necho 'No permissions to create a new namespace' >&2\nsleep 1 &\nexit 1\n");
        let owner_uid = fs::metadata(&trusted_root).expect("trusted root metadata").uid();
        let started = Instant::now();
        assert!(matches!(
            crate::launcher::probe_bwrap_with_trust(
                &probe,
                &trusted_root,
                owner_uid,
                Duration::from_millis(200),
            ),
            ProbeOutcome::UserNamespacesUnavailable { .. }
        ));
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn all_seccomp_modes_compile_the_always_on_base_filter() {
        let full = crate::seccomp::compiled_filter(NetworkMode::FullAccess)
            .expect("base filter");
        let restricted = crate::seccomp::compiled_filter(NetworkMode::Restricted)
            .expect("restricted filter");
        let proxy = crate::seccomp::compiled_filter(NetworkMode::ProxyRouted)
            .expect("proxy filter");
        assert!(!full.is_empty(), "full network still denies ptrace/process-vm/io_uring");
        assert!(restricted.len() > full.len(), "restricted mode adds network rules");
        assert!(proxy.len() > full.len(), "proxy mode adds socket-family rules");
    }

    #[test]
    fn probe_rejects_helper_without_perms_support() {
        let temp = TempDir::new().expect("temp directory");
        let trusted_root = temp.path().join("trusted-root");
        create_secure_directory(&trusted_root);
        let probe = trusted_root.join("old-bwrap");
        write_executable(
            &probe,
            "#!/bin/sh\ncase \" $* \" in *\" --perms \"*) echo 'Unknown option --perms' >&2; exit 1;; esac\nexit 0\n",
        );
        let owner_uid = fs::metadata(&trusted_root).expect("trusted root metadata").uid();
        assert!(matches!(
            crate::launcher::probe_bwrap_with_trust(
                &probe,
                &trusted_root,
                owner_uid,
                Duration::from_millis(200),
            ),
            ProbeOutcome::Exited { code: Some(1), stderr } if stderr.contains("--perms")
        ));
    }

    fn create_secure_directory(path: &Path) {
        fs::create_dir_all(path).expect("create secure directory");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .expect("set secure directory permissions");
    }

    fn write_executable(path: &Path, contents: &str) {
        fs::write(path, contents).expect("write executable");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod executable");
    }
}

#[cfg(not(target_os = "linux"))]
#[test]
fn non_linux_enforcement_api_is_explicitly_unsupported() {
    let cwd = fixture_root("unsupported");
    assert!(matches!(plan_system_bubblewrap(&["true".to_owned()], &cwd, &FileSystemPolicy::read_only(), NetworkMode::Restricted), Err(SandboxError::UnsupportedPlatform)));
    assert!(matches!(set_no_new_privs_current_thread(), Err(SandboxError::UnsupportedPlatform)));
    assert!(matches!(install_seccomp_current_thread(NetworkMode::Restricted), Err(SandboxError::UnsupportedPlatform)));
    assert!(matches!(apply_current_thread_restrictions(NetworkMode::Restricted), Err(SandboxError::UnsupportedPlatform)));
}
