// Derived from OpenAI Codex under Apache-2.0 and modified for pi-rs.
// Upstream commit 646f7c0a91b8e327d263335da68ae8ef212895ce; see `../NOTICE`.

use std::path::{Path, PathBuf};

use crate::policy::{path_depth, validate_policy_path};
use crate::{FileSystemPolicy, NetworkMode, SandboxError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BubblewrapPlan {
    pub program: PathBuf,
    pub arguments: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum MountAccess {
    Writable,
    Protected,
    Denied,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MountOperation {
    path: PathBuf,
    access: MountAccess,
}

pub fn build_bubblewrap_arguments(
    command: &[String],
    command_cwd: &Path,
    policy: &FileSystemPolicy,
    network: NetworkMode,
) -> Result<Vec<String>, SandboxError> {
    if command.is_empty() {
        return Err(SandboxError::EmptyCommand);
    }
    let command_cwd = validate_policy_path(command_cwd.to_path_buf())?;
    let operations = mount_operations(policy);
    let writable_paths = policy
        .writable_roots()
        .iter()
        .map(|root| root.root().to_path_buf())
        .collect::<Vec<_>>();

    let mut arguments = vec![
        "--new-session".to_owned(),
        "--die-with-parent".to_owned(),
        "--ro-bind".to_owned(),
        "/".to_owned(),
        "/".to_owned(),
        "--dev".to_owned(),
        "/dev".to_owned(),
    ];
    for operation in operations {
        append_mount_operation(&mut arguments, &operation, &writable_paths)?;
    }
    arguments.extend(["--unshare-user".to_owned(), "--unshare-pid".to_owned()]);
    if network.unshares_network() {
        arguments.push("--unshare-net".to_owned());
    }
    arguments.extend([
        "--proc".to_owned(),
        "/proc".to_owned(),
        "--chdir".to_owned(),
        path_argument(&command_cwd)?,
        "--".to_owned(),
    ]);
    arguments.extend(command.iter().cloned());
    Ok(arguments)
}

fn mount_operations(policy: &FileSystemPolicy) -> Vec<MountOperation> {
    let mut operations = Vec::new();
    for writable_root in policy.writable_roots() {
        operations.push(MountOperation {
            path: writable_root.root().to_path_buf(),
            access: MountAccess::Writable,
        });
        operations.extend(
            writable_root
                .protected_descendants()
                .iter()
                .cloned()
                .map(|path| MountOperation {
                    path,
                    access: MountAccess::Protected,
                }),
        );
    }
    operations.extend(policy.denied_paths().iter().cloned().map(|path| MountOperation {
        path,
        access: MountAccess::Denied,
    }));
    operations.sort_by(|left, right| {
        path_depth(&left.path)
            .cmp(&path_depth(&right.path))
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.access.cmp(&right.access))
    });
    operations.dedup();
    operations
}

fn append_mount_operation(
    arguments: &mut Vec<String>,
    operation: &MountOperation,
    writable_paths: &[PathBuf],
) -> Result<(), SandboxError> {
    match operation.access {
        MountAccess::Writable => append_bind(arguments, "--bind", &operation.path),
        MountAccess::Protected => {
            if operation.path.exists() {
                append_bind(arguments, "--ro-bind", &operation.path)
            } else {
                append_directory_mask(arguments, &operation.path, "555", writable_paths)
            }
        }
        MountAccess::Denied if operation.path.is_file() => {
            arguments.push("--ro-bind".to_owned());
            arguments.push("/dev/null".to_owned());
            arguments.push(path_argument(&operation.path)?);
            Ok(())
        }
        MountAccess::Denied => {
            let permissions = if writable_paths.iter().any(|path| {
                path != &operation.path && path.starts_with(&operation.path)
            }) {
                "111"
            } else {
                "000"
            };
            append_directory_mask(arguments, &operation.path, permissions, writable_paths)
        }
    }
}

fn append_bind(
    arguments: &mut Vec<String>,
    option: &str,
    path: &Path,
) -> Result<(), SandboxError> {
    let path = path_argument(path)?;
    arguments.extend([option.to_owned(), path.clone(), path]);
    Ok(())
}

fn append_directory_mask(
    arguments: &mut Vec<String>,
    path: &Path,
    permissions: &str,
    writable_paths: &[PathBuf],
) -> Result<(), SandboxError> {
    let path_argument = path_argument(path)?;
    arguments.extend([
        "--perms".to_owned(),
        permissions.to_owned(),
        "--tmpfs".to_owned(),
        path_argument.clone(),
    ]);
    append_writable_descendant_targets(arguments, path, writable_paths)?;
    arguments.extend(["--remount-ro".to_owned(), path_argument]);
    Ok(())
}

fn append_writable_descendant_targets(
    arguments: &mut Vec<String>,
    masking_root: &Path,
    writable_paths: &[PathBuf],
) -> Result<(), SandboxError> {
    let mut targets = writable_paths
        .iter()
        .filter(|path| path.starts_with(masking_root) && path.as_path() != masking_root)
        .flat_map(|path| path.ancestors().take_while(|ancestor| *ancestor != masking_root))
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    targets.sort_by(|left, right| {
        path_depth(left)
            .cmp(&path_depth(right))
            .then_with(|| left.cmp(right))
    });
    targets.dedup();
    for target in targets {
        arguments.extend(["--dir".to_owned(), path_argument(&target)?]);
    }
    Ok(())
}

fn path_argument(path: &Path) -> Result<String, SandboxError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| SandboxError::NonUtf8Path(path.to_path_buf()))
}
