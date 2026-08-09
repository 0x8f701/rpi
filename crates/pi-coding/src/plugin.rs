//! Minimal plugin marketplace: `rpi plugin list/install/remove/update`.
//!
//! A *plugin* is a standalone extension package — a directory whose root
//! carries a validated `pi-extension.json` manifest (the same
//! [`crate::PROCESS_EXTENSION_MANIFEST_FILE`] schema the extension runtime
//! loads). Installed plugins land in `<agent_dir>/extensions/<name>/` and are
//! picked up as extensions by the resource scan
//! ([`marketplace_extension_resources`]), so an installed plugin is loadable
//! through the normal extension pipeline.
//!
//! Sources accepted by `install`/`update`:
//! - a local directory containing `pi-extension.json`;
//! - a local `.tgz` / `.tar.gz` / `.tar` archive;
//! - an `http(s)` URL pointing at such an archive;
//! - a GitHub-style `owner/repo` reference, resolved to the codeload tarball
//!   (`https://codeload.github.com/<owner>/<repo>/tar.gz/HEAD`);
//! - a git URL — `git+https://host/owner/repo`, `git+ssh://git@host/owner/
//!   repo.git`, `https://host/owner/repo.git`, `ssh://git@host/owner/repo.git`,
//!   or `git@host:owner/repo.git` (scp-like) — cloned shallow (`git clone
//!   --depth 1`, argv-built so no shell string is ever involved, bounded by a
//!   hard timeout) into staging, stripped of its `.git` metadata, then
//!   validated and installed exactly like any other source;
//! - an `npm:<name>` / `npm:<name>@<version>` reference, resolved through the
//!   npm registry (`https://registry.npmjs.org/<name>` for the latest
//!   version, `<name>/<version>` for a pinned one) to the package's
//!   `dist.tarball` URL. The fetched tarball is extracted and validated like
//!   any other archive; the package's `pi-extension.json` entry points
//!   resolve relative to the installed extension root.
//!
//! Security model:
//! - Installing only writes files. Nothing in the package is ever executed or
//!   registered as an extension until a later explicit load.
//! - The staged package is validated against the `pi-extension.json` schema
//!   and converted through [`crate::extension_spec_from_package_resource`]
//!   (runtime path stays inside the package, file exists) *before* it is moved
//!   into the extensions root, so an invalid package can never become
//!   loadable.
//! - Archive extraction rejects absolute paths, `..` traversal, non-regular
//!   entries (symlinks, hardlinks, special files), and enforces byte and
//!   entry-count caps.
//! - Loading is gated on a `trusted` marker from the trust store: a plugin
//!   whose canonical path resolves to anything other than `Trusted` is listed
//!   but not included in [`marketplace_extension_resources`]. `install`
//!   records the user's explicit consent as a `Trusted` decision for the
//!   plugin path; `remove` clears it.
//!
//! The marketplace index is a JSON list of `{name, repo, version,
//! description?}` entries fetched from the `pluginMarketplace` setting
//! (a URL or a local index file) or the embedded default
//! [`DEFAULT_MARKETPLACE_INDEX_URL`]. The default is a documented constant
//! and is never fetched by tests — tests always use a local index file.

use std::cmp::Ordering;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::packages::{PackageResourceKind, PackageResourceSpec, PackageScope};
use crate::resources::agent_dir_path;
use crate::trust::{TrustDecision, TrustStore};
use crate::{
    ExtensionManifestRuntime, ProcessExtensionManifest, PROCESS_EXTENSION_MANIFEST_FILE,
    extension_spec_from_package_resource,
};

/// Settings key holding the marketplace index source: an `http(s)` URL or a
/// local index file path. Absent -> [`DEFAULT_MARKETPLACE_INDEX_URL`].
pub const PLUGIN_MARKETPLACE_SETTING: &str = "pluginMarketplace";

/// Default marketplace index location. A documented constant; tests never
/// fetch it (they use local index files), and real fetches surface an
/// actionable error when offline.
pub const DEFAULT_MARKETPLACE_INDEX_URL: &str = "https://plugins.rpi.dev/index.json";

/// Hard cap on a single plugin package: total extracted bytes.
const MAX_PACKAGE_BYTES: u64 = 256 * 1024 * 1024;
/// Hard cap on archive entries (zip-bomb/entry-flood guard).
const MAX_PACKAGE_ENTRIES: usize = 4096;
/// Hard cap on one manifest file.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
/// Hard cap on the marketplace index document.
const MAX_MARKETPLACE_INDEX_BYTES: u64 = 8 * 1024 * 1024;
/// Hard cap on marketplace index entries.
const MAX_MARKETPLACE_INDEX_ENTRIES: usize = 4096;
/// HTTP fetch timeout.
const FETCH_TIMEOUT_SECONDS: u64 = 60;
/// Hard timeout on one shallow `git clone` (network stalls or interactive
/// prompts must never hang an install indefinitely).
const GIT_CLONE_TIMEOUT_SECONDS: u64 = 120;
/// Default npm registry base used to resolve `npm:` plugin sources. Tests
/// swap this for a local mock registry (see [`npm_registry_base`]).
const DEFAULT_NPM_REGISTRY_BASE: &str = "https://registry.npmjs.org";
/// Hard cap on one npm package metadata document.
const MAX_NPM_METADATA_BYTES: u64 = 2 * 1024 * 1024;

/// npm registry base URL, overridable for tests. Real installs always use the
/// default; tests hold [`NPM_REGISTRY_TEST_LOCK`] while swapping the base so
/// parallel npm-source tests never race each other's registry.
static NPM_REGISTRY_BASE: std::sync::LazyLock<std::sync::Mutex<String>> =
    std::sync::LazyLock::new(|| {
        std::sync::Mutex::new(DEFAULT_NPM_REGISTRY_BASE.to_owned())
    });
/// Serializes npm-source tests that swap [`NPM_REGISTRY_BASE`].
#[cfg(test)]
static NPM_REGISTRY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[must_use]
fn npm_registry_base() -> String {
    NPM_REGISTRY_BASE
        .lock()
        .expect("npm registry base mutex poisoned")
        .clone()
}

#[cfg(test)]
fn set_npm_registry_base_for_tests(base: impl Into<String>) {
    *NPM_REGISTRY_BASE
        .lock()
        .expect("npm registry base mutex poisoned") = base.into();
}

/// Runtime kind of an installed plugin, for `plugin list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginRuntime {
    Process,
    QuickJs,
}

impl PluginRuntime {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::QuickJs => "quickjs",
        }
    }
}

/// One installed plugin, as shown by `rpi plugin list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInfo {
    /// Extension id from the manifest (also the directory name).
    pub name: String,
    /// Manifest `version`; `0.0.0` when the manifest carries none.
    pub version: String,
    pub runtime: PluginRuntime,
    /// Effective trust-store decision at the plugin path. Only trusted
    /// plugins are loadable extensions.
    pub trusted: bool,
    pub path: PathBuf,
}

/// One newer version available from the marketplace index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginUpdate {
    pub name: String,
    pub current: String,
    pub available: String,
    /// Install source recorded by the index entry (`repo`).
    pub repo: String,
}

/// One marketplace index entry: `{name, repo, version, description?}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MarketplaceEntry {
    /// Plugin name (must match the manifest id of the staged package).
    pub name: String,
    /// Install source: local directory path, `.tgz`/`.tar.gz`/`.tar` URL or
    /// local archive, or an `owner/repo` GitHub reference.
    pub repo: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The validated marketplace index: a JSON list of entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceIndex {
    entries: Vec<MarketplaceEntry>,
}

impl MarketplaceIndex {
    /// Parse and validate a JSON marketplace index document.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() as u64 > MAX_MARKETPLACE_INDEX_BYTES {
            bail!("marketplace index exceeds {MAX_MARKETPLACE_INDEX_BYTES} bytes");
        }
        let entries: Vec<MarketplaceEntry> = serde_json::from_slice(bytes)
            .context("marketplace index must be a JSON array of {name, repo, version} entries")?;
        if entries.len() > MAX_MARKETPLACE_INDEX_ENTRIES {
            bail!(
                "marketplace index has {} entries; limit is {MAX_MARKETPLACE_INDEX_ENTRIES}",
                entries.len()
            );
        }
        let mut seen = std::collections::BTreeSet::new();
        for entry in &entries {
            validate_plugin_name(&entry.name)?;
            if entry.repo.trim().is_empty() {
                bail!("marketplace entry {} has an empty repo", entry.name);
            }
            if entry.version.trim().is_empty() {
                bail!("marketplace entry {} has an empty version", entry.name);
            }
            if entry.version.len() > 128 {
                bail!("marketplace entry {} version exceeds 128 bytes", entry.name);
            }
            if !seen.insert(entry.name.as_str()) {
                bail!("marketplace index lists plugin {name} twice", name = entry.name);
            }
        }
        Ok(Self { entries })
    }

    #[must_use]
    pub fn entries(&self) -> &[MarketplaceEntry] {
        &self.entries
    }

    #[must_use]
    pub fn entry(&self, name: &str) -> Option<&MarketplaceEntry> {
        self.entries.iter().find(|entry| entry.name == name)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Fetch and parse the marketplace index from `source`: an `http(s)` URL or a
/// local index file path (`file://` prefixes are accepted).
pub async fn fetch_index(source: &str) -> Result<MarketplaceIndex> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        bail!("marketplace index source must not be empty");
    }
    let bytes = if let Some(url) = parse_http_url(trimmed) {
        fetch_bytes(url.as_str(), MAX_MARKETPLACE_INDEX_BYTES)
            .await
            .with_context(|| format!("fetching marketplace index from {url}"))?
    } else {
        let path = trimmed.strip_prefix("file://").unwrap_or(trimmed);
        let bytes = fs::read(path)
            .with_context(|| format!("reading marketplace index {}", Path::new(path).display()))?;
        if bytes.len() as u64 > MAX_MARKETPLACE_INDEX_BYTES {
            bail!(
                "marketplace index {} exceeds {MAX_MARKETPLACE_INDEX_BYTES} bytes",
                path
            );
        }
        bytes
    };
    MarketplaceIndex::parse(&bytes).with_context(|| format!("parsing marketplace index {source}"))
}

/// Compare two version strings for the marketplace. Numeric components compare
/// numerically (`1.0.10 > 1.0.2`), anything non-numeric falls back to a
/// lexical comparison; a longer component list wins ties (`1.0 < 1.0.1`).
/// Deterministic but intentionally minimal — versions come from manifests and
/// index entries that are not guaranteed to be strict semver.
#[must_use]
pub fn compare_versions(left: &str, right: &str) -> Ordering {
    let left_parts = version_parts(left);
    let right_parts = version_parts(right);
    for (a, b) in left_parts.iter().zip(right_parts.iter()) {
        let ordering = match (a.parse::<u64>(), b.parse::<u64>()) {
            (Ok(a), Ok(b)) => a.cmp(&b),
            _ => a.cmp(b),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left_parts.len().cmp(&right_parts.len())
}

fn version_parts(value: &str) -> Vec<&str> {
    value
        .split(|character| matches!(character, '.' | '-' | '_'))
        .filter(|part| !part.is_empty())
        .collect()
}

/// Plugin marketplace over one agent directory. Installed plugins live in
/// `<agent_dir>/extensions/<name>/`; trust lives in the agent trust store.
#[derive(Debug, Clone)]
pub struct PluginMarketplace {
    agent_dir: PathBuf,
}

impl PluginMarketplace {
    pub fn new(agent_dir: impl Into<PathBuf>) -> Self {
        Self {
            agent_dir: agent_dir.into(),
        }
    }

    #[must_use]
    pub fn global() -> Self {
        Self::new(agent_dir_path())
    }

    /// `<agent_dir>/extensions` — the root scanned by
    /// [`marketplace_extension_resources`].
    #[must_use]
    pub fn extensions_root(&self) -> PathBuf {
        self.agent_dir.join("extensions")
    }

    /// The install directory for one plugin name.
    #[must_use]
    pub fn installed_plugin_dir(&self, name: &str) -> PathBuf {
        self.extensions_root().join(name)
    }

    /// List installed plugins (name/version/runtime/trusted). Directories
    /// under the extensions root whose manifest is unreadable or invalid are
    /// reported in the second element so listing stays useful.
    pub fn list(&self, trust_store: &TrustStore) -> Result<(Vec<PluginInfo>, Vec<String>)> {
        let (installed, problems) = self.scan_installed()?;
        let plugins = installed
            .into_iter()
            .map(|plugin| PluginInfo {
                trusted: self.plugin_trusted(trust_store, &plugin.path),
                ..plugin
            })
            .collect();
        Ok((plugins, problems))
    }

    /// Install a plugin from `source` (local directory, local or remote
    /// `.tgz`/`.tar.gz`/`.tar` archive, `owner/repo` GitHub reference,
    /// `npm:<name>[@<version>]` reference, or git URL). Writes files only;
    /// never executes package code. On success the plugin is recorded as
    /// `Trusted` in the trust store, making it loadable.
    pub async fn install(&self, source: &str, trust_store: &TrustStore) -> Result<PluginInfo> {
        let root = self.extensions_root();
        fs::create_dir_all(&root)
            .with_context(|| format!("creating extensions root {}", root.display()))?;
        let staging = unique_sibling(&root, "plugin-staging");
        let mut guard = RemovePathOnDrop::new(staging.clone());
        stage_source(source, &staging)
            .await
            .with_context(|| format!("staging plugin source {}", redact_url_credentials(source)))?;
        let manifest = validate_package(&staging)
            .with_context(|| format!("invalid plugin package from {}", redact_url_credentials(source)))?;
        let name = manifest.id.clone();
        validate_plugin_name(&name)?;
        let target = root.join(&name);
        if target.exists() {
            bail!(
                "plugin {name} is already installed at {}; run `rpi plugin update {name}` or `rpi plugin remove {name}` first",
                target.display()
            );
        }
        fs::rename(&staging, &target)
            .with_context(|| format!("installing plugin {name} at {}", target.display()))?;
        guard.disarm();
        // The install command is the user's explicit consent: record trust so
        // the plugin becomes loadable.
        trust_store
            .set(&target, TrustDecision::Trusted)
            .with_context(|| format!("recording trust for plugin {name}"))?;
        Ok(plugin_info(&target, &manifest, true))
    }

    /// Remove an installed plugin and clear its stored trust decision.
    pub fn remove(&self, name: &str, trust_store: &TrustStore) -> Result<()> {
        validate_plugin_name(name)?;
        let root = self.extensions_root();
        let target = root.join(name);
        if !target.is_dir() {
            bail!("plugin {name} is not installed");
        }
        let canonical_root = fs::canonicalize(&root)
            .with_context(|| format!("resolving extensions root {}", root.display()))?;
        let canonical_target = fs::canonicalize(&target)
            .with_context(|| format!("resolving plugin {}", target.display()))?;
        if canonical_target != canonical_root && !canonical_target.starts_with(&canonical_root) {
            bail!(
                "refusing to remove path outside the extensions root: {}",
                target.display()
            );
        }
        fs::remove_dir_all(&target).with_context(|| format!("removing plugin {name}"))?;
        trust_store
            .set(&target, TrustDecision::Ask)
            .with_context(|| format!("clearing trust for plugin {name}"))?;
        Ok(())
    }

    /// Update one installed plugin from the marketplace index. The plugin is
    /// re-staged from the index entry's `repo`, validated, and atomically
    /// swapped in; the stored trust decision is preserved.
    pub async fn update(
        &self,
        name: &str,
        index: &MarketplaceIndex,
        trust_store: &TrustStore,
    ) -> Result<PluginInfo> {
        validate_plugin_name(name)?;
        let root = self.extensions_root();
        let target = root.join(name);
        if !target.is_dir() {
            bail!("plugin {name} is not installed");
        }
        let installed = read_plugin_manifest(&target)
            .with_context(|| format!("reading installed plugin {name}"))?;
        let installed_version = installed.version.as_deref().unwrap_or("0.0.0");
        let entry = index
            .entry(name)
            .ok_or_else(|| anyhow!("plugin {name} is not listed in the marketplace index"))?;
        if compare_versions(&entry.version, installed_version) != Ordering::Greater {
            bail!(
                "plugin {name} is already at the newest version {installed_version} (marketplace index has {})",
                entry.version
            );
        }
        let staging = unique_sibling(&root, "plugin-staging");
        let mut guard = RemovePathOnDrop::new(staging.clone());
        stage_source(&entry.repo, &staging)
            .await
            .with_context(|| {
                format!(
                    "staging plugin {name} from {}",
                    redact_url_credentials(&entry.repo)
                )
            })?;
        let manifest = validate_package(&staging)
            .with_context(|| {
                format!(
                    "invalid plugin package from {}",
                    redact_url_credentials(&entry.repo)
                )
            })?;
        if manifest.id != name {
            bail!(
                "marketplace entry {name} staged a manifest with id {:?}; refusing to replace",
                manifest.id
            );
        }
        directory_swap(&staging, &target).with_context(|| format!("replacing plugin {name}"))?;
        guard.disarm();
        let trusted = self.plugin_trusted(trust_store, &target);
        Ok(plugin_info(&target, &manifest, trusted))
    }

    /// Every installed plugin with a newer version in the index.
    pub fn available_updates(&self, index: &MarketplaceIndex) -> Result<Vec<PluginUpdate>> {
        let (installed, _) = self.scan_installed()?;
        let mut updates = Vec::new();
        for plugin in installed {
            let Some(entry) = index.entry(&plugin.name) else {
                continue;
            };
            if compare_versions(&entry.version, &plugin.version) == Ordering::Greater {
                updates.push(PluginUpdate {
                    name: plugin.name,
                    current: plugin.version,
                    available: entry.version.clone(),
                    repo: entry.repo.clone(),
                });
            }
        }
        updates.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(updates)
    }

    fn plugin_trusted(&self, trust_store: &TrustStore, path: &Path) -> bool {
        trust_store
            .resolve(path)
            .map(|resolution| resolution.decision == TrustDecision::Trusted)
            .unwrap_or(false)
    }

    fn scan_installed(&self) -> Result<(Vec<PluginInfo>, Vec<String>)> {
        let root = self.extensions_root();
        let mut plugins = Vec::new();
        let mut problems = Vec::new();
        if !root.is_dir() {
            return Ok((plugins, problems));
        }
        let entries = fs::read_dir(&root)
            .with_context(|| format!("reading extensions root {}", root.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", root.display()))?;
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path();
            if !path.is_dir() || !path.join(PROCESS_EXTENSION_MANIFEST_FILE).is_file() {
                continue;
            }
            match read_plugin_manifest(&path) {
                Ok(manifest) => plugins.push(plugin_info(&path, &manifest, false)),
                Err(error) => problems.push(format!("{}: {error:#}", path.display())),
            }
        }
        plugins.sort_by(|a, b| a.name.cmp(&b.name));
        Ok((plugins, problems))
    }
}

/// Scan `<agent_dir>/extensions` and return loadable extension resources.
///
/// The trust store gates loading: a plugin whose canonical path resolves to
/// anything other than a stored `Trusted` decision is skipped (it stays
/// visible in `rpi plugin list` but is not loadable). Each included resource
/// is a `PackageScope::Global` extension root, so the normal
/// [`crate::extension_spec_from_package_resource`] pipeline re-validates the
/// manifest and runtime path before launch.
pub fn marketplace_extension_resources(
    agent_dir: &Path,
    trust_store: &TrustStore,
) -> Result<Vec<PackageResourceSpec>> {
    let root = agent_dir.join("extensions");
    let mut resources = Vec::new();
    if !root.is_dir() {
        return Ok(resources);
    }
    let entries = fs::read_dir(&root)
        .with_context(|| format!("reading extensions root {}", root.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", root.display()))?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() || !path.join(PROCESS_EXTENSION_MANIFEST_FILE).is_file() {
            continue;
        }
        let trusted = trust_store.resolve(&path)?.decision == TrustDecision::Trusted;
        if !trusted {
            continue;
        }
        resources.push(PackageResourceSpec {
            kind: PackageResourceKind::Extension,
            path,
            package_id: format!("marketplace:{}", name.to_string_lossy()),
            scope: PackageScope::Global,
            trusted,
        });
    }
    resources.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(resources)
}

/// Validate a plugin package staged at `root`: the root must contain a
/// `pi-extension.json` that parses and validates, and the package must convert
/// to an [`crate::ExtensionSpec`] (runtime path inside the package, file
/// exists) exactly as the extension runtime will. Reads files only — nothing
/// is executed.
fn validate_package(root: &Path) -> Result<ProcessExtensionManifest> {
    let manifest = read_plugin_manifest(root)?;
    let spec = PackageResourceSpec {
        kind: PackageResourceKind::Extension,
        path: root.to_path_buf(),
        package_id: format!("marketplace:{}", manifest.id),
        scope: PackageScope::Global,
        trusted: true,
    };
    extension_spec_from_package_resource(&spec)
        .with_context(|| format!("plugin {} is not loadable", manifest.id))?;
    Ok(manifest)
}

/// Parse + schema-validate the manifest at `root/pi-extension.json` without
/// resolving the runtime path (listing stays tolerant of a broken runtime).
fn read_plugin_manifest(root: &Path) -> Result<ProcessExtensionManifest> {
    let manifest_path = root.join(PROCESS_EXTENSION_MANIFEST_FILE);
    if !manifest_path.is_file() {
        bail!(
            "plugin package must contain a {PROCESS_EXTENSION_MANIFEST_FILE} manifest at its root: {}",
            manifest_path.display()
        );
    }
    let bytes = fs::read(&manifest_path)
        .with_context(|| format!("reading plugin manifest {}", manifest_path.display()))?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        bail!(
            "plugin manifest {} exceeds {MAX_MANIFEST_BYTES} bytes",
            manifest_path.display()
        );
    }
    let manifest: ProcessExtensionManifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing plugin manifest {}", manifest_path.display()))?;
    manifest
        .validate(&manifest_path)
        .with_context(|| format!("invalid plugin manifest {}", manifest_path.display()))?;
    Ok(manifest)
}

fn plugin_info(path: &Path, manifest: &ProcessExtensionManifest, trusted: bool) -> PluginInfo {
    let runtime = match &manifest.runtime {
        ExtensionManifestRuntime::Process { .. } => PluginRuntime::Process,
        ExtensionManifestRuntime::QuickJs { .. } => PluginRuntime::QuickJs,
    };
    PluginInfo {
        name: manifest.id.clone(),
        version: manifest.version.clone().unwrap_or_else(|| "0.0.0".to_owned()),
        runtime,
        trusted,
        path: path.to_path_buf(),
    }
}

/// Validate a plugin name used as an extension directory name. Mirrors the
/// manifest identifier grammar (`[A-Za-z0-9._-]+`, ≤ 128 bytes) and rejects
/// the path-traversal spellings `.` and `..`.
fn validate_plugin_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("plugin name must not be empty");
    }
    if name.len() > 128 {
        bail!("plugin name exceeds 128 bytes");
    }
    if name == "." || name == ".." {
        bail!("plugin name {name:?} is not a valid directory name");
    }
    if name
        .chars()
        .any(|character| !(character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')))
    {
        bail!("plugin name {name:?} contains unsupported characters");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Staging
// ---------------------------------------------------------------------------

/// Materialize `source` into the staging directory as a plugin package root.
async fn stage_source(source: &str, staging: &Path) -> Result<()> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        bail!("plugin source must not be empty");
    }
    // Error messages never echo URL credentials, even when the caller's
    // `source` embeds them (e.g. `https://user:pass@host/repo.git`).
    let redacted = redact_url_credentials(trimmed);
    // A local path wins over a same-shaped owner/repo reference.
    if Path::new(trimmed).exists() {
        let metadata = fs::metadata(trimmed)
            .with_context(|| format!("reading plugin source {}", Path::new(trimmed).display()))?;
        if metadata.is_dir() {
            return copy_tree(Path::new(trimmed), staging);
        }
        if is_archive_path(Path::new(trimmed)) {
            let bytes = fs::read(trimmed)
                .with_context(|| format!("reading plugin archive {}", Path::new(trimmed).display()))?;
            return extract_tar_from_bytes(&bytes, staging)
                .with_context(|| format!("extracting plugin archive {}", Path::new(trimmed).display()));
        }
        bail!(
            "unsupported plugin source {redacted}: expected a directory, a .tgz/.tar.gz/.tar archive, an http(s) archive URL, an owner/repo GitHub reference, an npm:<name>[@<version>] reference, or a git URL (git+https://host/owner/repo, git+ssh://git@host/owner/repo.git, https://host/owner/repo.git, ssh://git@host/owner/repo.git, git@host:owner/repo.git)"
        );
    }
    // Git URL sources come before the http-archive check so
    // `https://host/owner/repo.git` is not mistaken for a tarball URL.
    if let Some(clone_url) = parse_git_reference(trimmed) {
        return stage_git_source(&clone_url, staging).await;
    }
    // A git-shaped URL that failed strict parsing gets a git-specific
    // actionable error instead of the archive-URL or generic source error.
    if looks_like_git_url(trimmed) {
        bail!(
            "unsupported plugin git source {redacted}: expected git+https://host/owner/repo, git+ssh://git@host/owner/repo.git, https://host/owner/repo.git, ssh://git@host/owner/repo.git, or git@host:owner/repo.git"
        );
    }
    if let Some(url) = parse_http_url(trimmed) {
        if !is_archive_path(Path::new(url.path())) {
            bail!(
                "unsupported plugin archive URL {redacted}: expected a .tgz/.tar.gz/.tar archive URL"
            );
        }
        let bytes = fetch_bytes(url.as_str(), MAX_PACKAGE_BYTES).await?;
        return extract_tar_from_bytes(&bytes, staging)
            .with_context(|| format!("extracting plugin archive {url}"));
    }
    if let Some((owner, repo)) = parse_github_reference(trimmed) {
        let url = format!("https://codeload.github.com/{owner}/{repo}/tar.gz/HEAD");
        let bytes = fetch_bytes(&url, MAX_PACKAGE_BYTES).await?;
        return extract_tar_from_bytes(&bytes, staging)
            .with_context(|| format!("extracting GitHub tarball {url}"));
    }
    if let Some(reference) = parse_npm_reference(trimmed) {
        return stage_npm_source(&reference, staging).await;
    }
    bail!(
        "unsupported plugin source {redacted}: the path does not exist; expected a local directory, a .tgz/.tar.gz/.tar archive, an http(s) archive URL, an owner/repo GitHub reference, an npm:<name>[@<version>] reference, or a git URL (git+https://host/owner/repo, git+ssh://git@host/owner/repo.git, https://host/owner/repo.git, ssh://git@host/owner/repo.git, git@host:owner/repo.git)"
    );
}

fn parse_http_url(value: &str) -> Option<url::Url> {
    let url = url::Url::parse(value).ok()?;
    matches!(url.scheme(), "http" | "https").then_some(url)
}

/// Replace credential-bearing URL userinfo (`scheme://user:pass@host`) with
/// `***`, so plugin errors never echo source credentials (git stderr, the
/// install context, and the unsupported-source messages can all carry the
/// raw source string).
pub fn redact_url_credentials(text: &str) -> String {
    static CREDENTIALS: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| {
            regex::Regex::new(r"(?i)([a-z][a-z0-9+.-]*://)[^/@\s]+@").expect("valid regex")
        });
    CREDENTIALS.replace_all(text, "$1***@").into_owned()
}

fn is_archive_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            extension.eq_ignore_ascii_case("tgz")
                || extension.eq_ignore_ascii_case("gz")
                || extension.eq_ignore_ascii_case("tar")
        })
        .unwrap_or(false)
}

/// Parse a GitHub-style `owner/repo` reference (exactly two segments, both
/// valid GitHub names). Returns `None` for URLs, paths with more segments,
/// or names that could never exist on GitHub.
fn parse_github_reference(value: &str) -> Option<(String, String)> {
    if value.contains("://") || value.starts_with('.') {
        return None;
    }
    let mut parts = value.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if parts.next().is_some() || owner.is_empty() || repo.is_empty() {
        return None;
    }
    let repo_name = repo.strip_suffix(".git").unwrap_or(repo);
    (valid_repo_name(owner) && valid_repo_name(repo_name)).then(|| (owner.to_owned(), repo_name.to_owned()))
}

/// Whether `part` could be a GitHub owner or repository name: ASCII
/// alphanumerics, `_`, `-`, `.`, at most 100 bytes, and no leading/trailing
/// dot.
fn valid_repo_name(part: &str) -> bool {
    !part.is_empty()
        && part.len() <= 100
        && !part.starts_with('.')
        && !part.ends_with('.')
        && part
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.'))
}

/// A git URL plugin source, normalized to a clone URL. Accepts:
/// - `git+https://host/owner/repo[.git]` (the `.git` suffix is optional);
/// - `git+ssh://git@host/owner/repo.git`, `ssh://git@host/owner/repo.git`;
/// - `https://host/owner/repo.git`;
/// - `git@host:owner/repo.git` (scp-like), normalized to `ssh://git@host/...`;
/// - `file://.../owner/repo.git` / `git+file://...` for local repositories
///   (used by tests and local mirrors).
///
/// Returns `None` for anything that is not a well-formed git URL: control
/// characters, whitespace, shell metacharacters, non-`https`/`ssh`/`file`
/// schemes, query/fragment components, or a path that is not an
/// `owner/repo(.git)` shape. The caller falls through to the other source
/// kinds (or, via [`looks_like_git_url`], reports a git-specific error).
fn parse_git_reference(value: &str) -> Option<String> {
    // Control characters, whitespace, and shell metacharacters are never part
    // of a well-formed git URL. The clone itself is argv-built (no shell
    // string is ever constructed), but rejecting them here keeps malformed
    // input from reaching the process table at all.
    if value
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return None;
    }
    const SHELL_METACHARACTERS: &[char] = &[
        '"', '\'', '\\', '`', '$', '&', '|', ';', '<', '>', '(', ')', '*',
        '?', '[', ']', '{', '}', '!', '#',
    ];
    if value
        .chars()
        .any(|character| SHELL_METACHARACTERS.contains(&character))
    {
        return None;
    }
    // The explicit `git+` prefix marks the value as a git URL and relaxes the
    // `.git` suffix requirement: `git+https://host/owner/repo` is still an
    // unambiguous repository path.
    let (candidate, bare_repo_allowed) = match value.strip_prefix("git+") {
        Some(rest) => (rest, true),
        None => (value, false),
    };
    // scp-like `[user@]host:owner/repo.git`: a colon before any scheme.
    if !candidate.contains("://") {
        let (host, path) = candidate.split_once(':')?;
        if host.is_empty() || host.contains('/') || path.is_empty() {
            return None;
        }
        let normalized = format!("ssh://{host}/{path}");
        let url = url::Url::parse(&normalized).ok()?;
        return valid_git_clone_url(&url, bare_repo_allowed).map(|url| url.to_string());
    }
    let url = url::Url::parse(candidate).ok()?;
    valid_git_clone_url(&url, bare_repo_allowed).map(|url| url.to_string())
}

/// Validate a normalized git clone URL: an `https`/`ssh`/`file` scheme, a
/// host (except `file:`), no query or fragment, and an
/// `owner/repo(.git)?` path — the `.git` suffix is optional only for
/// explicit `git+` sources. `file:` URLs may nest deeper than two segments
/// (`file:///tmp/.../owner/repo.git`); the final segment must still carry the
/// repository name. Returns the canonical clone URL on success.
fn valid_git_clone_url(url: &url::Url, bare_repo_allowed: bool) -> Option<String> {
    if !matches!(url.scheme(), "https" | "ssh" | "file") {
        return None;
    }
    if url.query().is_some() || url.fragment().is_some() {
        return None;
    }
    let is_file = url.scheme() == "file";
    if is_file {
        // `file://host/...` is a UNC path, not a local repository.
        if url.host_str().is_some_and(|host| !host.is_empty()) {
            return None;
        }
    } else if url.host_str().is_none_or(str::is_empty) {
        return None;
    }
    let segments: Vec<&str> = url
        .path()
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.len() < 2 {
        return None;
    }
    if !is_file && segments.len() != 2 {
        return None; // https/ssh repos are exactly host/owner/repo
    }
    let owner = segments[segments.len() - 2];
    let repo = *segments.last().expect("at least two segments");
    if owner == "." || owner == ".." || repo == "." || repo == ".." {
        return None;
    }
    if !is_file && !valid_repo_name(owner) {
        return None;
    }
    let (repo_name, git_suffix) = match repo.strip_suffix(".git") {
        Some(name) => (name, true),
        None => (repo, false),
    };
    if !git_suffix && !bare_repo_allowed {
        return None;
    }
    if repo_name.is_empty() || !valid_repo_name(repo_name) {
        return None;
    }
    Some(url.to_string())
}

/// Whether `value` was *intended* as a git URL even though strict parsing
/// rejected it: an explicit `git+` prefix, or a scp-like / URL form whose
/// last path segment ends in `.git`. Invalid git URLs then get a
/// git-specific actionable error instead of the archive-URL or generic
/// source error. The raw string is inspected (not the `Url`-parsed form),
/// so percent-encoding cannot hide the whitespace or metacharacters that
/// make the URL invalid.
fn looks_like_git_url(value: &str) -> bool {
    if value.starts_with("git+") {
        return true;
    }
    if value.contains("://") {
        let path = value
            .split_once("://")
            .and_then(|(_, rest)| rest.split_once('/'))
            .map(|(_, path)| path)
            .unwrap_or("");
        return last_path_segment(path).ends_with(".git");
    }
    // scp-like `[user@]host:owner/repo.git`.
    value.split_once(':').is_some_and(|(host, path)| {
        !host.is_empty()
            && !host.contains('/')
            && path.contains('/')
            && last_path_segment(path).ends_with(".git")
    })
}

/// The final `/`-separated segment of `path` (the whole input when it has no
/// slash).
fn last_path_segment(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or("")
}

/// An `npm:<name>[@<version>]` plugin source, split into the registry
/// reference halves. Scoped npm names (`@scope/name`) are supported; the
/// version is optional (latest is resolved through the registry).
#[derive(Debug, Clone, PartialEq, Eq)]
struct NpmReference {
    name: String,
    version: Option<String>,
}

/// Parse an `npm:` source: `npm:<name>` (latest) or `npm:<name>@<version>`
/// (pinned). The trailing `@` split is the *last* `@` so scoped names
/// (`@scope/name`) parse correctly. `None` for anything that is not an npm
/// reference (the caller then falls through to the other source kinds).
fn parse_npm_reference(value: &str) -> Option<NpmReference> {
    let spec = value.strip_prefix("npm:")?;
    if spec.is_empty() {
        return None;
    }
    let (name, version) = match spec.rfind('@') {
        Some(index) if index > 0 => {
            let (name, version) = spec.split_at(index);
            let version = &version[1..];
            (name, (!version.is_empty()).then(|| version.to_owned()))
        }
        _ => (spec, None),
    };
    if !valid_npm_name(name) {
        return None;
    }
    Some(NpmReference {
        name: name.to_owned(),
        version,
    })
}

/// npm package-name shape: scoped names (`@scope/name`) or bare names, no
/// path traversal, no control characters, no leading `.`/`_`, no uppercase
/// (the npm registry rejects those; rejecting them here keeps errors local
/// and actionable).
fn valid_npm_name(name: &str) -> bool {
    let (scope, package) = match name.strip_prefix('@') {
        Some(rest) => match rest.split_once('/') {
            Some((scope, package)) if !scope.is_empty() && !package.is_empty() => (scope, package),
            _ => return false,
        },
        None => (name, name),
    };
    [scope, package].into_iter().all(|part| {
        !part.starts_with('.')
            && !part.starts_with('_')
            && part
                .chars()
                .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit() || matches!(character, '-' | '_' | '.'))
    })
}

/// Clone a git repository into `staging` (`git clone --depth 1 <url>
/// <staging>`, argv-built so no shell string is ever constructed) with a
/// hard timeout, then drop the cloned `.git` metadata so credentials that
/// may be embedded in the source URL never persist in the installed package.
/// Failures are actionable — missing git binary, repository not found, auth
/// failure — and never echo URL credentials.
async fn stage_git_source(clone_url: &str, staging: &Path) -> Result<()> {
    let mut command = tokio::process::Command::new("git");
    command
        .args(["-c", "color.ui=false", "clone", "--depth", "1", clone_url])
        .arg(staging)
        // Never prompt interactively: a private or unknown repository fails
        // fast instead of hanging the install on a terminal prompt.
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "git is not installed; install git to install plugins from git URLs (clone URL: {})",
                redact_url_credentials(clone_url)
            );
        }
        Err(error) => {
            bail!(
                "starting git clone failed: {error} (clone URL: {})",
                redact_url_credentials(clone_url)
            );
        }
    };
    // Read stderr concurrently with waiting for the exit so a chatty git can
    // never block on the pipe buffer. `wait` borrows `child` mutably (unlike
    // `wait_with_output`, which moves it), so the timeout branch can still
    // kill and reap the child.
    let mut stderr = child.stderr.take().expect("stderr piped above");
    let mut stderr_bytes = Vec::new();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(GIT_CLONE_TIMEOUT_SECONDS),
        async {
            tokio::join!(
                child.wait(),
                tokio::io::AsyncReadExt::read_to_end(&mut stderr, &mut stderr_bytes)
            )
        },
    )
    .await;
    let (wait_result, _read_result) = match result {
        Ok(pair) => pair,
        Err(_elapsed) => {
            // Do not leave a hung clone behind.
            let _ = child.kill().await;
            let _ = child.wait().await;
            bail!(
                "git clone timed out after {GIT_CLONE_TIMEOUT_SECONDS}s (clone URL: {}); check the repository URL and network access",
                redact_url_credentials(clone_url)
            );
        }
    };
    let status = wait_result.with_context(|| {
        format!(
            "running git clone (clone URL: {})",
            redact_url_credentials(clone_url)
        )
    })?;
    if !status.success() {
        let detail = redact_url_credentials(&String::from_utf8_lossy(&stderr_bytes));
        let detail = detail.trim();
        let url = redact_url_credentials(clone_url);
        if detail.is_empty() {
            bail!("git clone of {url} failed with status {status}");
        }
        if detail.contains("not found") || detail.contains("does not appear to be a git repository") {
            bail!("git repository {url} was not found or is not accessible: {detail}");
        }
        if detail.contains("authentication failed")
            || detail.contains("Authentication failed")
            || detail.contains("Permission denied")
            || detail.contains("could not read")
            || detail.contains("401")
            || detail.contains("403")
        {
            bail!("git clone of {url} failed authentication: {detail}");
        }
        bail!("git clone of {url} failed: {detail}");
    }
    // Never carry the cloned repository's metadata into the installed
    // package: `.git/config` records the source URL (including any embedded
    // credentials), and the metadata is not part of the plugin.
    remove_git_metadata(staging)?;
    Ok(())
}

/// Remove the `.git` metadata directory (or gitfile) inside a freshly cloned
/// staging root, so the installed package mirrors what the archive sources
/// produce.
fn remove_git_metadata(root: &Path) -> Result<()> {
    let git = root.join(".git");
    let metadata = match fs::symlink_metadata(&git) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("inspecting {}", git.display())),
    };
    if metadata.is_dir() {
        fs::remove_dir_all(&git).with_context(|| format!("removing {}", git.display()))
    } else {
        fs::remove_file(&git).with_context(|| format!("removing {}", git.display()))
    }
}

/// Resolve an `npm:` source to a package tarball and stage it: fetch the
/// registry metadata (latest or pinned version), extract `dist.tarball`,
/// fetch the tarball (bounded), verify its `dist.integrity` sha512 digest
/// (authenticating the package content against the registry metadata), and
/// extract it into `staging` with the same traversal/size guards as every
/// other archive source. A 404 from the registry surfaces as an actionable
/// "package not found" error. Tarball URLs must be https (a registry that
/// serves plain-http tarballs is refused: the integrity check would
/// authenticate content fetched over an unauthenticated channel).
async fn stage_npm_source(reference: &NpmReference, staging: &Path) -> Result<()> {
    let registry = npm_registry_base();
    let metadata_url = match &reference.version {
        Some(version) => format!("{registry}/{}/{}", reference.name, version),
        None => format!("{registry}/{}", reference.name),
    };
    let version_hint = reference
        .version
        .as_ref()
        .map(|version| format!(" and version {version}"))
        .unwrap_or_default();
    let metadata = fetch_bytes(&metadata_url, MAX_NPM_METADATA_BYTES)
        .await
        .map_err(|error| {
            if error.to_string().contains("HTTP 404") {
                anyhow!(
                    "npm package {}{} was not found on the registry (HTTP 404)",
                    reference.name,
                    version_hint
                )
            } else {
                error
            }
        })
        .with_context(|| {
            format!(
                "resolving npm package {} from {metadata_url}",
                reference.name
            )
        })?;
    let document: serde_json::Value = serde_json::from_slice(&metadata)
        .with_context(|| format!("parsing npm metadata for {} from {metadata_url}", reference.name))?;
    let dist = document
        .get("dist")
        .ok_or_else(|| {
            anyhow!(
                "npm package {} metadata from {metadata_url} has no dist; the package is not a publishable tarball",
                reference.name
            )
        })?;
    let tarball_url = dist
        .get("tarball")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            anyhow!(
                "npm package {} metadata from {metadata_url} has no dist.tarball; the package is not a publishable tarball",
                reference.name
            )
        })?;
    let tarball_url = url::Url::parse(&tarball_url)
        .with_context(|| format!("npm package {} has an invalid tarball URL", reference.name))?;
    // A tarball served by an https registry must itself be https (the
    // integrity check would otherwise authenticate content fetched over an
    // unauthenticated channel). An explicitly configured http registry (local
    // mirrors, tests) may serve http tarballs — the integrity check still
    // authenticates the content against the registry metadata.
    let registry_url = url::Url::parse(&registry)
        .with_context(|| format!("npm registry base {registry} is not a valid URL"))?;
    if registry_url.scheme() == "https" && tarball_url.scheme() != "https" {
        bail!(
            "npm package {} tarball URL must be https (registry {registry} is https), got {}",
            reference.name,
            tarball_url
        );
    }
    let integrity = dist
        .get("integrity")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    // Fail closed: npm mandates `dist.integrity` for modern packages, so a
    // registry serving metadata without it (or with only the legacy
    // `dist.shasum` sha1 field, which this code does not accept) must never
    // install unauthenticated content. TLS alone is not the trust boundary —
    // the integrity check authenticates the tarball content against the
    // registry metadata, and silently skipping it would let a tampered
    // tarball install on a compromised-but-TLS-terminated path.
    let integrity = integrity.ok_or_else(|| {
        anyhow!(
            "npm package {} metadata from {metadata_url} has no dist.integrity; refusing to install unauthenticated content",
            reference.name
        )
    })?;
    let bytes = fetch_bytes(tarball_url.as_str(), MAX_PACKAGE_BYTES)
        .await
        .with_context(|| format!("downloading npm tarball {tarball_url}"))?;
    verify_npm_integrity(&bytes, &integrity)
        .with_context(|| format!("npm package {} tarball failed integrity verification", reference.name))?;
    extract_tar_from_bytes(&bytes, staging)
        .with_context(|| format!("extracting npm tarball {tarball_url}"))
}

/// Verify `bytes` against an npm `dist.integrity` value: `sha512-<base64>`.
/// Any other algorithm or a digest mismatch fails closed (the package content
/// is not authenticated and must not be installed).
fn verify_npm_integrity(bytes: &[u8], integrity: &str) -> Result<()> {
    let (algorithm, expected) = integrity
        .split_once('-')
        .ok_or_else(|| anyhow!("unsupported npm integrity format {integrity:?}"))?;
    if algorithm != "sha512" {
        bail!("unsupported npm integrity algorithm {algorithm:?}; only sha512 is accepted");
    }
    use base64::Engine as _;
    let expected = base64::engine::general_purpose::STANDARD
        .decode(expected)
        .with_context(|| format!("npm integrity digest {integrity:?} is not valid base64"))?;
    use sha2::Digest as _;
    let actual = sha2::Sha512::digest(bytes);
    if actual.as_slice() != expected.as_slice() {
        bail!("npm integrity digest mismatch: expected {integrity:?}");
    }
    Ok(())
}

/// Recursively copy a plugin directory. Skips symlinks and hidden/
/// `node_modules` entries (mirroring package resource discovery) and enforces
/// the package byte cap.
fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target)
        .with_context(|| format!("creating staging directory {}", target.display()))?;
    let mut copied: u64 = 0;
    for entry in WalkDir::new(source)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| entry.depth() == 0 || visible_entry(entry))
    {
        let entry = entry.with_context(|| format!("walking {}", source.display()))?;
        if entry.depth() == 0 {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(source)
            .with_context(|| format!("{} is outside {}", entry.path().display(), source.display()))?;
        let destination = target.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&destination)
                .with_context(|| format!("creating {}", destination.display()))?;
        } else {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            let size = entry
                .metadata()
                .with_context(|| format!("reading {}", entry.path().display()))?
                .len();
            copied = copied
                .checked_add(size)
                .ok_or_else(|| anyhow!("plugin package size overflow"))?;
            if copied > MAX_PACKAGE_BYTES {
                bail!("plugin package exceeds {MAX_PACKAGE_BYTES} bytes");
            }
            fs::copy(entry.path(), &destination)
                .with_context(|| format!("copying {} to {}", entry.path().display(), destination.display()))?;
        }
    }
    Ok(())
}

fn visible_entry(entry: &walkdir::DirEntry) -> bool {
    !entry.file_type().is_symlink()
        && !entry.file_name().to_string_lossy().starts_with('.')
        && entry.file_name() != "node_modules"
}

/// One archive entry staged for extraction, before the top-level directory
/// decision is applied.
struct PlannedEntry {
    path: PathBuf,
    is_dir: bool,
    data: Vec<u8>,
}

/// Extract a `.tgz`/`.tar.gz`/`.tar` archive into `target` with traversal,
/// entry-kind, and size guards. A single shared top-level directory (as in
/// GitHub codeload tarballs, `owner-repo-<sha>/...`) is stripped so the
/// manifest lands at the target root.
fn extract_tar_from_bytes(bytes: &[u8], target: &Path) -> Result<()> {
    if bytes.is_empty() {
        bail!("plugin archive is empty");
    }
    let reader: Box<dyn Read> = if bytes.starts_with(&[0x1f, 0x8b]) {
        Box::new(flate2::read::GzDecoder::new(std::io::Cursor::new(bytes)))
    } else {
        Box::new(std::io::Cursor::new(bytes))
    };
    let mut archive = tar::Archive::new(reader);
    let entries = archive.entries().context("reading plugin archive entries")?;

    let mut planned: Vec<PlannedEntry> = Vec::new();
    let mut total_bytes: u64 = 0;
    for entry in entries {
        let mut entry = entry.context("reading plugin archive entry")?;
        if planned.len() >= MAX_PACKAGE_ENTRIES {
            bail!("plugin archive has more than {MAX_PACKAGE_ENTRIES} entries");
        }
        let path = entry
            .path()
            .context("reading plugin archive entry path")?
            .into_owned();
        validate_archive_path(&path)?;
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            planned.push(PlannedEntry {
                path,
                is_dir: true,
                data: Vec::new(),
            });
            continue;
        }
        if !entry_type.is_file() {
            bail!(
                "plugin archive entry is not a regular file or directory: {}",
                path.display()
            );
        }
        let size = entry.size();
        total_bytes = total_bytes
            .checked_add(size)
            .ok_or_else(|| anyhow!("plugin archive size overflow"))?;
        if total_bytes > MAX_PACKAGE_BYTES {
            bail!("plugin archive exceeds {MAX_PACKAGE_BYTES} bytes");
        }
        let mut data = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
        entry
            .read_to_end(&mut data)
            .context("reading plugin archive entry contents")?;
        planned.push(PlannedEntry {
            path,
            is_dir: false,
            data,
        });
    }

    let strip_top = shared_top_directory(&planned);
    for entry in &planned {
        let relative: PathBuf = if strip_top {
            entry.path.components().skip(1).collect()
        } else {
            entry.path.clone()
        };
        let destination = target.join(&relative);
        if entry.is_dir {
            fs::create_dir_all(&destination)
                .with_context(|| format!("creating {}", destination.display()))?;
        } else {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            fs::write(&destination, &entry.data)
                .with_context(|| format!("writing {}", destination.display()))?;
        }
    }
    Ok(())
}

fn validate_archive_path(path: &Path) -> Result<()> {
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("plugin archive contains an unsafe path: {}", path.display());
    }
    if path.as_os_str().is_empty() {
        bail!("plugin archive contains an empty path");
    }
    Ok(())
}

/// Whether every entry shares one top-level directory and no top-level file
/// exists, so the shared directory can be stripped (GitHub codeload tarballs
/// use the `owner-repo-<sha>/...` shape).
fn shared_top_directory(entries: &[PlannedEntry]) -> bool {
    let Some(first) = entries.first() else {
        return false;
    };
    let Some(top) = first.path.components().next() else {
        return false;
    };
    let shared = entries.iter().all(|entry| {
        matches!(
            entry.path.components().next(),
            Some(component) if component.as_os_str() == top.as_os_str()
        )
    });
    if !shared {
        return false;
    }
    let nests = entries
        .iter()
        .any(|entry| entry.path.components().count() >= 2);
    let top_level_file = entries
        .iter()
        .any(|entry| entry.path.components().count() == 1 && !entry.is_dir);
    nests && !top_level_file
}

/// Bounded HTTP fetch (follows redirects, enforces a byte cap).
async fn fetch_bytes(url: &str, max: u64) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECONDS))
        .build()
        .context("building HTTP client")?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("fetching {url} failed: HTTP {status}");
    }
    if response.content_length().is_some_and(|length| length > max) {
        bail!("{url} exceeds {max} bytes");
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("reading response from {url}"))?;
        bytes.extend_from_slice(&chunk);
        if bytes.len() as u64 > max {
            bail!("{url} exceeds {max} bytes");
        }
    }
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Filesystem helpers
// ---------------------------------------------------------------------------

/// A sibling path like `<parent>/.<label>-<pid>-<millis>-<counter>`.
fn unique_sibling(path: &Path, label: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let name = format!(".{label}-{}-{millis}-{counter}", std::process::id());
    path.with_file_name(name)
}

/// Remove a directory on drop unless disarmed (used for staging cleanup).
struct RemovePathOnDrop {
    path: Option<PathBuf>,
}

impl RemovePathOnDrop {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for RemovePathOnDrop {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = fs::remove_dir_all(path);
        }
    }
}

/// Atomically replace `target` with `staged` (both directories): move the old
/// target aside, move the staged directory in, then remove the backup. On a
/// failed move-in the original is restored.
fn directory_swap(staged: &Path, target: &Path) -> Result<()> {
    let backup = unique_sibling(target, "plugin-backup");
    fs::rename(target, &backup).with_context(|| format!("moving {} aside", target.display()))?;
    if let Err(error) = fs::rename(staged, target) {
        let restore = fs::rename(&backup, target);
        return Err(error).with_context(|| {
            format!(
                "installing replacement at {} (restoring original: {:?})",
                target.display(),
                restore.err()
            )
        });
    }
    // Best-effort backup cleanup; a leftover hidden backup is harmless and
    // must not fail an otherwise complete update.
    let _ = fs::remove_dir_all(&backup);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::{ExtensionPermissionSet, ExtensionSpec, ResourceManagerOptions};
    use serde_json::json;
    use tempfile::TempDir;

    const QUICKJS_MANIFEST: &str = r#"{
        "schemaVersion": 1,
        "id": "demo",
        "version": "1.0.0",
        "runtime": "quickjs",
        "entry": "index.mjs",
        "capabilities": ["commands"]
    }"#;

    fn write_quickjs_plugin(root: &Path, id: &str, version: &str) -> Result<()> {
        let manifest = json!({
            "schemaVersion": 1,
            "id": id,
            "version": version,
            "runtime": "quickjs",
            "entry": "index.mjs",
            "capabilities": ["commands"],
        });
        fs::write(
            root.join(PROCESS_EXTENSION_MANIFEST_FILE),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        fs::write(root.join("index.mjs"), "export default function (pi) {}\n")?;
        Ok(())
    }

    /// Skip-guard for git fixture tests: does this machine have a working
    /// `git` binary on PATH?
    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .is_ok_and(|output| output.status.success())
    }

    /// `git init` (with a local identity so commits work without global
    /// config) and commit whatever is currently in `root`.
    fn init_git_repo(root: &Path) -> Result<()> {
        let git = |args: &[&str]| -> Result<()> {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .with_context(|| format!("running git {args:?}"))?;
            if !output.status.success() {
                bail!(
                    "git {args:?} failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Ok(())
        };
        git(&["init", "-q", "-b", "main"])?;
        git(&["config", "user.email", "plugin-test@example.com"])?;
        git(&["config", "user.name", "Plugin Test"])?;
        git(&["add", "."])?;
        git(&["commit", "-q", "-m", "add plugin"])?;
        Ok(())
    }

    /// Create a git repository at `root` containing a QuickJS plugin
    /// (`pi-extension.json` + entry), committed on `main`.
    fn write_git_plugin_repo(root: &Path, id: &str, version: &str) -> Result<()> {
        write_quickjs_plugin(root, id, version)?;
        init_git_repo(root)
    }

    /// Write a gzip tarball of `entries` (path -> bytes) at `archive`.
    fn write_tarball(archive: &Path, entries: &[(&str, &[u8])]) -> Result<()> {
        let file = fs::File::create(archive)?;
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for (name, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, *contents)?;
        }
        builder.finish()?;
        Ok(())
    }

    /// A gzip tarball of `entries` as in-memory bytes (mock npm tarballs).
    fn tarball_bytes(entries: &[(&str, &[u8])]) -> Result<Vec<u8>> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for (name, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, *contents)?;
        }
        builder.finish()?;
        Ok(builder.into_inner()?.finish()?)
    }

    /// A valid npm-shaped tarball: a `package/` top directory carrying a
    /// QuickJS `pi-extension.json` manifest and its entry point, mirroring
    /// the layout of real npm packages.
    fn side_chat_tarball() -> Result<Vec<u8>> {
        let manifest = serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": "side-chat",
            "version": "1.0.0",
            "runtime": "quickjs",
            "entry": "dist/index.mjs",
            "capabilities": ["overlays"],
        }))?;
        tarball_bytes(&[
            ("package/pi-extension.json", manifest.as_slice()),
            (
                "package/dist/index.mjs",
                b"export default function (pi) { pi.registerOverlay({ id: \"chat\", title: \"Side Chat\", render: () => [\"hello\"] }); }\n",
            ),
            ("package/package.json", br#"{"name":"pi-side-chat","version":"1.0.0","main":"dist/index.mjs"}"#),
        ])
    }

    /// The npm `dist.integrity` value (`sha512-<base64>`) for `bytes`.
    fn npm_integrity(bytes: &[u8]) -> String {
        use base64::Engine as _;
        use sha2::Digest as _;
        let digest = sha2::Sha512::digest(bytes);
        format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(digest)
        )
    }

    /// One mock npm registry response: `Ok(body)` or `Err(http_status)`.
    type RouteResponse = std::result::Result<Vec<u8>, u16>;

    /// Spawn a one-shot mock npm registry on 127.0.0.1:0. `build_routes`
    /// receives the bound port so tarball URLs can embed it. Returns the
    /// base URL (`http://127.0.0.1:<port>`).
    async fn spawn_registry_server(
        build_routes: impl FnOnce(u16) -> Vec<(String, RouteResponse)> + Send + 'static,
    ) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock registry");
        let address = listener.local_addr().expect("mock registry address");
        let routes: std::sync::Arc<std::collections::HashMap<String, RouteResponse>> =
            std::sync::Arc::new(build_routes(address.port()).into_iter().collect());
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let routes = routes.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 1024];
                    loop {
                        match socket.read(&mut chunk).await {
                            Ok(0) => return,
                            Ok(read) => {
                                request.extend_from_slice(&chunk[..read]);
                                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => return,
                        }
                    }
                    let path = String::from_utf8_lossy(&request)
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .split('?')
                        .next()
                        .unwrap_or("/")
                        .to_owned();
                    let (status, body) = match routes.get(&path) {
                        Some(Ok(body)) => (200u16, body.clone()),
                        Some(Err(status)) => (*status, b"{\"error\":\"not found\"}".to_vec()),
                        None => (404u16, b"{\"error\":\"not found\"}".to_vec()),
                    };
                    let reason = if status == 200 { "OK" } else { "Not Found" };
                    let header = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(header.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                });
            }
        });
        format!("http://{address}")
    }

    fn loadable_spec(path: &Path) -> Result<ExtensionSpec> {
        extension_spec_from_package_resource(&PackageResourceSpec {
            kind: PackageResourceKind::Extension,
            path: path.to_path_buf(),
            package_id: "marketplace:test".to_owned(),
            scope: PackageScope::Global,
            trusted: true,
        })
    }

    fn resources_for(agent_dir: &Path, trust_store: &TrustStore) -> Vec<PackageResourceSpec> {
        marketplace_extension_resources(agent_dir, trust_store).expect("scan resources")
    }

    #[tokio::test]
    async fn install_from_local_directory_lands_in_root_and_is_loadable() -> Result<()> {
        let agent = TempDir::new()?;
        let source = TempDir::new()?;
        write_quickjs_plugin(source.path(), "demo", "1.0.0")?;

        let marketplace = PluginMarketplace::new(agent.path());
        let trust_store = TrustStore::new(agent.path());
        let installed = marketplace.install(source.path().to_str().unwrap(), &trust_store).await?;

        assert_eq!(installed.name, "demo");
        assert_eq!(installed.version, "1.0.0");
        assert_eq!(installed.runtime, PluginRuntime::QuickJs);
        assert!(installed.trusted);

        let target = agent.path().join("extensions/demo");
        assert!(target.join("pi-extension.json").is_file());
        assert!(target.join("index.mjs").is_file());

        let (plugins, problems) = marketplace.list(&trust_store)?;
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "demo");
        assert_eq!(plugins[0].version, "1.0.0");
        assert!(plugins[0].trusted);

        // The installed plugin is loadable through the normal extension pipeline.
        let spec = loadable_spec(&target)?;
        assert_eq!(spec.id, "demo");
        let scanned = resources_for(agent.path(), &trust_store);
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].path, target);
        assert!(scanned[0].trusted);
        Ok(())
    }

    #[tokio::test]
    async fn install_from_local_tarball_extracts_and_strips_top_directory() -> Result<()> {
        let agent = TempDir::new()?;
        let archive_dir = TempDir::new()?;
        let archive = archive_dir.path().join("demo-1.0.0.tgz");
        let manifest = serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": "tar-demo",
            "version": "2.0.0",
            "runtime": "quickjs",
            "entry": "src/index.mjs",
            "capabilities": [],
        }))?;
        write_tarball(
            &archive,
            &[
                ("tar-demo-v2/pi-extension.json", manifest.as_slice()),
                ("tar-demo-v2/src/index.mjs", b"export default function (pi) {}\n"),
            ],
        )?;

        let marketplace = PluginMarketplace::new(agent.path());
        let trust_store = TrustStore::new(agent.path());
        let installed = marketplace
            .install(archive.to_str().unwrap(), &trust_store)
            .await?;
        assert_eq!(installed.name, "tar-demo");
        assert_eq!(installed.version, "2.0.0");

        let target = agent.path().join("extensions/tar-demo");
        assert!(target.join("pi-extension.json").is_file());
        assert!(target.join("src/index.mjs").is_file());
        assert!(!target.join("tar-demo-v2").exists(), "top directory must be stripped");
        assert!(loadable_spec(&target).is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn install_from_flat_tarball_without_top_directory() -> Result<()> {
        let agent = TempDir::new()?;
        let archive_dir = TempDir::new()?;
        let archive = archive_dir.path().join("flat.tar.gz");
        let manifest = serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": "flat",
            "runtime": "quickjs",
            "entry": "index.mjs",
            "capabilities": [],
        }))?;
        write_tarball(
            &archive,
            &[
                ("pi-extension.json", manifest.as_slice()),
                ("index.mjs", b"export default function (pi) {}\n"),
            ],
        )?;

        let marketplace = PluginMarketplace::new(agent.path());
        let trust_store = TrustStore::new(agent.path());
        let installed = marketplace.install(archive.to_str().unwrap(), &trust_store).await?;
        assert_eq!(installed.name, "flat");
        // A version-less manifest reports 0.0.0 and remains updatable.
        assert_eq!(installed.version, "0.0.0");
        assert!(loadable_spec(&agent.path().join("extensions/flat")).is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn install_rejects_invalid_manifest_actionably() -> Result<()> {
        let agent = TempDir::new()?;
        let source = TempDir::new()?;
        // schemaVersion 999 fails the schema check.
        fs::write(
            source.path().join("pi-extension.json"),
            serde_json::to_vec_pretty(&json!({
                "schemaVersion": 999,
                "id": "broken",
                "runtime": "quickjs",
                "entry": "index.mjs",
            }))?,
        )?;
        fs::write(source.path().join("index.mjs"), "")?;

        let marketplace = PluginMarketplace::new(agent.path());
        let trust_store = TrustStore::new(agent.path());
        let error = marketplace
            .install(source.path().to_str().unwrap(), &trust_store)
            .await
            .expect_err("invalid manifest must fail install");
        let message = format!("{error:#}");
        assert!(
            message.contains("schemaVersion") || message.contains("unsupported"),
            "{message}"
        );
        // Nothing landed: no plugin directory exists and no staging leftovers
        // remain (the extensions root itself may exist but must be empty).
        assert!(!agent.path().join("extensions/demo").exists());
        let root = agent.path().join("extensions");
        if root.exists() {
            assert!(
                fs::read_dir(&root)?.next().is_none(),
                "failed install must leave no plugins or staging behind"
            );
        }
        assert_eq!(marketplace.list(&trust_store)?.0.len(), 0);
        assert!(
            fs::read_dir(agent.path())?.all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".plugin-staging")
            }),
            "failed install must not leak staging directories"
        );
        Ok(())
    }

    #[tokio::test]
    async fn install_rejects_traversal_and_missing_runtime() -> Result<()> {
        let agent = TempDir::new()?;
        let source = TempDir::new()?;
        // Runtime path escapes the manifest directory: full loadability check
        // must reject it even though the file parses.
        fs::write(
            source.path().join("pi-extension.json"),
            serde_json::to_vec_pretty(&json!({
                "schemaVersion": 1,
                "id": "esc",
                "runtime": "quickjs",
                "entry": "../escape.mjs",
                "capabilities": [],
            }))?,
        )?;
        let marketplace = PluginMarketplace::new(agent.path());
        let trust_store = TrustStore::new(agent.path());
        let error = marketplace
            .install(source.path().to_str().unwrap(), &trust_store)
            .await
            .expect_err("traversal must fail install");
        assert!(format!("{error:#}").contains("must remain inside"), "{error:#}");
        assert!(!agent.path().join("extensions/esc").exists());

        // Missing runtime file: not loadable.
        let missing = TempDir::new()?;
        fs::write(
            missing.path().join("pi-extension.json"),
            serde_json::to_vec_pretty(&json!({
                "schemaVersion": 1,
                "id": "ghost",
                "runtime": "process",
                "executable": "payload.sh",
                "capabilities": [],
            }))?,
        )?;
        let error = marketplace
            .install(missing.path().to_str().unwrap(), &trust_store)
            .await
            .expect_err("missing runtime file must fail install");
        assert!(format!("{error:#}").contains("not loadable"), "{error:#}");
        Ok(())
    }

    #[tokio::test]
    async fn install_executes_no_package_code() -> Result<()> {
        let agent = TempDir::new()?;
        let source = TempDir::new()?;
        fs::write(
            source.path().join("pi-extension.json"),
            serde_json::to_vec_pretty(&json!({
                "schemaVersion": 1,
                "id": "payload",
                "version": "1.0.0",
                "runtime": "process",
                "executable": "payload.sh",
                "capabilities": [],
            }))?,
        )?;
        // If install ever executed package code, this script would create a
        // marker next to itself and modify a system file.
        fs::write(
            source.path().join("payload.sh"),
            "#!/bin/sh\necho executed > marker.txt\necho should-not-run > \"$(dirname \"$0\")/EXECUTED\"\n",
        )?;

        let marketplace = PluginMarketplace::new(agent.path());
        let trust_store = TrustStore::new(agent.path());
        let installed = marketplace.install(source.path().to_str().unwrap(), &trust_store).await?;
        assert_eq!(installed.name, "payload");

        let target = agent.path().join("extensions/payload");
        assert!(!target.join("marker.txt").exists(), "install must not run payload.sh");
        assert!(!target.join("EXECUTED").exists(), "install must not run payload.sh");
        // The payload itself is only validated for existence/containment.
        assert!(target.join("payload.sh").is_file());
        Ok(())
    }

    #[tokio::test]
    async fn remove_cleans_up_and_clears_trust() -> Result<()> {
        let agent = TempDir::new()?;
        let source = TempDir::new()?;
        write_quickjs_plugin(source.path(), "gone", "1.0.0")?;
        let marketplace = PluginMarketplace::new(agent.path());
        let trust_store = TrustStore::new(agent.path());
        marketplace.install(source.path().to_str().unwrap(), &trust_store).await?;
        let target = agent.path().join("extensions/gone");
        assert!(target.is_dir());

        marketplace.remove("gone", &trust_store)?;
        assert!(!target.exists(), "plugin directory must be removed");
        assert!(marketplace.list(&trust_store)?.0.is_empty());
        assert!(
            resources_for(agent.path(), &trust_store).is_empty(),
            "removed plugin must not be loadable"
        );
        // Trust decision was cleared.
        let resolution = trust_store.resolve(&target)?;
        assert_eq!(resolution.decision, TrustDecision::Ask);
        Ok(())
    }

    #[tokio::test]
    async fn update_uses_local_index_swaps_version_and_keeps_trust() -> Result<()> {
        let agent = TempDir::new()?;
        let source = TempDir::new()?;
        write_quickjs_plugin(source.path(), "demo", "1.0.0")?;
        let marketplace = PluginMarketplace::new(agent.path());
        let trust_store = TrustStore::new(agent.path());
        marketplace.install(source.path().to_str().unwrap(), &trust_store).await?;

        let next = TempDir::new()?;
        write_quickjs_plugin(next.path(), "demo", "2.0.0")?;
        let index_dir = TempDir::new()?;
        let index_path = index_dir.path().join("index.json");
        fs::write(
            &index_path,
            serde_json::to_vec_pretty(&[json!({
                "name": "demo",
                "repo": next.path().to_str().unwrap(),
                "version": "2.0.0",
                "description": "next",
            })])?,
        )?;

        let index = fetch_index(index_path.to_str().unwrap()).await?;
        let updated = marketplace.update("demo", &index, &trust_store).await?;
        assert_eq!(updated.version, "2.0.0");
        assert!(updated.trusted, "update must preserve trust");

        let (plugins, _) = marketplace.list(&trust_store)?;
        assert_eq!(plugins[0].version, "2.0.0");
        assert!(plugins[0].trusted);
        assert!(loadable_spec(&agent.path().join("extensions/demo")).is_ok());

        // Second update to the same version is a no-op error, not a reinstall.
        let error = marketplace
            .update("demo", &index, &trust_store)
            .await
            .expect_err("already newest");
        assert!(format!("{error:#}").contains("newest"), "{error:#}");

        // Updating an installed plugin that is missing from the index is
        // actionable.
        let loner = TempDir::new()?;
        write_quickjs_plugin(loner.path(), "loner", "1.0.0")?;
        marketplace.install(loner.path().to_str().unwrap(), &trust_store).await?;
        let error = marketplace
            .update("loner", &index, &trust_store)
            .await
            .expect_err("not in index");
        assert!(format!("{error:#}").contains("not listed"), "{error:#}");
        Ok(())
    }

    #[tokio::test]
    async fn list_updates_reports_available_versions() -> Result<()> {
        let agent = TempDir::new()?;
        let source = TempDir::new()?;
        write_quickjs_plugin(source.path(), "demo", "1.0.0")?;
        let marketplace = PluginMarketplace::new(agent.path());
        let trust_store = TrustStore::new(agent.path());
        marketplace.install(source.path().to_str().unwrap(), &trust_store).await?;

        let index_dir = TempDir::new()?;
        let index_path = index_dir.path().join("index.json");
        fs::write(
            &index_path,
            serde_json::to_vec_pretty(&[
                json!({ "name": "demo", "repo": "owner/demo", "version": "1.0.10" }),
                json!({ "name": "other", "repo": "owner/other", "version": "9.0.0" }),
            ])?,
        )?;
        let index = fetch_index(index_path.to_str().unwrap()).await?;
        let updates = marketplace.available_updates(&index)?;
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].name, "demo");
        assert_eq!(updates[0].current, "1.0.0");
        assert_eq!(updates[0].available, "1.0.10");
        Ok(())
    }

    #[test]
    fn marketplace_extension_resources_gates_loading_on_trust() -> Result<()> {
        let agent = TempDir::new()?;
        let source = TempDir::new()?;
        write_quickjs_plugin(source.path(), "gated", "1.0.0")?;
        let root = agent.path().join("extensions");
        fs::create_dir_all(&root)?;
        // Copy the plugin in manually (simulating a dropped-in package) and
        // verify it is listed but not loadable without a Trusted decision.
        let target = root.join("gated");
        fs::create_dir_all(&target)?;
        fs::copy(source.path().join("pi-extension.json"), target.join("pi-extension.json"))?;
        fs::copy(source.path().join("index.mjs"), target.join("index.mjs"))?;

        let trust_store = TrustStore::new(agent.path());
        let marketplace = PluginMarketplace::new(agent.path());
        let (plugins, problems) = marketplace.list(&trust_store)?;
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(plugins.len(), 1);
        assert!(!plugins[0].trusted, "dropped-in plugin starts untrusted");
        assert!(
            resources_for(agent.path(), &trust_store).is_empty(),
            "untrusted plugin must not be loadable"
        );

        // Storing a Trusted decision at the plugin path makes it loadable.
        trust_store.set(&target, TrustDecision::Trusted)?;
        assert!(plugins_are_trusted(&marketplace, &trust_store));
        let resources = resources_for(agent.path(), &trust_store);
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].path, target);

        // A stored Untrusted decision blocks loading again.
        trust_store.set(&target, TrustDecision::Untrusted)?;
        assert!(resources_for(agent.path(), &trust_store).is_empty());
        Ok(())
    }

    fn plugins_are_trusted(marketplace: &PluginMarketplace, trust_store: &TrustStore) -> bool {
        marketplace
            .list(trust_store)
            .expect("list")
            .0
            .iter()
            .all(|plugin| plugin.trusted)
    }

    #[tokio::test]
    async fn installed_plugin_appears_in_resource_manager_snapshot() -> Result<()> {
        let agent = TempDir::new()?;
        let cwd = TempDir::new()?;
        let source = TempDir::new()?;
        write_quickjs_plugin(source.path(), "rm-demo", "1.0.0")?;

        let marketplace = PluginMarketplace::new(agent.path());
        let trust_store = TrustStore::new(agent.path());
        marketplace.install(source.path().to_str().unwrap(), &trust_store).await?;

        let mut options = ResourceManagerOptions::new(cwd.path());
        options.agent_dir = agent.path().to_path_buf();
        options.project_trust_override = Some(true);
        options.headless = true;
        let resources = crate::ResourceManager::new(options)?;
        let specs = resources.extension_specs(&ExtensionPermissionSet::allow_all())?;
        assert!(
            specs.iter().any(|spec| spec.id == "rm-demo"),
            "installed plugin must be loadable via the resource snapshot: {specs:#?}"
        );
        Ok(())
    }

    /// Build a single well-formed ustar archive block sequence: one entry
    /// (`name`, `contents`, `typeflag`) followed by the end-of-archive block.
    /// Crafted by hand so the rejection of unsafe paths is independent of the
    /// `tar` builder's own path checks.
    fn raw_tar_entry(name: &str, contents: &[u8], typeflag: u8) -> Vec<u8> {
        fn header(name: &str, size: u64, typeflag: u8) -> Vec<u8> {
            let mut header = vec![b' '; 512];
            let name_bytes = name.as_bytes();
            header[..name_bytes.len()].copy_from_slice(name_bytes);
            // ustar names are NUL-terminated when shorter than the field.
            header[name_bytes.len()] = 0;
            let write_octal = |buf: &mut [u8], offset: usize, value: u64, width: usize| {
                let field = format!("{value:0width$o}", width = width - 1);
                let bytes = field.as_bytes();
                buf[offset..offset + bytes.len()].copy_from_slice(bytes);
            };
            write_octal(&mut header, 100, 0o644, 8); // mode
            write_octal(&mut header, 108, 0, 8); // uid
            write_octal(&mut header, 116, 0, 8); // gid
            write_octal(&mut header, 124, size, 12); // size
            write_octal(&mut header, 136, 0, 12); // mtime
            header[156] = typeflag;
            header[257..263].copy_from_slice(b"ustar\0");
            header[263..265].copy_from_slice(b"00");
            let sum: u32 = header.iter().map(|&byte| u32::from(byte)).sum();
            let checksum = format!("{sum:06o}\0 ");
            header[148..156].copy_from_slice(checksum.as_bytes());
            header
        }
        let mut archive = Vec::new();
        archive.extend_from_slice(&header(name, contents.len() as u64, typeflag));
        let mut block = contents.to_vec();
        block.resize(512, 0);
        archive.extend_from_slice(&block);
        archive.extend_from_slice(&[0u8; 512]); // end-of-archive marker
        archive
    }

    #[test]
    fn archive_traversal_entries_are_rejected() -> Result<()> {
        let target = TempDir::new()?;
        let error = extract_tar_from_bytes(&raw_tar_entry("../escape.txt", b"evil\n", b'0'), target.path())
            .expect_err("traversal entry must be rejected");
        assert!(format!("{error:#}").contains("unsafe path"), "{error:#}");
        assert!(!target.path().join("escape.txt").exists());
        assert!(!target
            .path()
            .parent()
            .unwrap()
            .join("escape.txt")
            .exists());
        Ok(())
    }

    #[test]
    fn archive_absolute_and_symlink_entries_are_rejected() -> Result<()> {
        // Absolute entries: the tar crate normalizes a leading slash, so an
        // absolute path in the archive lands inside the staging directory —
        // it must never escape it (defense in depth: our own is_absolute
        // check would also reject it if the crate ever stopped normalizing).
        let target = TempDir::new()?;
        match extract_tar_from_bytes(&raw_tar_entry("/etc/evil", b"evil\n", b'0'), target.path()) {
            Ok(()) => {
                assert!(
                    target.path().join("etc/evil").is_file(),
                    "normalized absolute entry must land inside staging"
                );
            }
            Err(error) => {
                assert!(format!("{error:#}").contains("unsafe path"), "{error:#}");
            }
        }
        assert!(
            !target.path().parent().unwrap().join("etc").exists(),
            "absolute entry must never escape the staging directory"
        );

        let symlink = TempDir::new()?;
        let error = extract_tar_from_bytes(&raw_tar_entry("pkg/evil", b"target", b'2'), symlink.path())
            .expect_err("symlink entry must be rejected");
        assert!(format!("{error:#}").contains("not a regular file"), "{error:#}");
        Ok(())
    }

    #[test]
    fn compare_versions_is_numeric_then_lexical() {
        use std::cmp::Ordering::{Equal, Greater, Less};
        let cases: &[(&str, &str, Ordering)] = &[
            ("1.0.0", "1.0.0", Equal),
            ("1.0.0", "1.0.1", Less),
            ("1.0.9", "1.0.10", Less),
            ("1.0.10", "1.0.9", Greater),
            ("1.1.0", "1.0.99", Greater),
            ("2.0.0", "1.9.9", Greater),
            ("1.0", "1.0.1", Less),
            ("0.0.0", "0.0.1", Less),
            ("1.0.0", "1.0.0-alpha", Less),
            ("1.0.0-alpha", "1.0.0-beta", Less),
            ("v1.0.0", "v1.0.0", Equal),
            ("1.0.0", "v1.0.0", Less),
        ];
        for (left, right, expected) in cases {
            assert_eq!(
                compare_versions(left, right),
                *expected,
                "{left} vs {right}"
            );
        }
    }

    #[test]
    fn index_parse_validates_entries_and_rejects_duplicates() -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&[json!({
            "name": "demo",
            "repo": "owner/demo",
            "version": "1.0.0",
            "description": "a demo",
        })])?;
        let index = MarketplaceIndex::parse(&bytes)?;
        assert_eq!(index.entries().len(), 1);
        assert_eq!(index.entry("demo").expect("entry").description.as_deref(), Some("a demo"));
        assert!(index.entry("missing").is_none());

        let duplicates = serde_json::to_vec_pretty(&[
            json!({ "name": "demo", "repo": "a/demo", "version": "1.0.0" }),
            json!({ "name": "demo", "repo": "b/demo", "version": "1.0.1" }),
        ])?;
        let error = MarketplaceIndex::parse(&duplicates).expect_err("duplicates rejected");
        assert!(format!("{error:#}").contains("twice"), "{error:#}");

        let empty_repo = serde_json::to_vec_pretty(&[json!({ "name": "demo", "repo": "", "version": "1.0.0" })])?;
        let error = MarketplaceIndex::parse(&empty_repo).expect_err("empty repo rejected");
        assert!(format!("{error:#}").contains("empty repo"), "{error:#}");

        let unknown_field = serde_json::to_vec_pretty(&[json!({
            "name": "demo", "repo": "a/demo", "version": "1.0.0", "extra": 1
        })])?;
        assert!(
            MarketplaceIndex::parse(&unknown_field).is_err(),
            "deny_unknown_fields must reject unknown index entry fields"
        );
        Ok(())
    }

    #[test]
    fn github_reference_parses_only_owner_repo_shapes() {
        assert_eq!(
            parse_github_reference("owner/repo"),
            Some(("owner".to_owned(), "repo".to_owned()))
        );
        assert_eq!(
            parse_github_reference("owner/repo.git"),
            Some(("owner".to_owned(), "repo".to_owned()))
        );
        assert_eq!(parse_github_reference("a/b/c"), None, "too many segments");
        assert_eq!(parse_github_reference("/repo"), None);
        assert_eq!(parse_github_reference("owner/"), None);
        assert_eq!(parse_github_reference("https://x/y"), None);
        assert_eq!(parse_github_reference("../relative/path"), None);
        assert_eq!(parse_github_reference(".hidden/repo"), None);
        assert_eq!(parse_github_reference("owner/repo!"), None);
    }

    #[test]
    fn version_field_is_optional_and_validated() -> Result<()> {
        let manifest: ProcessExtensionManifest = serde_json::from_value(json!({
            "schemaVersion": 1,
            "id": "with-version",
            "version": "1.2.3",
            "runtime": "quickjs",
            "entry": "index.mjs",
            "capabilities": [],
        }))?;
        assert_eq!(manifest.version.as_deref(), Some("1.2.3"));
        // Serialization keeps the version.
        let encoded = serde_json::to_value(&manifest)?;
        assert_eq!(encoded["version"], "1.2.3");

        let legacy: ProcessExtensionManifest = serde_json::from_value(json!({
            "schemaVersion": 1,
            "id": "legacy",
            "runtime": "quickjs",
            "entry": "index.mjs",
            "capabilities": [],
        }))?;
        assert_eq!(legacy.version, None);

        // An oversized version fails schema validation through the public
        // loadability pipeline (manifest.validate runs inside it).
        let oversized = TempDir::new()?;
        fs::write(
            oversized.path().join("pi-extension.json"),
            serde_json::to_vec_pretty(&json!({
                "schemaVersion": 1,
                "id": "big",
                "version": "x".repeat(200),
                "runtime": "quickjs",
                "entry": "index.mjs",
                "capabilities": [],
            }))?,
        )?;
        fs::write(oversized.path().join("index.mjs"), "")?;
        let error = extension_spec_from_package_resource(&PackageResourceSpec {
            kind: PackageResourceKind::Extension,
            path: oversized.path().to_path_buf(),
            package_id: "test".to_owned(),
            scope: PackageScope::Global,
            trusted: true,
        })
        .expect_err("oversized version must fail validation");
        assert!(format!("{error:#}").contains("version"), "{error:#}");
        Ok(())
    }

    #[test]
    fn plugin_name_validation_rejects_traversal() {
        let long = "x".repeat(200);
        assert!(validate_plugin_name("demo").is_ok());
        assert!(validate_plugin_name("demo.ext-2").is_ok());
        for bad in ["", ".", "..", "../x", "a/b", "a\\b", "a b", "a\x00b", long.as_str()] {
            assert!(validate_plugin_name(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[tokio::test]
    async fn list_reports_invalid_installed_manifests_as_problems() -> Result<()> {
        let agent = TempDir::new()?;
        let root = agent.path().join("extensions");
        fs::create_dir_all(root.join("corrupt"))?;
        fs::write(
            root.join("corrupt").join("pi-extension.json"),
            r#"{"schemaVersion":1,"id":{bad json"#,
        )?;
        let source = TempDir::new()?;
        write_quickjs_plugin(source.path(), "fine", "1.0.0")?;
        let marketplace = PluginMarketplace::new(agent.path());
        let trust_store = TrustStore::new(agent.path());
        // The valid plugin installs normally; listing still surfaces the
        // corrupt directory as a problem instead of failing.
        marketplace.install(source.path().to_str().unwrap(), &trust_store).await?;
        let (plugins, problems) = marketplace.list(&trust_store)?;
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "fine");
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("corrupt"), "{problems:?}");
        Ok(())
    }

    #[test]
    fn version_parts_split_on_separators() {
        assert_eq!(version_parts("1.0.0"), vec!["1", "0", "0"]);
        assert_eq!(version_parts("1.0.0-rc.1"), vec!["1", "0", "0", "rc", "1"]);
        assert_eq!(version_parts("v1"), vec!["v1"]);
    }

    #[test]
    fn npm_reference_parses_names_scopes_and_pinned_versions() {
        assert_eq!(
            parse_npm_reference("npm:pi-side-chat"),
            Some(NpmReference {
                name: "pi-side-chat".to_owned(),
                version: None,
            })
        );
        assert_eq!(
            parse_npm_reference("npm:pi-side-chat@1.2.3"),
            Some(NpmReference {
                name: "pi-side-chat".to_owned(),
                version: Some("1.2.3".to_owned()),
            })
        );
        assert_eq!(
            parse_npm_reference("npm:@scope/pkg"),
            Some(NpmReference {
                name: "@scope/pkg".to_owned(),
                version: None,
            })
        );
        assert_eq!(
            parse_npm_reference("npm:@scope/pkg@2.0.0"),
            Some(NpmReference {
                name: "@scope/pkg".to_owned(),
                version: Some("2.0.0".to_owned()),
            })
        );
        // Not npm references.
        assert_eq!(parse_npm_reference("pi-side-chat"), None);
        assert_eq!(parse_npm_reference("owner/repo"), None);
        assert_eq!(parse_npm_reference("npm:"), None);
        assert_eq!(parse_npm_reference("npm:@scope/"), None);
        assert_eq!(parse_npm_reference("npm:../escape"), None);
        assert_eq!(parse_npm_reference("npm:UPPER"), None);
        assert_eq!(parse_npm_reference("npm:@scope/PKG"), None);
        assert_eq!(parse_npm_reference("https://registry.npmjs.org/x"), None);
    }

    #[tokio::test]
    async fn install_npm_package_resolves_latest_registry_tarball() -> Result<()> {
        let _guard = NPM_REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tarball = side_chat_tarball()?;
        let integrity = npm_integrity(&tarball);
        let base = spawn_registry_server(move |port| {
            vec![
                (
                    "/pi-side-chat".to_owned(),
                    RouteResponse::Ok(
                        serde_json::to_vec(&json!({
                            "name": "pi-side-chat",
                            "version": "1.0.0",
                            "dist": {
                                "tarball": format!("http://127.0.0.1:{port}/pi-side-chat-1.0.0.tgz"),
                                "integrity": integrity,
                            },
                        }))
                        .expect("metadata json"),
                    ),
                ),
                (
                    "/pi-side-chat-1.0.0.tgz".to_owned(),
                    RouteResponse::Ok(tarball),
                ),
            ]
        })
        .await;
        set_npm_registry_base_for_tests(base);

        let agent = TempDir::new()?;
        let marketplace = PluginMarketplace::new(agent.path());
        let trust_store = TrustStore::new(agent.path());
        let installed = marketplace.install("npm:pi-side-chat", &trust_store).await?;
        assert_eq!(installed.name, "side-chat");
        assert_eq!(installed.version, "1.0.0");
        assert!(installed.trusted);

        // The `package/` top directory npm tarballs use is stripped; the
        // manifest and its entry point land at the installed extension root
        // and the runtime path resolves relative to it.
        let target = agent.path().join("extensions/side-chat");
        assert!(target.join("pi-extension.json").is_file());
        assert!(target.join("dist/index.mjs").is_file());
        assert!(!target.join("package").exists(), "npm top directory must be stripped");
        assert!(loadable_spec(&target).is_ok());
        assert_eq!(resources_for(agent.path(), &trust_store).len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn install_npm_package_pinned_version_requests_pinned_metadata() -> Result<()> {
        let _guard = NPM_REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tarball = side_chat_tarball()?;
        let integrity = npm_integrity(&tarball);
        let base = spawn_registry_server(move |port| {
            vec![
                // The pinned route (not the bare-name route) must be the one
                // served: a bare-name request would 404 and fail the install.
                (
                    "/pi-side-chat/1.2.3".to_owned(),
                    RouteResponse::Ok(
                        serde_json::to_vec(&json!({
                            "name": "pi-side-chat",
                            "version": "1.2.3",
                            "dist": {
                                "tarball": format!("http://127.0.0.1:{port}/pi-side-chat-1.2.3.tgz"),
                                "integrity": integrity,
                            },
                        }))
                        .expect("metadata json"),
                    ),
                ),
                (
                    "/pi-side-chat-1.2.3.tgz".to_owned(),
                    RouteResponse::Ok(tarball),
                ),
            ]
        })
        .await;
        set_npm_registry_base_for_tests(base);

        let agent = TempDir::new()?;
        let marketplace = PluginMarketplace::new(agent.path());
        let trust_store = TrustStore::new(agent.path());
        let installed = marketplace
            .install("npm:pi-side-chat@1.2.3", &trust_store)
            .await?;
        assert_eq!(installed.name, "side-chat");
        assert!(installed.trusted);
        assert!(loadable_spec(&agent.path().join("extensions/side-chat")).is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn install_npm_package_with_wrong_integrity_is_rejected() -> Result<()> {
        let _guard = NPM_REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tarball = side_chat_tarball()?;
        // A valid sha512 digest that does not match the served tarball.
        let wrong = npm_integrity(b"not the real tarball");
        let base = spawn_registry_server(move |port| {
            vec![
                (
                    "/pi-side-chat".to_owned(),
                    RouteResponse::Ok(
                        serde_json::to_vec(&json!({
                            "name": "pi-side-chat",
                            "version": "1.0.0",
                            "dist": {
                                "tarball": format!("http://127.0.0.1:{port}/pi-side-chat-1.0.0.tgz"),
                                "integrity": wrong,
                            },
                        }))
                        .expect("metadata json"),
                    ),
                ),
                (
                    "/pi-side-chat-1.0.0.tgz".to_owned(),
                    RouteResponse::Ok(tarball),
                ),
            ]
        })
        .await;
        set_npm_registry_base_for_tests(base);

        let agent = TempDir::new()?;
        let marketplace = PluginMarketplace::new(agent.path());
        let trust_store = TrustStore::new(agent.path());
        let error = marketplace
            .install("npm:pi-side-chat", &trust_store)
            .await
            .expect_err("an integrity mismatch must fail install");
        let message = format!("{error:#}");
        assert!(
            message.contains("integrity") && message.contains("mismatch"),
            "integrity failure must be actionable: {message}"
        );
        assert!(marketplace.list(&trust_store)?.0.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn install_npm_missing_package_404_is_actionable() -> Result<()> {
        let _guard = NPM_REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let base = spawn_registry_server(|_port| {
            vec![(
                "/no-such-package".to_owned(),
                RouteResponse::Err(404),
            )]
        })
        .await;
        set_npm_registry_base_for_tests(base);

        let agent = TempDir::new()?;
        let marketplace = PluginMarketplace::new(agent.path());
        let trust_store = TrustStore::new(agent.path());
        let error = marketplace
            .install("npm:no-such-package", &trust_store)
            .await
            .expect_err("a 404 package must fail install");
        let message = format!("{error:#}");
        assert!(
            message.contains("no-such-package") && message.contains("404"),
            "404 failure must name the package and the status: {message}"
        );
        assert!(!agent.path().join("extensions/no-such-package").exists());
        assert!(marketplace.list(&trust_store)?.0.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn install_npm_package_without_manifest_is_rejected() -> Result<()> {
        let _guard = NPM_REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tarball = tarball_bytes(&[(
            "package/dist/index.mjs",
            b"export default function (pi) {}\n",
        )])?;
        let integrity = npm_integrity(&tarball);
        let base = spawn_registry_server(move |port| {
            vec![
                (
                    "/no-manifest".to_owned(),
                    RouteResponse::Ok(
                        serde_json::to_vec(&json!({
                            "name": "no-manifest",
                            "version": "1.0.0",
                            "dist": {
                                "tarball": format!("http://127.0.0.1:{port}/no-manifest-1.0.0.tgz"),
                                "integrity": integrity,
                            },
                        }))
                        .expect("metadata json"),
                    ),
                ),
                (
                    "/no-manifest-1.0.0.tgz".to_owned(),
                    RouteResponse::Ok(tarball),
                ),
            ]
        })
        .await;
        set_npm_registry_base_for_tests(base);

        let agent = TempDir::new()?;
        let marketplace = PluginMarketplace::new(agent.path());
        let trust_store = TrustStore::new(agent.path());
        let error = marketplace
            .install("npm:no-manifest", &trust_store)
            .await
            .expect_err("a tarball without a manifest must fail install");
        assert!(
            format!("{error:#}").contains("pi-extension.json"),
            "missing manifest must be actionable: {error:#}"
        );
        assert!(marketplace.list(&trust_store)?.0.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn install_npm_package_metadata_without_tarball_is_rejected() -> Result<()> {
        let _guard = NPM_REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let base = spawn_registry_server(|_port| {
            vec![(
                "/metadata-only".to_owned(),
                RouteResponse::Ok(
                    serde_json::to_vec(&json!({
                        "name": "metadata-only",
                        "version": "1.0.0",
                    }))
                    .expect("metadata json"),
                ),
            )]
        })
        .await;
        set_npm_registry_base_for_tests(base);

        let agent = TempDir::new()?;
        let marketplace = PluginMarketplace::new(agent.path());
        let trust_store = TrustStore::new(agent.path());
        let error = marketplace
            .install("npm:metadata-only", &trust_store)
            .await
            .expect_err("metadata without dist.tarball must fail install");
        assert!(
            format!("{error:#}").contains("dist"),
            "missing dist must be actionable: {error:#}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn install_npm_package_without_dist_integrity_is_rejected() -> Result<()> {
        let _guard = NPM_REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tarball = side_chat_tarball()?;
        // Valid metadata + tarball, but NO `dist.integrity` (a mirror that
        // strips it, or one serving only the legacy `dist.shasum` sha1).
        let base = spawn_registry_server(move |port| {
            vec![(
                "/no-integrity".to_owned(),
                RouteResponse::Ok(
                    serde_json::to_vec(&json!({
                        "name": "no-integrity",
                        "version": "1.0.0",
                        "dist": {
                            "tarball": format!("http://127.0.0.1:{port}/no-integrity-1.0.0.tgz"),
                        },
                    }))
                    .expect("metadata json"),
                ),
            )]
        })
        .await;
        set_npm_registry_base_for_tests(base);

        let agent = TempDir::new()?;
        let marketplace = PluginMarketplace::new(agent.path());
        let trust_store = TrustStore::new(agent.path());
        let error = marketplace
            .install("npm:no-integrity", &trust_store)
            .await
            .expect_err("metadata without dist.integrity must fail closed");
        let message = format!("{error:#}");
        assert!(
            message.contains("no-integrity") && message.contains("dist.integrity"),
            "missing integrity must be actionable and name the package: {message}"
        );
        assert!(
            message.contains("refusing to install unauthenticated content"),
            "missing integrity must refuse install explicitly: {message}"
        );
        assert!(
            marketplace.list(&trust_store)?.0.is_empty(),
            "nothing may be installed without content authentication"
        );
        Ok(())
    }

    #[tokio::test]
    async fn install_from_git_url_clones_validates_and_trusts() -> Result<()> {
        if !git_available() {
            return Ok(()); // git is not on PATH — skip the fixture.
        }
        let agent = TempDir::new()?;
        let repo = TempDir::new()?;
        // A `.git`-suffixed repo directory so the `file://` URL matches the
        // required `owner/repo.git` shape.
        let repo_dir = repo.path().join("plugin-repo.git");
        fs::create_dir_all(&repo_dir)?;
        write_git_plugin_repo(&repo_dir, "gitdemo", "1.0.0")?;
        let url = format!("file://{}", repo_dir.display());

        let marketplace = PluginMarketplace::new(agent.path());
        let trust_store = TrustStore::new(agent.path());
        let installed = marketplace.install(&url, &trust_store).await?;
        assert_eq!(installed.name, "gitdemo");

        let target = agent.path().join("extensions/gitdemo");
        assert!(target.join("pi-extension.json").is_file(), "manifest must be cloned in");
        assert!(target.join("index.mjs").is_file(), "entry point must be cloned in");
        assert!(
            !target.join(".git").exists(),
            "cloned .git metadata must not ship in the installed plugin"
        );
        // Same trust gating as every other source: install records Trusted.
        let (plugins, problems) = marketplace.list(&trust_store)?;
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(plugins.len(), 1);
        assert!(plugins[0].trusted, "git-installed plugin must be trusted");
        assert!(
            loadable_spec(&target).is_ok(),
            "git-installed plugin must be loadable through the extension pipeline"
        );
        assert!(
            resources_for(agent.path(), &trust_store)
                .iter()
                .any(|spec| spec.path == target),
            "git-installed plugin must be picked up by marketplace_extension_resources"
        );
        Ok(())
    }

    #[tokio::test]
    async fn install_from_git_url_without_manifest_fails_actionably() -> Result<()> {
        if !git_available() {
            return Ok(()); // git is not on PATH — skip the fixture.
        }
        let agent = TempDir::new()?;
        let repo = TempDir::new()?;
        let repo_dir = repo.path().join("no-manifest-repo");
        fs::create_dir_all(&repo_dir)?;
        fs::write(repo_dir.join("README.md"), "this repo has no plugin manifest\n")?;
        init_git_repo(&repo_dir)?;
        // The `git+` prefix exercises the bare (`.git`-less) repo path.
        let url = format!("git+file://{}", repo_dir.display());

        let marketplace = PluginMarketplace::new(agent.path());
        let trust_store = TrustStore::new(agent.path());
        let error = marketplace
            .install(&url, &trust_store)
            .await
            .expect_err("clone without a manifest must fail install");
        let message = format!("{error:#}");
        assert!(
            message.contains("pi-extension.json"),
            "error must name the missing manifest: {message}"
        );
        assert!(
            marketplace.list(&trust_store)?.0.is_empty(),
            "nothing may be installed from a manifest-less repository"
        );
        assert!(
            fs::read_dir(agent.path())?.all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".plugin-staging")
            }),
            "failed git install must not leak staging directories"
        );
        Ok(())
    }

    #[tokio::test]
    async fn install_rejects_invalid_git_urls_without_spawning() -> Result<()> {
        let agent = TempDir::new()?;
        let marketplace = PluginMarketplace::new(agent.path());
        let trust_store = TrustStore::new(agent.path());
        let sources = [
            "git+https://host/owner/repo.git;rm -rf /", // shell metacharacters
            "git+ssh://user:pass@host/owner/repo.git`id`", // metachar + credentials
            "https://host/owner repo.git",              // whitespace
            "https://host/x y",                         // whitespace, not git-shaped
            "https://host/owner/repo",                  // no .git where required
            "git@host:owner/repo",                      // scp-like without .git
            "ssh://git@host/owner/repo.git?ref=main",   // query component
        ];
        for source in sources {
            let error = marketplace
                .install(source, &trust_store)
                .await
                .expect_err("invalid git URL must be rejected");
            let message = format!("{error:#}");
            assert!(
                message.contains("unsupported"),
                "{source} must be rejected with an actionable error: {message}"
            );
            assert!(
                !message.contains("git clone"),
                "{source} must be rejected before any clone is attempted: {message}"
            );
            assert!(
                !message.contains("user:pass"),
                "{source} must never echo URL credentials: {message}"
            );
        }
        assert!(
            marketplace.list(&trust_store)?.0.is_empty(),
            "rejected sources must install nothing"
        );
        Ok(())
    }
}
