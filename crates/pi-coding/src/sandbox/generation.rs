use std::path::PathBuf;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use sha2::Digest as _;

use pi_sandbox::{
    AbsolutePath, CredentialGrantId, CredentialPolicy, FileSystemPolicy, HostHttpDestination,
    HostHttpHost, HostHttpPolicy, HostPlatform, NetworkPolicy, PlatformFactInputs, PolicyDigest,
    PolicyRevision, PolicySnapshot, RequestedPolicy, ResourceLimits, SandboxBackend, SandboxError,
    SandboxMode, SandboxPolicy, SharedPolicySnapshot, WritableRoot, derive_platform_facts,
    probe_backend, resolve_policy_snapshot,
};
use serde::{Deserialize, Deserializer, Serialize, de};

use crate::application::generation::{
    ApplicationGeneration, DurableApplicationComponents, DurableGenerationLease,
    GenerationProvenance,
};
use crate::fs::{
    CapabilityAccess, CapabilityFs, CapabilityRevisionRoots, CapabilityRootPurpose, FileIdentity,
    FsAuthority, FsError, PolicyFs, PolicyScratchRoot, RootCreation, ScratchLease,
};
use crate::settings::{
    HostHttpDestinationWire, HostHttpHostWire, HostHttpPolicySettings, McpServerConfig,
    SandboxSettingsWire, WritableRootWire, resolved_sandbox_mode,
};
use crate::{RequestAuth, ResourceCandidate, WorkspaceRoots};

use super::grant_registry::{
    CredentialConfigError, CredentialGrantRegistry, CredentialGrantRegistrySnapshot,
};
use super::http::{
    BuiltinHttpEndpoints, RawSessionHttpTransport, SessionHttpAuthority, SessionHttpError,
    SessionHttpRequirements, SessionHttpTransport, SystemDnsResolver,
};

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SandboxConfigError {
    #[error("invalid policy configuration: {0}")]
    Policy(#[from] pi_sandbox::PolicyValidationError),
    #[error("conflicting sandbox settings: {0}")]
    ConflictingSandbox(String),
    #[error("invalid credential configuration: {0}")]
    Credential(#[from] CredentialConfigError),
}

impl SandboxConfigError {
    /// Renders a low-level [`PolicyValidationError`] into an actionable
    /// settings-level diagnostic.
    ///
    /// `danger-full-access` combined with any confined field, and overlapping
    /// read-only/writable roots, are both invalid *settings* — the clean-cutover
    /// schema rejects them instead of silently widening the sandbox. The
    /// operator context (`rpi models`/`llama`/`plugin`/`package`) builds a
    /// governed generation from the user's settings too, so it must surface
    /// the same actionable rejection rather than a cryptic internal
    /// "conflicting policy fields" chain. Describing the offending *settings
    /// keys* (never the concrete values) keeps the diagnostic actionable while
    /// touching no real configuration.
    fn from_policy_validation(error: pi_sandbox::PolicyValidationError) -> Self {
        match error {
            pi_sandbox::PolicyValidationError::ConflictingFields { left, right } => {
                Self::ConflictingSandbox(sandbox_conflict_message(left, right))
            }
            other => Self::Policy(other),
        }
    }
}

fn sandbox_conflict_message(left: &'static str, right: &'static str) -> String {
    if left == "mode" {
        // `mode` conflicts arise only under danger-full-access, where
        // every confined field is meaningless and the schema rejects it.
        format!(
            "sandbox.mode danger-full-access cannot be combined with {right}; \
             remove {right} or choose sandbox.mode workspace-write/read-only",
            right = sandbox_settings_key(right),
        )
    } else {
        format!(
            "{left} and {right} must not overlap (no root may be a prefix of another); \
             review the configured sandbox roots",
            left = sandbox_settings_key(left),
            right = sandbox_settings_key(right),
        )
    }
}

fn sandbox_settings_key(field: &'static str) -> String {
    // Map the pi-sandbox policy field names to their settings.json keys so the
    // diagnostic points at the actual configuration to fix.
    format!("sandbox.{field}")
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxGenerationError {
    #[error("sandbox configuration is invalid: {0}")]
    Config(#[from] SandboxConfigError),
    #[error("sandbox capability filesystem failed: {0}")]
    Filesystem(#[from] FsError),
    #[error("sandbox runtime staging failed: {0}")]
    Runtime(#[from] SandboxError),
    #[error("sandbox HTTP staging failed: {0}")]
    Http(#[from] SessionHttpError),
    #[error("sandbox aggregate staging failed: {0}")]
    Aggregate(#[source] anyhow::Error),
}

impl From<pi_sandbox::PolicyValidationError> for SandboxGenerationError {
    fn from(error: pi_sandbox::PolicyValidationError) -> Self {
        Self::Config(SandboxConfigError::Policy(error))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DurableStateValidationError {
    #[error("durable policy snapshot is invalid: {0}")]
    Policy(#[from] pi_sandbox::PolicyValidationError),
    #[error("durable credential registry is invalid: {0}")]
    Credential(#[from] CredentialConfigError),
}

#[derive(Debug, thiserror::Error)]
pub enum DurableResumeError {
    #[error("durable orchestration policy does not match resumed generation")]
    PolicyMismatch,
    #[error("durable state validation failed: {0}")]
    State(#[from] DurableStateValidationError),
    #[error("durable filesystem revalidation failed: {0}")]
    Filesystem(#[from] FsError),
    #[error("durable sandbox representation or backend probe failed: {0}")]
    Sandbox(#[from] SandboxError),
    #[error("durable HTTP authority staging failed: {0}")]
    Http(#[from] SessionHttpError),
    #[error("durable aggregate generation construction failed: {0}")]
    Generation(#[from] SandboxGenerationError),
}

impl DurableResumeError {
    #[must_use]
    pub fn find_in_chain(error: &anyhow::Error) -> Option<&Self> {
        error.chain().find_map(|cause| cause.downcast_ref::<Self>())
    }

    #[must_use]
    pub fn degrade_notice(error: &anyhow::Error) -> String {
        format!(
            "Orchestration unbound: durable sidecar does not match current policy ({}). Session resumed without child jobs.",
            error.root_cause()
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DurablePolicyState {
    snapshot: pi_sandbox::PolicySnapshot,
    credential_grants: CredentialGrantRegistrySnapshot,
}

impl DurablePolicyState {
    pub fn new(
        snapshot: pi_sandbox::PolicySnapshot,
        credential_grants: CredentialGrantRegistrySnapshot,
    ) -> Result<Self, DurableStateValidationError> {
        snapshot.validate_digest()?;
        let registry = CredentialGrantRegistry::from_snapshot(credential_grants)?;
        Ok(Self {
            snapshot,
            credential_grants: registry.snapshot().clone(),
        })
    }

    pub(crate) fn capture(
        generation: &ApplicationGeneration,
    ) -> Result<Self, DurableStateValidationError> {
        Self::new(
            generation.sandbox().policy().as_ref().clone(),
            generation.sandbox().credential_grants().snapshot().clone(),
        )
    }

    pub fn snapshot(&self) -> &pi_sandbox::PolicySnapshot {
        &self.snapshot
    }

    pub fn credential_grants(&self) -> &CredentialGrantRegistrySnapshot {
        &self.credential_grants
    }
}

/// Same sandbox authority after a generation reload. The new snapshot carries
/// the next [`pi_sandbox::PolicyRevision`] and digest; the policy body does not change.
pub fn durable_policy_same_authority(
    persisted: &DurablePolicyState,
    captured: &DurablePolicyState,
) -> bool {
    persisted.credential_grants() == captured.credential_grants()
        && persisted.snapshot().created_on() == captured.snapshot().created_on()
        && persisted.snapshot().policy() == captured.snapshot().policy()
}

pub fn durable_policy_is_protected_subset_upgrade(
    persisted: &DurablePolicyState,
    captured: &DurablePolicyState,
) -> bool {
    if persisted.credential_grants() != captured.credential_grants() {
        return false;
    }
    let persisted_snapshot = persisted.snapshot();
    let captured_snapshot = captured.snapshot();
    if persisted_snapshot.revision() != captured_snapshot.revision()
        || persisted_snapshot.created_on() != captured_snapshot.created_on()
        || persisted_snapshot.policy().mode() != captured_snapshot.policy().mode()
        || persisted_snapshot.policy().network() != captured_snapshot.policy().network()
        || persisted_snapshot.policy().host_http() != captured_snapshot.policy().host_http()
        || persisted_snapshot.policy().credentials() != captured_snapshot.policy().credentials()
        || persisted_snapshot.policy().limits() != captured_snapshot.policy().limits()
    {
        return false;
    }
    let (Some(persisted_fs), Some(captured_fs)) = (
        persisted_snapshot.policy().filesystem(),
        captured_snapshot.policy().filesystem(),
    ) else {
        return false;
    };
    if persisted_fs.sealed_scratch_root() != captured_fs.sealed_scratch_root()
        || persisted_fs.writable_roots().len() != captured_fs.writable_roots().len()
        || !persisted_paths_are_compatible(
            persisted_fs.read_only_roots(),
            captured_fs.read_only_roots(),
        )
        || !persisted_paths_are_compatible(persisted_fs.denied_paths(), captured_fs.denied_paths())
    {
        return false;
    }
    persisted_fs
        .writable_roots()
        .iter()
        .zip(captured_fs.writable_roots())
        .all(|(persisted_root, captured_root)| {
            persisted_root.root() == captured_root.root()
                && persisted_protected_is_compatible(
                    persisted_root.protected_descendants(),
                    captured_root.protected_descendants(),
                )
        })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DurablePolicyStateWire {
    snapshot: pi_sandbox::PolicySnapshot,
    credential_grants: CredentialGrantRegistrySnapshot,
}

impl<'de> Deserialize<'de> for DurablePolicyState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = DurablePolicyStateWire::deserialize(deserializer)?;
        Self::new(wire.snapshot, wire.credential_grants).map_err(de::Error::custom)
    }
}

pub(crate) struct SessionPolicyInputs {
    scope: pi_sandbox::SessionScope,
    capability_identities: Box<[FileIdentity]>,
}

pub(crate) type DurableComponentStager<'a> = Box<
    dyn FnOnce(
            Arc<GenerationProvenance>,
            Arc<crate::ResourceSnapshot>,
        ) -> BoxFuture<'a, anyhow::Result<DurableApplicationComponents>>
        + Send
        + 'a,
>;
pub(crate) struct DurableResumeStaging<'a> {
    lease: DurableGenerationLease,
    resource: ResourceCandidate,
    session: SessionPolicyInputs,
    http_requirements: SessionHttpRequirements,
    stage_components: DurableComponentStager<'a>,
    brush_exec_lock: Arc<tokio::sync::Mutex<()>>,
}

impl<'a> DurableResumeStaging<'a> {
    pub(crate) fn new(
        lease: DurableGenerationLease,
        resource: ResourceCandidate,
        session: SessionPolicyInputs,
        http_requirements: SessionHttpRequirements,
        stage_components: DurableComponentStager<'a>,
        brush_exec_lock: Arc<tokio::sync::Mutex<()>>,
    ) -> Self {
        Self {
            lease,
            resource,
            session,
            http_requirements,
            stage_components,
            brush_exec_lock,
        }
    }
}

impl SessionPolicyInputs {
    pub(crate) fn validate_with_capabilities(
        session_id: String,
        workspace_roots: WorkspaceRoots,
        agent_dir: PathBuf,
        session_dir: PathBuf,
        capabilities: &Arc<CapabilityFs>,
    ) -> Result<Self, FsError> {
        let workspace_paths = workspace_roots
            .roots()
            .iter()
            .cloned()
            .map(AbsolutePath::parse)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| FsError::InvalidPath)?;
        let agent_dir = AbsolutePath::parse(agent_dir).map_err(|_| FsError::InvalidPath)?;
        let session_dir = AbsolutePath::parse(session_dir).map_err(|_| FsError::InvalidPath)?;
        let scope =
            pi_sandbox::SessionScope::new(session_id, workspace_paths, agent_dir, session_dir)
                .map_err(|_| FsError::InvalidPath)?;
        let capability_identities = open_session_capability_identities(
            &scope,
            capabilities,
            Some(&workspace_roots),
        )?;
        Ok(Self {
            scope,
            capability_identities,
        })
    }

    fn revalidate_with_capabilities(
        &self,
        capabilities: &Arc<CapabilityFs>,
    ) -> Result<(), FsError> {
        let current = open_session_capability_identities(&self.scope, capabilities, None)?;
        if current.as_ref() == self.capability_identities.as_ref() {
            Ok(())
        } else {
            Err(FsError::IdentityChanged)
        }
    }

    pub(crate) fn session_scope(&self) -> &pi_sandbox::SessionScope {
        &self.scope
    }

    pub(crate) fn capability_identities(&self) -> &[FileIdentity] {
        &self.capability_identities
    }
}

fn open_session_capability_identities(
    scope: &pi_sandbox::SessionScope,
    capabilities: &Arc<CapabilityFs>,
    workspace: Option<&WorkspaceRoots>,
) -> Result<Box<[FileIdentity]>, FsError> {
    let mut identities = Vec::with_capacity(scope.workspace_roots().len() + 2);
    for (index, root) in scope.workspace_roots().iter().enumerate() {
        let identity = match workspace
            .and_then(|workspace| workspace.authority(index))
            .filter(|lease| lease.belongs_to(capabilities))
        {
            Some(lease) => {
                if lease.display_root() != root.as_path() {
                    return Err(FsError::IdentityChanged);
                }
                lease.identity()
            }
            None => capabilities
                .open_root(
                    CapabilityRootPurpose::ProjectResources,
                    root.clone(),
                    CapabilityAccess::ReadOnly,
                    RootCreation::Existing,
                )?
                .identity(),
        };
        identities.push(identity);
    }
    let agent_lease = capabilities.open_read_directory_allowing_link(
        CapabilityRootPurpose::AgentStore,
        scope.agent_dir().clone(),
    )?;
    if agent_lease.display_root() != scope.agent_dir().as_path() {
        return Err(FsError::IdentityChanged);
    }
    identities.push(agent_lease.identity());
    let session_lease = capabilities.open_root(
        CapabilityRootPurpose::SessionStore,
        scope.session_dir().clone(),
        CapabilityAccess::PrivateReadWrite,
        RootCreation::CreateDirectory,
    )?;
    if session_lease.display_root() != scope.session_dir().as_path() {
        return Err(FsError::IdentityChanged);
    }
    identities.push(session_lease.identity());
    Ok(identities.into_boxed_slice())
}

pub(crate) type SandboxEnvironmentFacts = pi_sandbox::PlatformFacts;

pub(crate) struct SandboxGeneration {
    provenance: Arc<GenerationProvenance>,
    policy: SharedPolicySnapshot,
    backend: SandboxBackend,
    environment_facts: SandboxEnvironmentFacts,
    revision_home: AbsolutePath,
    revision_tmp: AbsolutePath,
    revision_scratch: AbsolutePath,
    credential_grants: Arc<CredentialGrantRegistry>,
    scratch: ScratchLease,
    fs: FsAuthority,
    capabilities: Arc<CapabilityFs>,
    http: SessionHttpAuthority,
}

impl SandboxGeneration {
    pub(crate) fn provenance(&self) -> &Arc<GenerationProvenance> {
        &self.provenance
    }

    pub(crate) fn policy(&self) -> &SharedPolicySnapshot {
        &self.policy
    }

    pub(crate) fn policy_snapshot(&self) -> &SharedPolicySnapshot {
        &self.policy
    }

    pub(crate) fn revision(&self) -> PolicyRevision {
        self.policy.revision()
    }

    pub(crate) fn policy_digest(&self) -> PolicyDigest {
        self.policy.digest()
    }

    pub(crate) fn backend(&self) -> &SandboxBackend {
        &self.backend
    }

    pub(crate) fn environment_facts(&self) -> &SandboxEnvironmentFacts {
        &self.environment_facts
    }

    pub(crate) fn revision_home(&self) -> &AbsolutePath {
        &self.revision_home
    }

    pub(crate) fn revision_tmp(&self) -> &AbsolutePath {
        &self.revision_tmp
    }

    pub(crate) fn revision_scratch(&self) -> &AbsolutePath {
        &self.revision_scratch
    }

    pub(crate) fn credential_grants(&self) -> &Arc<CredentialGrantRegistry> {
        &self.credential_grants
    }

    pub(crate) fn scratch(&self) -> &ScratchLease {
        &self.scratch
    }

    pub(crate) fn capabilities(&self) -> &Arc<CapabilityFs> {
        &self.capabilities
    }
    pub(crate) fn fs(&self) -> &FsAuthority {
        &self.fs
    }

    pub(crate) fn http(&self) -> &SessionHttpAuthority {
        &self.http
    }
}

struct SandboxGenerationBuilder<'a> {
    settings: Option<&'a SandboxSettingsWire>,
    mcp_servers: &'a [McpServerConfig],
    revision: PolicyRevision,
    provenance: Arc<GenerationProvenance>,
    resource_capabilities: &'a Arc<CapabilityFs>,
    session: SessionPolicyInputs,
    selected_model: &'a pi_ai::Model,
    request_auth: &'a RequestAuth,
    stream: &'a pi_ai::StreamOptions,
    image_generation: Option<(
        &'a pi_ai::Model,
        Option<&'a str>,
        &'a std::collections::HashMap<String, String>,
    )>,
    builtins: &'a BuiltinHttpEndpoints,
}
pub(crate) fn build_sandbox_generation(
    settings: Option<&SandboxSettingsWire>,
    mcp_servers: &[McpServerConfig],
    revision: PolicyRevision,
    provenance: Arc<GenerationProvenance>,
    resource_capabilities: &Arc<CapabilityFs>,
    session: SessionPolicyInputs,
    selected_model: &pi_ai::Model,
    request_auth: &RequestAuth,
    stream: &pi_ai::StreamOptions,
    image_generation: Option<(
        &pi_ai::Model,
        Option<&str>,
        &std::collections::HashMap<String, String>,
    )>,
    builtins: &BuiltinHttpEndpoints,
) -> Result<Arc<SandboxGeneration>, SandboxGenerationError> {
    SandboxGenerationBuilder {
        settings,
        mcp_servers,
        revision,
        provenance,
        resource_capabilities,
        session,
        selected_model,
        request_auth,
        stream,
        image_generation,
        builtins,
    }
    .build()
}

impl SandboxGenerationBuilder<'_> {
    fn build(self) -> Result<Arc<SandboxGeneration>, SandboxGenerationError> {
        if self.provenance.revision().get() != self.revision.get() {
            return Err(SandboxConfigError::Policy(
                pi_sandbox::PolicyValidationError::InvalidField {
                    field: "revision",
                    reason: "policy and application revisions must match",
                },
            )
            .into());
        }
        let mode = resolved_sandbox_mode(self.settings);
        let network = self
            .settings
            .and_then(|settings| settings.network)
            .unwrap_or_else(|| {
                if mode == SandboxMode::DangerFullAccess {
                    NetworkPolicy::FullAccess
                } else {
                    NetworkPolicy::None
                }
            });
        let registry = Arc::new(
            CredentialGrantRegistry::build(
                self.settings
                    .map_or(&[][..], |settings| &settings.credential_grant_definitions),
                self.mcp_servers,
            )
            .map_err(SandboxConfigError::from)?,
        );
        let credentials = CredentialPolicy::new(
            self.settings
                .map_or(&[][..], |settings| &settings.credential_grants)
                .iter()
                .cloned()
                .map(CredentialGrantId::parse)
                .collect::<Result<Vec<_>, _>>()?,
        )?;
        registry
            .validate_policy(&credentials)
            .map_err(SandboxConfigError::from)?;
        let requirements = SessionHttpRequirements::derive_with_builtins(
            self.selected_model,
            self.request_auth,
            self.stream,
            self.image_generation,
            self.builtins,
        )?;
        let host_http = if mode == SandboxMode::DangerFullAccess {
            // Danger mode is raw ambient host authority; no confined HTTP
            // destination policy applies (RequestedPolicy::new rejects danger
            // combined with any confined field, and the danger HTTP authority
            // is built unconfined below).
            HostHttpPolicy::new(Vec::new())?
        } else {
            super::http::resolve_session_http_policy(
                self.settings
                    .and_then(|settings| settings.host_http.as_ref()),
                &requirements,
            )?
        };
        let requested = RequestedPolicy::new(
            mode,
            network,
            parse_absolute_roots(
                self.settings
                    .map_or(&[][..], |settings| &settings.read_only_roots),
            )?,
            parse_writable_roots(
                self.settings
                    .map_or(&[][..], |settings| &settings.writable_roots),
            )?,
            parse_absolute_roots(
                self.settings
                    .map_or(&[][..], |settings| &settings.denied_paths),
            )?,
            host_http,
            credentials,
            resolve_limits(self.settings.and_then(|settings| settings.limits.as_ref()))?,
        )
        .map_err(SandboxConfigError::from_policy_validation)?;
        let inputs = PlatformFactInputs::new(
            self.session.session_scope().session_id().to_owned(),
            self.session.session_scope().workspace_roots().to_vec(),
            self.session.session_scope().agent_dir().clone(),
            self.session.session_scope().session_dir().clone(),
        )?;
        let facts = derive_platform_facts(inputs)?;
        let environment_facts = facts.clone();
        let revision_root = revision_root(self.session.session_scope(), self.revision)?;
        let revision_home = AbsolutePath::parse(revision_root.as_path().join("home"))?;
        let revision_tmp = AbsolutePath::parse(revision_root.as_path().join("tmp"))?;
        let revision_scratch = AbsolutePath::parse(revision_root.as_path().join("scratch"))?;
        let revision_roots = CapabilityRevisionRoots::acquire(
            self.resource_capabilities,
            revision_home.clone(),
            revision_tmp.clone(),
            revision_scratch.clone(),
        )?;
        let scratch_mount =
            if mode != SandboxMode::DangerFullAccess && facts.platform() == HostPlatform::Linux {
                AbsolutePath::parse(PathBuf::from("/run/pi/scratch"))?
            } else {
                revision_scratch.clone()
            };
        let scratch_root = PolicyScratchRoot::from_lease(
            self.revision,
            revision_roots.scratch_lease(),
            scratch_mount,
        )?;
        let policy = Arc::new(
            resolve_policy_snapshot(
                self.revision,
                facts.platform(),
                self.session.session_scope(),
                requested,
                &facts,
                revision_home.clone(),
                revision_tmp.clone(),
                revision_scratch.clone(),
                home_private_roots(&facts, self.session.session_scope(), &revision_scratch),
            )
            .map_err(SandboxConfigError::from_policy_validation)?,
        );
        let backend = if mode == SandboxMode::DangerFullAccess {
            probe_backend(facts, policy.clone())?
        } else {
            #[cfg(unix)]
            {
                pi_sandbox::probe_backend_with_revision_roots(
                    facts,
                    policy.clone(),
                    revision_roots.into_retained(self.resource_capabilities)?,
                )?
            }
            #[cfg(not(unix))]
            {
                probe_backend(facts, policy.clone())?
            }
        };
        let policy_fs = PolicyFs::new(
            policy.clone(),
            self.session.session_scope().workspace_roots()[0].clone(),
            self.resource_capabilities.clone(),
            scratch_root.clone(),
        )?;
        let (scratch, fs) = if mode == SandboxMode::DangerFullAccess {
            (
                ScratchLease::DangerAmbient(scratch_root.clone()),
                FsAuthority::DangerAmbient(policy_fs),
            )
        } else {
            (
                ScratchLease::Confined(scratch_root),
                FsAuthority::Confined(policy_fs),
            )
        };
        let http = if mode == SandboxMode::DangerFullAccess {
            SessionHttpAuthority::ExplicitDanger(RawSessionHttpTransport::new_explicit_danger()?)
        } else {
            let resolver = SystemDnsResolver::new()?;
            SessionHttpAuthority::Confined(SessionHttpTransport::new(policy.clone(), resolver)?)
        };
        Ok(Arc::new(SandboxGeneration {
            provenance: self.provenance,
            policy,
            backend,
            environment_facts,
            revision_home,
            revision_tmp,
            revision_scratch,
            credential_grants: registry,
            scratch,
            fs,
            http,
            capabilities: self.resource_capabilities.clone(),
        }))
    }
}

fn parse_absolute_roots(paths: &[PathBuf]) -> Result<Vec<AbsolutePath>, SandboxConfigError> {
    paths
        .iter()
        .cloned()
        .map(AbsolutePath::parse)
        .collect::<Result<Vec<_>, _>>()
        .map_err(SandboxConfigError::from)
}

/// The retained agent-store bases that a confined workspace must never read.
/// Covers the default `HOME/.rpi` store and the active display root; sibling
/// profiles stay sealed because the profile root nests beneath the default
/// store base. The active display root is omitted when generated writable
/// authority already lives under it (a session relocated out of a workspace
/// into `agent_dir/sandbox`): a private parent cannot cover that writable.
fn home_private_roots(
    facts: &pi_sandbox::PlatformFacts,
    scope: &pi_sandbox::SessionScope,
    revision_scratch: &AbsolutePath,
) -> Vec<AbsolutePath> {
    let mut roots = Vec::new();
    if let Some(home) = facts.account_home() {
        let default_store = home.as_path().join(".rpi");
        if default_store != scope.agent_dir().as_path() {
            if let Ok(store) = AbsolutePath::parse(default_store) {
                roots.push(store);
            }
        }
    }
    if !private_store_covers_generated_write(scope, revision_scratch) {
        roots.push(scope.agent_dir().clone());
    }
    roots
}

fn private_store_covers_generated_write(
    scope: &pi_sandbox::SessionScope,
    revision_scratch: &AbsolutePath,
) -> bool {
    let agent = scope.agent_dir().as_path();
    let workspace_covers_scratch = scope
        .workspace_roots()
        .iter()
        .any(|root| revision_scratch.as_path().starts_with(root.as_path()));
    scope
        .workspace_roots()
        .iter()
        .any(|root| root.as_path().starts_with(agent))
        || (revision_scratch.as_path().starts_with(agent) && !workspace_covers_scratch)
}

fn parse_writable_roots(
    roots: &[WritableRootWire],
) -> Result<Vec<WritableRoot>, SandboxConfigError> {
    roots
        .iter()
        .map(|root| {
            let configured = AbsolutePath::parse(root.root.clone())?;
            let protected = root
                .protected_descendants
                .iter()
                .cloned()
                .map(pi_sandbox::RelativePath::parse)
                .collect::<Result<Vec<_>, _>>()?;
            WritableRoot::new(configured, protected)
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(SandboxConfigError::from)
}

fn resolve_limits(
    limits: Option<&crate::ResourceLimitSettingsWire>,
) -> Result<ResourceLimits, SandboxConfigError> {
    let defaults = ResourceLimits::confined_defaults();
    let Some(limits) = limits else {
        return Ok(defaults);
    };
    ResourceLimits::new(
        limits
            .address_space_bytes
            .unwrap_or(defaults.address_space_bytes()),
        limits.cpu_seconds.unwrap_or(defaults.cpu_seconds()),
        limits.file_size_bytes.unwrap_or(defaults.file_size_bytes()),
        limits.open_files.unwrap_or(defaults.open_files()),
    )
    .map_err(SandboxConfigError::from)
}

fn revision_root(
    scope: &pi_sandbox::SessionScope,
    revision: PolicyRevision,
) -> Result<AbsolutePath, SandboxConfigError> {
    let base = if scope
        .workspace_roots()
        .iter()
        .any(|root| scope.session_dir().as_path().starts_with(root.as_path()))
    {
        let digest = sha2::Sha256::digest(scope.session_id().as_bytes());
        let mut namespace = String::with_capacity(32);
        for byte in digest.iter().take(16) {
            use std::fmt::Write as _;
            let _ = write!(namespace, "{byte:02x}");
        }
        scope.agent_dir().as_path().join("sandbox").join(namespace)
    } else {
        scope.session_dir().as_path().join("sandbox")
    };
    AbsolutePath::parse(base.join(revision.get().to_string())).map_err(SandboxConfigError::from)
}

pub(crate) async fn resume_durable_generation(
    state: &DurablePolicyState,
    staging: DurableResumeStaging<'_>,
) -> Result<Arc<ApplicationGeneration>, DurableResumeError> {
    let validated =
        DurablePolicyState::new(state.snapshot.clone(), state.credential_grants.clone())?;
    let DurableResumeStaging {
        lease,
        resource,
        session,
        http_requirements,
        stage_components,
        brush_exec_lock,
    } = staging;
    let snapshot = Arc::new(validated.snapshot);
    if lease.provenance().revision().get() != snapshot.revision().get() {
        return Err(invalid_durable_state(
            "revision",
            "policy and application revisions must match",
        ));
    }
    session.revalidate_with_capabilities(resource.capabilities())?;
    let facts = derive_platform_facts(platform_fact_inputs(session.session_scope())?)?;
    let revision_root = revision_root(session.session_scope(), snapshot.revision())
        .map_err(SandboxGenerationError::from)?;
    let revision_home = durable_revision_path(&revision_root, "home")?;
    let revision_tmp = durable_revision_path(&revision_root, "tmp")?;
    let revision_scratch = durable_revision_path(&revision_root, "scratch")?;
    let snapshot = Arc::new(rewrite_required_session_roots(
        &snapshot,
        session.session_scope(),
        &facts,
        &revision_scratch,
    )?);
    let snapshot = Arc::new(rewrite_required_protected_descendants(
        &snapshot,
        session.session_scope(),
        &facts,
    )?);
    validate_current_session_policy(
        &snapshot,
        session.session_scope(),
        &facts,
        &revision_scratch,
    )?;
    validate_revision_scratch(&snapshot, &revision_scratch)?;
    let revision_roots = CapabilityRevisionRoots::acquire(
        resource.capabilities(),
        revision_home.clone(),
        revision_tmp.clone(),
        revision_scratch.clone(),
    )?;
    let scratch_mount = if snapshot.policy().mode() != SandboxMode::DangerFullAccess
        && facts.platform() == HostPlatform::Linux
    {
        AbsolutePath::parse(PathBuf::from("/run/pi/scratch"))
            .map_err(DurableStateValidationError::from)?
    } else {
        revision_scratch.clone()
    };
    let scratch_root = PolicyScratchRoot::from_lease(
        snapshot.revision(),
        revision_roots.scratch_lease(),
        scratch_mount,
    )?;
    let registry = Arc::new(
        CredentialGrantRegistry::from_snapshot(validated.credential_grants)
            .map_err(DurableStateValidationError::from)?,
    );
    registry
        .validate_policy(snapshot.policy().credentials())
        .map_err(DurableStateValidationError::from)?;
    let backend = if snapshot.policy().mode() == SandboxMode::DangerFullAccess {
        probe_backend(facts.clone(), snapshot.clone())?
    } else {
        #[cfg(unix)]
        {
            pi_sandbox::probe_backend_with_revision_roots(
                facts.clone(),
                snapshot.clone(),
                revision_roots.into_retained(resource.capabilities())?,
            )?
        }
        #[cfg(not(unix))]
        {
            probe_backend(facts.clone(), snapshot.clone())?
        }
    };
    let policy_fs = PolicyFs::new(
        snapshot.clone(),
        session.session_scope().workspace_roots()[0].clone(),
        resource.capabilities().clone(),
        scratch_root.clone(),
    )?;
    let (scratch, fs) = if snapshot.policy().mode() == SandboxMode::DangerFullAccess {
        (
            ScratchLease::DangerAmbient(scratch_root.clone()),
            FsAuthority::DangerAmbient(policy_fs),
        )
    } else {
        (
            ScratchLease::Confined(scratch_root),
            FsAuthority::Confined(policy_fs),
        )
    };
    let http = stage_durable_http(&snapshot, &http_requirements)?;
    let sandbox = Arc::new(SandboxGeneration {
        provenance: lease.provenance().clone(),
        policy: snapshot,
        backend,
        environment_facts: facts,
        revision_home,
        revision_tmp,
        revision_scratch,
        credential_grants: registry,
        scratch,
        fs,
        capabilities: resource.capabilities().clone(),
        http,
    });
    let resource = resource.finish(sandbox).map_err(aggregate_resume_error)?;
    let components = stage_components(lease.provenance().clone(), resource.clone())
        .await
        .map_err(aggregate_resume_error)?;
    lease
        .finish(resource, components, brush_exec_lock)
        .map_err(aggregate_resume_error)
}

fn aggregate_resume_error(error: anyhow::Error) -> DurableResumeError {
    DurableResumeError::Generation(SandboxGenerationError::Aggregate(error))
}

fn platform_fact_inputs(
    scope: &pi_sandbox::SessionScope,
) -> Result<PlatformFactInputs, DurableStateValidationError> {
    PlatformFactInputs::new(
        scope.session_id().to_owned(),
        scope.workspace_roots().to_vec(),
        scope.agent_dir().clone(),
        scope.session_dir().clone(),
    )
    .map_err(DurableStateValidationError::from)
}

fn persisted_protected_is_compatible(
    persisted: &[pi_sandbox::RelativePath],
    required: &[pi_sandbox::RelativePath],
) -> bool {
    persisted
        .iter()
        .all(|path| required.iter().any(|wanted| wanted == path))
}

fn persisted_paths_are_compatible(persisted: &[AbsolutePath], required: &[AbsolutePath]) -> bool {
    persisted
        .iter()
        .all(|path| required.iter().any(|wanted| wanted == path))
}

fn rewrite_required_session_roots(
    snapshot: &PolicySnapshot,
    scope: &pi_sandbox::SessionScope,
    facts: &SandboxEnvironmentFacts,
    revision_scratch: &AbsolutePath,
) -> Result<PolicySnapshot, DurableResumeError> {
    let Some(filesystem) = snapshot.policy().filesystem() else {
        return Ok(snapshot.clone());
    };
    let mut read_only = filesystem.read_only_roots().to_vec();
    let mut denied = filesystem.denied_paths().to_vec();
    let mut changed = false;
    let agent_is_reconciled = pi_sandbox::agent_overlaps_generated_write(
        scope,
        revision_scratch,
        snapshot.policy().mode(),
    );
    if !agent_is_reconciled {
        changed |= push_unique_path(&mut read_only, scope.agent_dir());
    }
    for root in facts.policy_runtime_roots() {
        changed |= push_unique_path(&mut read_only, root);
    }
    changed |= push_unique_path(&mut denied, scope.session_dir());
    for path in facts.mandatory_denied_paths() {
        changed |= push_unique_path(&mut denied, path);
    }
    if !changed {
        return Ok(snapshot.clone());
    }
    rebuild_policy_filesystem(
        snapshot,
        read_only,
        filesystem.writable_roots().to_vec(),
        denied,
    )
}

fn push_unique_path(paths: &mut Vec<AbsolutePath>, path: &AbsolutePath) -> bool {
    if paths.iter().any(|existing| existing == path) {
        return false;
    }
    paths.push(path.clone());
    true
}

fn rebuild_policy_filesystem(
    snapshot: &PolicySnapshot,
    read_only: Vec<AbsolutePath>,
    writable: Vec<WritableRoot>,
    denied: Vec<AbsolutePath>,
) -> Result<PolicySnapshot, DurableResumeError> {
    let Some(filesystem) = snapshot.policy().filesystem() else {
        return Ok(snapshot.clone());
    };
    let filesystem = match filesystem.sealed_scratch_root() {
        Some(scratch) => FileSystemPolicy::resolved(read_only, writable, denied, scratch.clone()),
        None => FileSystemPolicy::new(read_only, writable, denied),
    }
    .map_err(DurableStateValidationError::from)?;
    let network = snapshot.policy().network().ok_or_else(|| {
        invalid_durable_state(
            "snapshot.policy.network",
            "confined policy is missing network",
        )
    })?;
    let host_http = snapshot.policy().host_http().cloned().ok_or_else(|| {
        invalid_durable_state(
            "snapshot.policy.hostHttp",
            "confined policy is missing host HTTP",
        )
    })?;
    let limits = snapshot.policy().limits().ok_or_else(|| {
        invalid_durable_state(
            "snapshot.policy.limits",
            "confined policy is missing limits",
        )
    })?;
    let credentials = snapshot.policy().credentials().clone();
    let policy = match snapshot.policy().mode() {
        SandboxMode::WorkspaceWrite => {
            SandboxPolicy::workspace_write(filesystem, network, host_http, credentials, limits)
        }
        SandboxMode::ReadOnly if filesystem.sealed_scratch_root().is_some() => {
            Ok(SandboxPolicy::ReadOnly {
                filesystem,
                network,
                host_http,
                credentials,
                limits,
            })
        }
        SandboxMode::ReadOnly => {
            SandboxPolicy::read_only(filesystem, network, host_http, credentials, limits)
        }
        SandboxMode::DangerFullAccess => {
            return Ok(snapshot.clone());
        }
    }
    .map_err(DurableStateValidationError::from)?;
    PolicySnapshot::new(snapshot.revision(), snapshot.created_on(), policy)
        .map_err(DurableStateValidationError::from)
        .map_err(DurableResumeError::from)
}

fn rewrite_required_protected_descendants(
    snapshot: &PolicySnapshot,
    scope: &pi_sandbox::SessionScope,
    facts: &SandboxEnvironmentFacts,
) -> Result<PolicySnapshot, DurableResumeError> {
    let Some(filesystem) = snapshot.policy().filesystem() else {
        return Ok(snapshot.clone());
    };
    if snapshot.policy().mode() != SandboxMode::WorkspaceWrite {
        return Ok(snapshot.clone());
    }
    let mut changed = false;
    let mut writable = Vec::with_capacity(filesystem.writable_roots().len());
    for persisted in filesystem.writable_roots() {
        let required = if scope
            .workspace_roots()
            .iter()
            .any(|workspace| workspace == persisted.root())
        {
            pi_sandbox::required_workspace_protected_descendants(scope, facts, persisted.root())
                .map_err(DurableStateValidationError::from)?
        } else {
            facts
                .mandatory_protected_roots()
                .iter()
                .find(|root| root.root() == persisted.root())
                .map(|root| root.protected_descendants().to_vec())
                .unwrap_or_else(|| persisted.protected_descendants().to_vec())
        };
        if persisted.protected_descendants() != required.as_slice() {
            changed = true;
        }
        writable.push(
            WritableRoot::new(persisted.root().clone(), required)
                .map_err(DurableStateValidationError::from)?,
        );
    }
    if !changed {
        return Ok(snapshot.clone());
    }
    rebuild_policy_filesystem(
        snapshot,
        filesystem.read_only_roots().to_vec(),
        writable,
        filesystem.denied_paths().to_vec(),
    )
}

fn validate_current_session_policy(
    snapshot: &pi_sandbox::PolicySnapshot,
    scope: &pi_sandbox::SessionScope,
    facts: &SandboxEnvironmentFacts,
    revision_scratch: &AbsolutePath,
) -> Result<(), DurableResumeError> {
    if snapshot.created_on() != facts.platform() {
        return Err(SandboxError::UnsupportedMode {
            platform: facts.platform(),
            mode: snapshot.policy().mode(),
        }
        .into());
    }
    let Some(filesystem) = snapshot.policy().filesystem() else {
        return Ok(());
    };
    let has_read_only = |path: &AbsolutePath| filesystem.read_only_roots().contains(path);
    let has_writable = |path: &AbsolutePath| {
        filesystem
            .writable_roots()
            .iter()
            .any(|root| root.root() == path)
    };
    let agent_is_reconciled = pi_sandbox::agent_overlaps_generated_write(
        scope,
        revision_scratch,
        snapshot.policy().mode(),
    );
    if (!agent_is_reconciled && !has_read_only(scope.agent_dir()))
        || !facts.policy_runtime_roots().iter().all(has_read_only)
        || !filesystem.denied_paths().contains(scope.session_dir())
        || !facts
            .mandatory_denied_paths()
            .iter()
            .all(|path| filesystem.denied_paths().contains(path))
    {
        return Err(invalid_durable_state(
            "snapshot.policy.filesystem",
            "current platform or session roots do not match persisted authority",
        ));
    }
    let workspace_matches = match snapshot.policy().mode() {
        SandboxMode::ReadOnly => scope.workspace_roots().iter().all(has_read_only),
        SandboxMode::WorkspaceWrite => scope.workspace_roots().iter().all(has_writable),
        SandboxMode::DangerFullAccess => true,
    };
    let protected_match = match snapshot.policy().mode() {
        SandboxMode::WorkspaceWrite => scope.workspace_roots().iter().all(|workspace| {
            let Ok(required) =
                pi_sandbox::required_workspace_protected_descendants(scope, facts, workspace)
            else {
                return false;
            };
            filesystem.writable_roots().iter().any(|persisted| {
                persisted.root() == workspace
                    && persisted_protected_is_compatible(
                        persisted.protected_descendants(),
                        required.as_slice(),
                    )
            })
        }),
        _ => facts.mandatory_protected_roots().iter().all(|required| {
            filesystem.writable_roots().iter().any(|persisted| {
                persisted.root() == required.root()
                    && persisted_protected_is_compatible(
                        persisted.protected_descendants(),
                        required.protected_descendants(),
                    )
            })
        }),
    };
    if workspace_matches && protected_match {
        Ok(())
    } else {
        Err(invalid_durable_state(
            "snapshot.policy.filesystem",
            "current workspace identities do not match persisted authority",
        ))
    }
}

fn validate_revision_scratch(
    snapshot: &pi_sandbox::PolicySnapshot,
    revision_scratch: &AbsolutePath,
) -> Result<(), DurableResumeError> {
    let Some(filesystem) = snapshot.policy().filesystem() else {
        return Ok(());
    };
    let writable = filesystem
        .writable_roots()
        .iter()
        .any(|root| root.root() == revision_scratch);
    let sealed = filesystem
        .sealed_scratch_root()
        .is_some_and(|root| root == revision_scratch);
    if writable || sealed {
        Ok(())
    } else {
        Err(invalid_durable_state(
            "snapshot.policy.filesystem.writableRoots",
            "revision scratch does not match persisted authority",
        ))
    }
}

fn stage_durable_http(
    snapshot: &SharedPolicySnapshot,
    requirements: &SessionHttpRequirements,
) -> Result<SessionHttpAuthority, SessionHttpError> {
    if snapshot.policy().mode() == SandboxMode::DangerFullAccess {
        return RawSessionHttpTransport::new_explicit_danger()
            .map(SessionHttpAuthority::ExplicitDanger);
    }
    let persisted = snapshot
        .policy()
        .host_http()
        .ok_or(SessionHttpError::InvalidRequest)?;
    let settings = HostHttpPolicySettings {
        destinations: persisted
            .destinations()
            .iter()
            .map(http_destination_wire)
            .collect(),
    };
    let validated = super::http::resolve_session_http_policy(Some(&settings), requirements)?;
    if &validated != persisted {
        return Err(SessionHttpError::InvalidRequest);
    }
    let resolver = SystemDnsResolver::new()?;
    SessionHttpTransport::new(snapshot.clone(), resolver).map(SessionHttpAuthority::Confined)
}

fn http_destination_wire(destination: &HostHttpDestination) -> HostHttpDestinationWire {
    let host = match destination.host() {
        HostHttpHost::Dns { name } => HostHttpHostWire::Dns {
            name: name.as_str().to_owned(),
        },
        HostHttpHost::Ip { address } => HostHttpHostWire::Ip { address: *address },
    };
    HostHttpDestinationWire {
        class: destination.class(),
        scheme: destination.scheme(),
        host,
        port: destination.port(),
        allowed_ip_scopes: destination.allowed_ip_scopes().to_vec(),
    }
}

fn durable_revision_path(
    revision_root: &AbsolutePath,
    child: &'static str,
) -> Result<AbsolutePath, DurableResumeError> {
    AbsolutePath::parse(revision_root.as_path().join(child))
        .map_err(DurableStateValidationError::from)
        .map_err(DurableResumeError::from)
}

fn invalid_durable_state(field: &'static str, reason: &'static str) -> DurableResumeError {
    DurableStateValidationError::Policy(pi_sandbox::PolicyValidationError::InvalidField {
        field,
        reason,
    })
    .into()
}

#[cfg(test)]
pub(crate) fn test_durable_policy_state() -> DurablePolicyState {
    let credentials = CredentialPolicy::new(Vec::new()).expect("empty credential policy");
    let policy = pi_sandbox::SandboxPolicy::danger_full_access(credentials).expect("danger policy");
    let snapshot = pi_sandbox::PolicySnapshot::new(
        PolicyRevision::new(1).expect("policy revision"),
        current_host_platform(),
        policy,
    )
    .expect("policy snapshot");
    let grants = CredentialGrantRegistrySnapshot::new(Vec::new(), Vec::new())
        .expect("empty credential registry");
    DurablePolicyState::new(snapshot, grants).expect("durable policy state")
}

#[cfg(test)]
const fn current_host_platform() -> HostPlatform {
    #[cfg(target_os = "linux")]
    {
        HostPlatform::Linux
    }
    #[cfg(target_os = "macos")]
    {
        HostPlatform::Macos
    }
    #[cfg(target_os = "windows")]
    {
        HostPlatform::Windows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Clean-cutover rejection: danger-full-access combined with any
    /// confined field is invalid, never silently widened by dropping the
    /// field. The diagnostic names the offending settings key and the fix.
    #[test]
    fn danger_mode_conflict_messages_are_actionable() {
        for (right, key) in [
            ("readOnlyRoots", "sandbox.readOnlyRoots"),
            ("writableRoots", "sandbox.writableRoots"),
            ("deniedPaths", "sandbox.deniedPaths"),
            ("hostHttp", "sandbox.hostHttp"),
            ("limits", "sandbox.limits"),
            ("network", "sandbox.network"),
        ] {
            let message = sandbox_conflict_message("mode", right);
            assert!(
                message.contains("danger-full-access"),
                "mode conflict must name the danger mode: {message}"
            );
            assert!(
                message.contains(key),
                "mode conflict must name the confined settings key {key}: {message}"
            );
            assert!(
                !message.contains("sandbox.sandbox"),
                "mode conflict must prefix each settings key exactly once: {message}"
            );
            assert!(
                message.contains("remove"),
                "mode conflict must tell the user what to change: {message}"
            );
        }
    }

    /// Overlapping read-only/writable roots (the operator-command failure seen
    /// in the field) are a settings contradiction that must be reported
    /// actionably and not silently resolved by dropping either side.
    #[test]
    fn overlapping_roots_conflict_is_actionable_and_never_silently_widened() {
        let message = sandbox_conflict_message("readOnlyRoots", "writableRoots");
        assert!(
            message.contains("sandbox.readOnlyRoots") && message.contains("sandbox.writableRoots"),
            "overlap message must name both settings keys: {message}"
        );
        assert!(
            !message.contains("sandbox.sandbox"),
            "overlap message must prefix each settings key exactly once: {message}"
        );
        assert!(
            message.contains("must not overlap"),
            "overlap message must explain the contradiction: {message}"
        );
        // Symmetric direction must produce the same actionable shape.
        let reversed = sandbox_conflict_message("writableRoots", "readOnlyRoots");
        assert_eq!(
            message.contains("must not overlap"),
            reversed.contains("must not overlap"),
            "overlap rejection must be direction-independent"
        );
    }

    #[test]
    fn home_private_agent_store_overlays_the_generated_read_only_grant() {
        let (_root, scope, facts, revision_scratch, _) = workspace_write_fixture();
        let revision = revision_root(&scope, PolicyRevision::new(1).expect("revision"))
            .expect("revision root");
        let revision_home = AbsolutePath::parse(revision.as_path().join("home")).expect("home");
        let revision_tmp = AbsolutePath::parse(revision.as_path().join("tmp")).expect("tmp");
        let requested = RequestedPolicy::new(
            SandboxMode::WorkspaceWrite,
            NetworkPolicy::None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HostHttpPolicy::new(Vec::new()).expect("http"),
            CredentialPolicy::new(Vec::new()).expect("credentials"),
            ResourceLimits::confined_defaults(),
        )
        .expect("requested policy");
        let snapshot = resolve_policy_snapshot(
            PolicyRevision::new(1).expect("revision"),
            facts.platform(),
            &scope,
            requested,
            &facts,
            revision_home,
            revision_tmp,
            revision_scratch.clone(),
            home_private_roots(&facts, &scope, &revision_scratch),
        )
        .expect("confined generation must overlay the private agent store");
        let filesystem = snapshot.policy().filesystem().expect("filesystem");
        let agent = scope.agent_dir();
        assert!(filesystem.private_roots().contains(agent));
        assert!(!filesystem.read_only_roots().contains(agent));
    }

    #[test]
    fn home_private_omits_agent_store_when_relocated_scratch_is_writable() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        let agent = root.path().join("agent");
        let session = workspace.join(".pi-test-sessions");
        for path in [&workspace, &agent, &session] {
            std::fs::create_dir_all(path).expect("fixture directory");
        }
        let workspace = AbsolutePath::parse(workspace).expect("workspace");
        let agent = AbsolutePath::parse(agent).expect("agent");
        let session = AbsolutePath::parse(session).expect("session");
        let scope = pi_sandbox::SessionScope::new(
            "workspace-session".to_owned(),
            vec![workspace.clone()],
            agent.clone(),
            session.clone(),
        )
        .expect("scope");
        let revision = revision_root(&scope, PolicyRevision::new(1).expect("revision"))
            .expect("revision root");
        let revision_home = AbsolutePath::parse(revision.as_path().join("home")).expect("home");
        let revision_tmp = AbsolutePath::parse(revision.as_path().join("tmp")).expect("tmp");
        let revision_scratch =
            AbsolutePath::parse(revision.as_path().join("scratch")).expect("scratch");
        for path in [&revision_home, &revision_tmp, &revision_scratch] {
            std::fs::create_dir_all(path.as_path()).expect("revision root");
        }
        assert!(
            revision_scratch.as_path().starts_with(agent.as_path()),
            "relocated scratch must live under the agent store"
        );
        let facts = derive_platform_facts(
            PlatformFactInputs::new(
                scope.session_id().to_owned(),
                scope.workspace_roots().to_vec(),
                scope.agent_dir().clone(),
                scope.session_dir().clone(),
            )
            .expect("inputs"),
        )
        .expect("facts");
        let requested = RequestedPolicy::new(
            SandboxMode::WorkspaceWrite,
            NetworkPolicy::None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HostHttpPolicy::new(Vec::new()).expect("http"),
            CredentialPolicy::new(Vec::new()).expect("credentials"),
            ResourceLimits::confined_defaults(),
        )
        .expect("requested policy");
        let snapshot = resolve_policy_snapshot(
            PolicyRevision::new(1).expect("revision"),
            facts.platform(),
            &scope,
            requested,
            &facts,
            revision_home,
            revision_tmp,
            revision_scratch.clone(),
            home_private_roots(&facts, &scope, &revision_scratch),
        )
        .expect("relocated scratch under the agent store must stay representable");
        let filesystem = snapshot.policy().filesystem().expect("filesystem");
        assert!(!filesystem.private_roots().contains(&agent));
        assert!(
            filesystem
                .writable_roots()
                .iter()
                .any(|root| root.root() == &revision_scratch)
        );
    }



    /// The low-level pi-sandbox conflict surfaces as a typed, actionable
    /// settings error prefixed with "conflicting" so downstream product
    /// assertions (and the end-to-end hostile-literal smoke) stay stable.
    #[test]
    fn conflicting_fields_are_retained_as_actionable_settings_errors() {
        let error = SandboxConfigError::from_policy_validation(
            pi_sandbox::PolicyValidationError::ConflictingFields {
                left: "mode",
                right: "deniedPaths",
            },
        );
        let rendered = format!("{error}");
        assert!(
            rendered.contains("conflicting"),
            "rendered diagnostic must keep the conflicting marker: {rendered}"
        );
        assert!(
            rendered.contains("deniedPaths"),
            "rendered diagnostic must name the conflicting field: {rendered}"
        );
        assert!(
            rendered.contains("remove"),
            "rendered diagnostic must be actionable: {rendered}"
        );
        // Non-conflicting policy errors pass through unchanged.
        let passthrough = SandboxConfigError::from_policy_validation(
            pi_sandbox::PolicyValidationError::InvalidField {
                field: "sessionId",
                reason: "must be nonempty and control-free",
            },
        );
        assert!(matches!(
            passthrough,
            SandboxConfigError::Policy(pi_sandbox::PolicyValidationError::InvalidField { .. })
        ));
    }

    #[test]
    fn workspace_write_durable_validation_accepts_reconciled_agent_store() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        let agent = root.path().join("agent");
        let session = agent.join("sessions/project");
        let revision = session.join("sandbox/1");
        for path in [&workspace, &agent, &session, &revision] {
            std::fs::create_dir_all(path).expect("fixture directory");
        }
        let workspace = AbsolutePath::parse(workspace).expect("workspace");
        let agent = AbsolutePath::parse(agent).expect("agent");
        let session = AbsolutePath::parse(session).expect("session");
        let revision_home = AbsolutePath::parse(revision.join("home")).expect("home");
        let revision_tmp = AbsolutePath::parse(revision.join("tmp")).expect("tmp");
        let revision_scratch = AbsolutePath::parse(revision.join("scratch")).expect("scratch");
        for path in [&revision_home, &revision_tmp, &revision_scratch] {
            std::fs::create_dir_all(path.as_path()).expect("revision root");
        }
        let scope = pi_sandbox::SessionScope::new(
            "durable-workspace".to_owned(),
            vec![workspace.clone()],
            agent.clone(),
            session,
        )
        .expect("scope");
        let facts = derive_platform_facts(
            PlatformFactInputs::new(
                scope.session_id().to_owned(),
                scope.workspace_roots().to_vec(),
                scope.agent_dir().clone(),
                scope.session_dir().clone(),
            )
            .expect("inputs"),
        )
        .expect("facts");
        let requested = RequestedPolicy::new(
            SandboxMode::WorkspaceWrite,
            NetworkPolicy::None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HostHttpPolicy::new(Vec::new()).expect("http"),
            CredentialPolicy::new(Vec::new()).expect("credentials"),
            ResourceLimits::confined_defaults(),
        )
        .expect("requested policy");
        let snapshot = resolve_policy_snapshot(
            PolicyRevision::new(1).expect("revision"),
            facts.platform(),
            &scope,
            requested,
            &facts,
            revision_home,
            revision_tmp,
            revision_scratch.clone(),
            Vec::new(),
        )
        .expect("snapshot");
        assert!(
            !snapshot
                .policy()
                .filesystem()
                .expect("filesystem")
                .read_only_roots()
                .contains(&agent)
        );
        validate_current_session_policy(&snapshot, &scope, &facts, &revision_scratch)
            .expect("reconciled durable validation");
    }

    #[test]
    fn workspace_write_durable_validation_accepts_configured_session_in_workspace() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        let agent = root.path().join("agent");
        let session = workspace.join(".sessions");
        for path in [&workspace, &agent, &session] {
            std::fs::create_dir_all(path).expect("fixture directory");
        }
        let workspace = AbsolutePath::parse(workspace).expect("workspace");
        let agent = AbsolutePath::parse(agent).expect("agent");
        let session = AbsolutePath::parse(session).expect("session");
        let scope = pi_sandbox::SessionScope::new(
            "configured-durable".to_owned(),
            vec![workspace.clone()],
            agent.clone(),
            session.clone(),
        )
        .expect("scope");
        let revision = revision_root(&scope, PolicyRevision::new(1).expect("revision"))
            .expect("revision root");
        assert!(revision.as_path().starts_with(agent.as_path()));
        assert!(!revision.as_path().starts_with(session.as_path()));
        let revision_home = AbsolutePath::parse(revision.as_path().join("home")).expect("home");
        let revision_tmp = AbsolutePath::parse(revision.as_path().join("tmp")).expect("tmp");
        let revision_scratch =
            AbsolutePath::parse(revision.as_path().join("scratch")).expect("scratch");
        for path in [&revision_home, &revision_tmp, &revision_scratch] {
            std::fs::create_dir_all(path.as_path()).expect("revision root");
        }
        let facts = derive_platform_facts(
            PlatformFactInputs::new(
                scope.session_id().to_owned(),
                scope.workspace_roots().to_vec(),
                scope.agent_dir().clone(),
                scope.session_dir().clone(),
            )
            .expect("inputs"),
        )
        .expect("facts");
        let requested = RequestedPolicy::new(
            SandboxMode::WorkspaceWrite,
            NetworkPolicy::None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HostHttpPolicy::new(Vec::new()).expect("http"),
            CredentialPolicy::new(Vec::new()).expect("credentials"),
            ResourceLimits::confined_defaults(),
        )
        .expect("requested policy");
        let snapshot = resolve_policy_snapshot(
            PolicyRevision::new(1).expect("revision"),
            facts.platform(),
            &scope,
            requested,
            &facts,
            revision_home,
            revision_tmp,
            revision_scratch.clone(),
            Vec::new(),
        )
        .expect("snapshot");
        let filesystem = snapshot.policy().filesystem().expect("filesystem");
        assert!(filesystem.denied_paths().contains(&session));
        assert!(filesystem.writable_roots().iter().any(|root| {
            root.root() == &workspace
                && root
                    .protected_descendants()
                    .iter()
                    .any(|path| path.as_path() == std::path::Path::new(".sessions"))
        }));
        validate_current_session_policy(&snapshot, &scope, &facts, &revision_scratch)
            .expect("configured durable validation");
    }

    #[test]
    fn persisted_protected_subset_is_compatible_and_extra_is_not() {
        let required = [
            pi_sandbox::RelativePath::parse(PathBuf::from(".agents")).expect("agents"),
            pi_sandbox::RelativePath::parse(PathBuf::from(".git")).expect("git"),
            pi_sandbox::RelativePath::parse(PathBuf::from(".rpi")).expect("rpi"),
        ];
        let legacy = [
            pi_sandbox::RelativePath::parse(PathBuf::from(".git")).expect("git"),
            pi_sandbox::RelativePath::parse(PathBuf::from(".rpi")).expect("rpi"),
        ];
        let extra = [
            pi_sandbox::RelativePath::parse(PathBuf::from(".git")).expect("git"),
            pi_sandbox::RelativePath::parse(PathBuf::from("secret")).expect("secret"),
        ];
        assert!(persisted_protected_is_compatible(&legacy, &required));
        assert!(!persisted_protected_is_compatible(&extra, &required));
    }

    #[test]
    fn rewrite_required_protected_adds_missing_agents_descendant() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        let agent = root.path().join("agent");
        let session = workspace.join(".sessions");
        let agents = workspace.join(".agents");
        for path in [&workspace, &agent, &session, &agents] {
            std::fs::create_dir_all(path).expect("fixture directory");
        }
        let workspace = AbsolutePath::parse(workspace).expect("workspace");
        let agent = AbsolutePath::parse(agent).expect("agent");
        let session = AbsolutePath::parse(session).expect("session");
        let scope = pi_sandbox::SessionScope::new(
            "rewrite-durable".to_owned(),
            vec![workspace.clone()],
            agent.clone(),
            session.clone(),
        )
        .expect("scope");
        let revision = revision_root(&scope, PolicyRevision::new(1).expect("revision"))
            .expect("revision root");
        let revision_home = AbsolutePath::parse(revision.as_path().join("home")).expect("home");
        let revision_tmp = AbsolutePath::parse(revision.as_path().join("tmp")).expect("tmp");
        let revision_scratch =
            AbsolutePath::parse(revision.as_path().join("scratch")).expect("scratch");
        for path in [&revision_home, &revision_tmp, &revision_scratch] {
            std::fs::create_dir_all(path.as_path()).expect("revision root");
        }
        let facts = derive_platform_facts(
            PlatformFactInputs::new(
                scope.session_id().to_owned(),
                scope.workspace_roots().to_vec(),
                scope.agent_dir().clone(),
                scope.session_dir().clone(),
            )
            .expect("inputs"),
        )
        .expect("facts");
        let requested = RequestedPolicy::new(
            SandboxMode::WorkspaceWrite,
            NetworkPolicy::None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HostHttpPolicy::new(Vec::new()).expect("http"),
            CredentialPolicy::new(Vec::new()).expect("credentials"),
            ResourceLimits::confined_defaults(),
        )
        .expect("requested policy");
        let snapshot = resolve_policy_snapshot(
            PolicyRevision::new(1).expect("revision"),
            facts.platform(),
            &scope,
            requested,
            &facts,
            revision_home,
            revision_tmp,
            revision_scratch,
            Vec::new(),
        )
        .expect("snapshot");
        let filesystem = snapshot.policy().filesystem().expect("filesystem");
        let writable = filesystem
            .writable_roots()
            .iter()
            .map(|root| {
                if root.root() == &workspace {
                    WritableRoot::new(
                        root.root().clone(),
                        vec![
                            pi_sandbox::RelativePath::parse(PathBuf::from(".git")).expect("git"),
                            pi_sandbox::RelativePath::parse(PathBuf::from(".rpi")).expect("rpi"),
                        ],
                    )
                    .expect("legacy protected")
                } else {
                    root.clone()
                }
            })
            .collect::<Vec<_>>();
        let filesystem = FileSystemPolicy::new(
            filesystem.read_only_roots().to_vec(),
            writable,
            filesystem.denied_paths().to_vec(),
        )
        .expect("legacy filesystem");
        let policy = SandboxPolicy::workspace_write(
            filesystem,
            snapshot.policy().network().expect("network"),
            snapshot.policy().host_http().cloned().expect("http"),
            snapshot.policy().credentials().clone(),
            snapshot.policy().limits().expect("limits"),
        )
        .expect("legacy policy");
        let legacy = PolicySnapshot::new(snapshot.revision(), snapshot.created_on(), policy)
            .expect("legacy snapshot");
        let rewritten =
            rewrite_required_protected_descendants(&legacy, &scope, &facts).expect("rewrite");
        let filesystem = rewritten.policy().filesystem().expect("filesystem");
        assert!(filesystem.writable_roots().iter().any(|root| {
            root.root() == &workspace
                && root
                    .protected_descendants()
                    .iter()
                    .any(|path| path.as_path() == std::path::Path::new(".agents"))
        }));
    }

    fn workspace_write_fixture() -> (
        tempfile::TempDir,
        pi_sandbox::SessionScope,
        SandboxEnvironmentFacts,
        AbsolutePath,
        PolicySnapshot,
    ) {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        let agent = root.path().join("agent");
        let session = root.path().join("session");
        for path in [&workspace, &agent, &session] {
            std::fs::create_dir_all(path).expect("fixture directory");
        }
        let workspace = AbsolutePath::parse(workspace).expect("workspace");
        let agent = AbsolutePath::parse(agent).expect("agent");
        let session = AbsolutePath::parse(session).expect("session");
        let scope = pi_sandbox::SessionScope::new(
            "roots-denied-migrate".to_owned(),
            vec![workspace.clone()],
            agent.clone(),
            session.clone(),
        )
        .expect("scope");
        let revision = revision_root(&scope, PolicyRevision::new(1).expect("revision"))
            .expect("revision root");
        let revision_home = AbsolutePath::parse(revision.as_path().join("home")).expect("home");
        let revision_tmp = AbsolutePath::parse(revision.as_path().join("tmp")).expect("tmp");
        let revision_scratch =
            AbsolutePath::parse(revision.as_path().join("scratch")).expect("scratch");
        for path in [&revision_home, &revision_tmp, &revision_scratch] {
            std::fs::create_dir_all(path.as_path()).expect("revision root");
        }
        let facts = derive_platform_facts(
            PlatformFactInputs::new(
                scope.session_id().to_owned(),
                scope.workspace_roots().to_vec(),
                scope.agent_dir().clone(),
                scope.session_dir().clone(),
            )
            .expect("inputs"),
        )
        .expect("facts");
        let requested = RequestedPolicy::new(
            SandboxMode::WorkspaceWrite,
            NetworkPolicy::None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HostHttpPolicy::new(Vec::new()).expect("http"),
            CredentialPolicy::new(Vec::new()).expect("credentials"),
            ResourceLimits::confined_defaults(),
        )
        .expect("requested policy");
        let snapshot = resolve_policy_snapshot(
            PolicyRevision::new(1).expect("revision"),
            facts.platform(),
            &scope,
            requested,
            &facts,
            revision_home,
            revision_tmp,
            revision_scratch.clone(),
            Vec::new(),
        )
        .expect("snapshot");
        (root, scope, facts, revision_scratch, snapshot)
    }

    fn durable_from_snapshot(snapshot: PolicySnapshot) -> DurablePolicyState {
        DurablePolicyState::new(
            snapshot,
            CredentialGrantRegistrySnapshot::new(Vec::new(), Vec::new())
                .expect("empty credential registry"),
        )
        .expect("durable policy")
    }

    #[test]
    fn rewrite_required_session_roots_adds_missing_agent_runtime_and_denied() {
        let (_root, scope, facts, revision_scratch, snapshot) = workspace_write_fixture();
        let filesystem = snapshot.policy().filesystem().expect("filesystem");
        let read_only = filesystem
            .read_only_roots()
            .iter()
            .filter(|path| {
                *path != scope.agent_dir()
                    && !facts
                        .policy_runtime_roots()
                        .iter()
                        .any(|root| root == *path)
            })
            .cloned()
            .collect::<Vec<_>>();
        let denied = filesystem
            .denied_paths()
            .iter()
            .filter(|path| {
                *path != scope.session_dir()
                    && !facts
                        .mandatory_denied_paths()
                        .iter()
                        .any(|denied| denied == *path)
            })
            .cloned()
            .collect::<Vec<_>>();
        let filesystem =
            FileSystemPolicy::new(read_only, filesystem.writable_roots().to_vec(), denied)
                .expect("legacy filesystem");
        let policy = SandboxPolicy::workspace_write(
            filesystem,
            snapshot.policy().network().expect("network"),
            snapshot.policy().host_http().cloned().expect("http"),
            snapshot.policy().credentials().clone(),
            snapshot.policy().limits().expect("limits"),
        )
        .expect("legacy policy");
        let legacy = PolicySnapshot::new(snapshot.revision(), snapshot.created_on(), policy)
            .expect("legacy snapshot");
        validate_current_session_policy(&legacy, &scope, &facts, &revision_scratch)
            .expect_err("legacy roots must fail closed before migrate");
        let rewritten = rewrite_required_session_roots(&legacy, &scope, &facts, &revision_scratch)
            .expect("rewrite");
        let filesystem = rewritten.policy().filesystem().expect("filesystem");
        assert!(
            filesystem.read_only_roots().contains(scope.agent_dir()),
            "missing agentDir must be added as a read-only root"
        );
        for root in facts.policy_runtime_roots() {
            assert!(
                filesystem.read_only_roots().contains(root),
                "missing runtime root must be added"
            );
        }
        assert!(
            filesystem.denied_paths().contains(scope.session_dir()),
            "missing sessionDir denial must be added"
        );
        for path in facts.mandatory_denied_paths() {
            assert!(
                filesystem.denied_paths().contains(path),
                "missing mandatory denied path must be added"
            );
        }
        validate_current_session_policy(&rewritten, &scope, &facts, &revision_scratch)
            .expect("migrated roots must validate");
    }

    #[test]
    fn subset_upgrade_accepts_missing_agent_dir_and_mandatory_denied() {
        let (_root, scope, facts, _revision_scratch, captured_snapshot) = workspace_write_fixture();
        let filesystem = captured_snapshot.policy().filesystem().expect("filesystem");
        let read_only = filesystem
            .read_only_roots()
            .iter()
            .filter(|path| *path != scope.agent_dir())
            .cloned()
            .collect::<Vec<_>>();
        let denied = filesystem
            .denied_paths()
            .iter()
            .filter(|path| {
                facts
                    .mandatory_denied_paths()
                    .iter()
                    .all(|denied| denied != *path)
            })
            .cloned()
            .collect::<Vec<_>>();
        let filesystem =
            FileSystemPolicy::new(read_only, filesystem.writable_roots().to_vec(), denied)
                .expect("legacy filesystem");
        let policy = SandboxPolicy::workspace_write(
            filesystem,
            captured_snapshot.policy().network().expect("network"),
            captured_snapshot
                .policy()
                .host_http()
                .cloned()
                .expect("http"),
            captured_snapshot.policy().credentials().clone(),
            captured_snapshot.policy().limits().expect("limits"),
        )
        .expect("legacy policy");
        let persisted_snapshot = PolicySnapshot::new(
            captured_snapshot.revision(),
            captured_snapshot.created_on(),
            policy,
        )
        .expect("persisted snapshot");
        let persisted = durable_from_snapshot(persisted_snapshot);
        let captured = durable_from_snapshot(captured_snapshot);
        assert!(
            durable_policy_is_protected_subset_upgrade(&persisted, &captured),
            "missing agentDir/runtime/mandatory denied must be a tighten-only upgrade"
        );
    }

    #[test]
    fn same_authority_accepts_a_later_generation_revision() {
        let (_root, _scope, _facts, _revision_scratch, captured_snapshot) =
            workspace_write_fixture();
        let advanced = PolicySnapshot::new(
            PolicyRevision::new(captured_snapshot.revision().get() + 1).expect("next revision"),
            captured_snapshot.created_on(),
            captured_snapshot.policy().clone(),
        )
        .expect("advanced snapshot");
        let persisted = durable_from_snapshot(captured_snapshot);
        let captured = durable_from_snapshot(advanced);
        assert!(durable_policy_same_authority(&persisted, &captured));
        assert!(!durable_policy_is_protected_subset_upgrade(
            &persisted, &captured
        ));
    }

    #[test]
    fn subset_upgrade_rejects_extra_denied_path() {
        let root = tempfile::tempdir().expect("root");
        let extra_dir = root.path().join("extra-denied");
        std::fs::create_dir_all(&extra_dir).expect("extra denied");
        let extra = AbsolutePath::parse(extra_dir).expect("extra path");
        let (_root, _scope, _facts, _revision_scratch, captured_snapshot) =
            workspace_write_fixture();
        let filesystem = captured_snapshot.policy().filesystem().expect("filesystem");
        let mut denied = filesystem.denied_paths().to_vec();
        denied.push(extra);
        let filesystem = FileSystemPolicy::new(
            filesystem.read_only_roots().to_vec(),
            filesystem.writable_roots().to_vec(),
            denied,
        )
        .expect("extra denied filesystem");
        let policy = SandboxPolicy::workspace_write(
            filesystem,
            captured_snapshot.policy().network().expect("network"),
            captured_snapshot
                .policy()
                .host_http()
                .cloned()
                .expect("http"),
            captured_snapshot.policy().credentials().clone(),
            captured_snapshot.policy().limits().expect("limits"),
        )
        .expect("extra policy");
        let persisted_snapshot = PolicySnapshot::new(
            captured_snapshot.revision(),
            captured_snapshot.created_on(),
            policy,
        )
        .expect("persisted snapshot");
        let persisted = durable_from_snapshot(persisted_snapshot);
        let captured = durable_from_snapshot(captured_snapshot);
        assert!(
            !durable_policy_is_protected_subset_upgrade(&persisted, &captured),
            "an extra persisted denied path must stay fail-closed"
        );
    }
}
