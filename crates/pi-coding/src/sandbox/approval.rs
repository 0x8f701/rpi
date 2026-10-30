use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    path::PathBuf,
    sync::Arc,
};

use pi_agent::{
    ToolApprovalDigest, ToolApprovalFdDirection, ToolApprovalId, ToolApprovalTargetFdSummary,
    ToolCallApprovalToken,
};
use pi_sandbox::{
    AbsolutePath, CanonicalTargetFdMapping, CredentialGrantId, EnvironmentVariableName,
    SpawnFamily, StdioMode, TargetFdDirection, TerminalSize,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{generation::SandboxGeneration, grant_registry::CredentialAudience};

const APPROVAL_LIFETIME_MS: u64 = 5 * 60 * 1000;
const APPROVAL_DOMAIN: &[u8] = b"rpi-tool-spawn-approval";
const APPROVAL_VERSION: u8 = 1;
const MOUNT_PURPOSE_WORKFLOW_WORKTREES: u8 = 4;
const MOUNT_ACCESS_PRIVATE_READ_WRITE: u8 = 2;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CanonicalSpawnCall {
    pub argv: Vec<String>,
    pub cwd: AbsolutePath,
    pub requested_env: BTreeMap<EnvironmentVariableName, Option<String>>,
    pub requested_grants: BTreeSet<CredentialGrantId>,
    pub stdio: StdioMode,
    pub terminal_size: Option<TerminalSize>,
    pub timeout_ms: Option<u64>,
    pub target_fd_mappings: Vec<CanonicalTargetFdMapping>,
    pub mount_sources: Vec<CanonicalMountSource>,
    pub retained_executable: Option<CanonicalRetainedExecutable>,
    pub retained_working_directory: Option<CanonicalRetainedWorkingDirectory>,
}
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct CanonicalMountSource {
    purpose: u8,
    access: u8,
    destination: AbsolutePath,
    identity: CanonicalFileIdentity,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum CanonicalFileIdentity {
    Unix {
        device: u64,
        inode: u64,
        kind: u8,
    },
    Windows {
        volume_serial: u64,
        file_id: [u8; 16],
        kind: u8,
    },
}

impl CanonicalMountSource {
    #[cfg(unix)]
    fn from_capability(
        source: &crate::fs::CapabilitySpawnSource,
    ) -> Result<Self, SpawnAuthorizationError> {
        let (purpose, access, destination, identity) = source.canonical_mount();
        if purpose != crate::fs::CapabilityRootPurpose::WorkflowWorktrees
            || access != crate::fs::CapabilityAccess::PrivateReadWrite
        {
            return Err(SpawnAuthorizationError::MountSourceMismatch);
        }
        Ok(Self {
            purpose: MOUNT_PURPOSE_WORKFLOW_WORKTREES,
            access: MOUNT_ACCESS_PRIVATE_READ_WRITE,
            destination,
            identity: match identity {
                crate::fs::FileIdentity::Unix {
                    device,
                    inode,
                    kind,
                } => CanonicalFileIdentity::Unix {
                    device,
                    inode,
                    kind: match kind {
                        crate::fs::EntryKind::RegularFile => 0,
                        crate::fs::EntryKind::Directory => 1,
                    },
                },
                crate::fs::FileIdentity::Windows {
                    volume_serial,
                    file_id,
                    kind,
                } => CanonicalFileIdentity::Windows {
                    volume_serial,
                    file_id,
                    kind: match kind {
                        crate::fs::EntryKind::RegularFile => 0,
                        crate::fs::EntryKind::Directory => 1,
                    },
                },
            },
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CanonicalRetainedExecutable {
    path: AbsolutePath,
    identity: CanonicalFileIdentity,
}

impl CanonicalRetainedExecutable {
    #[cfg(unix)]
    fn from_capability(
        source: &crate::fs::CapabilityExecutableSource,
    ) -> Result<Self, SpawnAuthorizationError> {
        let (path, identity) = source.canonical_executable();
        Ok(Self {
            path: path.clone(),
            identity: canonical_file_identity(identity),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CanonicalRetainedWorkingDirectory {
    path: AbsolutePath,
    identity: CanonicalFileIdentity,
}

impl CanonicalRetainedWorkingDirectory {
    #[cfg(unix)]
    fn from_capability(
        source: &crate::fs::CapabilityWorkingDirectorySource,
    ) -> Result<Self, SpawnAuthorizationError> {
        let (path, identity) = source.canonical_working_directory();
        if identity.kind() != crate::fs::EntryKind::Directory {
            return Err(SpawnAuthorizationError::RetainedWorkingDirectoryMismatch);
        }
        Ok(Self {
            path: path.clone(),
            identity: canonical_file_identity(identity),
        })
    }
}

fn canonical_file_identity(identity: crate::fs::FileIdentity) -> CanonicalFileIdentity {
    match identity {
        crate::fs::FileIdentity::Unix {
            device,
            inode,
            kind,
        } => CanonicalFileIdentity::Unix {
            device,
            inode,
            kind: match kind {
                crate::fs::EntryKind::RegularFile => 0,
                crate::fs::EntryKind::Directory => 1,
            },
        },
        crate::fs::FileIdentity::Windows {
            volume_serial,
            file_id,
            kind,
        } => CanonicalFileIdentity::Windows {
            volume_serial,
            file_id,
            kind: match kind {
                crate::fs::EntryKind::RegularFile => 0,
                crate::fs::EntryKind::Directory => 1,
            },
        },
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum SpawnAuthorizationError {
    #[error("tool approval token is unknown or already consumed")]
    UnknownOrConsumed,
    #[error("tool approval token expired")]
    Expired,
    #[error("tool approval family does not match")]
    FamilyMismatch,
    #[error("tool approval audience does not match")]
    AudienceMismatch,
    #[error("tool approval digest does not match")]
    DigestMismatch,
    #[error("owned target descriptors do not match approved metadata")]
    TargetFdMismatch,
    #[error("owned mount sources do not match approved metadata")]
    MountSourceMismatch,
    #[error("system clock cannot represent the approval lifetime")]
    ClockUnavailable,
    #[error("retained executable does not match approved metadata")]
    RetainedExecutableMismatch,
    #[error("retained working directory does not match approved metadata")]
    RetainedWorkingDirectoryMismatch,
}

struct ApprovedSpawnEntry {
    generation: Arc<SandboxGeneration>,
    family: SpawnFamily,
    audience: CredentialAudience,
    expires_at_unix_ms: u64,
    digest: ToolApprovalDigest,
}

#[derive(Default)]
pub(crate) struct ApprovedSpawnRegistry {
    entries: parking_lot::Mutex<BTreeMap<ToolApprovalId, ApprovedSpawnEntry>>,
}

pub(crate) struct ConsumedApprovedSpawn {
    pub generation: Arc<SandboxGeneration>,
}

impl ApprovedSpawnRegistry {
    pub(crate) fn issue(
        &self,
        generation: Arc<SandboxGeneration>,
        family: SpawnFamily,
        audience: CredentialAudience,
        call: &CanonicalSpawnCall,
        now_unix_ms: u64,
    ) -> Result<ToolCallApprovalToken, SpawnAuthorizationError> {
        let expires_at_unix_ms = now_unix_ms
            .checked_add(APPROVAL_LIFETIME_MS)
            .ok_or(SpawnAuthorizationError::ClockUnavailable)?;
        let id = ToolApprovalId::from_uuid_v7_bytes(*Uuid::now_v7().as_bytes())
            .map_err(|_| SpawnAuthorizationError::ClockUnavailable)?;
        let digest = canonical_digest(id, &generation, family, &audience, expires_at_unix_ms, call);
        let token = ToolCallApprovalToken::new(id, digest);
        self.entries.lock().insert(
            id,
            ApprovedSpawnEntry {
                generation,
                family,
                audience,
                expires_at_unix_ms,
                digest,
            },
        );
        Ok(token)
    }

    pub(crate) fn consume(
        &self,
        token: ToolCallApprovalToken,
        family: SpawnFamily,
        audience: &CredentialAudience,
        call: &CanonicalSpawnCall,
        now_unix_ms: u64,
    ) -> Result<ConsumedApprovedSpawn, SpawnAuthorizationError> {
        let entry = self
            .entries
            .lock()
            .remove(&token.id())
            .ok_or(SpawnAuthorizationError::UnknownOrConsumed)?;
        if now_unix_ms > entry.expires_at_unix_ms {
            return Err(SpawnAuthorizationError::Expired);
        }
        if family != entry.family {
            return Err(SpawnAuthorizationError::FamilyMismatch);
        }
        if audience != &entry.audience {
            return Err(SpawnAuthorizationError::AudienceMismatch);
        }
        let expected = canonical_digest(
            token.id(),
            &entry.generation,
            entry.family,
            &entry.audience,
            entry.expires_at_unix_ms,
            call,
        );
        if !constant_time_equal(expected.as_bytes(), token.digest().as_bytes())
            || !constant_time_equal(entry.digest.as_bytes(), token.digest().as_bytes())
        {
            return Err(SpawnAuthorizationError::DigestMismatch);
        }
        Ok(ConsumedApprovedSpawn {
            generation: entry.generation,
        })
    }

    pub(crate) fn clear(&self) {
        self.entries.lock().clear();
    }
}

pub(crate) fn target_fd_summary(
    mappings: &[CanonicalTargetFdMapping],
) -> Vec<ToolApprovalTargetFdSummary> {
    let mut mappings = mappings.to_vec();
    mappings.sort_unstable();
    mappings
        .into_iter()
        .map(|mapping| ToolApprovalTargetFdSummary {
            target_fd: mapping.target(),
            direction: match mapping.direction() {
                TargetFdDirection::ParentReads => ToolApprovalFdDirection::ParentReads,
                TargetFdDirection::ParentWrites => ToolApprovalFdDirection::ParentWrites,
            },
        })
        .collect()
}

pub(crate) enum SpawnAuthorization {
    Captured(Arc<SandboxGeneration>),
    Approved(ToolCallApprovalToken),
}

pub(crate) struct RawSpawnRequest {
    pub authorization: SpawnAuthorization,
    pub family: SpawnFamily,
    pub audience: CredentialAudience,
    pub argv: Vec<OsString>,
    pub cwd: PathBuf,
    pub requested_env: BTreeMap<OsString, Option<OsString>>,
    pub requested_grants: BTreeSet<CredentialGrantId>,
    pub stdio: StdioMode,
    pub terminal_size: Option<TerminalSize>,
    pub timeout_ms: Option<u64>,
    pub target_fd_mappings: Vec<CanonicalTargetFdMapping>,
    #[cfg(unix)]
    pub owned_target_fd_mappings: Vec<pi_sandbox::TargetFdMapping>,
    #[cfg(unix)]
    pub mount_sources: Vec<crate::fs::CapabilitySpawnSource>,
    #[cfg(unix)]
    pub retained_executable: Option<crate::fs::CapabilityExecutableSource>,
    #[cfg(unix)]
    pub retained_working_directory: Option<crate::fs::CapabilityWorkingDirectorySource>,
}

impl RawSpawnRequest {
    pub(crate) fn canonical_call(&self) -> Result<CanonicalSpawnCall, SpawnAuthorizationError> {
        #[cfg(unix)]
        {
            let mut owned = self
                .owned_target_fd_mappings
                .iter()
                .map(pi_sandbox::TargetFdMapping::canonical)
                .collect::<Vec<_>>();
            owned.sort_unstable();
            let mut declared = self.target_fd_mappings.clone();
            declared.sort_unstable();
            if owned != declared {
                return Err(SpawnAuthorizationError::TargetFdMismatch);
            }
        }
        #[cfg(windows)]
        if !self.target_fd_mappings.is_empty() {
            return Err(SpawnAuthorizationError::TargetFdMismatch);
        }

        let argv = self
            .argv
            .iter()
            .map(|value| canonical_os_string(value))
            .collect::<Result<Vec<_>, _>>()?;
        #[cfg(unix)]
        if let Some(executable) = self.retained_executable.as_ref() {
            let executable_path = AbsolutePath::parse(std::path::PathBuf::from(
                argv.first()
                    .ok_or(SpawnAuthorizationError::RetainedExecutableMismatch)?,
            ))
            .map_err(|_| SpawnAuthorizationError::RetainedExecutableMismatch)?;
            if executable_path != *executable.canonical_executable().0 {
                return Err(SpawnAuthorizationError::RetainedExecutableMismatch);
            }
        }
        let cwd = AbsolutePath::parse(self.cwd.clone())
            .map_err(|_| SpawnAuthorizationError::DigestMismatch)?;
        #[cfg(unix)]
        if let Some(directory) = self.retained_working_directory.as_ref() {
            if cwd != *directory.canonical_working_directory().0 {
                return Err(SpawnAuthorizationError::RetainedWorkingDirectoryMismatch);
            }
        }
        let requested_env = self
            .requested_env
            .iter()
            .map(|(name, value)| {
                let name = EnvironmentVariableName::parse(canonical_os_string(name)?)
                    .map_err(|_| SpawnAuthorizationError::DigestMismatch)?;
                let value = value
                    .as_ref()
                    .map(|value| canonical_os_string(value))
                    .transpose()?;
                Ok((name, value))
            })
            .collect::<Result<BTreeMap<_, _>, SpawnAuthorizationError>>()?;
        let mut target_fd_mappings = self.target_fd_mappings.clone();
        target_fd_mappings.sort_unstable();
        Ok(CanonicalSpawnCall {
            argv,
            cwd,
            requested_env,
            requested_grants: self.requested_grants.clone(),
            stdio: self.stdio,
            terminal_size: self.terminal_size,
            mount_sources: self
                .mount_sources
                .iter()
                .map(CanonicalMountSource::from_capability)
                .collect::<Result<Vec<_>, _>>()?,
            retained_executable: self
                .retained_executable
                .as_ref()
                .map(CanonicalRetainedExecutable::from_capability)
                .transpose()?,
            timeout_ms: self.timeout_ms,
            retained_working_directory: self
                .retained_working_directory
                .as_ref()
                .map(CanonicalRetainedWorkingDirectory::from_capability)
                .transpose()?,
            target_fd_mappings,
        })
    }
}

fn canonical_digest(
    id: ToolApprovalId,
    generation: &SandboxGeneration,
    family: SpawnFamily,
    audience: &CredentialAudience,
    expires_at_unix_ms: u64,
    call: &CanonicalSpawnCall,
) -> ToolApprovalDigest {
    let mut hasher = Sha256::new();
    hash_bytes(&mut hasher, APPROVAL_DOMAIN);
    hasher.update([APPROVAL_VERSION]);
    hasher.update(id.as_bytes());
    hasher.update(generation.revision().get().to_be_bytes());
    hasher.update(generation.policy_digest().as_bytes());
    hash_bytes(&mut hasher, family.as_str().as_bytes());
    hash_audience(&mut hasher, audience);
    hasher.update(expires_at_unix_ms.to_be_bytes());
    hash_string_sequence(&mut hasher, &call.argv);
    hash_bytes(
        &mut hasher,
        call.cwd.as_path().as_os_str().as_encoded_bytes(),
    );
    hash_environment(&mut hasher, &call.requested_env);
    hash_grants(&mut hasher, &call.requested_grants);
    hasher.update([call.stdio.canonical_tag()]);
    match call.terminal_size {
        Some(size) => {
            hasher.update([1]);
            hasher.update(size.rows().to_be_bytes());
            hasher.update(size.columns().to_be_bytes());
        }
        None => hasher.update([0]),
    }
    match call.timeout_ms {
        Some(timeout_ms) => {
            hasher.update([1]);
            hasher.update(timeout_ms.to_be_bytes());
        }
        None => hasher.update([0]),
    }
    let mut mappings = call.target_fd_mappings.clone();
    mappings.sort_unstable();
    hash_count(&mut hasher, mappings.len());
    for mapping in mappings {
        hasher.update(mapping.target().to_be_bytes());
        hasher.update([match mapping.direction() {
            TargetFdDirection::ParentReads => 0,
            TargetFdDirection::ParentWrites => 1,
        }]);
    }
    hash_count(&mut hasher, call.mount_sources.len());
    for mount in &call.mount_sources {
        hasher.update([mount.purpose, mount.access]);
        hash_bytes(
            &mut hasher,
            mount.destination.as_path().as_os_str().as_encoded_bytes(),
        );
        match mount.identity {
            CanonicalFileIdentity::Unix {
                device,
                inode,
                kind,
            } => {
                hasher.update([0]);
                hasher.update(device.to_be_bytes());
                hasher.update(inode.to_be_bytes());
                hasher.update([kind]);
            }
            CanonicalFileIdentity::Windows {
                volume_serial,
                file_id,
                kind,
            } => {
                hasher.update([1]);
                hasher.update(volume_serial.to_be_bytes());
                hasher.update(file_id);
                hasher.update([kind]);
            }
        }
    }
    match &call.retained_executable {
        Some(executable) => {
            hasher.update([1]);
            hash_bytes(
                &mut hasher,
                executable.path.as_path().as_os_str().as_encoded_bytes(),
            );
            match executable.identity {
                CanonicalFileIdentity::Unix {
                    device,
                    inode,
                    kind,
                } => {
                    hasher.update([0]);
                    hasher.update(device.to_be_bytes());
                    hasher.update(inode.to_be_bytes());
                    hasher.update([kind]);
                }
                CanonicalFileIdentity::Windows {
                    volume_serial,
                    file_id,
                    kind,
                } => {
                    hasher.update([1]);
                    hasher.update(volume_serial.to_be_bytes());
                    hasher.update(file_id);
                    hasher.update([kind]);
                }
            }
        }
        None => hasher.update([0]),
    }
    match &call.retained_working_directory {
        Some(directory) => {
            hasher.update([1]);
            hash_bytes(
                &mut hasher,
                directory.path.as_path().as_os_str().as_encoded_bytes(),
            );
            match directory.identity {
                CanonicalFileIdentity::Unix {
                    device,
                    inode,
                    kind,
                } => {
                    hasher.update([0]);
                    hasher.update(device.to_be_bytes());
                    hasher.update(inode.to_be_bytes());
                    hasher.update([kind]);
                }
                CanonicalFileIdentity::Windows {
                    volume_serial,
                    file_id,
                    kind,
                } => {
                    hasher.update([1]);
                    hasher.update(volume_serial.to_be_bytes());
                    hasher.update(file_id);
                    hasher.update([kind]);
                }
            }
        }
        None => hasher.update([0]),
    }
    ToolApprovalDigest::from_sha256(hasher.finalize().into())
}

fn hash_audience(hasher: &mut Sha256, audience: &CredentialAudience) {
    match audience {
        CredentialAudience::McpServer { name } => {
            hasher.update([0]);
            hash_bytes(hasher, name.as_str().as_bytes());
        }
        CredentialAudience::SpawnFamily { family } => {
            hasher.update([1]);
            hash_bytes(hasher, family.as_str().as_bytes());
        }
    }
}

fn hash_string_sequence(hasher: &mut Sha256, values: &[String]) {
    hash_count(hasher, values.len());
    for value in values {
        hash_bytes(hasher, value.as_bytes());
    }
}

fn hash_environment(
    hasher: &mut Sha256,
    environment: &BTreeMap<EnvironmentVariableName, Option<String>>,
) {
    hash_count(hasher, environment.len());
    for (name, value) in environment {
        hash_bytes(hasher, name.as_str().as_bytes());
        match value {
            Some(value) => {
                hasher.update([1]);
                hash_bytes(hasher, value.as_bytes());
            }
            None => hasher.update([0]),
        }
    }
}

fn hash_grants(hasher: &mut Sha256, grants: &BTreeSet<CredentialGrantId>) {
    hash_count(hasher, grants.len());
    for grant in grants {
        hash_bytes(hasher, grant.as_str().as_bytes());
    }
}

fn hash_bytes(hasher: &mut Sha256, bytes: &[u8]) {
    let length = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    hasher.update(length.to_be_bytes());
    hasher.update(bytes);
}

fn hash_count(hasher: &mut Sha256, count: usize) {
    hasher.update(u32::try_from(count).unwrap_or(u32::MAX).to_be_bytes());
}

fn constant_time_equal(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right.iter())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn canonical_os_string(value: &OsStr) -> Result<String, SpawnAuthorizationError> {
    value
        .to_str()
        .map(str::to_owned)
        .ok_or(SpawnAuthorizationError::DigestMismatch)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use pi_ai::providers::{FauxProviderOptions, FauxProviderRegistration, register_faux_provider};
    use pi_sandbox::SandboxMode;

    use super::*;
    fn approval_test_application(
        global_settings: &str,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        tokio::runtime::Runtime,
        crate::Application,
        FauxProviderRegistration,
    ) {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let session_dir = agent.path().join("sessions");
        std::fs::create_dir_all(&session_dir).expect("session dir");
        std::fs::write(agent.path().join("settings.json"), global_settings)
            .expect("global settings");
        let mut options = crate::ResourceManagerOptions::new(cwd.path());
        options.agent_dir = agent.path().to_path_buf();
        options.project_trust_override = Some(true);
        options.disable_extensions = true;
        options.disable_skills = true;
        options.disable_prompt_templates = true;
        options.disable_themes = true;
        options.disable_context_files = true;
        let resources = crate::ResourceManager::new(options).expect("resources");
        let suffix = uuid::Uuid::now_v7();
        let api = format!("approval-test-api-{suffix}");
        let provider = format!("approval-test-provider-{suffix}");
        let model = pi_ai::Model {
            id: format!("approval-test-model-{suffix}"),
            name: "Approval Test Model".to_owned(),
            api: api.clone(),
            provider: provider.clone(),
            ..pi_ai::Model::default()
        };
        let faux = register_faux_provider(FauxProviderOptions {
            api,
            provider,
            models: vec![model.clone()],
            chunk_size: 8,
        });
        let workspace = crate::WorkspaceRoots::new(cwd.path(), Vec::<std::path::PathBuf>::new())
            .expect("workspace");
        let session =
            crate::Session::new_with_additional_tools_filtered_discovery_workspace_and_uri(
                crate::SessionOptions {
                    model,
                    cwd: cwd.path().to_path_buf(),
                    system_prompt: String::new(),
                    thinking_level: pi_agent::ThinkingLevel::Off,
                    api_key: "test".to_owned(),
                    compaction: None,
                    stream_options: Default::default(),
                    tools: Some(Vec::new()),
                    before_tool_call: None,
                    after_tool_call: None,
                    stream_fn: None,
                    auth_resolver: None,
                    auth_manager: None,
                },
                Vec::new(),
                crate::ToolSelection::default(),
                crate::ResourceDiscovery::Disabled,
                workspace,
                None,
            )
            .expect("session");
        session.set_session_dir(session_dir);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime
            .block_on(session.attach_resources(resources))
            .expect("attach resources");
        let application = runtime
            .block_on(crate::Application::from_runtime_candidate(
                crate::ApplicationRuntimeCandidate::new(session),
            ))
            .expect("publish approval test application");
        (agent, cwd, runtime, application, faux)
    }

    fn application_with_mode(
        mode: &str,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        tokio::runtime::Runtime,
        crate::Application,
        Arc<SandboxGeneration>,
        Arc<ApprovedSpawnRegistry>,
        FauxProviderRegistration,
    ) {
        let settings = format!(r#"{{"sandbox":{{"mode":"{mode}"}}}}"#);
        let (agent, cwd, runtime, application, faux) = approval_test_application(&settings);
        let (generation, approvals) = application.approval_test_authorities();
        (
            agent,
            cwd,
            runtime,
            application,
            generation,
            approvals,
            faux,
        )
    }

    fn audience(family: SpawnFamily) -> CredentialAudience {
        CredentialAudience::new_spawn_family(family).expect("spawn audience")
    }

    fn call(cwd: &std::path::Path) -> CanonicalSpawnCall {
        CanonicalSpawnCall {
            argv: vec!["tool".to_owned(), "argument".to_owned()],
            cwd: AbsolutePath::parse(cwd.to_path_buf()).expect("absolute cwd"),
            requested_env: BTreeMap::from([(
                EnvironmentVariableName::parse("ORDINARY".to_owned()).expect("environment"),
                Some("value".to_owned()),
            )]),
            requested_grants: BTreeSet::from([
                CredentialGrantId::parse("d3-grant".to_owned()).expect("grant")
            ]),
            stdio: StdioMode::Pty,
            terminal_size: Some(TerminalSize::new(40, 120).expect("terminal size")),
            timeout_ms: Some(30_000),
            target_fd_mappings: vec![
                CanonicalTargetFdMapping::new(3, TargetFdDirection::ParentWrites)
                    .expect("target mapping"),
            ],
            mount_sources: Vec::new(),
            retained_executable: None,
            retained_working_directory: None,
        }
    }

    fn assert_digest_mismatch_after_mutation(
        generation: &Arc<SandboxGeneration>,
        family: SpawnFamily,
        audience: &CredentialAudience,
        original: &CanonicalSpawnCall,
        mutate: impl FnOnce(&mut CanonicalSpawnCall),
    ) {
        let registry = ApprovedSpawnRegistry::default();
        let token = registry
            .issue(
                generation.clone(),
                family,
                audience.clone(),
                original,
                1_000,
            )
            .expect("issue approval");
        let mut changed = original.clone();
        mutate(&mut changed);
        assert!(matches!(
            registry.consume(token, family, audience, &changed, 1_001),
            Err(SpawnAuthorizationError::DigestMismatch),
        ));
    }

    #[test]
    fn consume_burns_before_validation_and_secrets() {
        let (_agent, cwd, _runtime, application, generation, approvals, _faux) =
            application_with_mode("danger-full-access");
        let family = SpawnFamily::ManagedPipeProcess;
        let audience = audience(family);
        let call = call(cwd.path());
        let token = approvals
            .issue(generation, family, audience.clone(), &call, 1_000)
            .expect("issue approval");

        assert!(matches!(
            approvals.consume(token, SpawnFamily::Hook, &audience, &call, 1_001),
            Err(SpawnAuthorizationError::FamilyMismatch),
        ));
        assert!(matches!(
            approvals.consume(token, family, &audience, &call, 1_001),
            Err(SpawnAuthorizationError::UnknownOrConsumed),
        ));
        drop(application);
    }

    #[test]
    fn digest_binds_family_audience_credentials_terminal_timeout_and_target_directions() {
        let (_agent, cwd, _runtime, application, generation, _, _faux) =
            application_with_mode("danger-full-access");
        let family = SpawnFamily::ManagedPipeProcess;
        let expected_audience = audience(family);
        let original = call(cwd.path());

        let registry = ApprovedSpawnRegistry::default();
        let token = registry
            .issue(
                generation.clone(),
                family,
                expected_audience.clone(),
                &original,
                1_000,
            )
            .expect("issue approval");
        assert!(matches!(
            registry.consume(
                token,
                family,
                &audience(SpawnFamily::Hook),
                &original,
                1_001,
            ),
            Err(SpawnAuthorizationError::AudienceMismatch),
        ));

        assert_digest_mismatch_after_mutation(
            &generation,
            family,
            &expected_audience,
            &original,
            |changed| {
                changed
                    .requested_grants
                    .insert(CredentialGrantId::parse("second-grant".to_owned()).expect("grant"));
            },
        );
        assert_digest_mismatch_after_mutation(
            &generation,
            family,
            &expected_audience,
            &original,
            |changed| {
                changed.terminal_size = Some(TerminalSize::new(41, 120).expect("terminal size"));
            },
        );
        assert_digest_mismatch_after_mutation(
            &generation,
            family,
            &expected_audience,
            &original,
            |changed| changed.timeout_ms = Some(30_001),
        );
        assert_digest_mismatch_after_mutation(
            &generation,
            family,
            &expected_audience,
            &original,
            |changed| {
                changed.target_fd_mappings = vec![
                    CanonicalTargetFdMapping::new(3, TargetFdDirection::ParentReads)
                        .expect("target mapping"),
                ];
            },
        );
        #[cfg(unix)]
        {
            let retained = CanonicalRetainedExecutable {
                path: AbsolutePath::parse(cwd.path().join("extension")).expect("retained path"),
                identity: CanonicalFileIdentity::Unix {
                    device: 11,
                    inode: 29,
                    kind: 0,
                },
            };
            assert_digest_mismatch_after_mutation(
                &generation,
                family,
                &expected_audience,
                &original,
                |changed| changed.retained_executable = Some(retained.clone()),
            );
            let mut executable_original = original.clone();
            executable_original.retained_executable = Some(retained);
            assert_digest_mismatch_after_mutation(
                &generation,
                family,
                &expected_audience,
                &executable_original,
                |changed| {
                    changed
                        .retained_executable
                        .as_mut()
                        .expect("retained executable")
                        .path = AbsolutePath::parse(cwd.path().join("replacement-extension"))
                        .expect("replacement path");
                },
            );
            assert_digest_mismatch_after_mutation(
                &generation,
                family,
                &expected_audience,
                &executable_original,
                |changed| {
                    changed
                        .retained_executable
                        .as_mut()
                        .expect("retained executable")
                        .identity = CanonicalFileIdentity::Unix {
                        device: 11,
                        inode: 30,
                        kind: 0,
                    };
                },
            );
        }
        drop(application);
    }

    #[test]
    fn approval_has_zero_policy_delta() {
        let (_agent, cwd, _runtime, application, generation, approvals, _faux) =
            application_with_mode("danger-full-access");
        let family = SpawnFamily::ManagedPipeProcess;
        let audience = audience(family);
        let call = call(cwd.path());
        let mode_before = generation.policy_snapshot().policy().mode();
        let digest_before = generation.policy_digest();
        let revision_before = generation.revision();
        let token = approvals
            .issue(generation.clone(), family, audience.clone(), &call, 1_000)
            .expect("issue approval");
        let consumed = approvals
            .consume(token, family, &audience, &call, 1_001)
            .expect("consume approval");
        assert_eq!(mode_before, SandboxMode::DangerFullAccess);
        assert_eq!(
            consumed.generation.policy_snapshot().policy().mode(),
            mode_before
        );
        assert_eq!(consumed.generation.policy_digest(), digest_before);
        assert_eq!(consumed.generation.revision(), revision_before);
        assert!(Arc::ptr_eq(&consumed.generation, &generation));
        drop(application);
    }

    #[cfg(unix)]
    #[test]
    fn unix_owned_target_fds_match_canonical_approval() {
        use std::fs::File;

        use pi_sandbox::{TargetFd, TargetFdMapping};

        let canonical = CanonicalTargetFdMapping::new(3, TargetFdDirection::ParentWrites)
            .expect("canonical mapping");
        let matching = RawSpawnRequest {
            authorization: SpawnAuthorization::Captured(
                application_with_mode("danger-full-access").4,
            ),
            family: SpawnFamily::Browser,
            audience: audience(SpawnFamily::Browser),
            argv: vec![OsString::from("browser")],
            cwd: std::env::current_dir().expect("current directory"),
            requested_env: BTreeMap::new(),
            requested_grants: BTreeSet::new(),
            stdio: StdioMode::Piped,
            terminal_size: None,
            timeout_ms: None,
            target_fd_mappings: vec![canonical],
            owned_target_fd_mappings: vec![
                TargetFdMapping::new(
                    File::open("/dev/null").expect("open null").into(),
                    TargetFd::new(3).expect("target fd"),
                    TargetFdDirection::ParentWrites,
                )
                .expect("owned mapping"),
            ],
            #[cfg(unix)]
            mount_sources: Vec::new(),
            #[cfg(unix)]
            retained_executable: None,
            #[cfg(unix)]
            retained_working_directory: None,
        };
        assert_eq!(
            matching
                .canonical_call()
                .expect("matching canonical call")
                .target_fd_mappings,
            vec![canonical],
        );

        let mismatching = RawSpawnRequest {
            authorization: SpawnAuthorization::Captured(
                application_with_mode("danger-full-access").4,
            ),
            family: SpawnFamily::Browser,
            audience: audience(SpawnFamily::Browser),
            argv: vec![OsString::from("browser")],
            cwd: std::env::current_dir().expect("current directory"),
            requested_env: BTreeMap::new(),
            requested_grants: BTreeSet::new(),
            stdio: StdioMode::Piped,
            terminal_size: None,
            timeout_ms: None,
            target_fd_mappings: vec![canonical],
            owned_target_fd_mappings: vec![
                TargetFdMapping::new(
                    File::open("/dev/null").expect("open null").into(),
                    TargetFd::new(3).expect("target fd"),
                    TargetFdDirection::ParentReads,
                )
                .expect("owned mapping"),
            ],
            #[cfg(unix)]
            mount_sources: Vec::new(),
            #[cfg(unix)]
            retained_executable: None,
            #[cfg(unix)]
            retained_working_directory: None,
        };
        assert!(matches!(
            mismatching.canonical_call(),
            Err(SpawnAuthorizationError::TargetFdMismatch),
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_rejects_nonempty_canonical_target_fds() {
        let raw = RawSpawnRequest {
            authorization: SpawnAuthorization::Captured(
                application_with_mode("danger-full-access").4,
            ),
            family: SpawnFamily::ManagedPipeProcess,
            audience: audience(SpawnFamily::ManagedPipeProcess),
            argv: vec![OsString::from("tool")],
            cwd: std::env::current_dir().expect("current directory"),
            requested_env: BTreeMap::new(),
            requested_grants: BTreeSet::new(),
            stdio: StdioMode::Piped,
            terminal_size: None,
            timeout_ms: None,
            target_fd_mappings: vec![
                CanonicalTargetFdMapping::new(3, TargetFdDirection::ParentReads).expect("mapping"),
            ],
        };
        assert!(matches!(
            raw.canonical_call(),
            Err(SpawnAuthorizationError::TargetFdMismatch),
        ));
    }

    #[test]
    fn authorization_errors_are_complete_and_redacted() {
        let errors = [
            SpawnAuthorizationError::UnknownOrConsumed,
            SpawnAuthorizationError::Expired,
            SpawnAuthorizationError::FamilyMismatch,
            SpawnAuthorizationError::AudienceMismatch,
            SpawnAuthorizationError::DigestMismatch,
            SpawnAuthorizationError::RetainedExecutableMismatch,
            SpawnAuthorizationError::TargetFdMismatch,
            SpawnAuthorizationError::MountSourceMismatch,
            SpawnAuthorizationError::ClockUnavailable,
        ];
        for error in errors {
            assert!(!error.to_string().contains("argv"));
            assert!(!format!("{error:?}").contains("secret"));
        }
    }
}
