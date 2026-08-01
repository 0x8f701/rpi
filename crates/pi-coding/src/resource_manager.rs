//! Atomic resource snapshots with complete candidate validation on reload.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use anyhow::{Context, Result, bail};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::prompt_templates::{LoadPromptTemplatesOptions, PromptTemplate, load_prompt_templates};
use crate::resources::{CONFIG_DIR_NAME, Skill, agent_dir_path, load_context_files, load_skills_trusted};
use crate::settings::{DefaultProjectTrust, Settings, SettingsManager};
use crate::system_prompt::ContextFile;
use crate::trust::{TrustResolution, TrustStore, resolve_project_trust};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceDiagnosticLevel { Warning, Error }

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
pub struct ThemeResource { pub name: String, pub path: PathBuf }

#[derive(Clone, Debug)]
pub struct ResourcePaths {
    pub context_roots: Vec<PathBuf>,
    pub skill_paths: Vec<PathBuf>,
    pub prompt_paths: Vec<PathBuf>,
    pub theme_dirs: Vec<PathBuf>,
    pub keybinding_files: Vec<PathBuf>,
    pub system_prompt_files: Vec<PathBuf>,
    pub append_system_prompt_files: Vec<PathBuf>,
}

impl ResourcePaths {
    #[must_use]
    pub fn discover(cwd: impl AsRef<Path>, project_trusted: bool) -> Self {
        let cwd = cwd.as_ref();
        let agent_dir = agent_dir_path();
        let project_dir = cwd.join(CONFIG_DIR_NAME);
        let mut paths = Self {
            context_roots: vec![agent_dir.clone()],
            skill_paths: vec![agent_dir.join("skills")],
            prompt_paths: vec![agent_dir.join("prompts")],
            theme_dirs: vec![agent_dir.join("themes")],
            keybinding_files: vec![agent_dir.join("keybindings.json")],
            system_prompt_files: vec![agent_dir.join("SYSTEM.md")],
            append_system_prompt_files: vec![agent_dir.join("APPEND_SYSTEM.md")],
        };
        if project_trusted {
            paths.context_roots.push(cwd.to_path_buf());
            paths.skill_paths.push(project_dir.join("skills"));
            paths.prompt_paths.push(project_dir.join("prompts"));
            paths.theme_dirs.push(project_dir.join("themes"));
            paths.keybinding_files.push(project_dir.join("keybindings.json"));
            paths.system_prompt_files.insert(0, project_dir.join("SYSTEM.md"));
            paths.append_system_prompt_files.insert(0, project_dir.join("APPEND_SYSTEM.md"));
        }
        paths
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
    pub append_system_prompt: Vec<String>,
    pub append_system_prompt_paths: Vec<Option<PathBuf>>,
}

impl ResourceManagerOptions {
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(), agent_dir: agent_dir_path(), headless: true,
            project_trust_override: None, explicit_extension_paths: Vec::new(),
            explicit_skill_paths: Vec::new(), explicit_prompt_paths: Vec::new(),
            explicit_theme_paths: Vec::new(), disable_extensions: false,
            disable_skills: false, disable_prompt_templates: false,
            disable_themes: false, disable_context_files: false,
            system_prompt: None, system_prompt_path: None,
            append_system_prompt: Vec::new(), append_system_prompt_paths: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct ResourceManager { inner: Arc<ResourceManagerInner> }
struct ResourceManagerInner {
    options: ResourceManagerOptions,
    settings: RwLock<SettingsManager>,
    trust_store: TrustStore,
    snapshot: RwLock<Arc<ResourceSnapshot>>,
    reload_lock: Arc<AsyncMutex<()>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReloadResult { pub generation: u64, pub diagnostics: Vec<ResourceDiagnostic> }

pub struct ResourceReloadCandidate {
    owner: Weak<ResourceManagerInner>,
    _guard: OwnedMutexGuard<()>,
    base_generation: u64,
    settings: SettingsManager,
    snapshot: Arc<ResourceSnapshot>,
}

impl ResourceReloadCandidate {
    #[must_use]
    pub fn snapshot(&self) -> Arc<ResourceSnapshot> {
        self.snapshot.clone()
    }

    pub fn extension_specs(
        &self,
        permissions: &crate::ExtensionPermissionSet,
    ) -> Result<Vec<crate::ExtensionSpec>> {
        extension_specs_for_snapshot(&self.snapshot, permissions)
    }
}

impl ResourceManager {
    pub fn new(options: ResourceManagerOptions) -> Result<Self> {
        let settings = SettingsManager::load_phase_one(&options.cwd, &options.agent_dir)?;
        let trust_store = TrustStore::new(&options.agent_dir);
        let candidate = build_candidate(&options, &settings, &trust_store, 1)?;
        Ok(Self { inner: Arc::new(ResourceManagerInner {
            options,
            settings: RwLock::new(settings),
            trust_store,
            snapshot: RwLock::new(Arc::new(candidate)),
            reload_lock: Arc::new(AsyncMutex::new(())),
        }) })
    }
    #[must_use] pub fn snapshot(&self) -> Arc<ResourceSnapshot> { self.inner.snapshot.read().clone() }
    #[must_use] pub fn generation(&self) -> u64 { self.inner.snapshot.read().generation }
    #[must_use] pub fn diagnostics(&self) -> Vec<ResourceDiagnostic> { self.inner.snapshot.read().diagnostics.clone() }
    #[must_use] pub fn settings_manager(&self) -> SettingsManager { self.inner.settings.read().clone() }
    #[must_use] pub fn trust_store(&self) -> TrustStore { self.inner.trust_store.clone() }

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
        extension_specs_for_snapshot(&self.snapshot(), permissions)
    }

    pub fn stage_reload(&self) -> Result<ResourceReloadCandidate> {
        let guard = self
            .inner
            .reload_lock
            .clone()
            .try_lock_owned()
            .map_err(|_| anyhow::anyhow!("resource reload is already in progress"))?;
        let base_generation = self.generation();
        let settings = SettingsManager::load_phase_one(
            &self.inner.options.cwd,
            &self.inner.options.agent_dir,
        )?;
        let snapshot = Arc::new(build_candidate(
            &self.inner.options,
            &settings,
            &self.inner.trust_store,
            base_generation.saturating_add(1),
        )?);
        Ok(ResourceReloadCandidate {
            owner: Arc::downgrade(&self.inner),
            _guard: guard,
            base_generation,
            settings,
            snapshot,
        })
    }

    pub fn commit_reload(&self, candidate: ResourceReloadCandidate) -> Result<ReloadResult> {
        if !Weak::ptr_eq(&candidate.owner, &Arc::downgrade(&self.inner)) {
            bail!("resource reload candidate belongs to another manager");
        }
        if self.generation() != candidate.base_generation {
            bail!("resource reload candidate is stale");
        }
        let result = ReloadResult {
            generation: candidate.snapshot.generation,
            diagnostics: candidate.snapshot.diagnostics.clone(),
        };
        *self.inner.settings.write() = candidate.settings;
        *self.inner.snapshot.write() = candidate.snapshot;
        Ok(result)
    }

    /// `/reload` entry point. The old snapshot remains active on any error.
    pub fn reload(&self) -> Result<ReloadResult> {
        let candidate = self.stage_reload()?;
        self.commit_reload(candidate)
    }
}

fn extension_specs_for_snapshot(
    snapshot: &ResourceSnapshot,
    permissions: &crate::ExtensionPermissionSet,
) -> Result<Vec<crate::ExtensionSpec>> {
    let specs = crate::extension_specs_from_package_resources(&snapshot.package_extensions)?;
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

fn build_candidate(
    options: &ResourceManagerOptions,
    settings_manager: &SettingsManager,
    trust_store: &TrustStore,
    generation: u64,
) -> Result<ResourceSnapshot> {
    settings_manager.load_project(false)?;
    settings_manager.reload()?;
    let default_trust = settings_manager
        .settings()
        .default_project_trust
        .unwrap_or(DefaultProjectTrust::Ask);
    let trust = resolve_project_trust(
        trust_store,
        &options.cwd,
        options.project_trust_override,
        default_trust,
        options.headless,
    )?;
    let project_trusted = trust.allows_project_resources(options.headless);
    settings_manager.load_project(project_trusted)?;
    settings_manager.reload()?;
    let settings = settings_manager.settings();
    let agents = crate::AgentCatalog::discover(&crate::AgentDiscoveryOptions {
        cwd: options.cwd.clone(),
        agent_dir: options.agent_dir.clone(),
        project_trusted,
    })?
    .agents()
    .to_vec();
    let paths = ResourcePaths::discover(&options.cwd, project_trusted);
    let packages = crate::PackageManager::with_agent_dir(
        &options.cwd,
        &options.agent_dir,
        project_trusted,
    )?
    .resolve_resources()?;
    let cwd_text = options.cwd.to_string_lossy();
    let context_files = if options.disable_context_files {
        Vec::new()
    } else {
        load_context_files(&cwd_text, project_trusted)
    };
    let (mut skills, skill_diagnostics) = if options.disable_skills {
        (Vec::new(), Vec::new())
    } else {
        load_skills_trusted(&cwd_text, project_trusted)
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

    let configured_skill_paths = settings
        .skills
        .iter()
        .map(|path| (PathBuf::from(path), crate::SkillSource::Explicit, true));
    let package_skill_paths = packages.skills.iter().map(|resource| {
        let source = match resource.scope {
            crate::PackageScope::Global => crate::SkillSource::PackageGlobal,
            crate::PackageScope::Project => crate::SkillSource::PackageProject,
        };
        (resource.path.clone(), source, resource.trusted)
    });
    let explicit_skill_paths = options
        .explicit_skill_paths
        .iter()
        .cloned()
        .map(|path| (path, crate::SkillSource::Explicit, true));
    let mut skill_paths = configured_skill_paths
        .chain(package_skill_paths)
        .filter(|_| !options.disable_skills)
        .chain(explicit_skill_paths)
        .collect::<Vec<_>>();
    let mut validated_skill_paths = skill_paths
        .iter()
        .map(|(path, _, _)| path.clone())
        .collect::<Vec<_>>();
    validate_explicit_project_paths(
        &options.cwd,
        &mut validated_skill_paths,
        project_trusted,
        "skill",
    )?;
    skill_paths.retain(|(path, _, _)| validated_skill_paths.contains(path));
    for (path, source, trusted) in skill_paths {
        skills.extend(load_explicit_skills(
            &resolve_explicit(&options.cwd, &path),
            source,
            trusted,
        )?);
    }
    dedupe_skills(&skills)?;

    let mut prompt_paths = settings
        .prompts
        .iter()
        .map(PathBuf::from)
        .chain(packages.prompts.iter().map(|resource| resource.path.clone()))
        .filter(|_| !options.disable_prompt_templates)
        .chain(options.explicit_prompt_paths.iter().cloned())
        .collect::<Vec<_>>();
    validate_explicit_project_paths(
        &options.cwd,
        &mut prompt_paths,
        project_trusted,
        "prompt template",
    )?;
    let prompts = load_prompt_templates(&LoadPromptTemplatesOptions {
        cwd: options.cwd.clone(),
        agent_dir: options.agent_dir.clone(),
        explicit_paths: prompt_paths,
        include_defaults: !options.disable_prompt_templates,
        include_project: project_trusted && !options.disable_prompt_templates,
    })?;

    let mut theme_paths = settings
        .themes
        .iter()
        .map(PathBuf::from)
        .chain(packages.themes.iter().map(|resource| resource.path.clone()))
        .filter(|_| !options.disable_themes)
        .chain(options.explicit_theme_paths.iter().cloned())
        .collect::<Vec<_>>();
    validate_explicit_project_paths(&options.cwd, &mut theme_paths, project_trusted, "theme")?;
    let theme_dirs = if options.disable_themes { &[] } else { paths.theme_dirs.as_slice() };
    let themes = load_themes(theme_dirs, &options.cwd, theme_paths)?;
    let keybinding_files = validate_keybindings(&paths.keybinding_files)?;
    let mut explicit_prompt_files = options
        .system_prompt_path
        .iter()
        .chain(options.append_system_prompt_paths.iter().flatten())
        .cloned()
        .collect::<Vec<_>>();
    validate_explicit_project_paths(
        &options.cwd,
        &mut explicit_prompt_files,
        project_trusted,
        "system prompt",
    )?;
    if !project_trusted {
        let project_root = options.cwd.canonicalize().with_context(|| {
            format!("resolving working directory {}", options.cwd.display())
        })?;
        if let Some(path) = explicit_prompt_files
            .iter()
            .find(|path| path.starts_with(&project_root))
        {
            bail!(
                "explicit project system prompt requires project trust: {} (pass --approve to allow it)",
                path.display()
            );
        }
    }
    let system_prompt = match (&options.system_prompt_path, &options.system_prompt) {
        (Some(path), _) => Some(crate::read_resource_text(path, "system prompt")?),
        (None, Some(prompt)) => Some(prompt.clone()),
        (None, None) => load_first_existing(&paths.system_prompt_files, "system prompt")?,
    };
    let append_system_prompt = if options.append_system_prompt.is_empty() {
        load_first_existing(&paths.append_system_prompt_files, "append system prompt")?
            .into_iter()
            .collect()
    } else {
        options
            .append_system_prompt
            .iter()
            .enumerate()
            .map(|(index, prompt)| match options
                .append_system_prompt_paths
                .get(index)
                .and_then(Option::as_ref)
            {
                Some(path) => crate::read_resource_text(path, "append system prompt"),
                None => Ok(prompt.clone()),
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
    package_extensions.extend(explicit_extension_resources(
        &options.cwd,
        &options.explicit_extension_paths,
        project_trusted,
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
    })
}

fn explicit_extension_resources(
    cwd: &Path,
    raw_paths: &[PathBuf],
    project_trusted: bool,
) -> Result<Vec<crate::PackageResourceSpec>> {
    let project_root = cwd
        .canonicalize()
        .with_context(|| format!("resolving working directory {}", cwd.display()))?;
    raw_paths
        .iter()
        .map(|raw| {
            let resolved = resolve_explicit(cwd, raw);
            let manifest = if resolved.is_dir() {
                resolved.join(crate::PROCESS_EXTENSION_MANIFEST_FILE)
            } else {
                resolved
            };
            if manifest.file_name().and_then(std::ffi::OsStr::to_str)
                != Some(crate::PROCESS_EXTENSION_MANIFEST_FILE)
            {
                bail!(
                    "extension path must be a directory containing {} or the manifest itself: {}",
                    crate::PROCESS_EXTENSION_MANIFEST_FILE,
                    manifest.display()
                );
            }
            if !manifest.is_file() {
                bail!("extension manifest does not exist: {}", manifest.display());
            }
            let manifest = manifest.canonicalize().with_context(|| {
                format!("resolving extension manifest {}", manifest.display())
            })?;
            let project_local = manifest.starts_with(&project_root);
            if project_local && !project_trusted {
                bail!(
                    "explicit project extension requires project trust: {} (pass --approve to allow it)",
                    manifest.display()
                );
            }
            Ok(crate::PackageResourceSpec {
                kind: crate::PackageResourceKind::Extension,
                path: manifest,
                package_id: "cli".to_owned(),
                scope: if project_local {
                    crate::PackageScope::Project
                } else {
                    crate::PackageScope::Global
                },
                trusted: !project_local || project_trusted,
            })
        })
        .collect()
}

fn validate_explicit_project_paths(
    cwd: &Path,
    paths: &mut Vec<PathBuf>,
    project_trusted: bool,
    kind: &str,
) -> Result<()> {
    let project_root = cwd
        .canonicalize()
        .with_context(|| format!("resolving working directory {}", cwd.display()))?;
    let project_pi = project_root.join(CONFIG_DIR_NAME);
    for raw in paths.iter_mut() {
        let resolved = resolve_explicit(cwd, raw);
        if !resolved.exists() {
            bail!("{kind} path does not exist: {}", resolved.display());
        }
        let canonical = resolved
            .canonicalize()
            .with_context(|| format!("resolving {kind} path {}", resolved.display()))?;
        if !raw.is_absolute() && !canonical.starts_with(&project_root) {
            bail!("explicit {kind} path escapes the working directory: {}", raw.display());
        }
        if (canonical == project_pi || canonical.starts_with(&project_pi)) && !project_trusted {
            bail!(
                "explicit project {kind} requires project trust: {} (pass --approve to allow it)",
                canonical.display()
            );
        }
        *raw = canonical;
    }
    Ok(())
}

fn load_explicit_skills(
    path: &Path,
    source: crate::SkillSource,
    trusted: bool,
) -> Result<Vec<Skill>> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("reading explicit skill path {}", path.display()))?;
    let mut files = Vec::new();
    if metadata.is_file() {
        files.push(path.to_path_buf());
    } else if metadata.is_dir() {
        let root = path.join("SKILL.md");
        if root.is_file() {
            files.push(root);
        } else {
            let mut entries = fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
            entries.sort_by_key(std::fs::DirEntry::file_name);
            for entry in entries {
                let candidate = entry.path();
                if candidate.is_file()
                    && candidate.extension().is_some_and(|extension| extension == "md")
                {
                    files.push(candidate);
                } else if candidate.join("SKILL.md").is_file() {
                    files.push(candidate.join("SKILL.md"));
                }
            }
        }
    }
    let mut skills = Vec::new();
    for file in files {
        let (skill, diagnostics) = crate::resources::load_skill_from_file(
            &file.to_string_lossy(),
            source,
            trusted,
        );
        if let Some(diagnostic) = diagnostics
            .into_iter()
            .find(|diagnostic| diagnostic.kind == "error")
        {
            bail!("explicit skill {}: {}", file.display(), diagnostic.message);
        }
        let skill = skill
            .ok_or_else(|| anyhow::anyhow!("skill {} is missing description", file.display()))?;
        skills.push(skill);
    }
    Ok(skills)
}


fn dedupe_skills(skills: &[Skill]) -> Result<()> {
    let mut names = BTreeMap::<&str, &str>::new();
    for skill in skills {
        if let Some(winner) = names.insert(&skill.name, &skill.file_path) {
            bail!("skill name {} collision: {} conflicts with {}", skill.name, skill.file_path, winner);
        }
    }
    Ok(())
}

fn load_themes(default_dirs: &[PathBuf], cwd: &Path, explicit_paths: Vec<PathBuf>) -> Result<Vec<ThemeResource>> {
    let mut files = Vec::new();
    for directory in default_dirs { collect_json_files(directory, &mut files)?; }
    for raw in explicit_paths {
        let path = resolve_explicit(cwd, &raw);
        if path.is_dir() { collect_json_files(&path, &mut files)?; } else { files.push(path); }
    }
    let mut themes = Vec::new();
    let mut names = BTreeMap::<String, PathBuf>::new();
    for file in files {
        let content = crate::read_resource_text(&file, "theme")?;
        let value: Value = serde_json::from_str(&content).with_context(|| format!("parsing theme {}", file.display()))?;
        let object = value.as_object().ok_or_else(|| anyhow::anyhow!("theme {} must be a JSON object", file.display()))?;
        let name = object.get("name").and_then(Value::as_str).map(str::trim)
            .filter(|name| !name.is_empty() && !name.contains('/'))
            .ok_or_else(|| anyhow::anyhow!("theme {} has an invalid or missing name", file.display()))?;
        let colors = object.get("colors").and_then(Value::as_object)
            .ok_or_else(|| anyhow::anyhow!("theme {} is missing colors", file.display()))?;
        if colors.is_empty() { bail!("theme {} colors must not be empty", file.display()); }
        for (role, color) in colors {
            if !(color.is_string() || color.as_u64().is_some_and(|value| value <= 255)) {
                bail!("theme {} color {role} must be a string or 0..255", file.display());
            }
        }
        if let Some(winner) = names.insert(name.to_owned(), file.clone()) {
            bail!("theme name {name} collision: {} conflicts with {}", file.display(), winner.display());
        }
        themes.push(ThemeResource { name: name.to_owned(), path: file });
    }
    Ok(themes)
}

fn collect_json_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries = match fs::read_dir(directory) {
        Ok(entries) => entries.collect::<std::io::Result<Vec<_>>>()?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("reading directory {}", directory.display())),
    };
    entries.sort_by_key(std::fs::DirEntry::file_name);
    files.extend(entries.into_iter().map(|entry| entry.path()).filter(|path|
        path.is_file() && path.extension().is_some_and(|extension| extension == "json")));
    Ok(())
}

fn validate_keybindings(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut existing = Vec::new();
    for path in paths {
        if !path.exists() { continue; }
        let content = crate::read_resource_text(path, "keybindings")?;
        let value: Value = serde_json::from_str(&content).with_context(|| format!("parsing keybindings {}", path.display()))?;
        if !value.is_object() { bail!("keybindings {} must be a JSON object", path.display()); }
        existing.push(path.clone());
    }
    Ok(existing)
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
        let bytes = fs::metadata(&skill.file_path)
            .map(|metadata| usize::try_from(metadata.len()).unwrap_or(usize::MAX))
            .unwrap_or_default();
        add(bytes, "skill")?;
    }
    for agent in agents {
        add(agent.system_prompt.len() + agent.description.len(), "agent definition")?;
    }
    for prompt in prompts {
        add(prompt.content.len() + prompt.description.len(), "prompt template")?;
    }
    for theme in themes {
        let bytes = fs::metadata(&theme.path)
            .map(|metadata| usize::try_from(metadata.len()).unwrap_or(usize::MAX))
            .unwrap_or_default();
        add(bytes, "theme")?;
    }
    for path in keybinding_files {
        let bytes = fs::metadata(path)
            .map(|metadata| usize::try_from(metadata.len()).unwrap_or(usize::MAX))
            .unwrap_or_default();
        add(bytes, "keybindings")?;
    }
    if let Some(prompt) = system_prompt {
        add(prompt.len(), "system prompt")?;
    }
    for prompt in append_system_prompt {
        add(prompt.len(), "append system prompt")?;
    }
    Ok(())
}

fn load_first_existing(paths: &[PathBuf], description: &str) -> Result<Option<String>> {
    for path in paths {
        if !path.exists() {
            continue;
        }
        return crate::read_resource_text(path, description).map(Some);
    }
    Ok(None)
}

fn resolve_explicit(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) }
}
