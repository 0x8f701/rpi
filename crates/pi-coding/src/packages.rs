//! Versioned Pi package schemas and the local/git package backend.
//!
//! Package operations are serialized, settings and package state are replaced
//! atomically, and git is always invoked directly with an argv vector.

use std::collections::{BTreeSet, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{OnceLock, atomic::{AtomicU64, Ordering}};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use globset::{GlobBuilder, GlobMatcher};
use parking_lot::{Mutex, MutexGuard};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use walkdir::{DirEntry, WalkDir};

pub const PACKAGE_SOURCE_SCHEMA_VERSION: u32 = 1;
pub const PACKAGE_MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const PACKAGE_STATE_SCHEMA_VERSION: u32 = 1;

const CONFIG_DIR_NAME: &str = ".pi";
const SETTINGS_FILE_NAME: &str = "settings.json";
const STATE_FILE_NAME: &str = "package-state.json";
const OPERATION_LOCK_FILE_NAME: &str = ".package-operation.lock";
const OPERATION_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const STALE_OPERATION_LOCK_AGE: Duration = Duration::from_secs(6 * 60 * 60);
const MAX_PACKAGE_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_PACKAGE_METADATA_BYTES: u64 = 8 * 1024 * 1024;

static PROCESS_PACKAGE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static UNIQUE_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A settings/install scope.
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

/// Versioned source record stored in package state.
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
                let identity_path = fs::canonicalize(path).unwrap_or_else(|_| path.clone());
                format!("local:{}", identity_path.to_string_lossy())
            }
            ManagedPackageSourceKind::Git { host, path, .. } => {
                format!("git:{}/{}", host.to_ascii_lowercase(), path)
            }
        }
    }
}

/// Versioned `package.json#pi` manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageManifest {
    #[serde(
        default = "package_manifest_schema_version",
        rename = "schemaVersion"
    )]
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
                "unsupported Pi package manifest schema version {}; expected {}",
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

/// Resource category supplied by a package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PackageResourceKind {
    Extension,
    Skill,
    Prompt,
    Theme,
}

/// An absolute resource path with package origin and trust information.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageResourceSpec {
    pub kind: PackageResourceKind,
    pub path: PathBuf,
    pub package_id: String,
    pub scope: PackageScope,
    pub trusted: bool,
}

/// All resources supplied by one package.
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

    fn append_deduped(
        &mut self,
        other: Self,
        seen: &mut HashSet<(PackageResourceKind, PathBuf)>,
    ) {
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

/// A package checkout/path recorded in a scope state file.
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
        self.packages.sort_by(|left, right| left.identity.cmp(&right.identity));
    }

    fn remove(&mut self, identity: &str) {
        self.packages.retain(|package| package.identity != identity);
    }
}

/// One settings entry shown by `pi list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredPackage {
    pub source: String,
    pub scope: PackageScope,
    pub installed_path: Option<PathBuf>,
    pub pinned: bool,
    pub supported: bool,
}

/// Result of an install/update operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageOperation {
    pub source: String,
    pub identity: String,
    pub scope: PackageScope,
    pub root: PathBuf,
    pub revision: Option<String>,
}

/// Kind of package change reported by [`PackageManager::check_available_updates`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageUpdateType {
    /// The configured git ref resolves to a different commit than the checkout.
    Git,
    /// A local package's manifest or discovered resource snapshot changed.
    LocalChanged,
}

/// A read-only preview of one configured package update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageUpdate {
    pub source: String,
    pub display_name: String,
    pub update_type: PackageUpdateType,
    pub scope: PackageScope,
}

/// One configured package with its freshly rediscovered manifest and resources,
/// for `pi config` resource toggling. Resources are re-read from the on-disk
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

/// Package service for one working directory.
#[derive(Debug, Clone)]
pub struct PackageManager {
    cwd: PathBuf,
    agent_dir: PathBuf,
    project_trusted: bool,
}

impl PackageManager {
    /// Construct a manager using `PI_CODING_AGENT_DIR`, or `~/.pi/agent`.
    pub fn new(cwd: impl AsRef<Path>, project_trusted: bool) -> Result<Self> {
        let cwd = absolute_existing_or_lexical(cwd.as_ref())
            .with_context(|| format!("resolving working directory {}", cwd.as_ref().display()))?;
        let agent_dir = agent_directory()?;
        Ok(Self {
            cwd,
            agent_dir,
            project_trusted,
        })
    }

    /// Construct with an explicit global agent directory.
    pub fn with_agent_dir(
        cwd: impl AsRef<Path>,
        agent_dir: impl AsRef<Path>,
        project_trusted: bool,
    ) -> Result<Self> {
        Ok(Self {
            cwd: absolute_existing_or_lexical(cwd.as_ref())?,
            agent_dir: absolute_existing_or_lexical(agent_dir.as_ref())?,
            project_trusted,
        })
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
        if !path.exists() {
            return Ok(None);
        }
        if matches!(parsed.kind, ManagedPackageSourceKind::Git { .. }) {
            ensure_managed_git_path(&self.scope_root(scope).join("git"), &path)?;
        }
        Ok(Some(path))
    }

    /// Install a local or git package and persist it to the selected settings.
    pub fn install(&self, source: &str, scope: PackageScope) -> Result<PackageOperation> {
        self.assert_scope_trusted(scope)?;
        let _operation_lock = self.lock_operations()?;
        let parsed = parse_cli_source(source, &self.cwd)?;
        let settings_source = self.settings_source(source, &parsed, scope)?;
        self.install_locked(parsed, settings_source, scope, true)
    }

    /// Remove a configured package from one scope.
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

    /// List configured global packages and trusted project packages.
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

    /// Reconcile one configured package by identity, across its configured scopes.
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
        let project = if self.project_trusted {
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
                // Resource resolution must not invent installs for npm entries.
                // Surface them as unsupported at list/config time; never load.
                continue;
            }
            let parsed = self.parse_configured_source(&entry.source, entry.scope)?;
            let root = self.installed_root(&parsed, entry.scope)?;
            if !root.exists() {
                continue;
            }
            let discovered = discover_package(&root, &parsed.identity(), entry.scope, true)?;
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
        OperationLock::acquire(&self.agent_dir)
    }

    fn parse_configured_source(&self, source: &str, scope: PackageScope) -> Result<ManagedPackageSource> {
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
        let state_path = self.state_path(scope);
        let mut state = load_state(&state_path)?;
        let state_snapshot = FileSnapshot::capture(&state_path)?;
        let settings_path = self.settings_path(scope);
        let (settings_snapshot, next_settings) = if persist_settings {
            let snapshot = FileSnapshot::capture(&settings_path)?;
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
                if !path.exists() {
                    bail!("local package path does not exist: {}", path.display());
                }
                let discovered = discover_package(path, &identity, scope, true)?;
                let installed = installed_package(
                    source.clone(),
                    scope,
                    path.clone(),
                    None,
                    discovered,
                )?;
                state.upsert(installed);
                commit_metadata(
                    &state_path,
                    &state,
                    &state_snapshot,
                    &settings_path,
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
                let target = self.installed_root(&source, scope)?;
                let staging = unique_sibling(&target, "staging");
                let mut staging_guard = RemovePathOnDrop::new(staging.clone());
                prepare_git_checkout(&source, &target, &staging)?;
                discover_package(&staging, &identity, scope, true)
                    .context("validating staged package resources")?;

                let mut checkout_swap = CheckoutSwap::activate(staging, target.clone())?;
                staging_guard.disarm();
                let result = (|| -> Result<PackageOperation> {
                    let revision = git_revision(&target)?;
                    let discovered = discover_package(&target, &identity, scope, true)?;
                    let installed = installed_package(
                        source.clone(),
                        scope,
                        target.clone(),
                        Some(revision.clone()),
                        discovered,
                    )?;
                    state.upsert(installed);
                    commit_metadata(
                        &state_path,
                        &state,
                        &state_snapshot,
                        &settings_path,
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
                        let rollback = checkout_swap.rollback();
                        if let Err(rollback_error) = rollback {
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
        let state_path = self.state_path(scope);
        let state_snapshot = FileSnapshot::capture(&state_path)?;
        let mut state = load_state(&state_path)?;
        state.remove(&identity);

        let settings_path = self.settings_path(scope);
        let settings_snapshot = FileSnapshot::capture(&settings_path)?;
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
        let mut removed_checkout = match checkout.filter(|path| path.exists()) {
            Some(path) => Some(RemovedCheckout::activate(path)?),
            None => None,
        };

        let metadata_result = commit_metadata(
            &state_path,
            &state,
            &state_snapshot,
            &settings_path,
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

    fn installed_root(&self, source: &ManagedPackageSource, scope: PackageScope) -> Result<PathBuf> {
        match &source.kind {
            ManagedPackageSourceKind::Local { path } => Ok(path.clone()),
            ManagedPackageSourceKind::Git { host, path, .. } => {
                let root = self.scope_root(scope).join("git");
                managed_join(&root, host, path)
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
                installed_path: installed_path.exists().then_some(installed_path),
                pinned: parsed.is_pinned(),
                supported: true,
            });
        }
        Ok(())
    }

    fn check_scope_available_updates(&self, scope: PackageScope) -> Result<Vec<PackageUpdate>> {
        self.assert_scope_trusted(scope)?;
        let state = load_state(&self.state_path(scope))?;
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
                    let local = git_revision(&root)?;
                    let Ok(remote) = resolve_remote_git_revision(&parsed) else {
                        continue;
                    };
                    (local != remote).then_some(PackageUpdateType::Git)
                }
                ManagedPackageSourceKind::Local { path } => {
                    let discovered = discover_package(path, &identity, scope, true)?;
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
        load_settings_packages(&self.settings_path(scope), scope)
    }

    /// Re-discover configured packages and their resources for one scope, for
    /// `pi config`. Refuses project scope when untrusted. npm entries are
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
            if !root.exists() {
                continue;
            }
            let identity = parsed.identity();
            let discovered = discover_package(&root, &identity, scope, true)?;
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

/// Parse a supported source relative to `base_dir`.
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
        if let Some(git) = parse_git_source(&shorthand)? {
            identities.insert(git.identity());
        }
    }
    Ok(identities)
}

const fn npm_deferred_error() -> &'static str {
    "npm package sources are not supported yet; use a local path or git source"
}

fn split_git_reference(raw: &str) -> Result<(String, Option<String>)> {
    if raw.bytes().any(|byte| matches!(byte, b'\0' | b'\n' | b'\r')) {
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
    Ok((repo.trim_end_matches('/').to_string(), Some(reference.to_string())))
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
            || candidate.chars().any(|ch| matches!(ch, '\\' | ':' | '?' | '*' | '[' | '^' | '~' | ' '))
            || candidate.bytes().any(|byte| byte.is_ascii_control())
            || candidate.split('/').any(|part| part.is_empty() || part == "." || part == ".." || part.ends_with(".lock"))
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
    let trimmed = path.trim_matches('/').strip_suffix(".git").unwrap_or(path.trim_matches('/'));
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

fn prepare_git_checkout(source: &ManagedPackageSource, target: &Path, staging: &Path) -> Result<()> {
    let ManagedPackageSourceKind::Git {
        repo, reference, ..
    } = &source.kind
    else {
        bail!("internal error: attempted git checkout for a local package");
    };
    if let Some(parent) = staging.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating git staging directory {}", parent.display()))?;
    }
    if staging.exists() {
        fs::remove_dir_all(staging)
            .with_context(|| format!("removing stale git staging path {}", staging.display()))?;
    }

    if target.join(".git").exists() {
        let args = vec![
            "clone".to_string(),
            "--no-hardlinks".to_string(),
            "--no-checkout".to_string(),
            target.to_string_lossy().into_owned(),
            staging.to_string_lossy().into_owned(),
        ];
        run_git(&args, None).context("cloning the previous checkout into staging")?;
        run_git(
            &["remote".into(), "set-url".into(), "origin".into(), repo.clone()],
            Some(staging),
        )
        .context("setting the staged checkout origin")?;
    } else if target.exists() {
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
        run_git(&args, None).context("cloning git package")?;
    }

    let commit = if let Some(reference) = reference {
        run_git(
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
            &["rev-parse".into(), "FETCH_HEAD^{commit}".into()],
            Some(staging),
        )?
    } else {
        reconcile_remote_default(staging)?
    };

    run_git(
        &["reset".into(), "--hard".into(), commit.trim().into()],
        Some(staging),
    )
    .context("resetting staged package checkout")?;
    run_git(
        &["clean".into(), "-fdx".into()],
        Some(staging),
    )
    .context("cleaning staged package checkout")?;
    Ok(())
}

fn reconcile_remote_default(checkout: &Path) -> Result<String> {
    run_git(
        &["fetch".into(), "--prune".into(), "origin".into()],
        Some(checkout),
    )
    .context("fetching remote default branch")?;
    let _ = run_git(
        &[
            "remote".into(),
            "set-head".into(),
            "origin".into(),
            "-a".into(),
        ],
        Some(checkout),
    );

    if let Ok(commit) = run_git_capture(
        &["rev-parse".into(), "origin/HEAD^{commit}".into()],
        Some(checkout),
    ) {
        return Ok(commit);
    }

    let remote = run_git_capture(
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
        &[
            "fetch".into(),
            "--force".into(),
            "origin".into(),
            format!("+refs/heads/{branch}:{remote_ref}"),
        ],
        Some(checkout),
    )?;
    run_git_capture(
        &["rev-parse".into(), format!("{remote_ref}^{{commit}}")],
        Some(checkout),
    )
}

fn resolve_remote_git_revision(source: &ManagedPackageSource) -> Result<String> {
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
                &[
                    "ls-remote".into(),
                    "--exit-code".into(),
                    repo.clone(),
                    reference.clone(),
                ],
                None,
            )?;
            let advertised = output.lines().any(|line| {
                line.split_once('\t').is_some_and(|(revision, _)| {
                    revision.eq_ignore_ascii_case(reference)
                })
            });
            if !advertised {
                bail!("remote does not advertise configured git commit {reference}");
            }
            return Ok(reference.to_ascii_lowercase());
        }
        let names = remote_reference_names(reference);
        let mut args = vec!["ls-remote".to_string(), "--exit-code".to_string(), repo.clone()];
        args.extend(names.iter().cloned());
        run_git_capture(&args, None)?
    } else {
        run_git_capture(
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

fn git_revision(checkout: &Path) -> Result<String> {
    let revision = run_git_capture(&["rev-parse".into(), "HEAD".into()], Some(checkout))?;
    let revision = revision.trim();
    if revision.is_empty() {
        bail!("git checkout has no HEAD revision");
    }
    Ok(revision.to_string())
}

fn run_git(args: &[String], cwd: Option<&Path>) -> Result<()> {
    let output = git_command(args, cwd)?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = stderr.trim();
    if message.is_empty() {
        bail!("git operation failed with status {}", output.status);
    }
    bail!("git operation failed: {message}");
}

fn run_git_capture(args: &[String], cwd: Option<&Path>) -> Result<String> {
    let output = git_command(args, cwd)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.trim();
        if message.is_empty() {
            bail!("git operation failed with status {}", output.status);
        }
        bail!("git operation failed: {message}");
    }
    String::from_utf8(output.stdout).context("git output was not UTF-8")
}

fn git_command(args: &[String], cwd: Option<&Path>) -> Result<Output> {
    let mut command = Command::new("git");
    command.args(["-c", "color.ui=false"]);
    command.args(args);
    command.env("GIT_TERMINAL_PROMPT", "0");
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command.output().context("starting git")
}

struct DiscoveredPackage {
    manifest: PackageManifest,
    resources: PackageResources,
}

fn discover_package(
    root: &Path,
    package_id: &str,
    scope: PackageScope,
    trusted: bool,
) -> Result<DiscoveredPackage> {
    if root.is_file() {
        let path = canonical_resource(root, root.parent().unwrap_or_else(|| Path::new(".")))?;
        return Ok(DiscoveredPackage {
            manifest: PackageManifest::default(),
            resources: PackageResources {
                extensions: vec![PackageResourceSpec {
                    kind: PackageResourceKind::Extension,
                    path,
                    package_id: package_id.to_string(),
                    scope,
                    trusted,
                }],
                ..PackageResources::default()
            },
        });
    }
    if !root.is_dir() {
        bail!("package root is not a file or directory: {}", root.display());
    }

    let manifest = read_package_manifest(root)?;
    let mut resources = PackageResources::default();
    if let Some(manifest) = &manifest {
        resources.extensions = discover_manifest_resources(
            root,
            &manifest.extensions,
            PackageResourceKind::Extension,
        )?;
        resources.skills =
            discover_manifest_resources(root, &manifest.skills, PackageResourceKind::Skill)?;
        resources.prompts =
            discover_manifest_resources(root, &manifest.prompts, PackageResourceKind::Prompt)?;
        resources.themes =
            discover_manifest_resources(root, &manifest.themes, PackageResourceKind::Theme)?;
    } else {
        resources.extensions = discover_conventional(
            root,
            "extensions",
            PackageResourceKind::Extension,
        )?;
        resources.skills =
            discover_conventional(root, "skills", PackageResourceKind::Skill)?;
        resources.prompts =
            discover_conventional(root, "prompts", PackageResourceKind::Prompt)?;
        resources.themes =
            discover_conventional(root, "themes", PackageResourceKind::Theme)?;
    }
    resources.remap_metadata(package_id, scope, trusted);
    Ok(DiscoveredPackage {
        manifest: manifest.unwrap_or_default(),
        resources,
    })
}

fn read_package_manifest(root: &Path) -> Result<Option<PackageManifest>> {
    let path = root.join("package.json");
    let Some(bytes) = read_optional_bounded(&path, MAX_PACKAGE_MANIFEST_BYTES)? else {
        return Ok(None);
    };
    let value: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing package manifest {}", path.display()))?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("package manifest root must be a JSON object: {}", path.display()))?;
    let Some(pi) = object.get("pi") else {
        return Ok(None);
    };
    if !pi.is_object() {
        bail!("package manifest `pi` field must be an object: {}", path.display());
    }
    let manifest: PackageManifest = serde_json::from_value(pi.clone())
        .with_context(|| format!("parsing Pi package manifest in {}", path.display()))?;
    manifest.validate()?;
    Ok(Some(manifest))
}

fn discover_manifest_resources(
    root: &Path,
    entries: &[String],
    kind: PackageResourceKind,
) -> Result<Vec<PackageResourceSpec>> {
    let mut positive = Vec::new();
    let mut exclusions = Vec::new();
    for entry in entries {
        validate_manifest_entry(entry)?;
        if let Some(exclusion) = entry.strip_prefix('!') {
            exclusions.push(compile_manifest_glob(exclusion)?);
        } else {
            positive.push(entry.as_str());
        }
    }

    let mut paths = BTreeSet::new();
    for entry in positive {
        if has_glob_meta(entry) {
            let matcher = compile_manifest_glob(entry)?;
            for candidate in walk_package_entries(root) {
                let candidate = candidate?;
                let relative = normalized_relative(root, candidate.path())?;
                if matcher.is_match(&relative) {
                    collect_resource_path(root, candidate.path(), kind, &mut paths)?;
                }
            }
        } else {
            let path = safe_manifest_join(root, entry)?;
            if path.exists() {
                collect_resource_path(root, &path, kind, &mut paths)?;
            }
        }
    }

    paths.retain(|path| {
        let Ok(relative) = normalized_relative(root, path) else {
            return false;
        };
        !exclusions.iter().any(|matcher| {
            matcher.is_match(&relative)
                || relative_ancestor_matches(Path::new(&relative), matcher)
        })
    });
    Ok(paths
        .into_iter()
        .map(|path| empty_resource_spec(kind, path))
        .collect())
}

fn discover_conventional(
    root: &Path,
    directory: &str,
    kind: PackageResourceKind,
) -> Result<Vec<PackageResourceSpec>> {
    let path = root.join(directory);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut paths = BTreeSet::new();
    collect_resource_path(root, &path, kind, &mut paths)?;
    Ok(paths
        .into_iter()
        .map(|path| empty_resource_spec(kind, path))
        .collect())
}

fn empty_resource_spec(kind: PackageResourceKind, path: PathBuf) -> PackageResourceSpec {
    PackageResourceSpec {
        kind,
        path,
        package_id: String::new(),
        scope: PackageScope::Global,
        trusted: false,
    }
}

fn collect_resource_path(
    package_root: &Path,
    path: &Path,
    kind: PackageResourceKind,
    output: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    if path.is_file() {
        if resource_file_matches(path, kind) {
            output.insert(canonical_resource(path, package_root)?);
        }
        return Ok(());
    }
    if !path.is_dir() {
        return Ok(());
    }
    if kind == PackageResourceKind::Skill {
        collect_skills(package_root, path, path, output)?;
        return Ok(());
    }
    for entry in WalkDir::new(path)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(visible_package_entry)
    {
        let entry = entry.with_context(|| format!("walking package directory {}", path.display()))?;
        if entry.file_type().is_file() && resource_file_matches(entry.path(), kind) {
            output.insert(canonical_resource(entry.path(), package_root)?);
        }
    }
    Ok(())
}

fn collect_skills(
    package_root: &Path,
    root: &Path,
    directory: &Path,
    output: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let skill_file = directory.join("SKILL.md");
    if skill_file.is_file() {
        output.insert(canonical_resource(&skill_file, package_root)?);
        return Ok(());
    }
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("reading skill directory {}", directory.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("reading skill directory {}", directory.display()))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name();
        if hidden_or_ignored_name(&name) {
            continue;
        }
        let file_type = entry
            .file_type()
            .with_context(|| format!("reading file type for {}", entry.path().display()))?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_skills(package_root, root, &entry.path(), output)?;
        } else if directory == root
            && file_type.is_file()
            && entry.path().extension() == Some(OsStr::new("md"))
        {
            output.insert(canonical_resource(&entry.path(), package_root)?);
        }
    }
    Ok(())
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

fn visible_package_entry(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    !hidden_or_ignored_name(entry.file_name()) && !entry.file_type().is_symlink()
}

fn hidden_or_ignored_name(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    name.starts_with('.') || name == "node_modules"
}

fn walk_package_entries(root: &Path) -> impl Iterator<Item = walkdir::Result<DirEntry>> {
    WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(visible_package_entry)
}

fn validate_manifest_entry(entry: &str) -> Result<()> {
    let path = entry.strip_prefix('!').unwrap_or(entry);
    if path.is_empty() {
        bail!("package manifest contains an empty resource path");
    }
    if path.starts_with('+') || path.starts_with('-') {
        bail!("package manifest resource paths may only use `!` exclusions");
    }
    let decoded = percent_decode_for_validation(path)?;
    for candidate in [path, decoded.as_str()] {
        if candidate.starts_with('/')
            || candidate.chars().any(|ch| matches!(ch, '\\' | '\0'))
            || candidate.split('/').any(|part| part == "..")
            || Path::new(candidate).is_absolute()
        {
            bail!("package manifest resource path escapes the package root: {entry}");
        }
    }
    Ok(())
}

fn safe_manifest_join(root: &Path, entry: &str) -> Result<PathBuf> {
    validate_manifest_entry(entry)?;
    let joined = root.join(entry);
    ensure_lexically_within(root, &joined)?;
    Ok(joined)
}

fn canonical_resource(path: &Path, package_root: &Path) -> Result<PathBuf> {
    let root = fs::canonicalize(package_root)
        .with_context(|| format!("canonicalizing package root {}", package_root.display()))?;
    let resource = fs::canonicalize(path)
        .with_context(|| format!("canonicalizing package resource {}", path.display()))?;
    if resource != root && !resource.starts_with(&root) {
        bail!(
            "package resource escapes package root: {}",
            path.display()
        );
    }
    Ok(resource)
}

fn ensure_lexically_within(root: &Path, path: &Path) -> Result<()> {
    let clean_root = lexical_normalize(root)?;
    let clean_path = lexical_normalize(path)?;
    if clean_path != clean_root && !clean_path.starts_with(&clean_root) {
        bail!("path escapes package root: {}", path.display());
    }
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

fn load_settings_packages(path: &Path, scope: PackageScope) -> Result<Vec<SettingsPackageEntry>> {
    let Some(bytes) = read_optional_bounded(path, MAX_PACKAGE_METADATA_BYTES)? else {
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
        .ok_or_else(|| anyhow!("settings package entry must be a string or object with a string `source`"))
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

fn load_state(path: &Path) -> Result<PackageState> {
    let Some(bytes) = read_optional_bounded(path, MAX_PACKAGE_METADATA_BYTES)? else {
        return Ok(PackageState::default());
    };
    let state: PackageState = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing package state {}", path.display()))?;
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
    state_path: &Path,
    state: &PackageState,
    state_snapshot: &FileSnapshot,
    settings_path: &Path,
    next_settings: Option<&[u8]>,
    settings_snapshot: Option<&FileSnapshot>,
) -> Result<()> {
    let state_bytes = serialize_state(state)?;
    atomic_write(state_path, &state_bytes)
        .with_context(|| format!("writing package state {}", state_path.display()))?;
    if let Some(settings) = next_settings
        && let Err(error) = atomic_write(settings_path, settings)
            .with_context(|| format!("writing package settings {}", settings_path.display()))
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

#[derive(Debug)]
struct FileSnapshot {
    path: PathBuf,
    bytes: Option<Vec<u8>>,
}

impl FileSnapshot {
    fn capture(path: &Path) -> Result<Self> {
        let bytes = read_optional_bounded(path, MAX_PACKAGE_METADATA_BYTES)?;
        Ok(Self {
            path: path.to_path_buf(),
            bytes,
        })
    }

    fn restore(&self) -> Result<()> {
        match &self.bytes {
            Some(bytes) => atomic_write(&self.path, bytes),
            None => match fs::remove_file(&self.path) {
                Ok(()) => sync_parent(&self.path),
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error).with_context(|| format!("removing {}", self.path.display())),
            },
        }
    }
}
fn read_optional_bounded(path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("opening {}", path.display())),
    };
    let length = file
        .metadata()
        .with_context(|| format!("reading metadata for {}", path.display()))?
        .len();
    if length > limit {
        bail!(
            "refusing to read {}: file is {} bytes, exceeding the {} byte limit",
            path.display(),
            length,
            limit
        );
    }
    let capacity = usize::try_from(length).unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    std::io::Read::by_ref(&mut file)
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() as u64 > limit {
        bail!(
            "refusing to read {}: file grew beyond the {} byte limit",
            path.display(),
            limit
        );
    }
    Ok(Some(bytes))
}


fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating directory {}", parent.display()))?;
    let temporary = unique_sibling(path, "tmp");
    let mut guard = RemovePathOnDrop::new(temporary.clone());
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .with_context(|| format!("creating temporary file {}", temporary.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("writing temporary file {}", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing temporary file {}", temporary.display()))?;
    drop(file);
    fs::rename(&temporary, path)
        .with_context(|| format!("replacing {} atomically", path.display()))?;
    guard.disarm();
    sync_parent(path)
}

fn sync_parent(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("syncing directory {}", parent.display()))
}

struct OperationLock<'a> {
    _process: MutexGuard<'a, ()>,
    path: PathBuf,
}

impl<'a> OperationLock<'a> {
    fn acquire(agent_dir: &Path) -> Result<Self> {
        let process = PROCESS_PACKAGE_LOCK.get_or_init(|| Mutex::new(())).lock();
        fs::create_dir_all(agent_dir)
            .with_context(|| format!("creating agent directory {}", agent_dir.display()))?;
        let path = agent_dir.join(OPERATION_LOCK_FILE_NAME);
        let started = Instant::now();
        loop {
            match OpenOptions::new().create_new(true).write(true).open(&path) {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id())
                        .with_context(|| format!("writing package operation lock {}", path.display()))?;
                    file.sync_all()
                        .with_context(|| format!("syncing package operation lock {}", path.display()))?;
                    return Ok(Self {
                        _process: process,
                        path,
                    });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    if operation_lock_is_stale(&path) {
                        match fs::remove_file(&path) {
                            Ok(()) => continue,
                            Err(remove_error) if remove_error.kind() == ErrorKind::NotFound => continue,
                            Err(_) => {}
                        }
                    }
                    if started.elapsed() >= OPERATION_LOCK_TIMEOUT {
                        bail!(
                            "timed out waiting for another package operation to finish (lock: {})",
                            path.display()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("creating package operation lock {}", path.display()));
                }
            }
        }
    }
}

impl Drop for OperationLock<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn operation_lock_is_stale(path: &Path) -> bool {
    let Ok(modified) = fs::metadata(path).and_then(|metadata| metadata.modified()) else {
        return false;
    };
    modified
        .elapsed()
        .is_ok_and(|age| age >= STALE_OPERATION_LOCK_AGE)
}

struct CheckoutSwap {
    target: PathBuf,
    backup: Option<PathBuf>,
    active: bool,
}

impl CheckoutSwap {
    fn activate(staging: PathBuf, target: PathBuf) -> Result<Self> {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating package directory {}", parent.display()))?;
        }
        let backup = if target.exists() {
            let backup = unique_sibling(&target, "backup");
            fs::rename(&target, &backup).with_context(|| {
                format!(
                    "moving previous package checkout {} to {}",
                    target.display(),
                    backup.display()
                )
            })?;
            Some(backup)
        } else {
            None
        };
        if let Err(error) = fs::rename(&staging, &target) {
            if let Some(backup) = &backup {
                let _ = fs::rename(backup, &target);
            }
            return Err(error).with_context(|| {
                format!(
                    "activating staged package checkout {} at {}",
                    staging.display(),
                    target.display()
                )
            });
        }
        Ok(Self {
            target,
            backup,
            active: true,
        })
    }

    fn rollback(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        if self.target.exists() {
            fs::remove_dir_all(&self.target)
                .with_context(|| format!("removing failed checkout {}", self.target.display()))?;
        }
        if let Some(backup) = &self.backup {
            fs::rename(backup, &self.target).with_context(|| {
                format!(
                    "restoring previous package checkout {}",
                    self.target.display()
                )
            })?;
        }
        self.active = false;
        Ok(())
    }

    fn finalize(mut self) {
        if let Some(backup) = &self.backup
            && backup.exists()
        {
            let _ = fs::remove_dir_all(backup);
        }
        self.active = false;
    }
}

struct RemovedCheckout {
    original: PathBuf,
    removed: PathBuf,
    active: bool,
}

impl RemovedCheckout {
    fn activate(original: PathBuf) -> Result<Self> {
        let removed = unique_sibling(&original, "removed");
        fs::rename(&original, &removed).with_context(|| {
            format!(
                "moving package checkout {} out of service",
                original.display()
            )
        })?;
        Ok(Self {
            original,
            removed,
            active: true,
        })
    }

    fn rollback(&mut self) -> Result<()> {
        if self.active {
            fs::rename(&self.removed, &self.original).with_context(|| {
                format!(
                    "restoring removed package checkout {}",
                    self.original.display()
                )
            })?;
            self.active = false;
        }
        Ok(())
    }

    fn finalize(mut self) {
        if self.removed.exists() {
            let _ = fs::remove_dir_all(&self.removed);
        }
        self.active = false;
    }
}

struct RemovePathOnDrop {
    path: PathBuf,
    armed: bool,
}

impl RemovePathOnDrop {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RemovePathOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if self.path.is_dir() {
            let _ = fs::remove_dir_all(&self.path);
        } else {
            let _ = fs::remove_file(&self.path);
        }
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

fn ensure_managed_git_path(root: &Path, path: &Path) -> Result<()> {
    let canonical_root = fs::canonicalize(root)
        .with_context(|| format!("resolving managed git root {}", root.display()))?;
    let canonical_path = fs::canonicalize(path)
        .with_context(|| format!("resolving managed git package {}", path.display()))?;
    if canonical_path == canonical_root || !canonical_path.starts_with(&canonical_root) {
        bail!(
            "managed git package path escapes its scope root: {}",
            path.display()
        );
    }
    Ok(())
}

fn unique_sibling(path: &Path, label: &str) -> PathBuf {
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

fn agent_directory() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PI_CODING_AGENT_DIR").filter(|path| !path.is_empty()) {
        return absolute_existing_or_lexical(Path::new(&path));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|path| !path.is_empty())
        .ok_or_else(|| anyhow!("home directory is unavailable; set PI_CODING_AGENT_DIR"))?;
    absolute_existing_or_lexical(&Path::new(&home).join(CONFIG_DIR_NAME).join("agent"))
}

fn absolute_existing_or_lexical(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return fs::canonicalize(path)
            .with_context(|| format!("canonicalizing {}", path.display()));
    }
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
