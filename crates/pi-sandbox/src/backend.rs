#[cfg(unix)]
use crate::RetainedRevisionRoots;
use crate::platform::{PlatformFacts, PlatformFactsInner};
use crate::{HostPlatform, PolicySnapshot, SandboxError, SandboxMode, SharedPolicySnapshot};

#[cfg(unix)]
use crate::launcher::TrustedExecutable;

pub struct SandboxBackend {
    facts: PlatformFacts,
    snapshot: SharedPolicySnapshot,
    kind: BackendKind,
    #[cfg(unix)]
    revision_roots: Option<RetainedRevisionRoots>,
}

pub(crate) enum BackendKind {
    #[cfg(target_os = "linux")]
    LinuxBubblewrap {
        bwrap: TrustedExecutable,
    },
    #[cfg(target_os = "macos")]
    MacosSeatbelt {
        sandbox_exec: TrustedExecutable,
    },
    RawHost {
        platform: HostPlatform,
    },
}

impl SandboxBackend {
    pub(crate) fn facts(&self) -> &PlatformFacts {
        &self.facts
    }

    pub(crate) fn snapshot(&self) -> &PolicySnapshot {
        &self.snapshot
    }

    pub(crate) fn kind(&self) -> &BackendKind {
        &self.kind
    }

    #[cfg(unix)]
    pub(crate) fn revision_roots(&self) -> Option<&RetainedRevisionRoots> {
        self.revision_roots.as_ref()
    }
}

pub fn probe_backend(
    facts: PlatformFacts,
    snapshot: SharedPolicySnapshot,
) -> Result<SandboxBackend, SandboxError> {
    probe_backend_with_roots(facts, snapshot, None)
}

#[cfg(unix)]
pub fn probe_backend_with_revision_roots(
    facts: PlatformFacts,
    snapshot: SharedPolicySnapshot,
    revision_roots: RetainedRevisionRoots,
) -> Result<SandboxBackend, SandboxError> {
    probe_backend_with_roots(facts, snapshot, Some(revision_roots))
}

fn probe_backend_with_roots(
    facts: PlatformFacts,
    snapshot: SharedPolicySnapshot,
    #[cfg(unix)] revision_roots: Option<RetainedRevisionRoots>,
) -> Result<SandboxBackend, SandboxError> {
    if facts.platform() != snapshot.created_on() {
        return Err(SandboxError::UnsupportedMode {
            platform: facts.platform(),
            mode: snapshot.policy().mode(),
        });
    }
    let mode = snapshot.policy().mode();
    if mode == SandboxMode::DangerFullAccess {
        let platform = facts.platform();
        return Ok(SandboxBackend {
            facts,
            snapshot,
            kind: BackendKind::RawHost { platform },
            #[cfg(unix)]
            revision_roots: None,
        });
    }
    #[cfg(unix)]
    let revision_roots = revision_roots.ok_or(SandboxError::InvalidSpawn {
        failure: crate::SpawnValidationError::InvalidRevisionRoots,
    })?;
    #[cfg(unix)]
    validate_revision_roots(&snapshot, &revision_roots)?;
    let kind = match &facts.inner {
        #[cfg(target_os = "linux")]
        PlatformFactsInner::Linux(_) => BackendKind::LinuxBubblewrap {
            bwrap: crate::launcher::probe_trusted_bwrap()?,
        },
        #[cfg(target_os = "macos")]
        PlatformFactsInner::Macos(_) => BackendKind::MacosSeatbelt {
            sandbox_exec: crate::launcher::probe_sandbox_exec()?,
        },
        #[cfg(target_os = "windows")]
        PlatformFactsInner::Windows => {
            return Err(SandboxError::UnsupportedMode {
                platform: HostPlatform::Windows,
                mode,
            });
        }
    };
    Ok(SandboxBackend {
        facts,
        snapshot,
        kind,
        #[cfg(unix)]
        revision_roots: Some(revision_roots),
    })
}

#[cfg(unix)]
fn validate_revision_roots(
    snapshot: &PolicySnapshot,
    revision_roots: &RetainedRevisionRoots,
) -> Result<(), SandboxError> {
    let filesystem = snapshot
        .policy()
        .filesystem()
        .ok_or(SandboxError::InvalidSpawn {
            failure: crate::SpawnValidationError::InvalidRevisionRoots,
        })?;
    let (home, tmp, scratch) = revision_roots.paths();
    let home_is_read_only =
        filesystem.read_only_roots().contains(home) || revision_root_is_protected(filesystem, home);
    let tmp_is_read_only =
        filesystem.read_only_roots().contains(tmp) || revision_root_is_protected(filesystem, tmp);
    let scratch_is_writable = filesystem
        .sealed_scratch_root()
        .is_some_and(|sealed| sealed == scratch)
        || filesystem
            .writable_roots()
            .iter()
            .any(|root| root.root() == scratch && root.protected_descendants().is_empty())
        || revision_root_is_protected(filesystem, scratch);
    let projections_are_distinct = home != tmp && home != scratch && tmp != scratch;
    if home_is_read_only && tmp_is_read_only && scratch_is_writable && projections_are_distinct {
        Ok(())
    } else {
        Err(SandboxError::InvalidSpawn {
            failure: crate::SpawnValidationError::InvalidRevisionRoots,
        })
    }
}

fn revision_root_is_protected(
    filesystem: &crate::FileSystemPolicy,
    path: &crate::AbsolutePath,
) -> bool {
    filesystem.writable_roots().iter().any(|root| {
        path.as_path()
            .strip_prefix(root.root().as_path())
            .is_ok_and(|relative| {
                !relative.as_os_str().is_empty()
                    && root
                        .protected_descendants()
                        .iter()
                        .any(|protected| relative.starts_with(protected.as_path()))
            })
    })
}
