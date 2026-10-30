//! Versioned rpi package schemas and the local/git package backend.
//!
//! Package operations are serialized, settings and package state are replaced
//! atomically, and git is always invoked directly with an argv vector.

use std::collections::{BTreeSet, HashSet};
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use globset::{GlobBuilder, GlobMatcher};
use parking_lot::{Mutex, MutexGuard};
use pi_sandbox::{AbsolutePath, RelativePath};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::fs::{
    CapabilityAccess, CapabilityDirectory, CapabilityEntry, CapabilityFs, CapabilityRootLease,
    CapabilityRootPurpose, EntryKind, FileIdentity, FileMode, FsError, Replacement, RootCreation,
};

use crate::process::{GovernedProcessContext, GovernedProcessOutput, GovernedProcessSpec};

const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_GIT_STDOUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_GIT_STDERR_BYTES: usize = 64 * 1024;

pub const PACKAGE_SOURCE_SCHEMA_VERSION: u32 = 1;
pub const PACKAGE_MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const PACKAGE_STATE_SCHEMA_VERSION: u32 = 1;

const CONFIG_DIR_NAME: &str = ".rpi";
const SETTINGS_FILE_NAME: &str = "settings.json";
const STATE_FILE_NAME: &str = "package-state.json";
const OPERATION_LOCK_FILE_NAME: &str = ".package-operation.lock";
const OPERATION_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const STALE_OPERATION_LOCK_AGE: Duration = Duration::from_secs(6 * 60 * 60);
const MAX_PACKAGE_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_PACKAGE_METADATA_BYTES: u64 = 8 * 1024 * 1024;

static PROCESS_PACKAGE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static UNIQUE_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PackageScope {
    Global,
    Project,
}

impl PackageScope {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedPackageSource {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(flatten)]
    pub kind: ManagedPackageSourceKind,
}

/// Supported package source backends. npm is deliberately absent until its
/// backend exists, so an npm source can never enter installed state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ManagedPackageSourceKind {
    Local {
        path: PathBuf,
    },
    Git {
        repo: String,
        host: String,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
    },
}

impl ManagedPackageSource {
    fn local(path: PathBuf) -> Self {
        Self {
            schema_version: PACKAGE_SOURCE_SCHEMA_VERSION,
            kind: ManagedPackageSourceKind::Local { path },
        }
    }

    fn git(repo: String, host: String, path: String, reference: Option<String>) -> Self {
        Self {
            schema_version: PACKAGE_SOURCE_SCHEMA_VERSION,
            kind: ManagedPackageSourceKind::Git {
                repo,
                host,
                path,
                reference,
            },
        }
    }

    fn validate_version(&self) -> Result<()> {
        if self.schema_version != PACKAGE_SOURCE_SCHEMA_VERSION {
            bail!(
                "unsupported package source schema version {}; expected {}",
                self.schema_version,
                PACKAGE_SOURCE_SCHEMA_VERSION
            );
        }
        Ok(())
    }

    #[must_use]
    pub fn is_pinned(&self) -> bool {
        matches!(
            &self.kind,
            ManagedPackageSourceKind::Git {
                reference: Some(_),
                ..
            }
        )
    }

    fn identity(&self) -> String {
        match &self.kind {
            ManagedPackageSourceKind::Local { path } => {
                format!("local:{}", path.to_string_lossy())
            }
            ManagedPackageSourceKind::Git { host, path, .. } => {
                format!("git:{}/{}", host.to_ascii_lowercase(), path)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageManifest {
    #[serde(default = "package_manifest_schema_version", rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub prompts: Vec<String>,
    #[serde(default)]
    pub themes: Vec<String>,
}

const fn package_manifest_schema_version() -> u32 {
    PACKAGE_MANIFEST_SCHEMA_VERSION
}

impl Default for PackageManifest {
    fn default() -> Self {
        Self {
            schema_version: PACKAGE_MANIFEST_SCHEMA_VERSION,
            extensions: Vec::new(),
            skills: Vec::new(),
            prompts: Vec::new(),
            themes: Vec::new(),
        }
    }
}

impl PackageManifest {
    fn validate(&self) -> Result<()> {
        if self.schema_version != PACKAGE_MANIFEST_SCHEMA_VERSION {
            bail!(
                "unsupported rpi package manifest schema version {}; expected {}",
                self.schema_version,
                PACKAGE_MANIFEST_SCHEMA_VERSION
            );
        }
        for entry in self
            .extensions
            .iter()
            .chain(&self.skills)
            .chain(&self.prompts)
            .chain(&self.themes)
        {
            validate_manifest_entry(entry)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PackageResourceKind {
    Extension,
    Skill,
    Prompt,
    Theme,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageResourceSpec {
    pub kind: PackageResourceKind,
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_root: Option<PathBuf>,
    pub package_id: String,
    pub scope: PackageScope,
    pub trusted: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageResources {
    #[serde(default)]
    pub extensions: Vec<PackageResourceSpec>,
    #[serde(default)]
    pub skills: Vec<PackageResourceSpec>,
    #[serde(default)]
    pub prompts: Vec<PackageResourceSpec>,
    #[serde(default)]
    pub themes: Vec<PackageResourceSpec>,
}

impl PackageResources {
    fn remap_metadata(&mut self, package_id: &str, scope: PackageScope, trusted: bool) {
        for resource in self
            .extensions
            .iter_mut()
            .chain(&mut self.skills)
            .chain(&mut self.prompts)
            .chain(&mut self.themes)
        {
            resource.package_id = package_id.to_string();
            resource.scope = scope;
            resource.trusted = trusted;
        }
    }

    fn append_deduped(&mut self, other: Self, seen: &mut HashSet<(PackageResourceKind, PathBuf)>) {
        for resource in other.extensions {
            if seen.insert((resource.kind, resource.path.clone())) {
                self.extensions.push(resource);
            }
        }
        for resource in other.skills {
            if seen.insert((resource.kind, resource.path.clone())) {
                self.skills.push(resource);
            }
        }
        for resource in other.prompts {
            if seen.insert((resource.kind, resource.path.clone())) {
                self.prompts.push(resource);
            }
        }
        for resource in other.themes {
            if seen.insert((resource.kind, resource.path.clone())) {
                self.themes.push(resource);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledPackage {
    pub identity: String,
    pub scope: PackageScope,
    pub source: ManagedPackageSource,
    pub root: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    pub manifest: PackageManifest,
    pub resources: PackageResources,
    pub installed_at_unix_ms: u64,
}

/// Versioned installed-package state, stored independently in each scope root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageState {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(default)]
    pub packages: Vec<InstalledPackage>,
}

impl Default for PackageState {
    fn default() -> Self {
        Self {
            schema_version: PACKAGE_STATE_SCHEMA_VERSION,
            packages: Vec::new(),
        }
    }
}

impl PackageState {
    fn validate(&self) -> Result<()> {
        if self.schema_version != PACKAGE_STATE_SCHEMA_VERSION {
            bail!(
                "unsupported package state schema version {}; expected {}",
                self.schema_version,
                PACKAGE_STATE_SCHEMA_VERSION
            );
        }
        for package in &self.packages {
            package.source.validate_version()?;
            package.manifest.validate()?;
            if package.identity != package.source.identity() {
                bail!("package state identity does not match its source");
            }
        }
        Ok(())
    }

    fn upsert(&mut self, package: InstalledPackage) {
        self.packages
            .retain(|existing| existing.identity != package.identity);
        self.packages.push(package);
        self.packages
            .sort_by(|left, right| left.identity.cmp(&right.identity));
    }

    fn remove(&mut self, identity: &str) {
        self.packages.retain(|package| package.identity != identity);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredPackage {
    pub source: String,
    pub scope: PackageScope,
    pub installed_path: Option<PathBuf>,
    pub pinned: bool,
    pub supported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageOperation {
    pub source: String,
    pub identity: String,
    pub scope: PackageScope,
    pub root: PathBuf,
    pub revision: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageUpdateType {
    /// The configured git ref resolves to a different commit than the checkout.
    Git,
    /// A local package's manifest or discovered resource snapshot changed.
    LocalChanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageUpdate {
    pub source: String,
    pub display_name: String,
    pub update_type: PackageUpdateType,
    pub scope: PackageScope,
}

/// One configured package with its freshly rediscovered manifest and resources,
/// for `rpi config` resource toggling. Resources are re-read from the on-disk
/// `package.json#pi` so a corrupt or invalid manifest surfaces as an error
/// before any settings write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopePackage {
    pub identity: String,
    pub source: String,
    pub scope: PackageScope,
    pub root: PathBuf,
    pub manifest: PackageManifest,
    pub resources: PackageResources,
}

#[derive(Clone)]
pub struct PackageManager {
    cwd: PathBuf,
    agent_dir: PathBuf,
    capabilities: Arc<CapabilityFs>,
    global: CapabilityRootLease,
    project: Option<CapabilityRootLease>,
    project_trusted: bool,
    process: Option<GovernedProcessContext>,
}

impl std::fmt::Debug for PackageManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PackageManager")
            .field("cwd", &self.cwd)
            .field("agent_dir", &self.agent_dir)
            .field("project_trusted", &self.project_trusted)
            .finish()
    }
}

impl PackageManager {
    pub fn new(cwd: impl AsRef<Path>, project_trusted: bool) -> Result<Self> {
        let agent_dir = crate::resources::agent_dir_path();
        Self::with_agent_dir(cwd, agent_dir, project_trusted)
    }

    pub fn with_agent_dir(
        cwd: impl AsRef<Path>,
        agent_dir: impl AsRef<Path>,
        project_trusted: bool,
    ) -> Result<Self> {
        let cwd = absolute_existing_or_lexical(cwd.as_ref())?;
        let agent_dir = absolute_existing_or_lexical(agent_dir.as_ref())?;
        let capabilities =
            CapabilityFs::new().context("creating package filesystem capabilities")?;
        let global = capabilities
            .open_root(
                CapabilityRootPurpose::AgentStore,
                AbsolutePath::parse(agent_dir.clone()).map_err(|_| FsError::InvalidPath)?,
                CapabilityAccess::PrivateReadWrite,
                RootCreation::CreateDirectory,
            )
            .context("opening global package store")?;
        let project = project_trusted
            .then(|| {
                capabilities.open_root(
                    CapabilityRootPurpose::ProjectResources,
                    AbsolutePath::parse(cwd.join(CONFIG_DIR_NAME))
                        .map_err(|_| FsError::InvalidPath)?,
                    CapabilityAccess::PrivateReadWrite,
                    RootCreation::CreateDirectory,
                )
            })
            .transpose()
            .context("opening project package resources")?;
        Ok(Self {
            cwd,
            agent_dir,
            capabilities,
            global,
            project,
            project_trusted,
            process: None,
        })
    }

    pub(crate) fn for_retained_resource_resolution(
        cwd: impl AsRef<Path>,
        capabilities: Arc<CapabilityFs>,
        global: CapabilityRootLease,
        project_root: Option<&CapabilityRootLease>,
        project_trusted: bool,
    ) -> Result<Self> {
        let cwd = absolute_existing_or_lexical(cwd.as_ref())?;
        let agent_dir = global.authority_root().to_path_buf();
        let project = if project_trusted {
            match project_root {
                Some(root) => match root.lease_directory(
                    Path::new(CONFIG_DIR_NAME),
                    CapabilityAccess::ReadOnly,
                    RootCreation::Existing,
                ) {
                    Ok(project) => Some(project),
                    Err(FsError::System {
                        kind: std::io::ErrorKind::NotFound,
                        ..
                    }) => None,
                    Err(error) => return Err(error.into()),
                },
                None => match capabilities.open_root(
                    CapabilityRootPurpose::ProjectResources,
                    AbsolutePath::parse(cwd.join(CONFIG_DIR_NAME))
                        .map_err(|_| FsError::InvalidPath)?,
                    CapabilityAccess::ReadOnly,
                    RootCreation::Existing,
                ) {
                    Ok(project) => Some(project),
                    Err(FsError::System {
                        kind: std::io::ErrorKind::NotFound,
                        ..
                    }) => None,
                    Err(error) => return Err(error.into()),
                },
            }
        } else {
            None
        };
        Ok(Self {
            cwd,
            agent_dir,
            capabilities,
            global,
            project,
            project_trusted,
            process: None,
        })
    }

    #[must_use]
    pub fn with_process_context(mut self, process: GovernedProcessContext) -> Self {
        self.process = Some(process);
        self
    }

    fn process_context(&self) -> Result<&GovernedProcessContext> {
        self.process
            .as_ref()
            .ok_or_else(|| anyhow!("package process generation is unavailable"))
    }

    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    #[must_use]
    pub fn agent_dir(&self) -> &Path {
        &self.agent_dir
    }

    #[must_use]
    pub fn scope_root(&self, scope: PackageScope) -> PathBuf {
        match scope {
            PackageScope::Global => self.agent_dir.clone(),
            PackageScope::Project => self.cwd.join(CONFIG_DIR_NAME),
        }
    }
    fn scope_lease(&self, scope: PackageScope) -> Result<&CapabilityRootLease> {
        match scope {
            PackageScope::Global => Ok(&self.global),
            PackageScope::Project => self.project.as_ref().ok_or_else(|| {
                anyhow!("project is not trusted; refusing to access project package storage")
            }),
        }
    }

    #[must_use]
    pub fn settings_path(&self, scope: PackageScope) -> PathBuf {
        self.scope_root(scope).join(SETTINGS_FILE_NAME)
    }

    #[must_use]
    pub fn state_path(&self, scope: PackageScope) -> PathBuf {
        self.scope_root(scope).join(STATE_FILE_NAME)
    }

    /// Resolve the existing installed path for a source in one scope.
    ///
    /// Relative local sources are resolved from that scope's settings root.
    /// Project storage is never inspected unless the project is trusted.
    pub fn installed_path(&self, source: &str, scope: PackageScope) -> Result<Option<PathBuf>> {
        self.assert_scope_trusted(scope)?;
        if source.trim_start().starts_with("npm:") {
            bail!(npm_deferred_error());
        }
        let parsed = self.parse_configured_source(source, scope)?;
        let path = self.installed_root(&parsed, scope)?;
        let exists = match &parsed.kind {
            ManagedPackageSourceKind::Local { path } => {
                operator_path_exists(&self.capabilities, path)
            }
            ManagedPackageSourceKind::Git { .. } => {
                match self
                    .scope_lease(scope)?
                    .metadata(&self.installed_relative(&parsed)?)
                {
                    Ok(_) => true,
                    Err(FsError::System {
                        kind: std::io::ErrorKind::NotFound,
                        ..
                    }) => false,
                    Err(error) => return Err(error.into()),
                }
            }
        };
        Ok(exists.then_some(path))
    }

    pub fn install(&self, source: &str, scope: PackageScope) -> Result<PackageOperation> {
        self.assert_scope_trusted(scope)?;
        let _operation_lock = self.lock_operations()?;
        let parsed = parse_cli_source(source, &self.cwd)?;
        let settings_source = self.settings_source(source, &parsed, scope)?;
        self.install_locked(parsed, settings_source, scope, true)
    }

    pub fn remove(&self, source: &str, scope: PackageScope) -> Result<bool> {
        self.assert_scope_trusted(scope)?;
        let _operation_lock = self.lock_operations()?;
        let configured = self.load_configured_scope(scope)?;
        let requested_identities = input_identity_candidates(source, &self.cwd)?;
        let Some(entry) = configured.into_iter().find(|entry| {
            configured_identity(&entry.source, scope, self)
                .is_ok_and(|identity| requested_identities.contains(&identity))
        }) else {
            return Ok(false);
        };
        if entry.source.trim_start().starts_with("npm:") {
            bail!(npm_deferred_error());
        }
        let parsed = self.parse_configured_source(&entry.source, scope)?;
        self.remove_locked(&parsed, &entry.source, scope)?;
        Ok(true)
    }

    pub fn list(&self) -> Result<Vec<ConfiguredPackage>> {
        let mut result = Vec::new();
        self.append_configured_packages(PackageScope::Global, &mut result)?;
        if self.project_trusted {
            self.append_configured_packages(PackageScope::Project, &mut result)?;
        }
        Ok(result)
    }

    /// Preview configured package updates without changing checkouts, settings,
    /// package state, or operation locks. Git sources compare the installed HEAD
    /// to the commit resolved directly from the configured remote/ref. Local
    /// sources compare freshly discovered manifest/resources to installed state.
    pub fn check_available_updates(&self) -> Result<Vec<PackageUpdate>> {
        let mut updates = self.check_scope_available_updates(PackageScope::Global)?;
        if self.project_trusted {
            updates.extend(self.check_scope_available_updates(PackageScope::Project)?);
        }
        Ok(updates)
    }

    /// Reconcile every configured git/local package. Pinned git refs are fetched
    /// and reset to the configured ref; unpinned sources follow remote HEAD.
    pub fn update_all(&self) -> Result<Vec<PackageOperation>> {
        let _operation_lock = self.lock_operations()?;
        let mut configured = self.load_configured_scope(PackageScope::Global)?;
        if self.project_trusted {
            configured.extend(self.load_configured_scope(PackageScope::Project)?);
        }

        let mut operations = Vec::new();
        for entry in configured {
            let scope = entry.scope;
            if entry.source.trim_start().starts_with("npm:") {
                bail!(
                    "{}; configured source {} cannot be updated",
                    npm_deferred_error(),
                    entry.source
                );
            }
            let parsed = self.parse_configured_source(&entry.source, scope)?;
            operations.push(self.install_locked(parsed, entry.source, scope, false)?);
        }
        Ok(operations)
    }

    pub fn update_one(&self, source: &str) -> Result<Vec<PackageOperation>> {
        let _operation_lock = self.lock_operations()?;
        let identities = input_identity_candidates(source, &self.cwd)?;
        let mut configured = self.load_configured_scope(PackageScope::Global)?;
        if self.project_trusted {
            configured.extend(self.load_configured_scope(PackageScope::Project)?);
        }
        let mut matched = Vec::new();
        for entry in configured {
            let identity = configured_identity(&entry.source, entry.scope, self)?;
            if identities.contains(&identity) {
                matched.push(entry);
            }
        }
        if matched.is_empty() {
            bail!("no matching configured package found for {source}");
        }

        let mut operations = Vec::new();
        for entry in matched {
            if entry.source.trim_start().starts_with("npm:") {
                bail!(npm_deferred_error());
            }
            let parsed = self.parse_configured_source(&entry.source, entry.scope)?;
            operations.push(self.install_locked(parsed, entry.source, entry.scope, false)?);
        }
        Ok(operations)
    }

    /// Resolve effective configured package resources. Project entries win over
    /// global entries with the same package identity.
    pub fn resolve_resources(&self) -> Result<PackageResources> {
        let global = self.load_configured_scope(PackageScope::Global)?;
        let project = if self.project_trusted && self.project.is_some() {
            self.load_configured_scope(PackageScope::Project)?
        } else {
            Vec::new()
        };
        let mut effective = Vec::new();
        let mut seen = HashSet::new();
        for entry in project.into_iter().chain(global) {
            let identity = configured_identity(&entry.source, entry.scope, self)?;
            if seen.insert(identity) {
                effective.push(entry);
            }
        }

        let mut resources = PackageResources::default();
        let mut paths = HashSet::new();
        for entry in effective {
            if entry.source.trim_start().starts_with("npm:") {
                continue;
            }
            let parsed = self.parse_configured_source(&entry.source, entry.scope)?;
            let root = self.installed_root(&parsed, entry.scope)?;
            let store_relative = match &parsed.kind {
                ManagedPackageSourceKind::Local { .. } => None,
                ManagedPackageSourceKind::Git { .. } => {
                    let relative = self.installed_relative(&parsed)?;
                    if self.scope_lease(entry.scope)?.metadata(&relative).is_err() {
                        continue;
                    }
                    Some(relative)
                }
            };
            let discovered = discover_package(
                &self.capabilities,
                store_relative.as_deref().map(|relative| {
                    (
                        self.scope_lease(entry.scope)
                            .expect("scope already validated"),
                        relative,
                    )
                }),
                &root,
                &parsed.identity(),
                entry.scope,
                true,
            )?;
            resources.append_deduped(discovered.resources, &mut paths);
        }
        Ok(resources)
    }

    fn assert_scope_trusted(&self, scope: PackageScope) -> Result<()> {
        if scope == PackageScope::Project && !self.project_trusted {
            bail!("project is not trusted; refusing to access project package storage");
        }
        Ok(())
    }

    fn lock_operations(&self) -> Result<OperationLock<'_>> {
        OperationLock::acquire(&self.global)
    }

    fn parse_configured_source(
        &self,
        source: &str,
        scope: PackageScope,
    ) -> Result<ManagedPackageSource> {
        if source.trim_start().starts_with("npm:") {
            bail!(npm_deferred_error());
        }
        parse_package_source(source, &self.scope_root(scope))
    }

    fn settings_source(
        &self,
        original: &str,
        parsed: &ManagedPackageSource,
        scope: PackageScope,
    ) -> Result<String> {
        match &parsed.kind {
            ManagedPackageSourceKind::Git { .. } => Ok(original.trim().to_string()),
            ManagedPackageSourceKind::Local { path } => {
                if scope == PackageScope::Project {
                    let base = self.scope_root(scope);
                    if let Ok(relative) = path.strip_prefix(&base)
                        && !relative.as_os_str().is_empty()
                    {
                        return Ok(path_to_settings_string(relative));
                    }
                }
                Ok(path.to_string_lossy().into_owned())
            }
        }
    }

    fn install_locked(
        &self,
        source: ManagedPackageSource,
        settings_source: String,
        scope: PackageScope,
        persist_settings: bool,
    ) -> Result<PackageOperation> {
        source.validate_version()?;
        self.assert_scope_trusted(scope)?;
        let identity = source.identity();
        let lease = self.scope_lease(scope)?;
        let state_path = self.state_path(scope);
        let mut state = load_state(lease, Path::new(STATE_FILE_NAME), &state_path)?;
        let state_snapshot =
            FileSnapshot::capture(lease.clone(), PathBuf::from(STATE_FILE_NAME), state_path)?;
        let settings_path = self.settings_path(scope);
        let (settings_snapshot, next_settings) = if persist_settings {
            let snapshot = FileSnapshot::capture(
                lease.clone(),
                PathBuf::from(SETTINGS_FILE_NAME),
                settings_path.clone(),
            )?;
            let next = add_source_to_settings(
                snapshot.bytes.as_deref(),
                &settings_source,
                &identity,
                scope,
                self,
            )?;
            (Some(snapshot), next)
        } else {
            (None, None)
        };

        match &source.kind {
            ManagedPackageSourceKind::Local { path } => {
                if !operator_path_exists(&self.capabilities, path) {
                    bail!("local package path does not exist: {}", path.display());
                }
                let discovered =
                    discover_package(&self.capabilities, None, path, &identity, scope, true)?;
                let installed =
                    installed_package(source.clone(), scope, path.clone(), None, discovered)?;
                state.upsert(installed);
                commit_metadata(
                    lease,
                    &state,
                    &state_snapshot,
                    next_settings.as_deref(),
                    settings_snapshot.as_ref(),
                )?;
                Ok(PackageOperation {
                    source: settings_source,
                    identity,
                    scope,
                    root: path.clone(),
                    revision: None,
                })
            }
            ManagedPackageSourceKind::Git { .. } => {
                let target_relative = self.installed_relative(&source)?;
                let target = self.scope_root(scope).join(&target_relative);
                let staging_relative = unique_sibling_relative(&target_relative, "staging");
                let staging = self.scope_root(scope).join(&staging_relative);
                let mut staging_guard =
                    RemovePathOnDrop::new(lease.clone(), staging_relative.clone());
                prepare_git_checkout(
                    lease,
                    self.process_context()?,
                    &self.cwd,
                    &source,
                    &target_relative,
                    &target,
                    &staging_relative,
                    &staging,
                )?;
                discover_package(
                    &self.capabilities,
                    Some((lease, &staging_relative)),
                    &staging,
                    &identity,
                    scope,
                    true,
                )
                .context("validating staged package resources")?;
                let mut checkout_swap = CheckoutSwap::activate(
                    lease.clone(),
                    staging_relative,
                    target_relative.clone(),
                    target.clone(),
                )?;
                staging_guard.disarm();
                let result = (|| -> Result<PackageOperation> {
                    let revision = git_revision(self.process_context()?, &self.cwd, &target)?;
                    let discovered = discover_package(
                        &self.capabilities,
                        Some((lease, &target_relative)),
                        &target,
                        &identity,
                        scope,
                        true,
                    )?;
                    let installed = installed_package(
                        source.clone(),
                        scope,
                        target.clone(),
                        Some(revision.clone()),
                        discovered,
                    )?;
                    state.upsert(installed);
                    commit_metadata(
                        lease,
                        &state,
                        &state_snapshot,
                        next_settings.as_deref(),
                        settings_snapshot.as_ref(),
                    )?;
                    Ok(PackageOperation {
                        source: settings_source,
                        identity,
                        scope,
                        root: target,
                        revision: Some(revision),
                    })
                })();
                match result {
                    Ok(operation) => {
                        checkout_swap.finalize();
                        Ok(operation)
                    }
                    Err(error) => {
                        if let Err(rollback_error) = checkout_swap.rollback() {
                            return Err(anyhow!(
                                "{error:#}; additionally failed to restore the previous checkout: {rollback_error:#}"
                            ));
                        }
                        Err(error)
                    }
                }
            }
        }
    }

    fn remove_locked(
        &self,
        source: &ManagedPackageSource,
        settings_source: &str,
        scope: PackageScope,
    ) -> Result<()> {
        let identity = source.identity();
        let lease = self.scope_lease(scope)?;
        let state_path = self.state_path(scope);
        let state_snapshot = FileSnapshot::capture(
            lease.clone(),
            PathBuf::from(STATE_FILE_NAME),
            state_path.clone(),
        )?;
        let mut state = load_state(lease, Path::new(STATE_FILE_NAME), &state_path)?;
        state.remove(&identity);

        let settings_path = self.settings_path(scope);
        let settings_snapshot = FileSnapshot::capture(
            lease.clone(),
            PathBuf::from(SETTINGS_FILE_NAME),
            settings_path.clone(),
        )?;
        let next_settings = remove_source_from_settings(
            settings_snapshot.bytes.as_deref(),
            settings_source,
            &identity,
            scope,
            self,
        )?;

        let checkout = match &source.kind {
            ManagedPackageSourceKind::Git { .. } => Some(self.installed_root(source, scope)?),
            ManagedPackageSourceKind::Local { .. } => None,
        };
        let mut removed_checkout = match checkout {
            Some(path)
                if self
                    .scope_lease(scope)?
                    .metadata(&self.installed_relative(source)?)
                    .is_ok() =>
            {
                Some(RemovedCheckout::activate(
                    self.scope_lease(scope)?.clone(),
                    self.installed_relative(source)?,
                    path,
                )?)
            }
            _ => None,
        };

        let metadata_result = commit_metadata(
            lease,
            &state,
            &state_snapshot,
            next_settings.as_deref(),
            Some(&settings_snapshot),
        );
        if let Err(error) = metadata_result {
            if let Some(removed) = &mut removed_checkout
                && let Err(rollback_error) = removed.rollback()
            {
                return Err(anyhow!(
                    "{error:#}; additionally failed to restore the previous checkout: {rollback_error:#}"
                ));
            }
            return Err(error);
        }
        if let Some(removed) = removed_checkout {
            removed.finalize();
        }
        Ok(())
    }

    fn installed_root(
        &self,
        source: &ManagedPackageSource,
        scope: PackageScope,
    ) -> Result<PathBuf> {
        match &source.kind {
            ManagedPackageSourceKind::Local { path } => Ok(path.clone()),
            ManagedPackageSourceKind::Git { .. } => Ok(self
                .scope_root(scope)
                .join(self.installed_relative(source)?)),
        }
    }

    fn installed_relative(&self, source: &ManagedPackageSource) -> Result<PathBuf> {
        match &source.kind {
            ManagedPackageSourceKind::Local { .. } => {
                Err(anyhow!("local packages are not store-relative"))
            }
            ManagedPackageSourceKind::Git { host, path, .. } => {
                managed_join(Path::new("git"), host, path)
            }
        }
    }

    fn append_configured_packages(
        &self,
        scope: PackageScope,
        result: &mut Vec<ConfiguredPackage>,
    ) -> Result<()> {
        for entry in self.load_configured_scope(scope)? {
            if entry.source.trim_start().starts_with("npm:") {
                result.push(ConfiguredPackage {
                    source: entry.source,
                    scope,
                    installed_path: None,
                    pinned: false,
                    supported: false,
                });
                continue;
            }
            let parsed = self.parse_configured_source(&entry.source, scope)?;
            let installed_path = self.installed_root(&parsed, scope)?;
            result.push(ConfiguredPackage {
                source: entry.source,
                scope,
                installed_path: match &parsed.kind {
                    ManagedPackageSourceKind::Local { path }
                        if operator_path_exists(&self.capabilities, path) =>
                    {
                        Some(installed_path)
                    }
                    ManagedPackageSourceKind::Git { .. }
                        if self
                            .scope_lease(scope)?
                            .metadata(&self.installed_relative(&parsed)?)
                            .is_ok() =>
                    {
                        Some(installed_path)
                    }
                    _ => None,
                },
                pinned: parsed.is_pinned(),
                supported: true,
            });
        }
        Ok(())
    }

    fn check_scope_available_updates(&self, scope: PackageScope) -> Result<Vec<PackageUpdate>> {
        self.assert_scope_trusted(scope)?;
        let state_path = self.state_path(scope);
        let state = load_state(
            self.scope_lease(scope)?,
            Path::new(STATE_FILE_NAME),
            &state_path,
        )?;
        let mut updates = Vec::new();
        for entry in self.load_configured_scope(scope)? {
            if entry.source.trim_start().starts_with("npm:") {
                // Preview only: list already marks npm as unsupported. Never
                // fabricate update metadata that would imply install support.
                continue;
            }
            let parsed = self.parse_configured_source(&entry.source, scope)?;
            let identity = parsed.identity();
            let Some(root) = self.installed_path(&entry.source, scope)? else {
                continue;
            };
            let update_type = match &parsed.kind {
                ManagedPackageSourceKind::Git { .. } => {
                    let local = git_revision(self.process_context()?, &self.cwd, &root)?;
                    let Ok(remote) =
                        resolve_remote_git_revision(self.process_context()?, &self.cwd, &parsed)
                    else {
                        continue;
                    };
                    (local != remote).then_some(PackageUpdateType::Git)
                }
                ManagedPackageSourceKind::Local { path } => {
                    let discovered =
                        discover_package(&self.capabilities, None, path, &identity, scope, true)?;
                    let installed = state
                        .packages
                        .iter()
                        .find(|package| package.identity == identity);
                    local_snapshot_changed(installed, &parsed, scope, path, &discovered)
                        .then_some(PackageUpdateType::LocalChanged)
                }
            };
            let Some(update_type) = update_type else {
                continue;
            };
            updates.push(PackageUpdate {
                source: entry.source,
                display_name: package_display_name(&parsed),
                update_type,
                scope,
            });
        }
        Ok(updates)
    }

    fn load_configured_scope(&self, scope: PackageScope) -> Result<Vec<SettingsPackageEntry>> {
        self.assert_scope_trusted(scope)?;
        load_settings_packages(self.scope_lease(scope)?, &self.settings_path(scope), scope)
    }

    /// Re-discover configured packages and their resources for one scope, for
    /// `rpi config`. Refuses project scope when untrusted. npm entries are
    /// skipped. A package whose checkout is missing is omitted; a present but
    /// invalid `package.json#pi` manifest is an error so no settings write can
    /// proceed against a corrupted package.
    pub fn discover_scope_packages(&self, scope: PackageScope) -> Result<Vec<ScopePackage>> {
        self.assert_scope_trusted(scope)?;
        let mut out = Vec::new();
        for entry in self.load_configured_scope(scope)? {
            if entry.source.trim_start().starts_with("npm:") {
                // Config discovery never mutates; npm entries stay list-only
                // unsupported and cannot contribute resources.
                continue;
            }
            let parsed = self.parse_configured_source(&entry.source, scope)?;
            let root = self.installed_root(&parsed, scope)?;
            let store_relative = match &parsed.kind {
                ManagedPackageSourceKind::Local { .. } => None,
                ManagedPackageSourceKind::Git { .. } => {
                    let relative = self.installed_relative(&parsed)?;
                    if self.scope_lease(scope)?.metadata(&relative).is_err() {
                        continue;
                    }
                    Some(relative)
                }
            };
            let identity = parsed.identity();
            let discovered = discover_package(
                &self.capabilities,
                store_relative.as_deref().map(|relative| {
                    (
                        self.scope_lease(scope).expect("scope already validated"),
                        relative,
                    )
                }),
                &root,
                &identity,
                scope,
                true,
            )?;
            out.push(ScopePackage {
                identity,
                source: entry.source,
                scope,
                root,
                manifest: discovered.manifest,
                resources: discovered.resources,
            });
        }
        Ok(out)
    }
}

pub fn parse_package_source(source: &str, base_dir: &Path) -> Result<ManagedPackageSource> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        bail!("package source cannot be empty");
    }
    if trimmed.starts_with("npm:") {
        bail!(npm_deferred_error());
    }
    if let Some(git) = parse_git_source(trimmed)? {
        return Ok(git);
    }
    if looks_like_unsupported_url(trimmed) {
        bail!("unsupported package URL: {trimmed}");
    }
    let path = absolute_from(base_dir, Path::new(trimmed))?;
    Ok(ManagedPackageSource::local(path))
}

/// Parse a git protocol URL or a `git:` shorthand. Without `git:`, only
/// explicit protocol URLs are accepted.
pub fn parse_git_source(source: &str) -> Result<Option<ManagedPackageSource>> {
    let trimmed = source.trim();
    let explicit_protocol = has_git_protocol(trimmed);
    let has_prefix = trimmed.starts_with("git:") && !trimmed.starts_with("git://");
    if !explicit_protocol && !has_prefix {
        return Ok(None);
    }
    let raw = if has_prefix {
        trimmed["git:".len()..].trim()
    } else {
        trimmed
    };
    if raw.is_empty() {
        bail!("git package source is missing a repository");
    }

    let (repo_without_ref, reference) = split_git_reference(raw)?;
    if let Some(reference) = &reference {
        validate_git_reference(reference)?;
    }

    let (repo, host, path) = if let Some((scheme, remainder)) = split_protocol(&repo_without_ref) {
        if !matches!(scheme, "https" | "http" | "ssh" | "git") {
            bail!("unsupported git URL scheme: {scheme}");
        }
        let slash = remainder
            .find('/')
            .ok_or_else(|| anyhow!("git URL is missing a repository path"))?;
        let authority = &remainder[..slash];
        let path = remainder[slash + 1..].to_string();
        let host = authority_host(authority)?;
        (repo_without_ref.clone(), host, path)
    } else if let Some(rest) = repo_without_ref.strip_prefix("git@") {
        let colon = rest
            .find(':')
            .ok_or_else(|| anyhow!("invalid git SSH shorthand"))?;
        let host = rest[..colon].to_string();
        let path = rest[colon + 1..].to_string();
        (repo_without_ref.clone(), host, path)
    } else {
        if !has_prefix {
            return Ok(None);
        }
        let slash = repo_without_ref
            .find('/')
            .ok_or_else(|| anyhow!("git shorthand must be host/owner/repository"))?;
        let host = repo_without_ref[..slash].to_string();
        if !host.contains('.') && host != "localhost" {
            bail!("git shorthand host must be a domain name or localhost");
        }
        let path = repo_without_ref[slash + 1..].to_string();
        (format!("https://{repo_without_ref}"), host, path)
    };

    let host = normalize_git_host(&host)?;
    let path = normalize_git_path(&path)?;
    Ok(Some(ManagedPackageSource::git(repo, host, path, reference)))
}

fn parse_cli_source(source: &str, cwd: &Path) -> Result<ManagedPackageSource> {
    parse_package_source(source, cwd)
}

fn input_identity(source: &str, cwd: &Path) -> Result<String> {
    if source.trim_start().starts_with("npm:") {
        bail!(npm_deferred_error());
    }
    Ok(parse_cli_source(source, cwd)?.identity())
}

fn input_identity_candidates(source: &str, cwd: &Path) -> Result<HashSet<String>> {
    if source.trim_start().starts_with("npm:") {
        bail!(npm_deferred_error());
    }
    let mut identities = HashSet::new();
    identities.insert(parse_cli_source(source, cwd)?.identity());
    if !source.starts_with("git:") && !has_git_protocol(source) {
        let shorthand = format!("git:{source}");
        if let Ok(Some(git)) = parse_git_source(&shorthand) {
            identities.insert(git.identity());
        }
    }
    Ok(identities)
}

const fn npm_deferred_error() -> &'static str {
    "npm package sources are not supported yet; use a local path or git source"
}

fn split_git_reference(raw: &str) -> Result<(String, Option<String>)> {
    if raw
        .bytes()
        .any(|byte| matches!(byte, b'\0' | b'\n' | b'\r'))
    {
        bail!("git package source contains an invalid control character");
    }
    if raw.contains('?') || raw.contains('#') {
        bail!("git package URLs cannot contain query strings or fragments");
    }

    if let Some(rest) = raw.strip_prefix("git@") {
        let colon = rest
            .find(':')
            .ok_or_else(|| anyhow!("invalid git SSH shorthand"))?;
        let path_start = "git@".len() + colon + 1;
        return split_reference_at(raw, path_start);
    }

    if let Some((_scheme, remainder)) = split_protocol(raw) {
        let authority_len = remainder.find('/').unwrap_or(remainder.len());
        let path_start = raw.len() - remainder.len() + authority_len.saturating_add(1);
        return split_reference_at(raw, path_start.min(raw.len()));
    }

    let slash = raw
        .find('/')
        .ok_or_else(|| anyhow!("git shorthand must be host/owner/repository"))?;
    split_reference_at(raw, slash + 1)
}

fn split_reference_at(raw: &str, path_start: usize) -> Result<(String, Option<String>)> {
    let path = &raw[path_start..];
    let Some(relative_at) = path.find('@') else {
        return Ok((raw.trim_end_matches('/').to_string(), None));
    };
    let separator = path_start + relative_at;
    let repo = &raw[..separator];
    let reference = &raw[separator + 1..];
    if repo.is_empty() || reference.is_empty() {
        bail!("git package source has an empty repository or ref");
    }
    Ok((
        repo.trim_end_matches('/').to_string(),
        Some(reference.to_string()),
    ))
}

fn validate_git_reference(reference: &str) -> Result<()> {
    let decoded = percent_decode_for_validation(reference)?;
    for candidate in [reference, decoded.as_str()] {
        if candidate.is_empty()
            || candidate.starts_with('-')
            || candidate.starts_with('/')
            || candidate.ends_with('/')
            || candidate.ends_with('.')
            || candidate.contains("..")
            || candidate.contains("//")
            || candidate.contains("@{")
            || candidate
                .chars()
                .any(|ch| matches!(ch, '\\' | ':' | '?' | '*' | '[' | '^' | '~' | ' '))
            || candidate.bytes().any(|byte| byte.is_ascii_control())
            || candidate.split('/').any(|part| {
                part.is_empty() || part == "." || part == ".." || part.ends_with(".lock")
            })
        {
            bail!("unsafe or invalid git ref: {reference}");
        }
    }
    Ok(())
}

fn normalize_git_host(host: &str) -> Result<String> {
    let decoded = percent_decode_for_validation(host)?;
    for candidate in [host, decoded.as_str()] {
        if candidate.is_empty()
            || candidate.starts_with('/')
            || candidate.starts_with('.')
            || candidate.ends_with('.')
            || candidate.chars().any(|ch| matches!(ch, '/' | '\\' | '\0'))
            || candidate == ".."
            || candidate.bytes().any(|byte| byte.is_ascii_control())
        {
            bail!("unsafe git host: {host}");
        }
    }
    Ok(host.to_ascii_lowercase())
}

fn normalize_git_path(path: &str) -> Result<String> {
    let trimmed = path
        .trim_matches('/')
        .strip_suffix(".git")
        .unwrap_or(path.trim_matches('/'));
    let decoded = percent_decode_for_validation(trimmed)?;
    for candidate in [trimmed, decoded.as_str()] {
        if candidate.is_empty()
            || candidate.starts_with('/')
            || candidate.chars().any(|ch| matches!(ch, '\\' | '\0'))
            || candidate.bytes().any(|byte| byte.is_ascii_control())
            || candidate
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            bail!("unsafe git repository path: {path}");
        }
    }
    if trimmed.split('/').count() < 2 {
        bail!("git repository path must include an owner and repository");
    }
    Ok(trimmed.to_string())
}

fn authority_host(authority: &str) -> Result<String> {
    if authority.is_empty() {
        bail!("git URL is missing a host");
    }
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    if host_port.starts_with('[') {
        bail!("IPv6 git hosts are not supported for managed install paths");
    }
    let host = host_port.split(':').next().unwrap_or_default();
    if host.is_empty() {
        bail!("git URL is missing a host");
    }
    Ok(host.to_string())
}

fn split_protocol(value: &str) -> Option<(&str, &str)> {
    let separator = value.find("://")?;
    Some((&value[..separator], &value[separator + 3..]))
}

fn has_git_protocol(value: &str) -> bool {
    split_protocol(value).is_some_and(|(scheme, _)| {
        matches!(
            scheme.to_ascii_lowercase().as_str(),
            "https" | "http" | "ssh" | "git"
        )
    })
}

fn looks_like_unsupported_url(value: &str) -> bool {
    value.contains("://")
}

fn percent_decode_for_validation(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        if index + 2 >= bytes.len() {
            bail!("invalid percent escape in package source");
        }
        let high = hex_value(bytes[index + 1])
            .ok_or_else(|| anyhow!("invalid percent escape in package source"))?;
        let low = hex_value(bytes[index + 2])
            .ok_or_else(|| anyhow!("invalid percent escape in package source"))?;
        decoded.push((high << 4) | low);
        index += 3;
    }
    String::from_utf8(decoded).context("package source percent escape is not UTF-8")
}

const fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn prepare_git_checkout(
    lease: &CapabilityRootLease,
    process: &GovernedProcessContext,
    default_cwd: &Path,
    source: &ManagedPackageSource,
    target_relative: &Path,
    target: &Path,
    staging_relative: &Path,
    staging: &Path,
) -> Result<()> {
    let ManagedPackageSourceKind::Git {
        repo, reference, ..
    } = &source.kind
    else {
        bail!("internal error: attempted git checkout for a local package");
    };
    if let Some(parent) = staging_relative.parent() {
        lease
            .create_dir_all(parent, package_mode(lease))
            .with_context(|| format!("creating git staging directory {}", parent.display()))?;
    }
    match lease.metadata(staging_relative) {
        Ok(metadata) if metadata.identity.kind() == EntryKind::Directory => lease
            .remove_tree(staging_relative)
            .with_context(|| format!("removing stale git staging path {}", staging.display()))?,
        Ok(_) => bail!("git staging path is not a directory: {}", staging.display()),
        Err(error) if fs_error_kind(&error) == Some(std::io::ErrorKind::NotFound) => {}
        Err(error) => return Err(error).context("inspecting git staging path"),
    }

    let target_metadata = match lease.metadata(target_relative) {
        Ok(metadata) => Some(metadata),
        Err(error) if fs_error_kind(&error) == Some(std::io::ErrorKind::NotFound) => None,
        Err(error) => return Err(error).context("inspecting package checkout target"),
    };
    let git_relative = target_relative.join(".git");
    let is_git_checkout = target_metadata
        .as_ref()
        .is_some_and(|metadata| metadata.identity.kind() == EntryKind::Directory)
        && lease.metadata(&git_relative).is_ok();

    if is_git_checkout {
        let args = vec![
            "clone".to_string(),
            "--no-hardlinks".to_string(),
            "--no-checkout".to_string(),
            target.to_string_lossy().into_owned(),
            staging.to_string_lossy().into_owned(),
        ];
        run_git(process, default_cwd, &args, None)
            .context("cloning the previous checkout into staging")?;
        run_git(
            process,
            default_cwd,
            &[
                "remote".into(),
                "set-url".into(),
                "origin".into(),
                repo.clone(),
            ],
            Some(staging),
        )
        .context("setting the staged checkout origin")?;
    } else if target_metadata.is_some() {
        bail!(
            "package install path exists but is not a git checkout: {}",
            target.display()
        );
    } else {
        let args = vec![
            "clone".to_string(),
            "--no-checkout".to_string(),
            repo.clone(),
            staging.to_string_lossy().into_owned(),
        ];
        run_git(process, default_cwd, &args, None).context("cloning git package")?;
    }

    let commit = if let Some(reference) = reference {
        run_git(
            process,
            default_cwd,
            &[
                "fetch".into(),
                "--force".into(),
                "--no-tags".into(),
                "origin".into(),
                reference.clone(),
            ],
            Some(staging),
        )
        .with_context(|| format!("fetching configured git ref {reference}"))?;
        run_git_capture(
            process,
            default_cwd,
            &["rev-parse".into(), "FETCH_HEAD^{commit}".into()],
            Some(staging),
        )?
    } else {
        reconcile_remote_default(process, default_cwd, staging)?
    };

    run_git(
        process,
        default_cwd,
        &["reset".into(), "--hard".into(), commit.trim().into()],
        Some(staging),
    )
    .context("resetting staged package checkout")?;
    run_git(
        process,
        default_cwd,
        &["clean".into(), "-fdx".into()],
        Some(staging),
    )
    .context("cleaning staged package checkout")?;
    Ok(())
}

fn reconcile_remote_default(
    process: &GovernedProcessContext,
    default_cwd: &Path,
    checkout: &Path,
) -> Result<String> {
    run_git(
        process,
        default_cwd,
        &["fetch".into(), "--prune".into(), "origin".into()],
        Some(checkout),
    )
    .context("fetching remote default branch")?;
    let _ = run_git(
        process,
        default_cwd,
        &[
            "remote".into(),
            "set-head".into(),
            "origin".into(),
            "-a".into(),
        ],
        Some(checkout),
    );

    if let Ok(commit) = run_git_capture(
        process,
        default_cwd,
        &["rev-parse".into(), "origin/HEAD^{commit}".into()],
        Some(checkout),
    ) {
        return Ok(commit);
    }

    let remote = run_git_capture(
        process,
        default_cwd,
        &[
            "ls-remote".into(),
            "--symref".into(),
            "origin".into(),
            "HEAD".into(),
        ],
        Some(checkout),
    )
    .context("discovering remote default branch")?;
    let branch = remote.lines().find_map(|line| {
        let rest = line.strip_prefix("ref: refs/heads/")?;
        rest.strip_suffix("\tHEAD")
    });
    let branch = branch.ok_or_else(|| anyhow!("remote does not advertise a default branch"))?;
    validate_git_reference(branch)?;
    let remote_ref = format!("refs/remotes/origin/{branch}");
    run_git(
        process,
        default_cwd,
        &[
            "fetch".into(),
            "--force".into(),
            "origin".into(),
            format!("+refs/heads/{branch}:{remote_ref}"),
        ],
        Some(checkout),
    )?;
    run_git_capture(
        process,
        default_cwd,
        &["rev-parse".into(), format!("{remote_ref}^{{commit}}")],
        Some(checkout),
    )
}

fn resolve_remote_git_revision(
    process: &GovernedProcessContext,
    default_cwd: &Path,
    source: &ManagedPackageSource,
) -> Result<String> {
    let ManagedPackageSourceKind::Git {
        repo, reference, ..
    } = &source.kind
    else {
        bail!("cannot resolve a remote revision for a local package");
    };
    let output = if let Some(reference) = reference {
        validate_git_reference(reference)?;
        if is_git_object_id(reference) {
            let output = run_git_capture(
                process,
                default_cwd,
                &[
                    "ls-remote".into(),
                    "--exit-code".into(),
                    repo.clone(),
                    reference.clone(),
                ],
                None,
            )?;
            let advertised = output.lines().any(|line| {
                line.split_once('\t')
                    .is_some_and(|(revision, _)| revision.eq_ignore_ascii_case(reference))
            });
            if !advertised {
                bail!("remote does not advertise configured git commit {reference}");
            }
            return Ok(reference.to_ascii_lowercase());
        }
        let names = remote_reference_names(reference);
        let mut args = vec![
            "ls-remote".to_string(),
            "--exit-code".to_string(),
            repo.clone(),
        ];
        args.extend(names.iter().cloned());
        run_git_capture(process, default_cwd, &args, None)?
    } else {
        run_git_capture(
            process,
            default_cwd,
            &[
                "ls-remote".into(),
                "--symref".into(),
                "--exit-code".into(),
                repo.clone(),
                "HEAD".into(),
            ],
            None,
        )?
    };
    let revision = match reference {
        Some(reference) => remote_revision_for_reference(&output, reference)?,
        None => remote_revision_for_name(&output, "HEAD")
            .ok_or_else(|| anyhow!("remote did not advertise HEAD"))?
            .to_string(),
    };
    if !is_git_object_id(&revision) {
        bail!("remote advertised an invalid git object id");
    }
    Ok(revision.to_ascii_lowercase())
}

fn remote_reference_names(reference: &str) -> Vec<String> {
    if reference.starts_with("refs/") {
        return vec![reference.to_string(), format!("{reference}^{{}}")];
    }
    vec![
        format!("refs/heads/{reference}"),
        format!("refs/tags/{reference}"),
        format!("refs/tags/{reference}^{{}}"),
    ]
}

fn remote_revision_for_reference(output: &str, reference: &str) -> Result<String> {
    if reference.starts_with("refs/") {
        let direct = remote_revision_for_name(output, reference);
        let peeled_name = format!("{reference}^{{}}");
        return remote_revision_for_name(output, &peeled_name)
            .or(direct)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("remote did not advertise the configured git ref {reference}"));
    }

    let branch_name = format!("refs/heads/{reference}");
    let tag_name = format!("refs/tags/{reference}");
    let peeled_tag_name = format!("{tag_name}^{{}}");
    let branch = remote_revision_for_name(output, &branch_name);
    let tag = remote_revision_for_name(output, &peeled_tag_name)
        .or_else(|| remote_revision_for_name(output, &tag_name));
    match (branch, tag) {
        (Some(branch), Some(tag)) if branch != tag => {
            bail!("configured git ref is ambiguous between a branch and tag: {reference}")
        }
        (Some(revision), _) | (_, Some(revision)) => Ok(revision.to_string()),
        (None, None) => bail!("remote did not advertise the configured git ref {reference}"),
    }
}

fn remote_revision_for_name<'a>(output: &'a str, name: &str) -> Option<&'a str> {
    output.lines().find_map(|line| {
        let (revision, advertised_name) = line.split_once('\t')?;
        (advertised_name == name).then_some(revision)
    })
}

fn is_git_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn git_revision(
    process: &GovernedProcessContext,
    default_cwd: &Path,
    checkout: &Path,
) -> Result<String> {
    let revision = run_git_capture(
        process,
        default_cwd,
        &["rev-parse".into(), "HEAD".into()],
        Some(checkout),
    )?;
    let revision = revision.trim();
    if revision.is_empty() {
        bail!("git checkout has no HEAD revision");
    }
    Ok(revision.to_string())
}

fn run_git(
    process: &GovernedProcessContext,
    default_cwd: &Path,
    args: &[String],
    cwd: Option<&Path>,
) -> Result<()> {
    let output = git_command(process, default_cwd, args, cwd)?;
    if output.status == Some(0) {
        return Ok(());
    }
    git_failure(&output)
}

fn run_git_capture(
    process: &GovernedProcessContext,
    default_cwd: &Path,
    args: &[String],
    cwd: Option<&Path>,
) -> Result<String> {
    let output = git_command(process, default_cwd, args, cwd)?;
    if output.status != Some(0) {
        git_failure(&output)?;
    }
    String::from_utf8(output.stdout).context("git output was not UTF-8")
}

fn git_command(
    process: &GovernedProcessContext,
    default_cwd: &Path,
    args: &[String],
    cwd: Option<&Path>,
) -> Result<GovernedProcessOutput> {
    let mut git_args = vec!["-c".into(), "color.ui=false".into()];
    git_args.extend_from_slice(args);
    process.run_blocking(
        pi_sandbox::SpawnFamily::PackageHelper,
        GovernedProcessSpec {
            program: process.git_program()?,
            args: git_args,
            cwd: cwd.unwrap_or(default_cwd).to_path_buf(),
            environment: std::collections::BTreeMap::from([(
                "GIT_TERMINAL_PROMPT".to_owned(),
                Some("0".to_owned()),
            )]),
            timeout: GIT_COMMAND_TIMEOUT,
            stdout_bytes: MAX_GIT_STDOUT_BYTES,
            stderr_bytes: MAX_GIT_STDERR_BYTES,
            #[cfg(unix)]
            mount_sources: Vec::new(),
            #[cfg(unix)]
            retained_executable: None,
            #[cfg(unix)]
            retained_working_directory: None,
        },
    )
}

fn git_failure(output: &GovernedProcessOutput) -> Result<()> {
    if output.timed_out {
        bail!("git operation timed out");
    }
    if output.cancelled {
        bail!("git operation was cancelled");
    }
    if output.stdout_capped || output.stderr_capped {
        bail!("git operation exceeded its output limit");
    }
    let stderr = crate::redact::redact_bounded(
        &String::from_utf8_lossy(&output.stderr),
        MAX_GIT_STDERR_BYTES,
    );
    let message = stderr.trim();
    if message.is_empty() {
        bail!("git operation failed with status {:?}", output.status);
    }
    bail!("git operation failed: {message}")
}

struct DiscoveredPackage {
    manifest: PackageManifest,
    resources: PackageResources,
}

fn discover_package(
    capabilities: &Arc<CapabilityFs>,
    store_root: Option<(&CapabilityRootLease, &Path)>,
    root: &Path,
    package_id: &str,
    scope: PackageScope,
    trusted: bool,
) -> Result<DiscoveredPackage> {
    let lease = if let Some((scope_lease, relative)) = store_root {
        scope_lease
            .lease_directory(relative, CapabilityAccess::ReadOnly, RootCreation::Existing)
            .context("leasing installed package root")?
    } else {
        let absolute = AbsolutePath::parse(root.to_path_buf()).map_err(|_| FsError::InvalidPath)?;
        let directory = capabilities.open_root(
            CapabilityRootPurpose::OperatorImport,
            absolute.clone(),
            CapabilityAccess::ReadOnly,
            RootCreation::Existing,
        );
        match directory {
            Ok(lease) => lease,
            Err(directory_error) => match capabilities
                .open_exact_file(CapabilityRootPurpose::OperatorImport, absolute)
            {
                Ok(file) => {
                    return Ok(DiscoveredPackage {
                        manifest: PackageManifest::default(),
                        resources: PackageResources {
                            extensions: vec![PackageResourceSpec {
                                kind: PackageResourceKind::Extension,
                                path: file.display_path(),
                                managed_root: Some(root.to_path_buf()),
                                package_id: package_id.to_string(),
                                scope,
                                trusted,
                            }],
                            ..PackageResources::default()
                        },
                    });
                }
                Err(file_error)
                    if fs_error_kind(&directory_error) == Some(std::io::ErrorKind::NotFound)
                        || fs_error_kind(&file_error) == Some(std::io::ErrorKind::NotFound) =>
                {
                    bail!("package root does not exist: {}", root.display());
                }
                Err(file_error) => {
                    return Err(file_error).context("opening package source");
                }
            },
        }
    };
    let manifest = read_package_manifest(&lease, root)?;
    let entries = collect_package_entries(lease.open_directory(Path::new(""))?, PathBuf::new())?;
    let mut resources = PackageResources::default();
    if let Some(manifest) = &manifest {
        resources.extensions = discover_manifest_resources(
            root,
            &entries,
            &manifest.extensions,
            PackageResourceKind::Extension,
        )?;
        resources.skills = discover_manifest_resources(
            root,
            &entries,
            &manifest.skills,
            PackageResourceKind::Skill,
        )?;
        resources.prompts = discover_manifest_resources(
            root,
            &entries,
            &manifest.prompts,
            PackageResourceKind::Prompt,
        )?;
        resources.themes = discover_manifest_resources(
            root,
            &entries,
            &manifest.themes,
            PackageResourceKind::Theme,
        )?;
    } else {
        resources.extensions =
            discover_conventional(root, &entries, "extensions", PackageResourceKind::Extension);
        resources.skills =
            discover_conventional(root, &entries, "skills", PackageResourceKind::Skill);
        resources.prompts =
            discover_conventional(root, &entries, "prompts", PackageResourceKind::Prompt);
        resources.themes =
            discover_conventional(root, &entries, "themes", PackageResourceKind::Theme);
    }
    for resource in resources
        .extensions
        .iter_mut()
        .chain(&mut resources.skills)
        .chain(&mut resources.prompts)
        .chain(&mut resources.themes)
    {
        resource.managed_root = Some(root.to_path_buf());
    }
    resources.remap_metadata(package_id, scope, trusted);
    Ok(DiscoveredPackage {
        manifest: manifest.unwrap_or_default(),
        resources,
    })
}

fn read_package_manifest(
    lease: &CapabilityRootLease,
    root: &Path,
) -> Result<Option<PackageManifest>> {
    let bytes = match lease.read_bounded(Path::new("package.json"), MAX_PACKAGE_MANIFEST_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if fs_error_kind(&error) == Some(std::io::ErrorKind::NotFound) => {
            return Ok(None);
        }
        Err(error) => return Err(error).context("reading package manifest"),
    };
    let path = root.join("package.json");
    let value: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing package manifest {}", path.display()))?;
    let object = value.as_object().ok_or_else(|| {
        anyhow!(
            "package manifest root must be a JSON object: {}",
            path.display()
        )
    })?;
    let Some(pi) = object.get("pi") else {
        return Ok(None);
    };
    let manifest: PackageManifest = serde_json::from_value(pi.clone())
        .with_context(|| format!("parsing rpi package manifest in {}", path.display()))?;
    manifest.validate()?;
    Ok(Some(manifest))
}

#[derive(Clone)]
struct OpenedPackageEntry {
    relative: PathBuf,
    kind: EntryKind,
}

fn collect_package_entries(
    directory: CapabilityDirectory,
    relative: PathBuf,
) -> Result<Vec<OpenedPackageEntry>> {
    let mut output = Vec::new();
    let mut entries = directory.read_dir()?;
    loop {
        let entry = match entries.next() {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) if crate::fs::is_link_blocked(&error) => {
                eprintln!(
                    "skipping package content symlink relative={}",
                    relative.display()
                );
                continue;
            }
            Err(error) => return Err(error).context("enumerating package entries"),
        };
        let name = entry.name().to_os_string();
        if hidden_or_ignored_name(&name) {
            continue;
        }
        let child_relative = relative.join(&name);
        match entry.into_entry() {
            CapabilityEntry::File(_) => output.push(OpenedPackageEntry {
                relative: child_relative,
                kind: EntryKind::RegularFile,
            }),
            CapabilityEntry::Directory(directory) => {
                output.push(OpenedPackageEntry {
                    relative: child_relative.clone(),
                    kind: EntryKind::Directory,
                });
                output.extend(collect_package_entries(directory, child_relative)?);
            }
        }
    }
    Ok(output)
}

fn discover_manifest_resources(
    root: &Path,
    available: &[OpenedPackageEntry],
    configured: &[String],
    kind: PackageResourceKind,
) -> Result<Vec<PackageResourceSpec>> {
    let mut inclusions = Vec::new();
    let mut exclusions = Vec::new();
    for entry in configured {
        validate_manifest_entry(entry)?;
        if let Some(exclusion) = entry.strip_prefix('!') {
            exclusions.push(compile_manifest_glob(exclusion)?);
        } else {
            inclusions.push(compile_manifest_glob(entry)?);
        }
    }
    let mut paths = BTreeSet::new();
    for entry in available {
        if entry.kind != EntryKind::RegularFile || !resource_file_matches(&entry.relative, kind) {
            continue;
        }
        let relative = entry.relative.to_string_lossy().replace('\\', "/");
        if inclusions.iter().any(|matcher| {
            matcher.is_match(&relative) || relative_ancestor_matches(&entry.relative, matcher)
        }) && !exclusions.iter().any(|matcher| {
            matcher.is_match(&relative) || relative_ancestor_matches(&entry.relative, matcher)
        }) {
            paths.insert(root.join(&entry.relative));
        }
    }
    Ok(paths
        .into_iter()
        .map(|path| empty_resource_spec(kind, path))
        .collect())
}

fn discover_conventional(
    root: &Path,
    available: &[OpenedPackageEntry],
    directory: &str,
    kind: PackageResourceKind,
) -> Vec<PackageResourceSpec> {
    let prefix = Path::new(directory);
    let mut paths = BTreeSet::new();
    for entry in available {
        if entry.kind == EntryKind::RegularFile
            && entry.relative.starts_with(prefix)
            && conventional_resource_file_matches(&entry.relative, kind)
        {
            paths.insert(root.join(&entry.relative));
        }
    }
    paths
        .into_iter()
        .map(|path| empty_resource_spec(kind, path))
        .collect()
}

/// Conventional (manifest-less) resource discovery. Skills are discovered only
/// through their `SKILL.md` entry point; auxiliary nested Markdown
/// (`skills/<name>/README.md`, `skills/<name>/examples/*.md`, …) is not a
/// skill and must not surface as one. Prompts and themes keep their
/// extension-based matching.
fn conventional_resource_file_matches(path: &Path, kind: PackageResourceKind) -> bool {
    match kind {
        PackageResourceKind::Skill => path
            .file_name()
            .is_some_and(|name| name == OsStr::new("SKILL.md")),
        _ => resource_file_matches(path, kind),
    }
}

fn empty_resource_spec(kind: PackageResourceKind, path: PathBuf) -> PackageResourceSpec {
    PackageResourceSpec {
        kind,
        path,
        managed_root: None,
        package_id: String::new(),
        scope: PackageScope::Global,
        trusted: false,
    }
}

fn resource_file_matches(path: &Path, kind: PackageResourceKind) -> bool {
    let extension = path.extension().and_then(OsStr::to_str).unwrap_or_default();
    match kind {
        PackageResourceKind::Extension => path
            .file_name()
            .is_some_and(|name| name == OsStr::new(crate::PROCESS_EXTENSION_MANIFEST_FILE)),
        PackageResourceKind::Skill | PackageResourceKind::Prompt => extension == "md",
        PackageResourceKind::Theme => extension == "json",
    }
}

fn hidden_or_ignored_name(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    name.starts_with('.') || name == "node_modules"
}

fn ensure_lexically_within(root: &Path, path: &Path) -> Result<()> {
    let clean_root = lexical_normalize(root)?;
    let clean_path = lexical_normalize(path)?;
    if clean_path != clean_root && !clean_path.starts_with(&clean_root) {
        bail!("path escapes package root: {}", path.display());
    }
    Ok(())
}

fn validate_manifest_entry(entry: &str) -> Result<()> {
    let path = entry.strip_prefix('!').unwrap_or(entry);
    RelativePath::parse(PathBuf::from(path))
        .with_context(|| format!("invalid package manifest entry {entry:?}"))?;
    Ok(())
}

fn compile_manifest_glob(pattern: &str) -> Result<GlobMatcher> {
    validate_manifest_entry(pattern)?;
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(false)
        .build()
        .with_context(|| format!("invalid package manifest glob {pattern:?}"))
        .map(|glob| glob.compile_matcher())
}

fn has_glob_meta(value: &str) -> bool {
    value.chars().any(|ch| matches!(ch, '*' | '?' | '[' | '{'))
}

fn relative_ancestor_matches(path: &Path, matcher: &GlobMatcher) -> bool {
    let mut ancestor = path.parent();
    while let Some(path) = ancestor {
        if matcher.is_match(path.to_string_lossy().replace('\\', "/")) {
            return true;
        }
        ancestor = path.parent();
    }
    false
}

fn normalized_relative(root: &Path, path: &Path) -> Result<String> {
    path.strip_prefix(root)
        .with_context(|| format!("{} is outside {}", path.display(), root.display()))
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
}

fn installed_package(
    source: ManagedPackageSource,
    scope: PackageScope,
    root: PathBuf,
    revision: Option<String>,
    discovered: DiscoveredPackage,
) -> Result<InstalledPackage> {
    let identity = source.identity();
    let installed_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    Ok(InstalledPackage {
        identity,
        scope,
        source,
        root,
        revision,
        manifest: discovered.manifest,
        resources: discovered.resources,
        installed_at_unix_ms,
    })
}

fn package_display_name(source: &ManagedPackageSource) -> String {
    match &source.kind {
        ManagedPackageSourceKind::Git { host, path, .. } => format!("{host}/{path}"),
        ManagedPackageSourceKind::Local { path } => path
            .file_name()
            .and_then(OsStr::to_str)
            .map_or_else(|| path.display().to_string(), str::to_string),
    }
}

fn local_snapshot_changed(
    installed: Option<&InstalledPackage>,
    source: &ManagedPackageSource,
    scope: PackageScope,
    root: &Path,
    discovered: &DiscoveredPackage,
) -> bool {
    let Some(installed) = installed else {
        return true;
    };
    installed.identity != source.identity()
        || installed.scope != scope
        || installed.source != *source
        || installed.root != root
        || installed.revision.is_some()
        || installed.manifest != discovered.manifest
        || installed.resources != discovered.resources
}

#[derive(Debug, Clone)]
struct SettingsPackageEntry {
    source: String,
    scope: PackageScope,
}

fn load_settings_packages(
    lease: &CapabilityRootLease,
    path: &Path,
    scope: PackageScope,
) -> Result<Vec<SettingsPackageEntry>> {
    let Some(bytes) = read_optional_bounded(
        lease,
        Path::new(SETTINGS_FILE_NAME),
        MAX_PACKAGE_METADATA_BYTES,
    )?
    else {
        return Ok(Vec::new());
    };
    let root = parse_settings(&bytes, path)?;
    let Some(packages) = root.get("packages") else {
        return Ok(Vec::new());
    };
    let packages = packages
        .as_array()
        .ok_or_else(|| anyhow!("settings `packages` must be an array: {}", path.display()))?;
    packages
        .iter()
        .map(|entry| {
            Ok(SettingsPackageEntry {
                source: settings_entry_source(entry)?.to_string(),
                scope,
            })
        })
        .collect()
}

fn add_source_to_settings(
    current: Option<&[u8]>,
    source: &str,
    identity: &str,
    scope: PackageScope,
    manager: &PackageManager,
) -> Result<Option<Vec<u8>>> {
    let mut root = parse_or_empty_settings(current, &manager.settings_path(scope))?;
    let object = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings root must be a JSON object"))?;
    let packages = packages_array_mut(object)?;
    let mut match_index = None;
    for (index, entry) in packages.iter().enumerate() {
        let existing_source = settings_entry_source(entry)?;
        if configured_identity(existing_source, scope, manager)? == identity {
            match_index = Some(index);
            break;
        }
    }

    match match_index {
        Some(index) => {
            if settings_entry_source(&packages[index])? == source {
                return Ok(None);
            }
            if let Some(object) = packages[index].as_object_mut() {
                object.insert("source".to_string(), Value::String(source.to_string()));
            } else {
                packages[index] = Value::String(source.to_string());
            }
        }
        None => packages.push(Value::String(source.to_string())),
    }
    Ok(Some(serialize_settings(&root)?))
}

fn remove_source_from_settings(
    current: Option<&[u8]>,
    _source: &str,
    identity: &str,
    scope: PackageScope,
    manager: &PackageManager,
) -> Result<Option<Vec<u8>>> {
    let Some(current) = current else {
        return Ok(None);
    };
    let mut root = parse_settings(current, &manager.settings_path(scope))?;
    let Some(object) = root.as_object_mut() else {
        bail!("settings root must be a JSON object");
    };
    let Some(packages_value) = object.get_mut("packages") else {
        return Ok(None);
    };
    let packages = packages_value.as_array_mut().ok_or_else(|| {
        anyhow!(
            "settings `packages` must be an array: {}",
            manager.settings_path(scope).display()
        )
    })?;
    let original_len = packages.len();
    let mut retained = Vec::with_capacity(original_len);
    for entry in packages.drain(..) {
        let entry_identity = configured_identity(settings_entry_source(&entry)?, scope, manager)?;
        if entry_identity != identity {
            retained.push(entry);
        }
    }
    *packages = retained;
    if packages.len() == original_len {
        return Ok(None);
    }
    Ok(Some(serialize_settings(&root)?))
}

fn configured_identity(
    source: &str,
    scope: PackageScope,
    manager: &PackageManager,
) -> Result<String> {
    if let Some(npm) = source.trim().strip_prefix("npm:") {
        let name = npm_package_name(npm)?;
        return Ok(format!("npm:{name}"));
    }
    Ok(manager.parse_configured_source(source, scope)?.identity())
}

fn npm_package_name(spec: &str) -> Result<&str> {
    let spec = spec.trim();
    if spec.is_empty() {
        bail!("npm package source is missing a package name");
    }
    if spec.starts_with('@') {
        let slash = spec
            .find('/')
            .ok_or_else(|| anyhow!("invalid scoped npm package source"))?;
        let version = spec[slash + 1..].find('@').map(|index| slash + 1 + index);
        Ok(version.map_or(spec, |index| &spec[..index]))
    } else {
        Ok(spec.split('@').next().unwrap_or(spec))
    }
}

fn settings_entry_source(entry: &Value) -> Result<&str> {
    if let Some(source) = entry.as_str() {
        return Ok(source);
    }
    entry
        .as_object()
        .and_then(|object| object.get("source"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            anyhow!("settings package entry must be a string or object with a string `source`")
        })
}

fn parse_or_empty_settings(current: Option<&[u8]>, path: &Path) -> Result<Value> {
    match current {
        Some(bytes) => parse_settings(bytes, path),
        None => Ok(Value::Object(Map::new())),
    }
}

fn parse_settings(bytes: &[u8], path: &Path) -> Result<Value> {
    let value: Value = serde_json::from_slice(bytes)
        .with_context(|| format!("parsing settings {}", path.display()))?;
    if !value.is_object() {
        bail!("settings root must be a JSON object: {}", path.display());
    }
    Ok(value)
}

fn packages_array_mut(object: &mut Map<String, Value>) -> Result<&mut Vec<Value>> {
    let packages = object
        .entry("packages".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    packages
        .as_array_mut()
        .ok_or_else(|| anyhow!("settings `packages` must be an array"))
}

fn serialize_settings(value: &Value) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value).context("serializing package settings")?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn load_state(
    lease: &CapabilityRootLease,
    relative: &Path,
    display_path: &Path,
) -> Result<PackageState> {
    let Some(bytes) = read_optional_bounded(lease, relative, MAX_PACKAGE_METADATA_BYTES)? else {
        return Ok(PackageState::default());
    };
    let state: PackageState = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing package state {}", display_path.display()))?;
    state.validate()?;
    Ok(state)
}

fn serialize_state(state: &PackageState) -> Result<Vec<u8>> {
    state.validate()?;
    let mut bytes = serde_json::to_vec_pretty(state).context("serializing package state")?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn commit_metadata(
    lease: &CapabilityRootLease,
    state: &PackageState,
    state_snapshot: &FileSnapshot,
    next_settings: Option<&[u8]>,
    settings_snapshot: Option<&FileSnapshot>,
) -> Result<()> {
    atomic_write(
        lease,
        Path::new(STATE_FILE_NAME),
        &serialize_state(state)?,
        state_snapshot.identity,
    )?;
    if let Some(settings) = next_settings
        && let Err(error) = atomic_write(
            lease,
            Path::new(SETTINGS_FILE_NAME),
            settings,
            settings_snapshot.and_then(|snapshot| snapshot.identity),
        )
    {
        let state_rollback = state_snapshot.restore();
        if let Some(snapshot) = settings_snapshot {
            let _ = snapshot.restore();
        }
        return match state_rollback {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(anyhow!(
                "{error:#}; additionally failed to restore package state: {rollback_error:#}"
            )),
        };
    }
    Ok(())
}

#[derive(Clone)]
struct FileSnapshot {
    lease: CapabilityRootLease,
    relative: PathBuf,
    display_path: PathBuf,
    identity: Option<FileIdentity>,
    bytes: Option<Vec<u8>>,
}

fn operator_path_exists(capabilities: &Arc<CapabilityFs>, path: &Path) -> bool {
    let Ok(absolute) = AbsolutePath::parse(path.to_path_buf()) else {
        return false;
    };
    capabilities
        .open_root(
            CapabilityRootPurpose::OperatorImport,
            absolute.clone(),
            CapabilityAccess::ReadOnly,
            RootCreation::Existing,
        )
        .is_ok()
        || capabilities
            .open_exact_file(CapabilityRootPurpose::OperatorImport, absolute)
            .is_ok()
}

impl FileSnapshot {
    fn capture(
        lease: CapabilityRootLease,
        relative: PathBuf,
        display_path: PathBuf,
    ) -> Result<Self> {
        let identity = optional_identity(&lease, &relative)?;
        let bytes = read_optional_bounded(&lease, &relative, MAX_PACKAGE_METADATA_BYTES)?;
        Ok(Self {
            lease,
            relative,
            display_path,
            identity,
            bytes,
        })
    }

    fn restore(&self) -> Result<()> {
        match &self.bytes {
            Some(bytes) => atomic_write(
                &self.lease,
                &self.relative,
                bytes,
                optional_identity(&self.lease, &self.relative)?,
            ),
            None => match self.lease.remove_file(&self.relative) {
                Ok(()) => Ok(()),
                Err(error) if fs_error_kind(&error) == Some(std::io::ErrorKind::NotFound) => Ok(()),
                Err(error) => {
                    Err(error).with_context(|| format!("removing {}", self.display_path.display()))
                }
            },
        }
    }
}

fn read_optional_bounded(
    lease: &CapabilityRootLease,
    relative: &Path,
    limit: u64,
) -> Result<Option<Vec<u8>>> {
    match lease.read_bounded_allowing_link(relative, limit) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if fs_error_kind(&error) == Some(std::io::ErrorKind::NotFound) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", relative.display())),
    }
}

fn atomic_write(
    lease: &CapabilityRootLease,
    relative: &Path,
    bytes: &[u8],
    expected: Option<FileIdentity>,
) -> Result<()> {
    lease
        .replace_file(Replacement {
            relative: relative.to_path_buf(),
            expected,
            bytes: bytes.to_vec(),
            mode: match lease.access() {
                CapabilityAccess::PrivateReadWrite => FileMode::Private,
                CapabilityAccess::ReadOnly | CapabilityAccess::ReadWrite => FileMode::Shared,
            },
        })
        .with_context(|| format!("atomically replacing {}", relative.display()))?;
    Ok(())
}

fn optional_identity(lease: &CapabilityRootLease, relative: &Path) -> Result<Option<FileIdentity>> {
    match lease.metadata_allowing_link(relative) {
        Ok(metadata) => Ok(Some(metadata.identity)),
        Err(error) if fs_error_kind(&error) == Some(std::io::ErrorKind::NotFound) => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("reading identity for {}", relative.display()))
        }
    }
}

fn fs_error_kind(error: &FsError) -> Option<std::io::ErrorKind> {
    match error {
        FsError::System { kind, .. } => Some(*kind),
        _ => None,
    }
}

struct OperationLock<'a> {
    _process: MutexGuard<'a, ()>,
    lease: CapabilityRootLease,
}

impl<'a> OperationLock<'a> {
    fn acquire(lease: &CapabilityRootLease) -> Result<Self> {
        let process = PROCESS_PACKAGE_LOCK.get_or_init(|| Mutex::new(())).lock();
        let path = Path::new(OPERATION_LOCK_FILE_NAME);
        let started = Instant::now();
        loop {
            match lease.create_new(path, FileMode::Private) {
                Ok(mut file) => {
                    file.write_all(format!("{}\n", std::process::id()).as_bytes())?;
                    file.finish()?;
                    return Ok(Self {
                        _process: process,
                        lease: lease.clone(),
                    });
                }
                Err(error) if fs_error_kind(&error) == Some(std::io::ErrorKind::AlreadyExists) => {
                    if operation_lock_is_stale(lease, path) {
                        match lease.remove_file(path) {
                            Ok(()) => continue,
                            Err(error)
                                if fs_error_kind(&error) == Some(std::io::ErrorKind::NotFound) =>
                            {
                                continue;
                            }
                            Err(_) => {}
                        }
                    }
                    if started.elapsed() >= OPERATION_LOCK_TIMEOUT {
                        bail!("timed out waiting for another package operation to finish");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => return Err(error).context("creating package operation lock"),
            }
        }
    }
}

impl Drop for OperationLock<'_> {
    fn drop(&mut self) {
        let _ = self.lease.remove_file(Path::new(OPERATION_LOCK_FILE_NAME));
    }
}

fn operation_lock_is_stale(lease: &CapabilityRootLease, path: &Path) -> bool {
    let Ok(metadata) = lease.metadata(path) else {
        return false;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as i128)
        .unwrap_or(0);
    now.saturating_sub(metadata.modified_ns) >= STALE_OPERATION_LOCK_AGE.as_nanos() as i128
}

struct CheckoutSwap {
    lease: CapabilityRootLease,
    target: PathBuf,
    backup: Option<PathBuf>,
    display_target: PathBuf,
    active: bool,
}

impl CheckoutSwap {
    fn activate(
        lease: CapabilityRootLease,
        staging: PathBuf,
        target: PathBuf,
        display_target: PathBuf,
    ) -> Result<Self> {
        if let Some(parent) = target.parent() {
            lease.create_dir_all(parent, package_mode(&lease))?;
        }
        let backup = if lease.metadata(&target).is_ok() {
            let backup = unique_sibling_relative(&target, "backup");
            lease.rename(&target, &backup)?;
            Some(backup)
        } else {
            None
        };
        if let Err(error) = lease.rename(&staging, &target) {
            if let Some(backup) = &backup {
                let _ = lease.rename(backup, &target);
            }
            return Err(error).context("activating staged package checkout");
        }
        Ok(Self {
            lease,
            target,
            backup,
            display_target,
            active: true,
        })
    }

    fn rollback(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        if self.lease.metadata(&self.target).is_ok() {
            self.lease.remove_tree(&self.target)?;
        }
        if let Some(backup) = &self.backup {
            self.lease.rename(backup, &self.target)?;
        }
        self.active = false;
        Ok(())
    }

    fn finalize(mut self) {
        if let Some(backup) = &self.backup {
            let _ = self.lease.remove_tree(backup);
        }
        self.active = false;
    }
}

struct RemovedCheckout {
    lease: CapabilityRootLease,
    original: PathBuf,
    removed: PathBuf,
    active: bool,
}

impl RemovedCheckout {
    fn activate(
        lease: CapabilityRootLease,
        original: PathBuf,
        _display_original: PathBuf,
    ) -> Result<Self> {
        let removed = unique_sibling_relative(&original, "removed");
        lease.rename(&original, &removed)?;
        Ok(Self {
            lease,
            original,
            removed,
            active: true,
        })
    }

    fn rollback(&mut self) -> Result<()> {
        if self.active {
            self.lease.rename(&self.removed, &self.original)?;
            self.active = false;
        }
        Ok(())
    }

    fn finalize(mut self) {
        let _ = self.lease.remove_tree(&self.removed);
        self.active = false;
    }
}

struct RemovePathOnDrop {
    lease: CapabilityRootLease,
    path: PathBuf,
    armed: bool,
}

impl RemovePathOnDrop {
    fn new(lease: CapabilityRootLease, path: PathBuf) -> Self {
        Self {
            lease,
            path,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RemovePathOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.lease.remove_tree(&self.path);
        }
    }
}

fn unique_sibling_relative(path: &Path, label: &str) -> PathBuf {
    let counter = UNIQUE_PATH_COUNTER.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let name = path.file_name().unwrap_or_else(|| OsStr::new("package"));
    let mut unique = name.to_os_string();
    unique.push(format!(
        ".{label}-{}-{timestamp}-{counter}",
        std::process::id()
    ));
    path.with_file_name(unique)
}

fn package_mode(lease: &CapabilityRootLease) -> FileMode {
    match lease.access() {
        CapabilityAccess::PrivateReadWrite => FileMode::Private,
        CapabilityAccess::ReadOnly | CapabilityAccess::ReadWrite => FileMode::Shared,
    }
}

fn managed_join(root: &Path, host: &str, repository_path: &str) -> Result<PathBuf> {
    let mut target = root.join(host);
    for component in repository_path.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            bail!("unsafe managed package path component");
        }
        target.push(component);
    }
    ensure_lexically_within(root, &target)?;
    Ok(target)
}

fn absolute_existing_or_lexical(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("getting current directory")?
            .join(path)
    };
    lexical_normalize(&absolute)
}

fn absolute_from(base: &Path, path: &Path) -> Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    absolute_existing_or_lexical(&joined)
}

fn lexical_normalize(path: &Path) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    bail!("path traversal escapes its root: {}", path.display());
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

fn path_to_settings_string(path: &Path) -> String {
    let value = path.to_string_lossy().into_owned();
    if value.starts_with('.') {
        value
    } else {
        format!("./{value}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(relative: &str, kind: EntryKind) -> OpenedPackageEntry {
        OpenedPackageEntry {
            relative: PathBuf::from(relative),
            kind,
        }
    }

    #[test]
    fn conventional_skill_discovery_stops_at_skill_md() {
        let root = PathBuf::from("/package");
        let available = vec![
            entry("skills/alpha/SKILL.md", EntryKind::RegularFile),
            // Auxiliary nested Markdown is documentation, not a skill.
            entry("skills/alpha/README.md", EntryKind::RegularFile),
            entry("skills/alpha/examples/deep.md", EntryKind::RegularFile),
            entry("skills/alpha/SKILL.md.bak", EntryKind::RegularFile),
            entry("skills/alpha", EntryKind::Directory),
        ];
        let skills = discover_conventional(&root, &available, "skills", PackageResourceKind::Skill);
        assert_eq!(
            skills
                .iter()
                .map(|spec| spec.path.clone())
                .collect::<Vec<_>>(),
            vec![root.join("skills/alpha/SKILL.md")]
        );
        assert!(
            skills
                .iter()
                .all(|spec| spec.kind == PackageResourceKind::Skill)
        );
    }

    #[test]
    fn conventional_skill_discovery_keeps_skill_md_at_any_depth() {
        let root = PathBuf::from("/package");
        let available = vec![
            entry("skills/SKILL.md", EntryKind::RegularFile),
            entry("skills/alpha/SKILL.md", EntryKind::RegularFile),
            entry(
                "skills/alpha/nested/deeper/SKILL.md",
                EntryKind::RegularFile,
            ),
            entry("skills/alpha/skills/other/SKILL.md", EntryKind::RegularFile),
        ];
        let skills = discover_conventional(&root, &available, "skills", PackageResourceKind::Skill);
        let paths: Vec<PathBuf> = skills.iter().map(|spec| spec.path.clone()).collect();
        assert_eq!(
            paths,
            vec![
                root.join("skills/SKILL.md"),
                root.join("skills/alpha/SKILL.md"),
                root.join("skills/alpha/nested/deeper/SKILL.md"),
                root.join("skills/alpha/skills/other/SKILL.md"),
            ]
        );
    }

    #[test]
    fn conventional_prompt_and_theme_discovery_keep_extension_matching() {
        let root = PathBuf::from("/package");
        let available = vec![
            entry("prompts/plan.md", EntryKind::RegularFile),
            entry("prompts/plan.txt", EntryKind::RegularFile),
            entry("prompts/nested/deep.md", EntryKind::RegularFile),
            entry("themes/light.json", EntryKind::RegularFile),
            entry("themes/light.txt", EntryKind::RegularFile),
            entry("extensions/pi-extension.json", EntryKind::RegularFile),
            entry("extensions/not-a-manifest.json", EntryKind::RegularFile),
        ];
        let prompts =
            discover_conventional(&root, &available, "prompts", PackageResourceKind::Prompt);
        assert_eq!(
            prompts
                .iter()
                .map(|spec| spec.path.clone())
                .collect::<Vec<_>>(),
            vec![
                root.join("prompts/nested/deep.md"),
                root.join("prompts/plan.md"),
            ]
        );
        let themes = discover_conventional(&root, &available, "themes", PackageResourceKind::Theme);
        assert_eq!(
            themes
                .iter()
                .map(|spec| spec.path.clone())
                .collect::<Vec<_>>(),
            vec![root.join("themes/light.json")]
        );
        let extensions = discover_conventional(
            &root,
            &available,
            "extensions",
            PackageResourceKind::Extension,
        );
        assert_eq!(
            extensions
                .iter()
                .map(|spec| spec.path.clone())
                .collect::<Vec<_>>(),
            vec![root.join("extensions/pi-extension.json")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn agent_settings_symlink_is_followed() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().expect("root");
        let target = root.path().join("real-settings.json");
        std::fs::write(&target, "{\"theme\":\"dark\"}\n").expect("target");
        symlink(&target, root.path().join(SETTINGS_FILE_NAME)).expect("settings symlink");
        let capabilities = CapabilityFs::new().expect("capabilities");
        let lease = capabilities
            .open_root(
                CapabilityRootPurpose::AgentStore,
                AbsolutePath::parse(root.path().to_path_buf()).expect("root"),
                CapabilityAccess::ReadOnly,
                RootCreation::Existing,
            )
            .expect("lease");
        let bytes = read_optional_bounded(
            &lease,
            Path::new(SETTINGS_FILE_NAME),
            MAX_PACKAGE_METADATA_BYTES,
        )
        .expect("agent settings.json symlink must be followed");
        let body = String::from_utf8(bytes.expect("settings body")).expect("utf8");
        assert!(body.contains("dark"), "followed settings body: {body}");
        let identity = optional_identity(&lease, Path::new(SETTINGS_FILE_NAME))
            .expect("followed settings identity");
        assert!(identity.is_some(), "followed settings identity");
    }
}
