use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow, bail};
use arc_swap::ArcSwap;
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::sandbox::{ApprovedSpawnRegistry, SandboxGeneration};
use crate::{
    AgentDefinition, ContextFile, ExtensionPermissionSet, ExtensionRuntime, HostHooks,
    OrchestrationEvent, OrchestrationRuntime, PermissionRule, ReloadResult, ResourceSnapshot,
    RuntimeSettingsSnapshot, SelectorSettings, Skill,
};
pub(crate) type GenerationCaptureFn =
    Arc<dyn Fn() -> Result<Arc<ApplicationGeneration>> + Send + Sync>;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct ApplicationGenerationRevision(u64);

impl ApplicationGenerationRevision {
    pub(crate) fn new(value: u64) -> Result<Self> {
        if value == 0 {
            bail!("application generation revision must be nonzero");
        }
        Ok(Self(value))
    }

    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GenerationOrigin {
    Bootstrap,
    Reload { base: ApplicationGenerationRevision },
    DurableResume,
}

pub(crate) struct GenerationProvenance {
    revision: ApplicationGenerationRevision,
    origin: GenerationOrigin,
}

impl GenerationProvenance {
    fn bootstrap(revision: ApplicationGenerationRevision) -> Result<Arc<Self>> {
        if revision.get() != 1 {
            bail!("bootstrap generation must use revision one");
        }
        Ok(Arc::new(Self {
            revision,
            origin: GenerationOrigin::Bootstrap,
        }))
    }

    fn reload(
        base: ApplicationGenerationRevision,
        revision: ApplicationGenerationRevision,
    ) -> Result<Arc<Self>> {
        if revision.get() <= base.get() {
            bail!("reload generation revision must advance the captured base");
        }
        Ok(Arc::new(Self {
            revision,
            origin: GenerationOrigin::Reload { base },
        }))
    }

    pub(crate) const fn revision(&self) -> ApplicationGenerationRevision {
        self.revision
    }

    pub(super) const fn origin(&self) -> GenerationOrigin {
        self.origin
    }
}

/// An immutable application generation retained across multi-file operations.
/// Filesystem operations remain policy checked; internal authorities stay private.
pub struct ApplicationGeneration {
    provenance: Arc<GenerationProvenance>,
    resource: Arc<ResourceSnapshot>,
    tools: Arc<ToolGeneration>,
    extensions: Arc<ExtensionGeneration>,
    orchestration: Arc<OrchestrationGeneration>,
    runtime: Arc<RuntimeGeneration>,
    security: Arc<SecurityGeneration>,
    brush_exec_lock: Arc<Mutex<()>>,
}

impl ApplicationGeneration {
    /// Read through this generation's retained, no-follow filesystem authority.
    pub fn read_file_bounded(&self, path: &std::path::Path, max_bytes: u64) -> Result<Vec<u8>> {
        Ok(self.sandbox().fs().confined()?.read_bounded(path, max_bytes)?)
    }

    /// Inspect through this generation; the display path is not filesystem authority.
    pub fn inspect_directory(&self, path: &std::path::Path) -> Result<std::path::PathBuf> {
        let directory = self.sandbox().fs().confined()?.open_directory(path)?;
        Ok(directory.display_path())
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        provenance: Arc<GenerationProvenance>,
        resource: Arc<ResourceSnapshot>,
        tools: Arc<ToolGeneration>,
        extensions: Arc<ExtensionGeneration>,
        orchestration: Arc<OrchestrationGeneration>,
        runtime: Arc<RuntimeGeneration>,
        security: Arc<SecurityGeneration>,
        brush_exec_lock: Arc<Mutex<()>>,
    ) -> Result<Arc<Self>> {
        if provenance.revision().get() == 0
            || resource.generation != provenance.revision().get()
            || resource.sandbox().revision().get() != provenance.revision().get()
        {
            bail!("application generation revisions do not match");
        }
        require_same_provenance(&provenance, resource.provenance(), "resource")?;
        require_same_provenance(&provenance, resource.sandbox().provenance(), "sandbox")?;
        require_same_provenance(&provenance, tools.provenance(), "tools")?;
        require_same_provenance(&provenance, extensions.provenance(), "extensions")?;
        require_same_provenance(&provenance, orchestration.provenance(), "orchestration")?;
        require_same_provenance(&provenance, runtime.provenance(), "runtime")?;
        require_same_provenance(&provenance, security.provenance(), "security")?;
        Ok(Arc::new(Self {
            provenance,
            resource,
            tools,
            extensions,
            orchestration,
            runtime,
            security,
            brush_exec_lock,
        }))
    }

    pub(crate) fn provenance(&self) -> &Arc<GenerationProvenance> {
        &self.provenance
    }

    pub(crate) fn revision(&self) -> ApplicationGenerationRevision {
        self.provenance.revision()
    }

    pub(crate) fn resource(&self) -> &Arc<ResourceSnapshot> {
        &self.resource
    }

    pub(crate) fn sandbox(&self) -> &Arc<SandboxGeneration> {
        self.resource.sandbox()
    }

    pub(crate) fn tools(&self) -> &Arc<ToolGeneration> {
        &self.tools
    }

    pub(crate) fn extensions(&self) -> &Arc<ExtensionGeneration> {
        &self.extensions
    }

    pub(crate) fn orchestration(&self) -> &Arc<OrchestrationGeneration> {
        &self.orchestration
    }

    pub(crate) fn runtime(&self) -> &Arc<RuntimeGeneration> {
        &self.runtime
    }

    pub(crate) fn security(&self) -> &Arc<SecurityGeneration> {
        &self.security
    }

    pub(crate) fn brush_exec_lock(&self) -> Arc<Mutex<()>> {
        self.brush_exec_lock.clone()
    }
}

pub(crate) struct ToolGeneration {
    provenance: Arc<GenerationProvenance>,
    active: Arc<[pi_agent::AgentTool]>,
    all: Arc<[pi_agent::AgentTool]>,
    system_prompt: Arc<str>,
    skills: Arc<[Skill]>,
    agents: Arc<[AgentDefinition]>,
    context_files: Arc<[ContextFile]>,
    selector: Arc<SelectorSettings>,
}

impl ToolGeneration {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        provenance: Arc<GenerationProvenance>,
        active: Vec<pi_agent::AgentTool>,
        all: Vec<pi_agent::AgentTool>,
        system_prompt: String,
        skills: Vec<Skill>,
        agents: Vec<AgentDefinition>,
        context_files: Vec<ContextFile>,
        selector: SelectorSettings,
    ) -> Result<Arc<Self>> {
        let mut search_from = 0;
        for tool in &active {
            let Some(offset) = all[search_from..]
                .iter()
                .position(|candidate| candidate.name == tool.name)
            else {
                bail!("active tools must be an ordered subset of all tools");
            };
            search_from += offset + 1;
        }
        Ok(Arc::new(Self {
            provenance,
            active: active.into(),
            all: all.into(),
            system_prompt: system_prompt.into(),
            skills: skills.into(),
            agents: agents.into(),
            context_files: context_files.into(),
            selector: Arc::new(selector),
        }))
    }

    pub(crate) fn provenance(&self) -> &Arc<GenerationProvenance> {
        &self.provenance
    }

    pub(crate) fn active(&self) -> &[pi_agent::AgentTool] {
        &self.active
    }

    pub(crate) fn all(&self) -> &[pi_agent::AgentTool] {
        &self.all
    }

    pub(crate) fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    pub(crate) fn skills(&self) -> &[Skill] {
        &self.skills
    }

    pub(crate) fn agents(&self) -> &[AgentDefinition] {
        &self.agents
    }

    pub(crate) fn context_files(&self) -> &[ContextFile] {
        &self.context_files
    }

    pub(crate) fn selector(&self) -> &SelectorSettings {
        &self.selector
    }

    pub(crate) fn agent_execution_snapshot(&self) -> AgentExecutionSnapshot {
        AgentExecutionSnapshot {
            tools: self.active.clone(),
            system_prompt: self.system_prompt.clone(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct AgentExecutionSnapshot {
    tools: Arc<[pi_agent::AgentTool]>,
    system_prompt: Arc<str>,
}

impl AgentExecutionSnapshot {
    pub(crate) fn tools(&self) -> &[pi_agent::AgentTool] {
        &self.tools
    }

    pub(crate) fn system_prompt(&self) -> &str {
        &self.system_prompt
    }
}

pub(crate) enum ExtensionGeneration {
    Disabled {
        provenance: Arc<GenerationProvenance>,
    },
    Enabled {
        provenance: Arc<GenerationProvenance>,
        runtime: ExtensionRuntime,
        snapshot: Arc<ExtensionSnapshot>,
        permissions: Arc<ExtensionPermissionSet>,
    },
}

impl ExtensionGeneration {
    pub(super) fn disabled(provenance: Arc<GenerationProvenance>) -> Arc<Self> {
        Arc::new(Self::Disabled { provenance })
    }

    pub(super) fn enabled(
        provenance: Arc<GenerationProvenance>,
        runtime: ExtensionRuntime,
        snapshot: Arc<ExtensionSnapshot>,
        permissions: Arc<ExtensionPermissionSet>,
    ) -> Result<Arc<Self>> {
        Ok(Arc::new(Self::Enabled {
            provenance,
            runtime,
            snapshot,
            permissions,
        }))
    }

    pub(crate) fn provenance(&self) -> &Arc<GenerationProvenance> {
        match self {
            Self::Disabled { provenance } | Self::Enabled { provenance, .. } => provenance,
        }
    }

    pub(crate) fn runtime(&self) -> Option<&ExtensionRuntime> {
        match self {
            Self::Disabled { .. } => None,
            Self::Enabled { runtime, .. } => Some(runtime),
        }
    }

    pub(crate) fn snapshot(&self) -> Option<&Arc<ExtensionSnapshot>> {
        match self {
            Self::Disabled { .. } => None,
            Self::Enabled { snapshot, .. } => Some(snapshot),
        }
    }

    pub(crate) fn permissions(&self) -> Option<&Arc<ExtensionPermissionSet>> {
        match self {
            Self::Disabled { .. } => None,
            Self::Enabled { permissions, .. } => Some(permissions),
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct ExtensionSnapshot {
    instance_ids: Arc<[crate::ExtensionInstanceId]>,
}

impl ExtensionSnapshot {
    pub(crate) fn new(instance_ids: Vec<crate::ExtensionInstanceId>) -> Arc<Self> {
        Arc::new(Self {
            instance_ids: instance_ids.into(),
        })
    }

    pub(crate) fn instance_ids(&self) -> &[crate::ExtensionInstanceId] {
        &self.instance_ids
    }
}

pub(crate) enum OrchestrationGeneration {
    Disabled {
        provenance: Arc<GenerationProvenance>,
    },
    Enabled {
        provenance: Arc<GenerationProvenance>,
        runtime: OrchestrationRuntime,
        snapshot: Arc<OrchestrationSnapshot>,
        artifacts: crate::fs::CapabilityRootLease,
    },
}

impl OrchestrationGeneration {
    pub(super) fn disabled(provenance: Arc<GenerationProvenance>) -> Arc<Self> {
        Arc::new(Self::Disabled { provenance })
    }

    pub(crate) fn enabled(
        provenance: Arc<GenerationProvenance>,
        runtime: OrchestrationRuntime,
        snapshot: Arc<OrchestrationSnapshot>,
        artifacts: crate::fs::CapabilityRootLease,
    ) -> Result<Arc<Self>> {
        Ok(Arc::new(Self::Enabled {
            provenance,
            runtime,
            snapshot,
            artifacts,
        }))
    }

    pub(crate) fn provenance(&self) -> &Arc<GenerationProvenance> {
        match self {
            Self::Disabled { provenance } | Self::Enabled { provenance, .. } => provenance,
        }
    }

    pub(crate) fn runtime(&self) -> Option<&OrchestrationRuntime> {
        match self {
            Self::Disabled { .. } => None,
            Self::Enabled { runtime, .. } => Some(runtime),
        }
    }

    pub(crate) fn snapshot(&self) -> Option<&Arc<OrchestrationSnapshot>> {
        match self {
            Self::Disabled { .. } => None,
            Self::Enabled { snapshot, .. } => Some(snapshot),
        }
    }

    pub(crate) fn artifacts(&self) -> Option<&crate::fs::CapabilityRootLease> {
        match self {
            Self::Disabled { .. } => None,
            Self::Enabled { artifacts, .. } => Some(artifacts),
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct OrchestrationSnapshot {
    events: Arc<[OrchestrationEvent]>,
}

impl OrchestrationSnapshot {
    pub(crate) fn new(events: Vec<OrchestrationEvent>) -> Arc<Self> {
        Arc::new(Self {
            events: events.into(),
        })
    }

    pub(crate) fn events(&self) -> &[OrchestrationEvent] {
        &self.events
    }
}

pub(crate) struct RuntimeGeneration {
    provenance: Arc<GenerationProvenance>,
    settings: Arc<RuntimeSettingsSnapshot>,
}

impl RuntimeGeneration {
    pub(super) fn new(
        provenance: Arc<GenerationProvenance>,
        settings: RuntimeSettingsSnapshot,
    ) -> Arc<Self> {
        Arc::new(Self {
            provenance,
            settings: Arc::new(settings),
        })
    }

    pub(crate) fn provenance(&self) -> &Arc<GenerationProvenance> {
        &self.provenance
    }

    pub(crate) fn settings(&self) -> &Arc<RuntimeSettingsSnapshot> {
        &self.settings
    }
}

pub(crate) struct SecurityGeneration {
    provenance: Arc<GenerationProvenance>,
    approvals: Arc<ApprovedSpawnRegistry>,
    permission_rules: Arc<[PermissionRule]>,
    host_hooks: Option<Arc<HostHooks>>,
}

impl SecurityGeneration {
    pub(super) fn new(
        provenance: Arc<GenerationProvenance>,
        approvals: Arc<ApprovedSpawnRegistry>,
        permission_rules: Vec<PermissionRule>,
        host_hooks: Option<Arc<HostHooks>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            provenance,
            approvals,
            permission_rules: permission_rules.into(),
            host_hooks,
        })
    }

    pub(crate) fn provenance(&self) -> &Arc<GenerationProvenance> {
        &self.provenance
    }

    pub(crate) fn approvals(&self) -> &Arc<ApprovedSpawnRegistry> {
        &self.approvals
    }

    pub(crate) fn permission_rules(&self) -> &[PermissionRule] {
        &self.permission_rules
    }

    pub(crate) fn host_hooks(&self) -> Option<&Arc<HostHooks>> {
        self.host_hooks.as_ref()
    }
}

pub(crate) struct ApplicationPublication {
    inner: Arc<ApplicationPublicationInner>,
}

struct ApplicationPublicationInner {
    active: ArcSwap<ApplicationGeneration>,
    reload: Arc<Mutex<()>>,
    next_revision: AtomicU64,
}

pub(super) struct BootstrapGenerationLease {
    provenance: Arc<GenerationProvenance>,
}

pub(crate) struct ApplicationReloadLease {
    owner: Arc<ApplicationPublicationInner>,
    guard: Option<OwnedMutexGuard<()>>,
    base: Arc<ApplicationGeneration>,
    provenance: Arc<GenerationProvenance>,
    /// Set once this lease's candidate has been published; a published lease
    /// keeps its claimed revision, an abandoned one rolls it back on drop.
    claimed: bool,
}

pub(crate) struct DurableGenerationLease {
    provenance: Arc<GenerationProvenance>,
}

pub(crate) struct DurableApplicationComponents {
    tools: Arc<ToolGeneration>,
    extensions: Arc<ExtensionGeneration>,
    orchestration: Arc<OrchestrationGeneration>,
    runtime: Arc<RuntimeGeneration>,
    security: Arc<SecurityGeneration>,
}

pub(super) struct PostPublishActions {
    provenance: Arc<GenerationProvenance>,
    extension_events: Vec<crate::ExtensionRuntimeEvent>,
    retired_extensions: Vec<ExtensionRuntime>,
    start_extension_session_hook: bool,
    orchestration_events: Option<tokio::sync::broadcast::Receiver<OrchestrationEvent>>,
}

impl PostPublishActions {
    pub(super) fn new(
        provenance: Arc<GenerationProvenance>,
        extension_events: Vec<crate::ExtensionRuntimeEvent>,
        retired_extensions: Vec<ExtensionRuntime>,
        start_extension_session_hook: bool,
        orchestration_events: Option<tokio::sync::broadcast::Receiver<OrchestrationEvent>>,
    ) -> Self {
        Self {
            provenance,
            extension_events,
            retired_extensions,
            start_extension_session_hook,
            orchestration_events,
        }
    }

    pub(crate) fn provenance(&self) -> &Arc<GenerationProvenance> {
        &self.provenance
    }
}

pub(crate) struct PreparedApplicationGeneration {
    lease: ApplicationReloadLease,
    next: Arc<ApplicationGeneration>,
    reload_result: ReloadResult,
    post_publish: PostPublishActions,
}

pub(crate) struct PublicationReceipt {
    current: Arc<ApplicationGeneration>,
    previous: Arc<ApplicationGeneration>,
    reload_result: ReloadResult,
    post_publish: PostPublishActions,
}

impl BootstrapGenerationLease {
    pub(super) fn new() -> Result<Self> {
        Ok(Self {
            provenance: GenerationProvenance::bootstrap(ApplicationGenerationRevision::new(1)?)?,
        })
    }

    pub(super) fn provenance(&self) -> &Arc<GenerationProvenance> {
        &self.provenance
    }

    pub(super) fn finish(
        self,
        initial: Arc<ApplicationGeneration>,
    ) -> Result<Arc<ApplicationPublication>> {
        require_same_provenance(
            &self.provenance,
            initial.provenance(),
            "bootstrap aggregate",
        )?;
        if initial.revision().get() != 1 || self.provenance.origin() != GenerationOrigin::Bootstrap
        {
            bail!("invalid bootstrap generation");
        }
        Ok(Arc::new(ApplicationPublication {
            inner: Arc::new(ApplicationPublicationInner {
                active: ArcSwap::from(initial),
                reload: Arc::new(Mutex::new(())),
                next_revision: AtomicU64::new(2),
            }),
        }))
    }
}

impl ApplicationPublication {
    pub(super) fn from_durable(generation: Arc<ApplicationGeneration>) -> Result<Arc<Self>> {
        if generation.provenance().origin() != GenerationOrigin::DurableResume {
            bail!("durable publication requires a resumed generation");
        }
        let next_revision = generation
            .revision()
            .get()
            .checked_add(1)
            .ok_or_else(|| anyhow!("application generation revision overflow"))?;
        Ok(Arc::new(Self {
            inner: Arc::new(ApplicationPublicationInner {
                active: ArcSwap::from(generation),
                reload: Arc::new(Mutex::new(())),
                next_revision: AtomicU64::new(next_revision),
            }),
        }))
    }
}

impl DurableGenerationLease {
    pub(super) fn new(revision: ApplicationGenerationRevision) -> Result<Self> {
        Ok(Self {
            provenance: Arc::new(GenerationProvenance {
                revision,
                origin: GenerationOrigin::DurableResume,
            }),
        })
    }
    pub(crate) fn provenance(&self) -> &Arc<GenerationProvenance> {
        &self.provenance
    }

    pub(crate) fn finish(
        self,
        resource: Arc<ResourceSnapshot>,
        components: DurableApplicationComponents,
        brush_exec_lock: Arc<Mutex<()>>,
    ) -> Result<Arc<ApplicationGeneration>> {
        let generation = ApplicationGeneration::new(
            self.provenance.clone(),
            resource,
            components.tools,
            components.extensions,
            components.orchestration,
            components.runtime,
            components.security,
            brush_exec_lock,
        )?;
        require_same_provenance(
            &self.provenance,
            generation.provenance(),
            "durable resume aggregate",
        )?;
        if self.provenance.origin() != GenerationOrigin::DurableResume {
            bail!("invalid durable resume generation");
        }
        Ok(generation)
    }
}

impl DurableApplicationComponents {
    pub(super) fn new(
        provenance: &Arc<GenerationProvenance>,
        tools: Arc<ToolGeneration>,
        extensions: Arc<ExtensionGeneration>,
        orchestration: Arc<OrchestrationGeneration>,
        runtime: Arc<RuntimeGeneration>,
        security: Arc<SecurityGeneration>,
    ) -> Result<Self> {
        require_same_provenance(provenance, tools.provenance(), "durable tools")?;
        require_same_provenance(provenance, extensions.provenance(), "durable extensions")?;
        require_same_provenance(
            provenance,
            orchestration.provenance(),
            "durable orchestration",
        )?;
        require_same_provenance(provenance, runtime.provenance(), "durable runtime")?;
        require_same_provenance(provenance, security.provenance(), "durable security")?;
        Ok(Self {
            tools,
            extensions,
            orchestration,
            runtime,
            security,
        })
    }
}

impl ApplicationPublication {
    pub(crate) fn capture(&self) -> Arc<ApplicationGeneration> {
        self.inner.active.load_full()
    }

    pub(crate) fn capture_fn(self: &Arc<Self>) -> GenerationCaptureFn {
        let publication = Arc::downgrade(self);
        Arc::new(move || {
            publication
                .upgrade()
                .map(|publication| publication.capture())
                .ok_or_else(|| anyhow!("application publication is unavailable"))
        })
    }
    pub(crate) async fn begin_reload(self: &Arc<Self>) -> Result<ApplicationReloadLease> {
        let guard = self.inner.reload.clone().lock_owned().await;
        let base = self.capture();
        let revision = self
            .inner
            .next_revision
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| anyhow!("application generation revision overflow"))?;
        let revision = ApplicationGenerationRevision::new(revision)?;
        let provenance = GenerationProvenance::reload(base.revision(), revision)?;
        Ok(ApplicationReloadLease {
            owner: self.inner.clone(),
            guard: Some(guard),
            base,
            provenance,
            claimed: false,
        })
    }
}

impl Drop for ApplicationReloadLease {
    fn drop(&mut self) {
        // `begin_reload` reserves the next revision by advancing
        // `next_revision` via fetch_update. If this lease is dropped without
        // being published (the candidate failed validation and no `publish`
        // was reached), roll the reserved revision back so the next successful
        // reload increments exactly once relative to the last COMMITTED
        // generation (instead of burning two revision numbers). The reload
        // mutex guard is still held during Drop, so no other begin_reload can
        // be mid-flight — the rollback is race-free. A published lease set
        // `claimed`, so its revision is kept.
        if self.claimed {
            return;
        }
        self.owner
            .next_revision
            .store(self.provenance.revision().get(), Ordering::Release);
    }
}

impl ApplicationReloadLease {
    pub(crate) fn base(&self) -> &Arc<ApplicationGeneration> {
        &self.base
    }

    pub(crate) fn next_revision(&self) -> ApplicationGenerationRevision {
        self.provenance.revision()
    }

    pub(super) fn provenance(&self) -> &Arc<GenerationProvenance> {
        &self.provenance
    }

    pub(super) fn prepare(
        self,
        next: Arc<ApplicationGeneration>,
        reload_result: ReloadResult,
        post_publish: PostPublishActions,
    ) -> Result<PreparedApplicationGeneration> {
        require_same_provenance(&self.provenance, next.provenance(), "reload aggregate")?;
        require_same_provenance(
            &self.provenance,
            post_publish.provenance(),
            "post-publication actions",
        )?;
        if self.provenance.origin()
            != (GenerationOrigin::Reload {
                base: self.base.revision(),
            })
            || reload_result.generation != self.provenance.revision().get()
        {
            bail!("reload candidate is not bound to its captured base revision");
        }
        Ok(PreparedApplicationGeneration {
            lease: self,
            next,
            reload_result,
            post_publish,
        })
    }
}

impl PreparedApplicationGeneration {
    pub(crate) fn capture_fn(&self) -> GenerationCaptureFn {
        let generation = self.next.clone();
        Arc::new(move || Ok(generation.clone()))
    }

    pub(crate) fn publish(mut self) -> PublicationReceipt {
        self.lease.claimed = true;
        let previous = self.lease.owner.active.swap(self.next.clone());
        drop(self.lease.guard.take().expect("reload guard"));
        PublicationReceipt {
            current: self.next,
            previous,
            reload_result: self.reload_result,
            post_publish: self.post_publish,
        }
    }
}

impl PublicationReceipt {
    pub(crate) fn current(&self) -> &Arc<ApplicationGeneration> {
        &self.current
    }

    pub(crate) fn previous(&self) -> &Arc<ApplicationGeneration> {
        &self.previous
    }

    pub(crate) fn reload_result(&self) -> &ReloadResult {
        &self.reload_result
    }

    pub(crate) async fn notify_and_retire(self, _application: &super::Application) {
        let _ = (
            self.post_publish.extension_events,
            self.post_publish.start_extension_session_hook,
            self.post_publish.orchestration_events,
        );
        for runtime in self.post_publish.retired_extensions {
            runtime.shutdown_with_reason("reload").await;
        }
    }
}

fn require_same_provenance(
    expected: &Arc<GenerationProvenance>,
    actual: &Arc<GenerationProvenance>,
    component: &str,
) -> Result<()> {
    if Arc::ptr_eq(expected, actual) {
        Ok(())
    } else {
        Err(anyhow!("{component} has mixed generation provenance"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_origin_binds_captured_base_revision() {
        let base = ApplicationGenerationRevision::new(7).expect("base revision");
        let next = ApplicationGenerationRevision::new(8).expect("next revision");
        let provenance = GenerationProvenance::reload(base, next).expect("reload provenance");
        assert_eq!(provenance.origin(), GenerationOrigin::Reload { base });
    }

    #[test]
    fn bootstrap_provenance_is_revision_one() {
        let lease = BootstrapGenerationLease::new().expect("bootstrap lease");
        assert_eq!(lease.provenance().revision().get(), 1);
        assert_eq!(lease.provenance().origin(), GenerationOrigin::Bootstrap);
    }
}
