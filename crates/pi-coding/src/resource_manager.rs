//! Atomic resource snapshots with complete candidate validation on reload.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::application::generation::GenerationProvenance;
use crate::fs::CapabilityFs;
use crate::prompt_templates::{LoadPromptTemplatesOptions, PromptTemplate, load_prompt_templates};
use crate::resources::{CONFIG_DIR_NAME, Skill, SkillDiagnostic, agent_dir_path, home_dir};
use crate::sandbox::SandboxGeneration;
use crate::settings::{DefaultProjectTrust, HookEvent, Settings, SettingsManager};
use crate::system_prompt::ContextFile;
use crate::trust::{TrustResolution, TrustStore, resolve_project_trust};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceDiagnosticLevel {
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceDiagnostic {
    pub level: ResourceDiagnosticLevel,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeResource {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct ResourcePaths {
    pub context_roots: Vec<PathBuf>,
    pub skill_paths: Vec<PathBuf>,
    pub prompt_paths: Vec<PathBuf>,
    pub theme_dirs: Vec<PathBuf>,
    pub keybinding_files: Vec<PathBuf>,
    pub system_prompt_files: Vec<PathBuf>,
    pub append_system_prompt_files: Vec<PathBuf>,
    /// Persona directories discovered for complete resource path accounting
    /// (`<agent_dir>/personas` and, when the project is trusted,
    /// `<cwd>/.rpi/personas`). Persona definitions themselves are part of
    /// [`ResourceSnapshot::agents`]; these dirs let reload/watch logic track
    /// persona state alongside other resource roots.
    pub persona_dirs: Vec<PathBuf>,
}

impl ResourcePaths {
    #[must_use]
    pub fn discover(cwd: impl AsRef<Path>, project_trusted: bool) -> Self {
        Self::discover_from_agent_dir(cwd.as_ref(), &agent_dir_path(), project_trusted)
    }

    fn discover_from_agent_dir(cwd: &Path, agent_dir: &Path, project_trusted: bool) -> Self {
        let skill_paths = discover_skill_roots(cwd, agent_dir, project_trusted)
            .into_iter()
            .map(|root| root.path)
            .collect();
        let project_dir = cwd.join(CONFIG_DIR_NAME);
        let mut paths = Self {
            context_roots: vec![agent_dir.to_path_buf()],
            skill_paths,
            prompt_paths: vec![agent_dir.join("prompts")],
            theme_dirs: vec![agent_dir.join("themes")],
            keybinding_files: vec![agent_dir.join("keybindings.json")],
            system_prompt_files: vec![agent_dir.join("SYSTEM.md")],
            append_system_prompt_files: vec![agent_dir.join("APPEND_SYSTEM.md")],
            persona_dirs: vec![agent_dir.join("personas")],
        };
        if project_trusted {
            paths.context_roots.push(cwd.to_path_buf());
            paths.prompt_paths.push(project_dir.join("prompts"));
            paths.theme_dirs.push(project_dir.join("themes"));
            paths
                .keybinding_files
                .push(project_dir.join("keybindings.json"));
            paths
                .system_prompt_files
                .insert(0, project_dir.join("SYSTEM.md"));
            paths
                .append_system_prompt_files
                .insert(0, project_dir.join("APPEND_SYSTEM.md"));
            paths.persona_dirs.push(project_dir.join("personas"));
        }
        paths
    }
}

#[derive(Clone, Debug)]
struct SkillRoot {
    path: PathBuf,
    source: crate::SkillSource,
}

fn discover_skill_roots(cwd: &Path, agent_dir: &Path, project_trusted: bool) -> Vec<SkillRoot> {
    let canonical_cwd = if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| cwd.to_path_buf(), |base| base.join(cwd))
    };
    let project_boundary =
        crate::project_context_boundary(&canonical_cwd).unwrap_or_else(|_| canonical_cwd.clone());
    let mut roots = vec![SkillRoot {
        path: agent_dir.join("skills"),
        source: crate::SkillSource::User,
    }];
    let configured_home = home_from_agent_dir(agent_dir).map(Path::to_path_buf);
    let configured_candidate = configured_home
        .as_ref()
        .map(|home| home.join(".agents").join("skills"));
    let configured_is_project = configured_candidate.as_ref().is_some_and(|path| {
        let path = if path.is_absolute() {
            path.clone()
        } else {
            canonical_cwd.join(path)
        };
        resolve_skill_root_target(&path).starts_with(&project_boundary)
    });
    if !configured_is_project && let Some(path) = configured_candidate {
        push_skill_root(
            &mut roots,
            path,
            crate::SkillSource::User,
            &project_boundary,
            project_trusted,
        );
    } else if let Some(home) = platform_home_dir() {
        push_skill_root(
            &mut roots,
            home.join(".agents").join("skills"),
            crate::SkillSource::User,
            &project_boundary,
            project_trusted,
        );
    }
    if project_trusted {
        for ancestor in trusted_project_ancestors(&canonical_cwd) {
            push_skill_root(
                &mut roots,
                ancestor.join(".agents").join("skills"),
                crate::SkillSource::Project,
                &project_boundary,
                true,
            );
        }
        push_skill_root(
            &mut roots,
            canonical_cwd.join(CONFIG_DIR_NAME).join("skills"),
            crate::SkillSource::Project,
            &project_boundary,
            true,
        );
    }
    roots
}

fn push_skill_root(
    roots: &mut Vec<SkillRoot>,
    path: PathBuf,
    source: crate::SkillSource,
    project_boundary: &Path,
    project_trusted: bool,
) -> bool {
    let target = if path.is_absolute() {
        path.clone()
    } else {
        project_boundary.join(&path)
    };
    // Trust classification follows the RESOLVED target: a global skill root
    // that is a managed symlink into the project must be treated as project
    // content (excluded when the project is untrusted, reclassified when
    // trusted) exactly like a configured path inside the boundary.
    let target = resolve_skill_root_target(&target);
    let source = if target.starts_with(project_boundary) {
        if !project_trusted {
            return false;
        }
        crate::SkillSource::Project
    } else {
        source
    };
    if let Some(index) = roots.iter().position(|root| same_path(&root.path, &path)) {
        let existing = roots.remove(index);
        roots.push(SkillRoot {
            path,
            source: if source == crate::SkillSource::Project {
                source
            } else {
                existing.source
            },
        });
        return true;
    }
    roots.push(SkillRoot { path, source });
    true
}

/// Resolve managed-config symlinks along a skill root (read-only, best
/// effort): trust classification must follow where the root actually points.
/// On any resolution failure the configured path is used unchanged, matching
/// the pre-existing strict-open behavior.
fn resolve_skill_root_target(path: &Path) -> PathBuf {
    let Ok(capabilities) = crate::fs::CapabilityFs::new() else {
        return path.to_path_buf();
    };
    let Ok(absolute) = pi_sandbox::AbsolutePath::parse(path.to_path_buf()) else {
        return path.to_path_buf();
    };
    capabilities
        .resolve_managed_path_for_read(crate::fs::CapabilityRootPurpose::ProjectResources, absolute)
        .unwrap_or_else(|_| path.to_path_buf())
}

fn home_from_agent_dir(agent_dir: &Path) -> Option<&Path> {
    let pi_dir = agent_dir.parent()?;
    (agent_dir.file_name()? == "agent" && pi_dir.file_name()? == CONFIG_DIR_NAME)
        .then(|| pi_dir.parent())
        .flatten()
}

fn platform_home_dir() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(home) = RESOURCE_HOME_OVERRIDE.with(|override_home| override_home.borrow().clone())
    {
        return Some(home);
    }
    home_dir().map(PathBuf::from)
}

#[cfg(test)]
thread_local! {
    static RESOURCE_HOME_OVERRIDE: std::cell::RefCell<Option<PathBuf>> = const {
        std::cell::RefCell::new(None)
    };
}

fn trusted_project_ancestors(cwd: &Path) -> Vec<PathBuf> {
    let cwd = if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| cwd.to_path_buf(), |base| base.join(cwd))
    };
    let git_root = crate::project_context_boundary(&cwd).unwrap_or_else(|_| cwd.clone());
    let mut ancestors = cwd
        .ancestors()
        .take_while(|ancestor| *ancestor != git_root)
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    ancestors.push(git_root);
    ancestors.reverse();
    ancestors
}

fn load_default_skills(
    capabilities: &Arc<CapabilityFs>,
    agent_root: &crate::fs::CapabilityRootLease,
    project_root: Option<&crate::fs::CapabilityRootLease>,
    roots: &[SkillRoot],
    managed_project_boundary: Option<&Path>,
) -> (Vec<Skill>, Vec<SkillDiagnostic>) {
    let mut skills = Vec::new();
    let mut skill_indexes = BTreeMap::<String, usize>::new();
    let mut diagnostics = Vec::new();
    for (index, root) in roots.iter().enumerate() {
        let (found, found_diagnostics) = if index == 0 && root.source == crate::SkillSource::User {
            crate::resources::load_skills_from_agent_root_lease(
                agent_root,
                Path::new("skills"),
                root.source,
                true,
                managed_project_boundary,
            )
        } else if let Some(project_root) = project_root
            && let Ok(relative) = root.path.strip_prefix(project_root.display_root())
        {
            crate::resources::load_skills_from_agent_root_lease(
                project_root,
                relative,
                root.source,
                true,
                root.source.allows_managed_links().then_some(managed_project_boundary).flatten(),
            )
        } else {
            crate::resources::load_skills_from_dir_with_capabilities_and_boundary(
                capabilities,
                &root.path.to_string_lossy(),
                root.source,
                true,
                matches!(root.source, crate::SkillSource::User),
                root.source.allows_managed_links().then_some(managed_project_boundary).flatten(),
            )
        };
        diagnostics.extend(found_diagnostics);
        let mut root_names = BTreeMap::new();
        for skill in found {
            if root_names.insert(skill.name.clone(), ()).is_some() {
                continue;
            }
            if let Some(index) = skill_indexes.get(&skill.name).copied() {
                skills[index] = skill;
            } else {
                skill_indexes.insert(skill.name.clone(), skills.len());
                skills.push(skill);
            }
        }
    }
    (skills, diagnostics)
}

fn same_path(left: &Path, right: &Path) -> bool {
    let absolute = |path: &Path| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().map_or_else(|_| path.to_path_buf(), |base| base.join(path))
        }
    };
    absolute(left) == absolute(right)
}

#[derive(Clone, Copy)]
enum ProjectResourceAccess {
    Enabled,
    SuppressedForAgentStoreOverlap,
}

impl ProjectResourceAccess {
    const fn allows(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

struct OpenedAgentRoot {
    lease: crate::fs::CapabilityRootLease,
    project_resources: ProjectResourceAccess,
}
#[derive(Clone)]
enum ResourceAuthority {
    Unpublished,
    Published {
        provenance: Arc<GenerationProvenance>,
        sandbox: Arc<SandboxGeneration>,
    },
}

impl std::fmt::Debug for ResourceAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unpublished => formatter.write_str("Unpublished"),
            Self::Published { provenance, .. } => formatter
                .debug_struct("Published")
                .field("revision", &provenance.revision().get())
                .finish_non_exhaustive(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResourceSnapshot {
    pub generation: u64,
    pub cwd: PathBuf,
    pub trust: TrustResolution,
    pub settings: Settings,
    pub context_files: Vec<ContextFile>,
    pub skills: Vec<Skill>,
    pub agents: Vec<crate::AgentDefinition>,
    pub prompts: Vec<PromptTemplate>,
    pub themes: Vec<ThemeResource>,
    /// Validated process extension manifests with package id, scope, and trust metadata.
    /// Convert them with [`crate::extension_specs_from_package_resources`] before launch.
    pub package_extensions: Vec<crate::PackageResourceSpec>,
    pub theme_dirs: Vec<PathBuf>,
    pub keybinding_files: Vec<PathBuf>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Vec<String>,
    pub diagnostics: Vec<ResourceDiagnostic>,
    authority: ResourceAuthority,
}

#[derive(Clone, Debug)]
pub struct ResourceManagerOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub headless: bool,
    pub project_trust_override: Option<bool>,
    pub explicit_extension_paths: Vec<PathBuf>,
    pub explicit_skill_paths: Vec<PathBuf>,
    pub explicit_prompt_paths: Vec<PathBuf>,
    pub explicit_theme_paths: Vec<PathBuf>,
    pub disable_extensions: bool,
    pub disable_skills: bool,
    pub disable_prompt_templates: bool,
    pub disable_themes: bool,
    pub disable_context_files: bool,
    pub system_prompt: Option<String>,
    pub system_prompt_path: Option<PathBuf>,
    /// Trust-validation origin for preloaded `system_prompt` content. This path
    /// is diagnostic only and is never reopened.
    pub system_prompt_source: Option<PathBuf>,
    pub append_system_prompt: Vec<String>,
    pub append_system_prompt_paths: Vec<Option<PathBuf>>,
    /// Trust-validation origins for preloaded append prompts. These paths are
    /// diagnostic only and are never reopened.
    pub append_system_prompt_sources: Vec<Option<PathBuf>>,
}

impl ResourceManagerOptions {
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            agent_dir: agent_dir_path(),
            headless: true,
            project_trust_override: None,
            explicit_extension_paths: Vec::new(),
            explicit_skill_paths: Vec::new(),
            explicit_prompt_paths: Vec::new(),
            explicit_theme_paths: Vec::new(),
            disable_extensions: false,
            disable_skills: false,
            disable_prompt_templates: false,
            disable_themes: false,
            disable_context_files: false,
            system_prompt: None,
            system_prompt_path: None,
            system_prompt_source: None,
            append_system_prompt: Vec::new(),
            append_system_prompt_paths: Vec::new(),
            append_system_prompt_sources: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct ResourceManager {
    inner: Arc<ResourceManagerInner>,
}
struct ResourceManagerInner {
    options: ResourceManagerOptions,
    trust_store: TrustStore,
    agent_root: crate::fs::CapabilityRootLease,
    project_root: Option<crate::fs::CapabilityRootLease>,
    project_resources: ProjectResourceAccess,
    initial_snapshot: Arc<ResourceSnapshot>,
    initial_capabilities: Arc<CapabilityFs>,
    publication: OnceLock<Weak<crate::application::generation::ApplicationPublication>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReloadResult {
    pub generation: u64,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

pub(crate) struct ResourceCandidate {
    provenance: Arc<GenerationProvenance>,
    capabilities: Arc<CapabilityFs>,
    snapshot: ResourceSnapshot,
}

pub(crate) struct GlobalResourceCandidate {
    provenance: Arc<GenerationProvenance>,
    capabilities: Arc<CapabilityFs>,
    settings: Settings,
}

impl ResourceSnapshot {
    pub(crate) fn capabilities(&self) -> &Arc<CapabilityFs> {
        match &self.authority {
            ResourceAuthority::Published { sandbox, .. } => sandbox.capabilities(),
            ResourceAuthority::Unpublished => {
                panic!("unpublished resource snapshot has no capabilities")
            }
        }
    }
}

impl ResourceCandidate {
    pub(crate) fn snapshot(&self) -> &ResourceSnapshot {
        &self.snapshot
    }

    pub(crate) fn capabilities(&self) -> &Arc<CapabilityFs> {
        &self.capabilities
    }

    pub(crate) fn finish(
        mut self,
        sandbox: Arc<SandboxGeneration>,
    ) -> Result<Arc<ResourceSnapshot>> {
        if !Arc::ptr_eq(&self.provenance, sandbox.provenance())
            || self.snapshot.generation != self.provenance.revision().get()
            || sandbox.revision().get() != self.provenance.revision().get()
        {
            bail!("resource candidate has mixed generation provenance");
        }
        self.snapshot.authority = ResourceAuthority::Published {
            provenance: self.provenance,
            sandbox,
        };
        Ok(Arc::new(self.snapshot))
    }

    pub(crate) fn extension_specs(
        &self,
        permissions: &crate::ExtensionPermissionSet,
    ) -> Result<Vec<crate::ExtensionSpec>> {
        extension_specs_for_snapshot(&self.snapshot, &self.capabilities, permissions)
    }
}

impl GlobalResourceCandidate {
    pub(crate) fn provenance(&self) -> &Arc<GenerationProvenance> {
        &self.provenance
    }

    pub(crate) fn capabilities(&self) -> &Arc<CapabilityFs> {
        &self.capabilities
    }

    pub(crate) fn settings(&self) -> &Settings {
        &self.settings
    }
}

impl ResourceSnapshot {
    pub(crate) fn provenance(&self) -> &Arc<GenerationProvenance> {
        match &self.authority {
            ResourceAuthority::Published { provenance, .. } => provenance,
            ResourceAuthority::Unpublished => {
                panic!("unpublished resource snapshot has no provenance")
            }
        }
    }

    pub(crate) fn sandbox(&self) -> &Arc<SandboxGeneration> {
        match &self.authority {
            ResourceAuthority::Published { sandbox, .. } => sandbox,
            ResourceAuthority::Unpublished => {
                panic!("unpublished resource snapshot has no sandbox")
            }
        }
    }

    pub(crate) fn extension_specs(
        &self,
        permissions: &crate::ExtensionPermissionSet,
    ) -> Result<Vec<crate::ExtensionSpec>> {
        extension_specs_for_snapshot(self, self.capabilities(), permissions)
    }
}

impl ResourceManager {
    pub fn new(options: ResourceManagerOptions) -> Result<Self> {
        Self::new_with_workspace(options, None)
    }

    pub fn new_for_workspace(
        options: ResourceManagerOptions,
        workspace: &crate::WorkspaceRoots,
    ) -> Result<Self> {
        Self::new_with_workspace(options, Some(workspace))
    }

    fn new_with_workspace(
        options: ResourceManagerOptions,
        workspace: Option<&crate::WorkspaceRoots>,
    ) -> Result<Self> {
        let capabilities = workspace
            .and_then(crate::WorkspaceRoots::capabilities)
            .unwrap_or(crate::fs::CapabilityFs::new()?);
        let candidate_project_root = workspace.and_then(|workspace| workspace.authority(0)).cloned();
        let opened_agent_root = open_agent_root(&capabilities, &options.agent_dir, &options.cwd)?;
        let project_root = if opened_agent_root.project_resources.allows() {
            candidate_project_root
        } else {
            None
        };
        let agent_root = opened_agent_root.lease;
        let settings = SettingsManager::load_phase_one_with_roots(
            &options.cwd,
            &options.agent_dir,
            agent_root.clone(),
            project_root.clone(),
        )?;
        let trust_store = TrustStore::new(agent_root.authority_root());
        let initial_snapshot = Arc::new(build_candidate(
            &options,
            &settings,
            &trust_store,
            1,
            None,
            &capabilities,
            agent_root.clone(),
            project_root.clone(),
            opened_agent_root.project_resources,
        )?);
        Ok(Self {
            inner: Arc::new(ResourceManagerInner {
                options,
                trust_store,
                project_root: project_root.clone(),
                agent_root: agent_root.clone(),
                project_resources: opened_agent_root.project_resources,
                initial_snapshot,
                initial_capabilities: capabilities,
                publication: OnceLock::new(),
            }),
        })
    }

    pub(crate) fn stage_global_pre_trust_candidate(
        &self,
        provenance: Arc<GenerationProvenance>,
    ) -> Result<Option<GlobalResourceCandidate>> {
        self.inner
            .agent_root
            .validate_live_root()
            .context("validating retained agent resource root")?;
        let capabilities = self.inner.initial_capabilities.clone();
        let settings_manager = SettingsManager::load_phase_one_with_agent_root(
            &self.inner.options.cwd,
            &self.inner.options.agent_dir,
            self.inner.agent_root.clone(),
        )?;
        let settings = settings_manager.settings();
        let has_pre_trust_hook = settings.hooks.as_ref().is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.event == HookEvent::PreTrustDecision && hook.enabled != Some(false)
            })
        });
        if !has_pre_trust_hook {
            return Ok(None);
        }
        Ok(Some(GlobalResourceCandidate {
            provenance,
            capabilities,
            settings,
        }))
    }

    pub(crate) fn stage_initial_candidate(
        &self,
        provenance: Arc<GenerationProvenance>,
        composed_trust: Option<&TrustResolution>,
    ) -> Result<ResourceCandidate> {
        self.stage_candidate(provenance, composed_trust)
    }

    #[must_use]
    pub fn snapshot(&self) -> Arc<ResourceSnapshot> {
        self.inner
            .publication
            .get()
            .and_then(Weak::upgrade)
            .map_or_else(
                || self.inner.initial_snapshot.clone(),
                |publication| publication.capture().resource().clone(),
            )
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.snapshot().generation
    }

    #[must_use]
    pub fn diagnostics(&self) -> Vec<ResourceDiagnostic> {
        self.snapshot().diagnostics.clone()
    }

    pub fn settings_manager(&self) -> Result<SettingsManager> {
        self.inner
            .agent_root
            .validate_live_root()
            .context("validating retained agent resource root")?;
        let manager = SettingsManager::load_phase_one_with_agent_root(
            &self.inner.options.cwd,
            &self.inner.options.agent_dir,
            self.inner.agent_root.clone(),
        )?;
        let project_trusted = self.inner.project_resources.allows()
            && self
                .snapshot()
                .trust
                .allows_project_resources(self.inner.options.headless);
        manager.load_project(project_trusted)?;
        manager.reload()?;
        Ok(manager)
    }

    pub(crate) fn bind_publication(
        &self,
        publication: &Arc<crate::application::generation::ApplicationPublication>,
    ) -> Result<()> {
        self.inner
            .publication
            .set(Arc::downgrade(publication))
            .map_err(|_| anyhow::anyhow!("resource manager publication is already bound"))
    }

    pub(crate) fn capture_application_generation(
        &self,
    ) -> Result<Arc<crate::application::generation::ApplicationGeneration>> {
        self.inner
            .publication
            .get()
            .and_then(Weak::upgrade)
            .map(|publication| publication.capture())
            .ok_or_else(|| anyhow::anyhow!("application generation is not published"))
    }

    pub(crate) fn capture_fn(&self) -> crate::application::generation::GenerationCaptureFn {
        let manager = self.clone();
        Arc::new(move || manager.capture_application_generation())
    }
    pub fn child_generation_capture(&self) -> crate::ChildGenerationCapture {
        crate::ChildGenerationCapture::new(self.capture_fn())
    }

    #[must_use]
    pub fn trust_store(&self) -> TrustStore {
        self.inner.trust_store.clone()
    }

    #[must_use]
    pub fn options(&self) -> ResourceManagerOptions {
        self.inner.options.clone()
    }

    pub fn rebuild_for_cwd(&self, cwd: impl Into<PathBuf>) -> Result<Self> {
        let mut options = self.options();
        options.cwd = cwd.into();
        Self::new(options)
    }

    /// Converts the active snapshot's explicit process extension manifests into
    /// launch specifications. Untrusted project resources, malformed manifests,
    /// and capabilities outside the host policy fail closed before process launch.
    pub fn extension_specs(
        &self,
        permissions: &crate::ExtensionPermissionSet,
    ) -> Result<Vec<crate::ExtensionSpec>> {
        match self.inner.publication.get().and_then(Weak::upgrade) {
            Some(publication) => publication
                .capture()
                .resource()
                .extension_specs(permissions),
            None => extension_specs_for_snapshot(
                &self.inner.initial_snapshot,
                &self.inner.initial_capabilities,
                permissions,
            ),
        }
    }

    pub(crate) fn stage_candidate(
        &self,
        provenance: Arc<GenerationProvenance>,
        composed_trust: Option<&TrustResolution>,
    ) -> Result<ResourceCandidate> {
        self.inner
            .agent_root
            .validate_live_root()
            .context("validating retained agent resource root")?;
        let capabilities = self.inner.initial_capabilities.clone();
        let agent_root = self.inner.agent_root.clone();
        let project_root = self.inner.project_root.clone();
        let settings = SettingsManager::load_phase_one_with_roots(
            &self.inner.options.cwd,
            &self.inner.options.agent_dir,
            agent_root.clone(),
            project_root.clone(),
        )?;
        let snapshot = build_candidate(
            &self.inner.options,
            &settings,
            &self.inner.trust_store,
            provenance.revision().get(),
            composed_trust,
            &capabilities,
            agent_root,
            project_root,
            self.inner.project_resources,
        )?;
        Ok(ResourceCandidate {
            provenance,
            capabilities,
            snapshot,
        })
    }
}

fn extension_specs_for_snapshot(
    snapshot: &ResourceSnapshot,
    capabilities: &Arc<CapabilityFs>,
    permissions: &crate::ExtensionPermissionSet,
) -> Result<Vec<crate::ExtensionSpec>> {
    let specs = crate::extension_specs_from_package_resources_with_capabilities(
        capabilities,
        &snapshot.package_extensions,
    )?;
    for spec in &specs {
        if let Some(capability) = spec
            .permissions
            .capabilities
            .iter()
            .find(|capability| !permissions.capabilities.contains(capability))
        {
            bail!(
                "extension {} requests capability {capability:?} outside the host policy",
                spec.id
            );
        }
        if let Some(capability) = spec
            .permissions
            .ui_capabilities
            .iter()
            .find(|capability| !permissions.ui_capabilities.contains(capability))
        {
            bail!(
                "extension {} requests UI capability {capability:?} outside the host policy",
                spec.id
            );
        }
    }
    Ok(specs)
}

fn marketplace_extension_resources_from_root(
    agent_root: &crate::fs::CapabilityRootLease,
    trust_store: &TrustStore,
) -> Result<Vec<crate::PackageResourceSpec>> {
    let extensions = match agent_root.open_directory(Path::new("extensions")) {
        Ok(directory) => directory,
        Err(crate::fs::FsError::System {
            kind: std::io::ErrorKind::NotFound,
            ..
        }) => return Ok(Vec::new()),
        Err(error) => return Err(error).context("opening retained marketplace extensions"),
    };
    let mut resources = Vec::new();
    for name in extensions.read_dir_names_rejecting_unsupported()? {
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let entry = extensions.open_entry_exact(Path::new(&name))?;
        let crate::fs::CapabilityEntry::Directory(directory) = entry else {
            continue;
        };
        let path = directory.display_path();
        let manifest_path = path.join(crate::PROCESS_EXTENSION_MANIFEST_FILE);
        let manifest = match directory.open_file(Path::new(crate::PROCESS_EXTENSION_MANIFEST_FILE))
        {
            Ok(file) => file,
            Err(crate::fs::FsError::System {
                kind: std::io::ErrorKind::NotFound,
                ..
            }) => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "opening marketplace extension manifest {}",
                        manifest_path.display()
                    )
                });
            }
        };
        let bytes = manifest.read_bounded(1024 * 1024).with_context(|| {
            format!(
                "reading marketplace extension manifest {}",
                manifest_path.display()
            )
        })?;
        let parsed: crate::ProcessExtensionManifest =
            serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "parsing marketplace extension manifest {}",
                    manifest_path.display()
                )
            })?;
        parsed.validate(&manifest_path).with_context(|| {
            format!(
                "validating marketplace extension manifest {}",
                manifest_path.display()
            )
        })?;
        let trusted =
            trust_store.resolve_exact(directory.authority_path())? == crate::TrustDecision::Trusted;
        if !trusted {
            continue;
        }
        resources.push(crate::PackageResourceSpec {
            kind: crate::PackageResourceKind::Extension,
            path,
            managed_root: Some(agent_root.authority_root().to_path_buf()),
            package_id: format!("marketplace:{}", name.to_string_lossy()),
            scope: crate::PackageScope::Global,
            trusted,
        });
    }
    resources.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(resources)
}

fn open_agent_root(
    capabilities: &Arc<CapabilityFs>,
    agent_dir: &Path,
    cwd: &Path,
) -> Result<OpenedAgentRoot> {
    let path = pi_sandbox::AbsolutePath::parse(agent_dir.to_path_buf())
        .map_err(|_| anyhow::anyhow!("invalid agent resource root"))?;
    let lease = capabilities
        .open_private_directory_allowing_link(crate::fs::CapabilityRootPurpose::AgentStore, path)
        .context("opening retained agent resource root")?;
    let project_boundary =
        crate::project_context_boundary(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    if !lease.authority_root().starts_with(&project_boundary) {
        return Ok(OpenedAgentRoot {
            lease,
            project_resources: ProjectResourceAccess::Enabled,
        });
    }
    let owner_home = home_dir().map(PathBuf::from);
    let is_operator_home_store = owner_home.as_ref().is_some_and(|home| {
        same_path(cwd, home)
            && !same_path(lease.authority_root(), home)
            && same_path(agent_dir, &agent_dir_path())
            && lease.authority_root().starts_with(home)
            && matches!(crate::resources::has_git_context_boundary(cwd), Ok(false))
    });
    if !is_operator_home_store {
        bail!("agent resource root resolves inside the project boundary");
    }
    Ok(OpenedAgentRoot {
        lease,
        project_resources: ProjectResourceAccess::SuppressedForAgentStoreOverlap,
    })
}

fn build_candidate(
    options: &ResourceManagerOptions,
    settings_manager: &SettingsManager,
    trust_store: &TrustStore,
    generation: u64,
    composed_trust: Option<&TrustResolution>,
    capabilities: &Arc<CapabilityFs>,
    agent_root: crate::fs::CapabilityRootLease,
    retained_project_root: Option<crate::fs::CapabilityRootLease>,
    project_resources: ProjectResourceAccess,
) -> Result<ResourceSnapshot> {
    settings_manager.load_project(false)?;
    settings_manager.reload()?;
    let default_trust = settings_manager
        .settings()
        .default_project_trust
        .unwrap_or(DefaultProjectTrust::Ask);
    // A decision the Application already composed through the fail-open hook
    let trust = match composed_trust {
        Some(trust) => trust.clone(),
        None => match retained_project_root.as_ref() {
            Some(root) => crate::trust::resolve_project_trust_from_root(
                trust_store,
                root,
                options.project_trust_override,
                default_trust,
                options.headless,
            )?,
            None => resolve_project_trust(
                trust_store,
                &options.cwd,
                options.project_trust_override,
                default_trust,
                options.headless,
            )?,
        },
    };
    let project_trusted = project_resources.allows()
        && trust.allows_project_resources(options.headless);
    settings_manager.load_project(project_trusted)?;
    settings_manager.reload()?;
    let settings = settings_manager.settings();
    let project_root = if project_trusted {
        match retained_project_root {
            Some(root) => Some(root),
            None => Some(capabilities.open_root(
                crate::fs::CapabilityRootPurpose::ProjectResources,
                pi_sandbox::AbsolutePath::parse(options.cwd.clone())
                    .map_err(|_| anyhow::anyhow!("invalid project resource root"))?,
                crate::fs::CapabilityAccess::ReadOnly,
                crate::fs::RootCreation::Existing,
            )?),
        }
    } else {
        None
    };
    let agents = crate::AgentCatalog::discover_with_roots(
        &crate::AgentDiscoveryOptions {
            cwd: options.cwd.clone(),
            agent_dir: options.agent_dir.clone(),
            project_trusted,
        },
        agent_root.clone(),
        project_root.clone(),
    )?
    .agents()
    .to_vec();
    let paths =
        ResourcePaths::discover_from_agent_dir(&options.cwd, &options.agent_dir, project_trusted);
    let packages = crate::PackageManager::for_retained_resource_resolution(
        &options.cwd,
        capabilities.clone(),
        agent_root.clone(),
        project_root.as_ref(),
        project_trusted,
    )?
    .resolve_resources()?;
    let context_files = if options.disable_context_files {
        Vec::new()
    } else {
        crate::resources::load_context_files_from_agent_root_lease(
            capabilities,
            &agent_root,
            project_root.as_ref(),
            &options.cwd,
            project_trusted,
        )?
    };
    let (mut skills, skill_diagnostics) = if options.disable_skills {
        (Vec::new(), Vec::new())
    } else {
        let skill_roots = discover_skill_roots(&options.cwd, &options.agent_dir, project_trusted);
        let project_boundary = project_root
            .as_ref()
            .map(|root| root.display_root().to_path_buf())
            .unwrap_or_else(|| {
                crate::project_context_boundary(&options.cwd).unwrap_or_else(|_| options.cwd.clone())
            });
        // Stamp even for untrusted projects: a managed User skills root must
        // not follow a link into the current project.
        let managed_project_boundary = Some(project_boundary.as_path());
        load_default_skills(
            capabilities,
            &agent_root,
            project_root.as_ref(),
            &skill_roots,
            managed_project_boundary,
        )
    };
    let mut diagnostics = skill_diagnostics
        .into_iter()
        .map(|diagnostic| ResourceDiagnostic {
            level: if diagnostic.kind == "error" {
                ResourceDiagnosticLevel::Error
            } else {
                ResourceDiagnosticLevel::Warning
            },
            message: diagnostic.message,
            path: Some(PathBuf::from(diagnostic.path)),
        })
        .collect::<Vec<_>>();
    let available_models = crate::available_models();
    let parent_model = pi_ai::Model::default();
    diagnostics.extend(agents.iter().filter_map(|agent| {
        crate::agent_compatibility_error(
            agent,
            settings.agents.get(&agent.name),
            &parent_model,
            &available_models,
        )
        .map(|error| ResourceDiagnostic {
            level: ResourceDiagnosticLevel::Warning,
            message: error.to_string(),
            path: agent.path.clone(),
        })
    }));
    diagnostics.extend(agents.iter().filter_map(|agent| {
        let unknown = crate::unsupported_agent_tools(agent, settings.agents.get(&agent.name));
        if unknown.is_empty() {
            return None;
        }
        Some(ResourceDiagnostic {
            level: ResourceDiagnosticLevel::Warning,
            message: crate::unknown_tools_warning(&agent.name, &unknown),
            path: agent.path.clone(),
        })
    }));

    let configured_skill_paths = settings.skills.iter().map(|path| {
        (
            PathBuf::from(path),
            crate::SkillSource::Explicit,
            true,
            None,
        )
    });
    let package_skill_paths = packages.skills.iter().map(|resource| {
        let source = match resource.scope {
            crate::PackageScope::Global => crate::SkillSource::PackageGlobal,
            crate::PackageScope::Project => crate::SkillSource::PackageProject,
        };
        (
            resource.path.clone(),
            source,
            resource.trusted,
            resource.managed_root.clone(),
        )
    });
    let explicit_skill_paths = options
        .explicit_skill_paths
        .iter()
        .cloned()
        .map(|path| (path, crate::SkillSource::Explicit, true, None));
    let mut skill_paths = configured_skill_paths
        .chain(package_skill_paths)
        .filter(|_| !options.disable_skills)
        .chain(explicit_skill_paths)
        .collect::<Vec<_>>();
    let mut validated_skill_paths = skill_paths
        .iter()
        .map(|(path, _, _, _)| path.clone())
        .collect::<Vec<_>>();
    validate_explicit_project_paths(
        capabilities,
        project_root.as_ref(),
        &options.cwd,
        &mut validated_skill_paths,
        project_trusted,
        "skill",
    )?;
    skill_paths.retain(|(path, _, _, _)| validated_skill_paths.contains(path));
    let project_boundary = project_root
        .as_ref()
        .map(|root| root.display_root().to_path_buf())
        .unwrap_or_else(|| {
            crate::project_context_boundary(&options.cwd).unwrap_or_else(|_| options.cwd.clone())
        });
    for (path, source, trusted, managed_root) in skill_paths {
        let resolved = resolve_explicit(&options.cwd, &path);
        if !resolved.is_file()
            && let Some(root) = project_root.as_ref()
            && let Ok(relative) = resolved.strip_prefix(root.display_root())
        {
            let (found, diagnostics) = crate::resources::load_skills_from_agent_root_lease(
                root,
                relative,
                source,
                trusted,
                source.allows_managed_links().then_some(&project_boundary),
            );
            if let Some(diagnostic) = diagnostics.into_iter().find(|diagnostic| diagnostic.kind == "error") {
                bail!("explicit skill {}: {}", resolved.display(), diagnostic.message);
            }
            skills.extend(found);
        } else {
            skills.extend(load_explicit_skills(
                capabilities,
                &resolved,
                source,
                trusted,
                &project_boundary,
                managed_root.as_deref(),
            )?);
        }
    }
    dedupe_skills(&skills)?;

    let mut prompt_paths = settings
        .prompts
        .iter()
        .map(PathBuf::from)
        .chain(
            packages
                .prompts
                .iter()
                .map(|resource| resource.path.clone()),
        )
        .filter(|_| !options.disable_prompt_templates)
        .chain(options.explicit_prompt_paths.iter().cloned())
        .collect::<Vec<_>>();
    validate_explicit_project_paths(
        capabilities,
        project_root.as_ref(),
        &options.cwd,
        &mut prompt_paths,
        project_trusted,
        "prompt template",
    )?;
    let (project_prompt_paths, non_project_prompt_paths): (Vec<_>, Vec<_>) = prompt_paths
        .into_iter()
        .partition(|path| project_root.as_ref().is_some_and(|root| {
            resolve_explicit(&options.cwd, path).starts_with(root.display_root())
        }));
    let mut prompts = load_prompt_templates(&LoadPromptTemplatesOptions {
        cwd: options.cwd.clone(),
        agent_dir: options.agent_dir.clone(),
        explicit_paths: non_project_prompt_paths,
        include_defaults: !options.disable_prompt_templates,
        include_project: false,
    })?;
    if project_trusted
        && !options.disable_prompt_templates
        && let Some(root) = project_root.as_ref()
    {
        prompts.extend(crate::prompt_templates::load_project_prompt_templates_from_root(
            root,
            &project_prompt_paths,
        )?);
    }

    let mut theme_paths = settings
        .themes
        .iter()
        .map(PathBuf::from)
        .chain(packages.themes.iter().map(|resource| resource.path.clone()))
        .filter(|_| !options.disable_themes)
        .chain(options.explicit_theme_paths.iter().cloned())
        .collect::<Vec<_>>();
    validate_explicit_project_paths(
        capabilities,
        project_root.as_ref(),
        &options.cwd,
        &mut theme_paths,
        project_trusted,
        "theme",
    )?;
    let theme_dirs = if options.disable_themes {
        &[]
    } else {
        paths.theme_dirs.as_slice()
    };
    let themes = load_themes(capabilities, theme_dirs, &options.cwd, theme_paths)?;
    let keybinding_files = validate_keybindings(capabilities, &paths.keybinding_files)?;
    let mut explicit_prompt_files = options
        .system_prompt_path
        .iter()
        .chain(options.system_prompt_source.iter())
        .chain(options.append_system_prompt_paths.iter().flatten())
        .chain(options.append_system_prompt_sources.iter().flatten())
        .cloned()
        .collect::<Vec<_>>();
    validate_explicit_project_paths(
        capabilities,
        project_root.as_ref(),
        &options.cwd,
        &mut explicit_prompt_files,
        project_trusted,
        "system prompt",
    )?;
    if !project_trusted {
        let project_root = if options.cwd.is_absolute() {
            options.cwd.clone()
        } else {
            std::env::current_dir()?.join(&options.cwd)
        };
        if let Some(path) = explicit_prompt_files
            .iter()
            .find(|path| resolve_explicit(&options.cwd, path).starts_with(&project_root))
        {
            bail!(
                "explicit project system prompt requires project trust: {} (pass --approve to allow it)",
                path.display()
            );
        }
    }
    let read_prompt = |path: &Path, description: &str| {
        if let Some(root) = project_root.as_ref()
            && let Ok(relative) = path.strip_prefix(root.display_root())
        {
            crate::resources::read_resource_text_from_root(root, relative, description)
        } else {
            crate::resources::read_resource_text_with_capabilities(capabilities, path, description)
        }
    };
    let system_prompt = match (&options.system_prompt_path, &options.system_prompt) {
        (Some(path), _) => Some(read_prompt(path, "system prompt")?),
        (None, Some(prompt)) => Some(prompt.clone()),
        (None, None) => load_first_existing(capabilities, &paths.system_prompt_files, "system prompt")?,
    };
    let append_system_prompt = if options.append_system_prompt.is_empty() {
        load_first_existing(capabilities, &paths.append_system_prompt_files, "append system prompt")?
            .into_iter()
            .collect()
    } else {
        options
            .append_system_prompt
            .iter()
            .enumerate()
            .map(|(index, prompt)| {
                match options
                    .append_system_prompt_paths
                    .get(index)
                    .and_then(Option::as_ref)
                {
                    Some(path) => read_prompt(path, "append system prompt"),
                    None => Ok(prompt.clone()),
                }
            })
            .collect::<Result<Vec<_>>>()?
    };
    if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.level == ResourceDiagnosticLevel::Error)
    {
        bail!("resource candidate contains validation errors");
    }
    let mut package_extensions = if options.disable_extensions {
        Vec::new()
    } else {
        packages.extensions
    };
    // Marketplace plugins (`agent_dir/extensions/<name>`) become extensions
    // only when the trust store marks them trusted; installed plugins are
    // trusted by `rpi plugin install`.
    if !options.disable_extensions {
        package_extensions.extend(marketplace_extension_resources_from_root(
            &agent_root,
            trust_store,
        )?);
    }
    // A composed re-stage (a decision the hook surfaces already made) omits
    // explicit project extensions the decision does not trust instead of
    // failing the build: the host hook denied the project, so its extensions
    // simply must not run. The plain store-based initial build keeps failing
    // closed so a user passing `--extension` into an untrusted project still
    // sees the actionable "pass --approve" error.
    let skip_untrusted_explicit_extensions = composed_trust.is_some();
    package_extensions.extend(explicit_extension_resources(
        capabilities,
        project_root.as_ref(),
        &options.cwd,
        &options.explicit_extension_paths,
        project_trusted,
        skip_untrusted_explicit_extensions,
    )?);
    validate_snapshot_size(
        &context_files,
        &skills,
        &agents,
        &prompts,
        &themes,
        &keybinding_files,
        system_prompt.as_deref(),
        &append_system_prompt,
    )?;
    agent_root
        .validate_live_root()
        .context("revalidating retained agent resource root after resource build")?;
    Ok(ResourceSnapshot {
        generation,
        cwd: options.cwd.clone(),
        trust,
        settings,
        context_files,
        skills,
        agents,
        prompts,
        themes,
        package_extensions,
        theme_dirs: paths.theme_dirs,
        keybinding_files,
        system_prompt,
        append_system_prompt,
        diagnostics,
        authority: ResourceAuthority::Unpublished,
    })
}

fn explicit_extension_resources(
    capabilities: &Arc<CapabilityFs>,
    retained_project_root: Option<&crate::fs::CapabilityRootLease>,
    cwd: &Path,
    raw_paths: &[PathBuf],
    project_trusted: bool,
    skip_untrusted: bool,
) -> Result<Vec<crate::PackageResourceSpec>> {
    let project_root = if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        std::env::current_dir()?.join(cwd)
    };
    let resolved = raw_paths
        .iter()
        .map(|raw| -> Result<Option<crate::PackageResourceSpec>> {
            let resolved = resolve_explicit(cwd, raw);
            let direct_manifest = resolved.file_name().and_then(std::ffi::OsStr::to_str)
                == Some(crate::PROCESS_EXTENSION_MANIFEST_FILE);
            let manifest = if direct_manifest {
                resolved
            } else {
                resolved.join(crate::PROCESS_EXTENSION_MANIFEST_FILE)
            };
            let project_local = manifest.starts_with(&project_root);
            if project_local {
                let root = retained_project_root
                    .ok_or_else(|| anyhow::anyhow!("project authority unavailable"))?;
                let relative = manifest
                    .strip_prefix(root.display_root())
                    .map_err(|_| anyhow::anyhow!("extension manifest is outside retained project authority"))?;
                root.open_file(relative)
                    .with_context(|| format!("opening extension manifest {}", manifest.display()))?;
            } else {
                let absolute = pi_sandbox::AbsolutePath::parse(manifest.clone())
                    .map_err(|_| anyhow::anyhow!("invalid extension manifest {}", manifest.display()))?;
                capabilities
                    .open_exact_file(crate::fs::CapabilityRootPurpose::OperatorImport, absolute)
                    .with_context(|| format!("opening extension manifest {}", manifest.display()))?;
            }
            if project_local && !project_trusted {
                if skip_untrusted { return Ok(None); }
                bail!("explicit project extension requires project trust: {} (pass --approve to allow it)", manifest.display());
            }
            Ok(Some(crate::PackageResourceSpec {
                kind: crate::PackageResourceKind::Extension,
                path: manifest,
                managed_root: None,
                package_id: "cli".to_owned(),
                scope: if project_local { crate::PackageScope::Project } else { crate::PackageScope::Global },
                trusted: !project_local || project_trusted,
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(resolved.into_iter().flatten().collect())
}

fn load_explicit_skills(
    capabilities: &Arc<CapabilityFs>,
    path: &Path,
    source: crate::SkillSource,
    trusted: bool,
    project_boundary: &Path,
    managed_root: Option<&Path>,
) -> Result<Vec<Skill>> {
    let (mut skills, diagnostics) =
        crate::resources::load_skills_from_dir_with_capabilities_and_boundary(
            capabilities,
            &path.to_string_lossy(),
            source,
            trusted,
            source.allows_managed_links(),
            source.allows_managed_links().then_some(project_boundary),
        );
    // A skill file is not a directory; the file reader below loads it.
    if !path.is_file() {
        if let Some(diagnostic) = diagnostics
            .into_iter()
            .find(|diagnostic| diagnostic.kind == "error")
        {
            bail!("explicit skill {}: {}", path.display(), diagnostic.message);
        }
    }
    if skills.is_empty() {
        let (skill, diagnostics) = if source.allows_managed_links() {
            let root = managed_root
                .or_else(|| path.parent())
                .ok_or_else(|| anyhow::anyhow!("managed skill has no source root"))?;
            let relative = path
                .strip_prefix(root)
                .map_err(|_| anyhow::anyhow!("managed skill is outside its source root"))?;
            let lease = capabilities.open_root(
                crate::fs::CapabilityRootPurpose::ProjectResources,
                pi_sandbox::AbsolutePath::parse(root.to_path_buf())
                    .map_err(|_| anyhow::anyhow!("invalid managed skill source root"))?,
                crate::fs::CapabilityAccess::ReadOnly,
                crate::fs::RootCreation::Existing,
            )?;
            let managed = lease.open_managed_file_within_root(relative)?;
            if managed.followed_link() && managed.resolved_path().starts_with(project_boundary) {
                bail!(
                    "explicit skill {}: managed skill link crosses the current project boundary",
                    path.display()
                );
            }
            let resolved = managed.resolved_path().to_path_buf();
            let content = String::from_utf8(managed.read_bounded(crate::MAX_RESOURCE_FILE_BYTES)?)
                .map_err(|error| anyhow::anyhow!("managed skill is not valid UTF-8: {error}"))?;
            crate::resources::load_skill_from_text_for_resource_manager(
                &resolved, &content, source, trusted,
            )
        } else {
            let absolute = pi_sandbox::AbsolutePath::parse(path.to_path_buf())
                .map_err(|_| anyhow::anyhow!("invalid explicit skill path"))?;
            let file = capabilities
                .open_exact_file(crate::fs::CapabilityRootPurpose::ProjectResources, absolute)?;
            let content = String::from_utf8(file.read_bounded(crate::MAX_RESOURCE_FILE_BYTES)?)
                .map_err(|error| anyhow::anyhow!("explicit skill is not valid UTF-8: {error}"))?;
            crate::resources::load_skill_from_text_for_resource_manager(
                path, &content, source, trusted,
            )
        };
        if let Some(diagnostic) = diagnostics
            .into_iter()
            .find(|diagnostic| diagnostic.kind == "error")
        {
            bail!("explicit skill {}: {}", path.display(), diagnostic.message);
        }
        skills = skill.into_iter().collect();
    }
    if source.allows_managed_links() {
        let root = managed_root
            .or_else(|| path.parent())
            .ok_or_else(|| anyhow::anyhow!("managed skill has no source root"))?;
        for skill in &mut skills {
            if skill.managed_project_boundary.is_none() {
                skill.managed_project_boundary = Some(project_boundary.to_path_buf());
            }
            if skill.managed_root.is_none() {
                skill.managed_root = Some(root.to_path_buf());
            }
        }
    }
    Ok(skills)
}

fn dedupe_skills(skills: &[Skill]) -> Result<()> {
    let mut names = BTreeMap::<&str, &str>::new();
    for skill in skills {
        if let Some(winner) = names.insert(&skill.name, &skill.file_path) {
            bail!(
                "skill name {} collision: {} conflicts with {}",
                skill.name,
                skill.file_path,
                winner
            );
        }
    }
    Ok(())
}

fn load_themes(
    capabilities: &Arc<CapabilityFs>,
    default_dirs: &[PathBuf],
    cwd: &Path,
    explicit_paths: Vec<PathBuf>,
) -> Result<Vec<ThemeResource>> {
    let mut files = Vec::new();
    for directory in default_dirs {
        match collect_json_files(&capabilities, directory, &mut files) {
            Ok(()) => {}
            // Absent default theme directories are an empty theme source.
            Err(error) if fs_error_is_missing(&error) => {}
            Err(error) => return Err(error),
        }
    }
    for raw in explicit_paths {
        let path = resolve_explicit(cwd, &raw);
        match collect_json_files(&capabilities, &path, &mut files) {
            Ok(()) => {}
            // An explicitly configured theme path must exist; a missing one is
            // a hard error rather than a silent no-op.
            Err(error) if fs_error_is_missing(&error) => {
                bail!("explicit theme path does not exist: {}", path.display());
            }
            // The path exists but is not a directory: treat it as a single
            // theme file (parsing below still fails closed on bad content).
            Err(error) if fs_error_is_not_a_directory(&error) => files.push(path),
            Err(error) => return Err(error),
        }
    }
    let mut themes = Vec::new();
    let mut names = BTreeMap::<String, PathBuf>::new();
    for file in files {
        let content =
            crate::resources::read_resource_text_with_capabilities(&capabilities, &file, "theme")?;
        let value: Value = serde_json::from_str(&content)
            .with_context(|| format!("parsing theme {}", file.display()))?;
        let object = value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("theme {} must be a JSON object", file.display()))?;
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty() && !name.contains('/'))
            .ok_or_else(|| {
                anyhow::anyhow!("theme {} has an invalid or missing name", file.display())
            })?;
        let colors = object
            .get("colors")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow::anyhow!("theme {} is missing colors", file.display()))?;
        if colors.is_empty() {
            bail!("theme {} colors must not be empty", file.display());
        }
        for (role, color) in colors {
            if !(color.is_string() || color.as_u64().is_some_and(|value| value <= 255)) {
                bail!(
                    "theme {} color {role} must be a string or 0..255",
                    file.display()
                );
            }
        }
        if let Some(existing) = names.insert(name.to_owned(), file.clone()) {
            bail!(
                "theme name {name} collision: {} conflicts with {}",
                file.display(),
                existing.display()
            );
        }
        themes.push(ThemeResource {
            name: name.to_owned(),
            path: file,
        });
    }
    Ok(themes)
}

fn validate_explicit_project_paths(
    capabilities: &Arc<CapabilityFs>,
    retained_project_root: Option<&crate::fs::CapabilityRootLease>,
    cwd: &Path,
    paths: &mut Vec<PathBuf>,
    project_trusted: bool,
    kind: &str,
) -> Result<()> {
    let project_root = retained_project_root
        .map(|root| root.display_root().to_path_buf())
        .unwrap_or_else(|| cwd.to_path_buf());
    for raw in paths.iter_mut() {
        let resolved = resolve_explicit(cwd, raw);
        if !raw.is_absolute() && !resolved.starts_with(&project_root) {
            bail!(
                "explicit {kind} path escapes the working directory: {}",
                raw.display()
            );
        }
        if resolved.starts_with(&project_root) && !project_trusted {
            bail!(
                "explicit project {kind} requires project trust: {} (pass --approve to allow it)",
                resolved.display()
            );
        }
        if let Some(root) = retained_project_root
            && let Ok(relative) = resolved.strip_prefix(root.display_root())
        {
            root.open_entry(relative)?;
        } else {
            let absolute = pi_sandbox::AbsolutePath::parse(resolved.clone())
                .map_err(|_| anyhow::anyhow!("invalid {kind} path {}", resolved.display()))?;
            capabilities.open_exact_parent(
                crate::fs::CapabilityRootPurpose::OperatorImport,
                absolute,
                crate::fs::CapabilityAccess::ReadOnly,
                crate::fs::RootCreation::Existing,
            )?;
        }
        *raw = resolved;
    }
    Ok(())
}

fn validate_snapshot_size(
    context_files: &[ContextFile],
    skills: &[Skill],
    agents: &[crate::AgentDefinition],
    prompts: &[PromptTemplate],
    themes: &[ThemeResource],
    keybinding_files: &[PathBuf],
    system_prompt: Option<&str>,
    append_system_prompt: &[String],
) -> Result<()> {
    let mut total = 0usize;
    let mut add = |bytes: usize, kind: &str| -> Result<()> {
        total = total.saturating_add(bytes);
        if total > crate::MAX_RESOURCE_SNAPSHOT_BYTES {
            bail!(
                "resource snapshot exceeds {} byte limit while adding {kind}",
                crate::MAX_RESOURCE_SNAPSHOT_BYTES
            );
        }
        Ok(())
    };
    for context in context_files {
        add(context.content.len(), "context file")?;
    }
    for skill in skills {
        add(skill.description.len() + skill.name.len(), "skill")?;
    }
    for agent in agents {
        add(
            agent.system_prompt.len() + agent.description.len(),
            "agent definition",
        )?;
    }
    for prompt in prompts {
        add(
            prompt.content.len() + prompt.description.len(),
            "prompt template",
        )?;
    }
    for theme in themes {
        add(theme.name.len(), "theme")?;
    }
    for path in keybinding_files {
        add(path.as_os_str().len(), "keybindings")?;
    }
    if let Some(prompt) = system_prompt {
        add(prompt.len(), "system prompt")?;
    }
    for prompt in append_system_prompt {
        add(prompt.len(), "append system prompt")?;
    }
    Ok(())
}

fn collect_json_files(
    capabilities: &Arc<CapabilityFs>,
    directory: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<()> {
    let absolute = pi_sandbox::AbsolutePath::parse(directory.to_path_buf())
        .map_err(|_| anyhow::anyhow!("invalid theme directory {}", directory.display()))?;
    let lease = capabilities.open_read_directory_allowing_link(
        crate::fs::CapabilityRootPurpose::OperatorImport,
        absolute,
    )?;
    let mut entries = lease.read_dir(Path::new(""))?;
    while let Some(entry) = entries.next()? {
        if matches!(entry.entry(), crate::fs::CapabilityEntry::File(_))
            && Path::new(entry.name())
                .extension()
                .is_some_and(|extension| extension == "json")
        {
            files.push(entry.entry().display_path());
        }
    }
    Ok(())
}

fn fs_error_is_missing(error: &anyhow::Error) -> bool {
    error.chain().any(|source| {
        source
            .downcast_ref::<crate::fs::FsError>()
            .is_some_and(|error| {
                matches!(
                    error,
                    crate::fs::FsError::System {
                        kind: std::io::ErrorKind::NotFound,
                        ..
                    }
                )
            })
    })
}
fn fs_error_is_not_a_directory(error: &anyhow::Error) -> bool {
    error.chain().any(|source| {
        source
            .downcast_ref::<crate::fs::FsError>()
            .is_some_and(|error| {
                matches!(
                    error,
                    crate::fs::FsError::UnsupportedEntryType
                        | crate::fs::FsError::System {
                            kind: std::io::ErrorKind::NotADirectory,
                            ..
                        }
                )
            })
    })
}

fn validate_keybindings(
    capabilities: &Arc<CapabilityFs>,
    paths: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    let mut existing = Vec::new();
    for path in paths {
        let content = match crate::resources::read_resource_text_with_capabilities(
            capabilities,
            path,
            "keybindings",
        ) {
            Ok(content) => content,
            Err(error) if fs_error_is_missing(&error) => continue,
            Err(error) => return Err(error),
        };
        let value: Value = serde_json::from_str(&content)
            .with_context(|| format!("parsing keybindings {}", path.display()))?;
        if !value.is_object() {
            bail!("keybindings {} must be a JSON object", path.display());
        }
        existing.push(path.clone());
    }
    Ok(existing)
}

fn load_first_existing(
    capabilities: &Arc<CapabilityFs>,
    paths: &[PathBuf],
    description: &str,
) -> Result<Option<String>> {
    for path in paths {
        match crate::resources::read_resource_text_with_capabilities(
            capabilities,
            path,
            description,
        ) {
            Ok(content) => return Ok(Some(content)),
            Err(error) if fs_error_is_missing(&error) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

fn resolve_explicit(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RESOURCE_HOME_OVERRIDE, ResourceManager, ResourceManagerOptions, ResourcePaths,
        ResourceSnapshot,
    };
    use crate::HookEvent;
    use std::fs;
    use std::path::{Path, PathBuf};
    fn create_git_boundary(repository: &Path) {
        let git_dir = repository.join(".git");
        fs::create_dir_all(&git_dir).expect("git directory");
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/test\n").expect("git HEAD");
    }
    fn manager_options(cwd: &Path, agent_dir: &Path, trusted: bool) -> ResourceManagerOptions {
        let mut options = ResourceManagerOptions::new(cwd);
        options.agent_dir = agent_dir.to_path_buf();
        options.project_trust_override = Some(trusted);
        options.disable_extensions = true;
        options.disable_prompt_templates = true;
        options.disable_themes = true;
        options.disable_context_files = true;
        options
    }

    fn manager_snapshot(
        cwd: &Path,
        agent_dir: &Path,
        trusted: bool,
    ) -> std::sync::Arc<ResourceSnapshot> {
        ResourceManager::new(manager_options(cwd, agent_dir, trusted))
            .expect("resource manager")
            .snapshot()
    }

    #[test]
    fn pre_trust_candidate_uses_only_global_hooks() {
        let root = tempfile::tempdir().expect("temporary root");
        let cwd = root.path().join("project");
        let agent_dir = root.path().join("agent");
        fs::create_dir_all(&cwd).expect("project directory");
        fs::create_dir_all(&agent_dir).expect("agent directory");
        fs::write(
            agent_dir.join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"},"hooks":[{"event":"pre_trust_decision","command":["/global/hook"]}]}"#,
        )
        .expect("global settings");
        let resources = ResourceManager::new(manager_options(&cwd, &agent_dir, true))
            .expect("resource manager");
        let suffix = uuid::Uuid::now_v7();
        let api = format!("resource-manager-api-{suffix}");
        let provider = format!("resource-manager-provider-{suffix}");
        let model = pi_ai::Model {
            id: format!("resource-manager-model-{suffix}"),
            name: "Resource Manager Test Model".to_owned(),
            api: api.clone(),
            provider: provider.clone(),
            base_url: "http://localhost:0".to_owned(),
            ..pi_ai::Model::default()
        };
        let _faux =
            pi_ai::providers::register_faux_provider(pi_ai::providers::FauxProviderOptions {
                api,
                provider,
                models: vec![model.clone()],
                chunk_size: 1,
            });
        let session = crate::Session::new_with_additional_tools_filtered_and_discovery(
            crate::SessionOptions {
                model,
                cwd: cwd.clone(),
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
            Default::default(),
            crate::ResourceDiscovery::Disabled,
        )
        .expect("session");
        fs::create_dir_all(root.path().join("sessions")).expect("session directory");
        session.set_session_dir(root.path().join("sessions"));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime
            .block_on(session.attach_resources(resources.clone()))
            .expect("attach resources");
        let application = runtime.block_on(crate::Application::new(session));
        let generation = application.capture_generation();
        let candidate = resources
            .stage_global_pre_trust_candidate(generation.provenance().clone())
            .expect("candidate")
            .expect("global pre-trust hook");
        let hooks = candidate.settings().hooks.as_ref().expect("hooks");
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].event, HookEvent::PreTrustDecision);
        assert_eq!(hooks[0].command, vec!["/global/hook"]);
        assert_ne!(hooks[0].command, vec!["/project/hook"]);
    }

    #[test]
    fn resource_paths_order_global_ancestors_then_project() {
        let root = tempfile::tempdir().expect("temporary root");
        let home = root.path().join("home");
        let repository = root.path().join("work").join("repository");
        let cwd = repository.join("crates").join("pi-coding");
        fs::create_dir_all(&cwd).expect("project directory");
        create_git_boundary(&repository);
        let agent_dir = home.join(".rpi").join("agent");

        let paths = ResourcePaths::discover_from_agent_dir(&cwd, &agent_dir, true);

        assert_eq!(
            paths.skill_paths,
            vec![
                agent_dir.join("skills"),
                home.join(".agents").join("skills"),
                repository.join(".agents").join("skills"),
                repository.join("crates").join(".agents").join("skills"),
                cwd.join(".agents").join("skills"),
                cwd.join(".rpi").join("skills"),
            ]
        );
    }

    #[test]
    fn resource_paths_exclude_all_project_roots_when_untrusted() {
        let root = tempfile::tempdir().expect("temporary root");
        let home = root.path().join("home");
        let cwd = root.path().join("repository").join("nested");
        fs::create_dir_all(&cwd).expect("project directory");
        create_git_boundary(&root.path().join("repository"));
        let agent_dir = home.join(".rpi").join("agent");

        let paths = ResourcePaths::discover_from_agent_dir(&cwd, &agent_dir, false);

        assert_eq!(
            paths.skill_paths,
            vec![
                agent_dir.join("skills"),
                home.join(".agents").join("skills")
            ]
        );
    }

    #[test]
    fn resource_paths_stop_at_nearest_git_root() {
        let root = tempfile::tempdir().expect("temporary root");
        let outer = root.path().join("outer");
        let repository = outer.join("repository");
        let cwd = repository.join("nested");
        fs::create_dir_all(&cwd).expect("project directory");
        create_git_boundary(&repository);
        let agent_dir = root.path().join("home").join(".rpi").join("agent");

        let paths = ResourcePaths::discover_from_agent_dir(&cwd, &agent_dir, true);

        assert!(
            paths
                .skill_paths
                .contains(&repository.join(".agents").join("skills"))
        );
        assert!(
            paths
                .skill_paths
                .contains(&cwd.join(".agents").join("skills"))
        );
        assert!(
            !paths
                .skill_paths
                .contains(&outer.join(".agents").join("skills"))
        );
        assert!(
            !paths
                .skill_paths
                .contains(&root.path().join(".agents").join("skills"))
        );
    }

    #[test]
    fn resource_paths_without_git_root_use_only_cwd_and_deduplicate() {
        let root = tempfile::tempdir().expect("temporary root");
        let home = root.path().join("home");
        fs::create_dir_all(&home).expect("home directory");
        let agent_dir = home.join(".rpi").join("agent");

        let paths = RESOURCE_HOME_OVERRIDE.with(|override_home| {
            let previous = override_home.replace(Some(home.clone()));
            let paths = ResourcePaths::discover_from_agent_dir(&home, &agent_dir, true);
            override_home.replace(previous);
            paths
        });

        assert_eq!(
            paths.skill_paths,
            vec![
                agent_dir.join("skills"),
                home.join(".agents").join("skills"),
                home.join(".rpi").join("skills"),
            ]
        );
    }

    #[test]
    fn resource_paths_without_git_exclude_cwd_agents_when_untrusted() {
        let root = tempfile::tempdir().expect("temporary root");
        let platform_home = root.path().join("platform-home");
        let cwd = root.path().join("project");
        let agent_dir = cwd.join(".rpi").join("agent");
        fs::create_dir_all(&cwd).expect("project directory");

        let paths = RESOURCE_HOME_OVERRIDE.with(|override_home| {
            let previous = override_home.replace(Some(platform_home.clone()));
            let paths = ResourcePaths::discover_from_agent_dir(&cwd, &agent_dir, false);
            override_home.replace(previous);
            paths
        });

        assert_eq!(
            paths.skill_paths,
            vec![
                agent_dir.join("skills"),
                platform_home.join(".agents").join("skills"),
            ]
        );
        assert!(
            !paths
                .skill_paths
                .contains(&cwd.join(".agents").join("skills"))
        );
    }

    fn write_skill(root: &std::path::Path, name: &str) {
        write_skill_with_description(root, name, &format!("{name} skill"));
    }

    fn write_skill_with_description(root: &std::path::Path, name: &str, description: &str) {
        fs::create_dir_all(root).expect("skill directory");
        fs::write(
            root.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n"),
        )
        .expect("skill markdown");
    }

    #[test]
    fn resource_manager_snapshot_loads_global_and_ancestor_agents_skills() {
        let root = tempfile::tempdir().expect("temporary root");
        let home = root.path().join("home");
        let repository = root.path().join("repository");
        let cwd = repository.join("nested");
        let agent_dir = home.join(".rpi").join("agent");
        fs::create_dir_all(&cwd).expect("project directory");
        fs::create_dir_all(&agent_dir).expect("agent directory");

        create_git_boundary(&repository);
        write_skill(
            &home.join(".agents").join("skills").join("global"),
            "global-agents",
        );
        write_skill(
            &repository.join(".agents").join("skills").join("repository"),
            "repository-agents",
        );
        write_skill(
            &cwd.join(".agents").join("skills").join("nested"),
            "nested-agents",
        );
        write_skill(
            &cwd.join(".rpi").join("skills").join("project"),
            "project-pi",
        );
        let snapshot = manager_snapshot(&cwd, &agent_dir, true);
        let names = snapshot
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>();

        assert!(names.contains(&"global-agents"), "{names:?}");
        assert!(names.contains(&"repository-agents"), "{names:?}");
        assert!(names.contains(&"nested-agents"), "{names:?}");
        assert!(names.contains(&"project-pi"), "{names:?}");
    }
    #[test]
    fn resource_paths_do_not_treat_project_agent_dir_parent_as_global_home() {
        let root = tempfile::tempdir().expect("temporary root");
        let repository = root.path().join("repository");
        let cwd = repository.join("nested");
        fs::create_dir_all(&cwd).expect("project directory");
        create_git_boundary(&repository);
        let agent_dir = repository.join(".rpi").join("agent");
        let paths = ResourcePaths::discover_from_agent_dir(&cwd, &agent_dir, false);

        assert_eq!(paths.skill_paths.first(), Some(&agent_dir.join("skills")));
        assert!(
            !paths
                .skill_paths
                .contains(&repository.join(".agents").join("skills"))
        );
    }

    #[test]
    fn project_local_agent_dir_is_rejected_regardless_of_trust() {
        let root = tempfile::tempdir().expect("temporary root");
        let repository = root.path().join("repository");
        let cwd = repository.join("nested");
        let agent_dir = repository.join(".rpi").join("agent");
        fs::create_dir_all(&cwd).expect("project directory");
        fs::create_dir_all(&agent_dir).expect("agent directory");
        create_git_boundary(&repository);

        for trusted in [false, true] {
            let error = ResourceManager::new(manager_options(&cwd, &agent_dir, trusted))
                .err()
                .expect("project-local agent root must fail closed");
            assert_eq!(
                error.to_string(),
                "agent resource root resolves inside the project boundary"
            );
        }
    }

    #[test]
    fn owner_home_workspace_keeps_agent_store_global() {
        let root = tempfile::tempdir().expect("temporary root");
        let home = root.path().join("operator-home");
        fs::create_dir_all(&home).expect("home directory");

        crate::resources::with_home_override(&home, || {
            let agent_dir = crate::agent_dir_path();
            fs::create_dir_all(&agent_dir).expect("agent directory");
            write_skill(&agent_dir.join("skills/global"), "global");
            write_skill(&home.join(".agents/skills/project"), "project");
            let workspace = crate::WorkspaceRoots::new(&home, Vec::<PathBuf>::new())
                .expect("home workspace");
            let resources = ResourceManager::new_for_workspace(
                manager_options(&home, &agent_dir, true),
                &workspace,
            )
            .expect("owner home resource manager");
            let snapshot = resources.snapshot();

            assert_eq!(snapshot.cwd, home);
            let global = snapshot
                .skills
                .iter()
                .find(|skill| skill.name == "global")
                .expect("global skill");
            assert_eq!(global.source, crate::SkillSource::User);
            assert!(!snapshot.skills.iter().any(|skill| skill.name == "project"));
        });
    }

    #[test]
    fn non_home_workspace_agent_store_stays_rejected() {
        let root = tempfile::tempdir().expect("temporary root");
        let home = root.path().join("operator-home");
        let project = root.path().join("plain-directory");
        let agent_dir = project.join(".rpi");
        fs::create_dir_all(&home).expect("home directory");
        fs::create_dir_all(&agent_dir).expect("agent directory");

        crate::resources::with_home_override(&home, || {
            let error = ResourceManager::new(manager_options(&project, &agent_dir, true))
                .err()
                .expect("non-home agent store must fail closed");
            assert_eq!(
                error.to_string(),
                "agent resource root resolves inside the project boundary"
            );
        });
    }

    #[test]
    fn git_owner_home_agent_store_stays_rejected() {
        let root = tempfile::tempdir().expect("temporary root");
        let home = root.path().join("operator-home");
        fs::create_dir_all(&home).expect("home directory");
        create_git_boundary(&home);

        crate::resources::with_home_override(&home, || {
            let agent_dir = crate::agent_dir_path();
            fs::create_dir_all(&agent_dir).expect("agent directory");
            let error = ResourceManager::new(manager_options(&home, &agent_dir, true))
                .err()
                .expect("Git home agent store must fail closed");
            assert_eq!(
                error.to_string(),
                "agent resource root resolves inside the project boundary"
            );
        });
    }

    #[test]
    fn owner_home_workspace_rejects_home_as_agent_store() {
        let root = tempfile::tempdir().expect("temporary root");
        let home = root.path().join("operator-home");
        fs::create_dir_all(&home).expect("home directory");

        crate::resources::with_home_override(&home, || {
            let error = ResourceManager::new(manager_options(&home, &home, true))
                .err()
                .expect("home root must not become the agent store");
            assert_eq!(
                error.to_string(),
                "agent resource root resolves inside the project boundary"
            );
        });
    }
    #[test]
    fn resource_manager_snapshot_excludes_untrusted_ancestor_agents_skills() {
        let root = tempfile::tempdir().expect("temporary root");
        let home = root.path().join("home");
        let repository = root.path().join("repository");
        let cwd = repository.join("nested");
        let agent_dir = home.join(".rpi").join("agent");
        fs::create_dir_all(&cwd).expect("project directory");
        fs::create_dir_all(&agent_dir).expect("agent directory");
        create_git_boundary(&repository);
        write_skill(
            &home.join(".agents").join("skills").join("global"),
            "global-agents",
        );
        write_skill(
            &repository.join(".agents").join("skills").join("repository"),
            "repository-agents",
        );
        let snapshot = manager_snapshot(&cwd, &agent_dir, false);
        let names = snapshot
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>();

        assert!(names.contains(&"global-agents"), "{names:?}");
        assert!(!names.contains(&"repository-agents"), "{names:?}");
    }

    #[test]
    fn resource_manager_snapshot_prefers_project_pi_for_duplicate_skill_name() {
        let root = tempfile::tempdir().expect("temporary root");
        let home = root.path().join("home");
        let repository = root.path().join("repository");
        let cwd = repository.join("nested");
        let agent_dir = home.join(".rpi").join("agent");
        fs::create_dir_all(&cwd).expect("project directory");
        fs::create_dir_all(&agent_dir).expect("agent directory");
        create_git_boundary(&repository);
        write_skill_with_description(&home.join(".agents/skills/shared"), "shared", "global");
        write_skill_with_description(
            &repository.join(".agents/skills/shared"),
            "shared",
            "ancestor",
        );
        write_skill_with_description(&cwd.join(".agents/skills/shared"), "shared", "deepest");
        write_skill_with_description(&cwd.join(".rpi/skills/shared"), "shared", "project pi");
        let snapshot = manager_snapshot(&cwd, &agent_dir, true);
        let shared = snapshot
            .skills
            .iter()
            .find(|skill| skill.name == "shared")
            .expect("shared");

        assert_eq!(shared.description, "project pi");
        assert_eq!(shared.source, crate::SkillSource::Project);
        assert_eq!(
            snapshot
                .skills
                .iter()
                .filter(|skill| skill.name == "shared")
                .count(),
            1
        );
    }

    #[test]
    fn resource_manager_snapshot_prefers_deepest_agents_skill() {
        let root = tempfile::tempdir().expect("temporary root");
        let repository = root.path().join("repository");
        let cwd = repository.join("nested");
        let agent_dir = root.path().join("home/.rpi/agent");
        fs::create_dir_all(&cwd).expect("project directory");
        fs::create_dir_all(&agent_dir).expect("agent directory");
        create_git_boundary(&repository);
        write_skill_with_description(
            &repository.join(".agents/skills/shared"),
            "shared",
            "shallow",
        );
        write_skill_with_description(&cwd.join(".agents/skills/shared"), "shared", "deepest");
        let snapshot = manager_snapshot(&cwd, &agent_dir, true);
        let shared = snapshot
            .skills
            .iter()
            .find(|skill| skill.name == "shared")
            .expect("shared");

        assert_eq!(shared.description, "deepest");
        assert_eq!(
            snapshot
                .skills
                .iter()
                .filter(|skill| skill.name == "shared")
                .count(),
            1
        );
    }

    #[test]
    fn overlapping_platform_home_agents_obeys_project_trust_and_source() {
        let root = tempfile::tempdir().expect("temporary root");
        let cwd = root.path().join("project");
        let agent_dir = root.path().join("agent-home");
        fs::create_dir_all(&cwd).expect("project directory");
        fs::create_dir_all(&agent_dir).expect("agent directory");
        write_skill(&cwd.join(".agents/skills/overlap"), "overlap");

        RESOURCE_HOME_OVERRIDE.with(|override_home| {
            let previous = override_home.replace(Some(cwd.clone()));
            let untrusted = manager_snapshot(&cwd, &agent_dir, false);
            assert!(!untrusted.skills.iter().any(|skill| skill.name == "overlap"));

            let trusted = manager_snapshot(&cwd, &agent_dir, true);
            override_home.replace(previous);
            let overlap = trusted
                .skills
                .iter()
                .find(|skill| skill.name == "overlap")
                .expect("overlap");
            assert_eq!(overlap.source, crate::SkillSource::Project);
        });
    }

    #[cfg(unix)]
    #[test]
    fn platform_global_agents_symlink_into_project_is_excluded_when_untrusted() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("temporary root");
        let platform_home = root.path().join("home");
        let repository = root.path().join("repository");
        let cwd = repository.join("nested");
        let agent_dir = platform_home.join(".rpi/agent");
        let project_agents = repository.join(".agents/skills");
        fs::create_dir_all(&cwd).expect("project directory");
        fs::create_dir_all(&agent_dir).expect("agent directory");
        create_git_boundary(&repository);
        write_skill(&project_agents.join("linked"), "linked-project");
        fs::create_dir_all(platform_home.join(".agents")).expect("global agents parent");
        symlink(&project_agents, platform_home.join(".agents/skills")).expect("skills symlink");

        RESOURCE_HOME_OVERRIDE.with(|override_home| {
            let previous = override_home.replace(Some(platform_home.clone()));
            let snapshot = manager_snapshot(&cwd, &agent_dir, false);
            override_home.replace(previous);
            assert!(
                !snapshot
                    .skills
                    .iter()
                    .any(|skill| skill.name == "linked-project")
            );
        });
    }

    #[test]
    fn aliased_global_root_keeps_deepest_project_precedence() {
        let root = tempfile::tempdir().expect("temporary root");
        let repository = root.path().join("repository");
        let cwd = repository.join("nested");
        let agent_dir = root.path().join("agent-home/.rpi/agent");
        fs::create_dir_all(&cwd).expect("project directory");
        fs::create_dir_all(&agent_dir).expect("agent directory");
        create_git_boundary(&repository);
        write_skill_with_description(
            &repository.join(".agents/skills/shared"),
            "shared",
            "shallow",
        );
        write_skill_with_description(&cwd.join(".agents/skills/shared"), "shared", "deepest");

        RESOURCE_HOME_OVERRIDE.with(|override_home| {
            let previous = override_home.replace(Some(cwd.clone()));
            let snapshot = manager_snapshot(&cwd, &agent_dir, true);
            override_home.replace(previous);
            let shared = snapshot
                .skills
                .iter()
                .find(|skill| skill.name == "shared")
                .expect("shared");

            assert_eq!(shared.description, "deepest");
            assert_eq!(shared.source, crate::SkillSource::Project);
        });
    }

    fn write_persona(root: &Path, name: &str, description: &str) -> PathBuf {
        let dir = root.join("personas").join(name);
        fs::create_dir_all(&dir).expect("persona dir");
        fs::write(
            dir.join("persona.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n{name} prompt"),
        )
        .expect("persona.md");
        dir
    }

    #[test]
    fn resource_snapshot_includes_user_persona_atomically() {
        // A user persona discovered under agent_dir/personas appears in the
        // snapshot's agents atomically (with the rest of the resource build),
        // marked Persona with a valid persona_root.
        let root = tempfile::tempdir().expect("temporary root");
        let home = root.path().join("home");
        let cwd = root.path().join("project");
        let agent_dir = home.join(".rpi").join("agent");
        fs::create_dir_all(&cwd).expect("project directory");
        fs::create_dir_all(&agent_dir).expect("agent directory");
        let mentor_dir = write_persona(&agent_dir, "mentor", "durable mentor");
        // Untrusted: the user persona is global and must still load.
        let snapshot = manager_snapshot(&cwd, &agent_dir, false);
        let mentor = snapshot
            .agents
            .iter()
            .find(|agent| agent.name == "mentor")
            .expect("persona in snapshot.agents");
        assert_eq!(
            mentor.kind,
            crate::orchestration::AgentDefinitionKind::Persona
        );
        assert!(mentor.is_persona());
        assert_eq!(
            mentor.source,
            crate::orchestration::AgentDefinitionSource::User
        );
        assert_eq!(mentor.persona_root().as_deref(), Some(mentor_dir.as_path()));
        assert_eq!(snapshot.cwd, cwd);
    }

    #[test]
    fn resource_snapshot_excludes_untrusted_project_persona() {
        let root = tempfile::tempdir().expect("temporary root");
        let home = root.path().join("home");
        let cwd = root.path().join("project");
        let agent_dir = home.join(".rpi").join("agent");
        fs::create_dir_all(&cwd).expect("project directory");
        fs::create_dir_all(&agent_dir).expect("agent directory");
        create_git_boundary(&cwd);
        write_persona(&cwd.join(".rpi"), "mentor", "project mentor");
        let snapshot = manager_snapshot(&cwd, &agent_dir, false);
        assert!(
            !snapshot.agents.iter().any(|agent| agent.name == "mentor"),
            "untrusted project persona must not be in snapshot"
        );
    }

    #[test]
    fn resource_snapshot_includes_project_persona_when_trusted() {
        let root = tempfile::tempdir().expect("temporary root");
        let home = root.path().join("home");
        let cwd = root.path().join("project");
        let agent_dir = home.join(".rpi").join("agent");
        fs::create_dir_all(&cwd).expect("project directory");
        fs::create_dir_all(&agent_dir).expect("agent directory");
        create_git_boundary(&cwd);
        let mentor_dir = write_persona(&cwd.join(".rpi"), "mentor", "project mentor");
        let snapshot = manager_snapshot(&cwd, &agent_dir, true);
        let mentor = snapshot
            .agents
            .iter()
            .find(|agent| agent.name == "mentor")
            .expect("project persona in snapshot.agents");
        assert_eq!(
            mentor.kind,
            crate::orchestration::AgentDefinitionKind::Persona
        );
        assert_eq!(
            mentor.source,
            crate::orchestration::AgentDefinitionSource::Project
        );
        assert_eq!(mentor.persona_root().as_deref(), Some(mentor_dir.as_path()));
    }

    #[cfg(unix)]
    #[test]
    fn agent_store_root_symlink_resolves_retained_target_for_agents_and_skills() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("temporary root");
        let real_store = root.path().join("real-store");
        let link_store = root.path().join("agent");
        let cwd = root.path().join("project");
        fs::create_dir_all(&cwd).expect("project");
        fs::create_dir_all(&real_store).expect("real store");
        fs::write(
            real_store.join("settings.json"),
            r#"{"theme":"linked-theme"}"#,
        )
        .expect("global settings");
        fs::create_dir_all(real_store.join("extensions")).expect("extensions");
        symlink(&real_store, &link_store).expect("agent root symlink");

        write_skill(&real_store.join("skills").join("linked"), "linked");
        fs::create_dir_all(real_store.join("agents")).expect("agents dir");
        fs::write(
            real_store.join("agents").join("linked.md"),
            "---\nname: linked\ndescription: linked agent\n---\nlinked agent prompt",
        )
        .expect("linked agent");

        let manager = ResourceManager::new(manager_options(&cwd, &link_store, false))
            .expect("resource manager through root link");
        let snapshot = manager.snapshot();
        assert!(
            snapshot.skills.iter().any(|skill| skill.name == "linked"),
            "skill through root symlink must load: {:?}",
            snapshot
                .skills
                .iter()
                .map(|skill| &skill.name)
                .collect::<Vec<_>>()
        );
        assert!(
            snapshot.agents.iter().any(|agent| agent.name == "linked"),
            "agent through root symlink must load: {:?}",
            snapshot
                .agents
                .iter()
                .map(|agent| &agent.name)
                .collect::<Vec<_>>()
        );
        assert_eq!(snapshot.settings.theme.as_deref(), Some("linked-theme"));
        manager
            .settings_manager()
            .expect("retained settings manager")
            .reload()
            .expect("retained settings reload");
    }

    #[cfg(unix)]
    #[test]
    fn agent_store_root_symlink_missing_target_fails_closed() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("temporary root");
        let link_store = root.path().join("agent");
        let cwd = root.path().join("project");
        fs::create_dir_all(&cwd).expect("project");
        symlink(root.path().join("missing"), &link_store).expect("broken symlink");

        let error = ResourceManager::new(manager_options(&cwd, &link_store, false))
            .err()
            .expect("broken root symlink must fail closed");
        let message = format!("{error:#}");
        // The resource manager fails closed on a broken configured root.
        assert!(
            !message.is_empty(),
            "broken root symlink must surface an error: {message}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn agent_store_root_symlink_non_directory_fails_closed() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("temporary root");
        let file = root.path().join("not-a-dir");
        fs::write(&file, "x").expect("file");
        let link_store = root.path().join("agent");
        let cwd = root.path().join("project");
        fs::create_dir_all(&cwd).expect("project");
        symlink(&file, &link_store).expect("file symlink");

        let error = ResourceManager::new(manager_options(&cwd, &link_store, false))
            .err()
            .expect("non-directory root symlink must fail closed");
        let _ = format!("{error:#}");
    }

    #[cfg(unix)]
    #[test]
    fn agent_store_root_symlink_retarget_after_capture_fails_closed() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("temporary root");
        let first = root.path().join("first-store");
        let second = root.path().join("second-store");
        let link_store = root.path().join("agent");
        let cwd = root.path().join("project");
        fs::create_dir_all(&cwd).expect("project");
        fs::create_dir_all(&first).expect("first store");
        fs::create_dir_all(&second).expect("second store");
        fs::write(first.join("settings.json"), r#"{"theme":"first"}"#).expect("first settings");
        fs::write(second.join("settings.json"), r#"{"theme":"second"}"#).expect("second settings");
        symlink(&first, &link_store).expect("root symlink to first");

        let manager = ResourceManager::new(manager_options(&cwd, &link_store, false))
            .expect("manager bound to first store");
        assert_eq!(manager.snapshot().settings.theme.as_deref(), Some("first"));

        fs::remove_file(&link_store).expect("remove original link");
        symlink(&second, &link_store).expect("retarget to second");

        let error = manager
            .settings_manager()
            .err()
            .expect("retargeted root must fail before settings reload");
        assert!(
            error
                .to_string()
                .contains("validating retained agent resource root"),
            "unexpected retarget failure: {error:#}"
        );
        assert_eq!(manager.snapshot().settings.theme.as_deref(), Some("first"));
    }

    #[cfg(unix)]
    #[test]
    fn agent_store_root_symlink_target_swap_at_same_path_fails_closed() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("temporary root");
        let target = root.path().join("real-store");
        let archived = root.path().join("archived-store");
        let link_store = root.path().join("agent");
        let cwd = root.path().join("project");
        fs::create_dir_all(&cwd).expect("project");
        fs::create_dir_all(&target).expect("first store");
        fs::write(target.join("settings.json"), r#"{"theme":"first"}"#).expect("first settings");
        symlink(&target, &link_store).expect("root symlink");

        let manager = ResourceManager::new(manager_options(&cwd, &link_store, false))
            .expect("manager bound to first target identity");
        fs::rename(&target, &archived).expect("archive first target");
        fs::create_dir(&target).expect("replacement target at same path");
        fs::write(target.join("settings.json"), r#"{"theme":"second"}"#).expect("second settings");

        let error = manager
            .settings_manager()
            .err()
            .expect("same-path target swap must fail before reload");
        assert!(
            error
                .to_string()
                .contains("validating retained agent resource root"),
            "unexpected same-path swap failure: {error:#}"
        );
        assert_eq!(manager.snapshot().settings.theme.as_deref(), Some("first"));
    }

    #[test]
    fn marketplace_malformed_manifest_fails_resource_build() {
        let root = tempfile::tempdir().expect("temporary root");
        let cwd = root.path().join("project");
        let agent_dir = root.path().join("agent");
        let extension_dir = agent_dir.join("extensions").join("broken");
        fs::create_dir_all(&cwd).expect("project");
        fs::create_dir_all(&extension_dir).expect("extension directory");
        fs::write(
            extension_dir.join(crate::PROCESS_EXTENSION_MANIFEST_FILE),
            b"not-json",
        )
        .expect("malformed manifest");
        let trust_store = crate::TrustStore::new(&agent_dir);
        trust_store
            .set_exact(&extension_dir, crate::TrustDecision::Trusted)
            .expect("trust malformed extension path");
        let mut options = manager_options(&cwd, &agent_dir, false);
        options.disable_extensions = false;

        let error = ResourceManager::new(options)
            .err()
            .expect("malformed marketplace manifest must fail closed");
        assert!(
            format!("{error:#}").contains("parsing marketplace extension manifest"),
            "unexpected malformed manifest error: {error:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn marketplace_symlink_entry_fails_resource_build() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("temporary root");
        let cwd = root.path().join("project");
        let agent_dir = root.path().join("agent");
        let external = root.path().join("external-extension");
        fs::create_dir_all(&cwd).expect("project");
        fs::create_dir_all(agent_dir.join("extensions")).expect("extensions directory");
        fs::create_dir(&external).expect("external extension");
        symlink(&external, agent_dir.join("extensions").join("linked"))
            .expect("marketplace symlink");
        let mut options = manager_options(&cwd, &agent_dir, false);
        options.disable_extensions = false;

        ResourceManager::new(options)
            .err()
            .expect("marketplace symlink entry must fail closed");
    }

    #[test]
    fn resource_paths_account_for_persona_dirs() {
        let root = tempfile::tempdir().expect("temporary root");
        let home = root.path().join("home");
        let cwd = root.path().join("project");
        let agent_dir = home.join(".rpi").join("agent");
        fs::create_dir_all(&cwd).expect("project directory");
        fs::create_dir_all(&agent_dir).expect("agent directory");
        create_git_boundary(&cwd);

        let untrusted = ResourcePaths::discover_from_agent_dir(&cwd, &agent_dir, false);
        assert_eq!(untrusted.persona_dirs, vec![agent_dir.join("personas")]);

        let trusted = ResourcePaths::discover_from_agent_dir(&cwd, &agent_dir, true);
        assert_eq!(
            trusted.persona_dirs,
            vec![
                agent_dir.join("personas"),
                cwd.join(".rpi").join("personas")
            ]
        );
    }

    fn theme_options(cwd: &Path, agent_dir: &Path, trusted: bool) -> ResourceManagerOptions {
        let mut options = manager_options(cwd, agent_dir, trusted);
        options.disable_themes = false;
        options
    }

    fn write_theme(root: &Path, name: &str) -> PathBuf {
        let path = root.join(format!("{name}.json"));
        fs::write(
            &path,
            format!(r##"{{"name":"{name}","colors":{{"accent":"#00ff00"}}}}"##),
        )
        .expect("theme file");
        path
    }

    #[test]
    fn absent_default_theme_dirs_yield_empty_themes() {
        let root = tempfile::tempdir().expect("temporary root");
        let cwd = root.path().join("project");
        let agent_dir = root.path().join("agent");
        fs::create_dir_all(&cwd).expect("project directory");
        fs::create_dir_all(&agent_dir).expect("agent directory");
        // Neither agent_dir/themes nor cwd/.rpi/themes exists; a fresh install
        // must see an empty theme set, not a hard error.
        let resources =
            ResourceManager::new(theme_options(&cwd, &agent_dir, true)).expect("resource manager");
        assert!(resources.snapshot().themes.is_empty());
    }

    #[test]
    fn themes_load_from_existing_default_directories() {
        let root = tempfile::tempdir().expect("temporary root");
        let cwd = root.path().join("project");
        let agent_dir = root.path().join("agent");
        fs::create_dir_all(&cwd).expect("project directory");
        fs::create_dir_all(&agent_dir).expect("agent directory");
        let global_dir = agent_dir.join("themes");
        fs::create_dir_all(&global_dir).expect("global themes dir");
        write_theme(&global_dir, "global-theme");
        // Project `.rpi/themes` exists but holds no themes; the project dir
        // itself is absent from the theme set when untrusted.
        let resources =
            ResourceManager::new(theme_options(&cwd, &agent_dir, false)).expect("resource manager");
        let themes = resources.snapshot().themes.clone();
        assert_eq!(themes.len(), 1);
        assert_eq!(themes[0].name, "global-theme");

        let project_dir = cwd.join(".rpi").join("themes");
        fs::create_dir_all(&project_dir).expect("project themes dir");
        write_theme(&project_dir, "project-theme");
        let resources =
            ResourceManager::new(theme_options(&cwd, &agent_dir, true)).expect("resource manager");
        let names: Vec<String> = resources
            .snapshot()
            .themes
            .iter()
            .map(|theme| theme.name.clone())
            .collect();
        assert_eq!(names, vec!["global-theme", "project-theme"]);
    }

    #[test]
    fn explicit_missing_theme_fails() {
        let root = tempfile::tempdir().expect("temporary root");
        let cwd = root.path().join("project");
        let agent_dir = root.path().join("agent");
        fs::create_dir_all(&cwd).expect("project directory");
        fs::create_dir_all(&agent_dir).expect("agent directory");
        let mut options = theme_options(&cwd, &agent_dir, true);
        options.explicit_theme_paths = vec![root.path().join("missing-theme.json")];

        let error = ResourceManager::new(options)
            .err()
            .expect("explicit missing theme must fail the resource build");
        let message = format!("{error:#}");
        assert!(
            message.contains("explicit theme path does not exist"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn explicit_theme_file_is_loaded() {
        let root = tempfile::tempdir().expect("temporary root");
        let cwd = root.path().join("project");
        let agent_dir = root.path().join("agent");
        fs::create_dir_all(&cwd).expect("project directory");
        fs::create_dir_all(&agent_dir).expect("agent directory");
        let theme = write_theme(root.path(), "explicit-theme");
        let mut options = theme_options(&cwd, &agent_dir, false);
        options.explicit_theme_paths = vec![theme];

        let resources = ResourceManager::new(options).expect("resource manager");
        let themes = resources.snapshot().themes.clone();
        assert_eq!(themes.len(), 1);
        assert_eq!(themes[0].name, "explicit-theme");
    }

    #[cfg(unix)]
    #[test]
    fn user_skills_root_symlink_into_untrusted_project_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("temporary root");
        let cwd = root.path().join("project");
        let agent_dir = root.path().join("home").join(".rpi").join("agent");
        let outside = root.path().join("outside-skills").join("keep");
        let planted = cwd.join("planted-skills").join("planted");
        fs::create_dir_all(&cwd).expect("project");
        fs::create_dir_all(&agent_dir).expect("agent");
        create_git_boundary(&cwd);
        write_skill(&outside, "keep");
        write_skill(&planted, "planted");
        symlink(cwd.join("planted-skills"), agent_dir.join("skills")).expect("skills root link");

        let snapshot = manager_snapshot(&cwd, &agent_dir, false);
        let names = snapshot
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>();
        assert!(
            !names.contains(&"planted"),
            "primary User skills root must not follow a managed link into an untrusted project: {names:?}"
        );

        fs::remove_file(agent_dir.join("skills")).expect("clear planted link");
        symlink(root.path().join("outside-skills"), agent_dir.join("skills"))
            .expect("external skills root");
        let snapshot = manager_snapshot(&cwd, &agent_dir, false);
        let names = snapshot
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>();
        assert!(
            names.contains(&"keep"),
            "a managed User skills root outside the project must still load: {names:?}"
        );
    }
}
