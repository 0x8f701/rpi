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
//! - a GitHub-style `owner/repo` or `owner/repo@ref` reference, resolved to
//!   the codeload tarball (`…/tar.gz/<ref>` with `HEAD` when unpinned);
//! - a git URL — `http://host/owner/repo.git`, `https://host/owner/repo.git`,
//!   `git+https://host/owner/repo`, `git+ssh://git@host/owner/repo.git`,
//!   `ssh://git@host/owner/repo.git`, or `git@host:owner/repo.git` (scp-like)
//!   — cloned shallow (`git clone --depth 1`, argv-built so no shell string
//!   is ever involved, bounded by a hard timeout) into staging, stripped of
//!   its `.git` metadata, then validated and installed exactly like any other
//!   source;
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
//! - Loading is gated on an exact-canonical `trusted` marker from the trust
//!   store: a plugin whose canonical directory has anything other than its own
//!   `Trusted` decision is listed but not included in
//!   [`marketplace_extension_resources`]. `install` records the user's explicit
//!   consent for that plugin directory; `remove` clears only that decision.
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
use pi_sandbox::AbsolutePath;
use serde::{Deserialize, Serialize};

use crate::fs::{
    CapabilityAccess, CapabilityDirectory, CapabilityEntry, CapabilityFs, CapabilityRootLease,
    CapabilityRootPurpose, EntryKind, FileMode, FsError, RootCreation,
};
use crate::packages::{PackageResourceKind, PackageResourceSpec, PackageScope};
use crate::process::{GovernedProcessContext, GovernedProcessSpec};
use crate::resources::agent_dir_path;
use crate::trust::{TrustDecision, TrustStore};
use crate::{
    ExtensionManifestRuntime, PROCESS_EXTENSION_MANIFEST_FILE, ProcessExtensionManifest,
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
    std::sync::LazyLock::new(|| std::sync::Mutex::new(DEFAULT_NPM_REGISTRY_BASE.to_owned()));
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
/// Revision-bound HTTP executors for the four distinct plugin network classes.
///
/// Keeping them separate ensures marketplace indexes, generic archives, npm
/// metadata, and npm tarballs cannot widen one another's redirect authority.
#[derive(Clone)]
pub struct PluginHttpExecutors {
    marketplace: pi_ai::SharedHttpExecutor,
    archive: pi_ai::SharedHttpExecutor,
    npm_registry: pi_ai::SharedHttpExecutor,
    npm_tarball: pi_ai::SharedHttpExecutor,
}

impl PluginHttpExecutors {
    pub fn new(
        marketplace: pi_ai::SharedHttpExecutor,
        archive: pi_ai::SharedHttpExecutor,
        npm_registry: pi_ai::SharedHttpExecutor,
        npm_tarball: pi_ai::SharedHttpExecutor,
    ) -> Self {
        Self {
            marketplace,
            archive,
            npm_registry,
            npm_tarball,
        }
    }
}

impl std::fmt::Debug for PluginHttpExecutors {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PluginHttpExecutors([CAPTURED])")
    }
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
    /// Exact stored decision for the canonical plugin directory. Only trusted
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
                bail!(
                    "marketplace index lists plugin {name} twice",
                    name = entry.name
                );
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
/// local index file path (`file://` prefixes are accepted). Remote indexes
/// require a captured `PluginMarketplace` executor; there is no ambient HTTP
/// fallback.
async fn fetch_index_with_executor(
    cwd: &Path,
    source: &str,
    executor: Option<&pi_ai::SharedHttpExecutor>,
) -> Result<MarketplaceIndex> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        bail!("marketplace index source must not be empty");
    }
    let bytes = if let Some(url) = parse_http_url(trimmed) {
        let executor =
            executor.ok_or_else(|| anyhow!("plugin marketplace HTTP generation is unavailable"))?;
        fetch_bytes(executor, url.as_str(), MAX_MARKETPLACE_INDEX_BYTES)
            .await
            .with_context(|| format!("fetching marketplace index from {url}"))?
    } else {
        let (parent, name) = open_operator_source(cwd, trimmed)?;
        parent
            .read_bounded(Path::new(&name), MAX_MARKETPLACE_INDEX_BYTES)
            .with_context(|| format!("reading marketplace index {trimmed}"))?
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
#[derive(Clone)]
pub struct PluginMarketplace {
    agent_dir: PathBuf,
    /// The operator working directory relative local/file plugin sources and
    /// local marketplace index files are resolved against. Defaults to the
    /// process cwd at construction; the CLI overrides it with its `--cwd`
    /// (or the process cwd) so relative sources always resolve where the
    /// operator ran the command.
    cwd: PathBuf,
    extensions: CapabilityRootLease,
    process: Option<GovernedProcessContext>,
    http: Option<PluginHttpExecutors>,
}

impl std::fmt::Debug for PluginMarketplace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginMarketplace")
            .field("agent_dir", &self.agent_dir)
            .field("cwd", &self.cwd)
            .finish()
    }
}

impl PluginMarketplace {
    pub fn new(agent_dir: impl Into<PathBuf>) -> Result<Self> {
        let agent_dir = agent_dir.into();
        let cwd = std::env::current_dir().context("resolving plugin operator working directory")?;
        let capabilities =
            CapabilityFs::new().context("creating plugin filesystem capabilities")?;
        let absolute = AbsolutePath::parse(agent_dir.clone()).map_err(|_| FsError::InvalidPath)?;
        let agent = capabilities
            .open_root(
                CapabilityRootPurpose::AgentStore,
                absolute,
                CapabilityAccess::PrivateReadWrite,
                RootCreation::CreateDirectory,
            )
            .context("opening private agent store for plugins")?;
        let extensions = agent
            .lease_directory(
                Path::new("extensions"),
                CapabilityAccess::PrivateReadWrite,
                RootCreation::CreateDirectory,
            )
            .context("opening private plugin store")?;
        Ok(Self {
            agent_dir,
            cwd,
            extensions,
            process: None,
            http: None,
        })
    }

    #[must_use]
    pub fn with_process_context(mut self, process: GovernedProcessContext) -> Self {
        self.process = Some(process);
        self
    }

    /// Override the operator working directory that relative local/file
    /// plugin sources and local marketplace index files are resolved against.
    /// The CLI passes the `--cwd` it already computes for settings; tests use
    /// it to control resolution without touching the process cwd.
    #[must_use]
    pub fn with_operator_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = cwd.into();
        self
    }

    #[must_use]
    pub fn with_http_executors(mut self, http: PluginHttpExecutors) -> Self {
        self.http = Some(http);
        self
    }

    /// Fetch a local or remote marketplace index using this marketplace's
    /// captured `PluginMarketplace` HTTP authority.
    pub async fn fetch_index(&self, source: &str) -> Result<MarketplaceIndex> {
        fetch_index_with_executor(
            &self.cwd,
            source,
            self.http.as_ref().map(|http| &http.marketplace),
        )
        .await
    }

    pub fn global() -> Result<Self> {
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
        let staging_relative = unique_name("plugin-staging");
        self.extensions
            .create_dir_all(&staging_relative, FileMode::Private)
            .context("creating plugin staging directory")?;
        let staging = self.extensions.display_root().join(&staging_relative);
        let mut guard = RemovePathOnDrop::new(self.extensions.clone(), staging_relative.clone());
        stage_source(
            &self.extensions,
            self.process.as_ref(),
            self.http.as_ref(),
            &self.cwd,
            source,
            &staging_relative,
        )
        .await
        .with_context(|| format!("staging plugin source {}", redact_url_credentials(source)))?;
        let manifest = validate_package(&staging).with_context(|| {
            format!(
                "invalid plugin package from {}",
                redact_url_credentials(source)
            )
        })?;
        let name = manifest.id.clone();
        validate_plugin_name(&name)?;
        let target_relative = PathBuf::from(&name);
        if self.extensions.metadata(&target_relative).is_ok() {
            bail!("plugin {name} is already installed");
        }
        self.extensions
            .rename(&staging_relative, &target_relative)
            .with_context(|| format!("installing plugin {name}"))?;
        let target = self.extensions.display_root().join(&target_relative);
        if let Err(error) = trust_store
            .set_exact(&target, TrustDecision::Trusted)
            .with_context(|| format!("recording trust for plugin {name}"))
        {
            return match self.extensions.rename(&target_relative, &staging_relative) {
                Ok(()) => Err(error),
                Err(restore_error) => Err(error.context(format!(
                    "restoring staged plugin {name} after trust recording failed: {restore_error}"
                ))),
            };
        }
        guard.disarm();
        Ok(plugin_info(&target, &manifest, true))
    }

    /// Remove an installed plugin and clear its stored trust decision.
    pub fn remove(&self, name: &str, trust_store: &TrustStore) -> Result<()> {
        validate_plugin_name(name)?;
        let target_relative = PathBuf::from(name);
        let metadata = self
            .extensions
            .metadata(&target_relative)
            .map_err(|_| anyhow!("plugin {name} is not installed"))?;
        if metadata.identity.kind() != EntryKind::Directory {
            bail!("plugin {name} is not installed");
        }
        let target = self.extensions.display_root().join(&target_relative);
        let removing = unique_name("plugin-removing");
        self.extensions
            .rename(&target_relative, &removing)
            .with_context(|| format!("isolating plugin {name}"))?;
        let trust_cleanup = (|| {
            #[cfg(test)]
            remove_test_hook::before_cleanup()?;
            trust_store.clear_exact(&target)
        })()
        .with_context(|| format!("clearing trust for plugin {name}"));
        if let Err(error) = trust_cleanup {
            return match self.extensions.rename(&removing, &target_relative) {
                Ok(()) => Err(error),
                Err(restore_error) => Err(error.context(format!(
                    "restoring plugin {name} after trust cleanup failed: {restore_error}"
                ))),
            };
        }
        self.extensions
            .remove_tree(&removing)
            .with_context(|| format!("removing plugin {name}"))?;
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
        let target_relative = PathBuf::from(name);
        let target_directory = self
            .extensions
            .open_directory(&target_relative)
            .map_err(|_| anyhow!("plugin {name} is not installed"))?;
        let target = target_directory.display_path();
        let installed = read_plugin_manifest_from(target_directory)
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
        let staging_relative = unique_name("plugin-staging");
        self.extensions
            .create_dir_all(&staging_relative, FileMode::Private)
            .context("creating plugin staging directory")?;
        let staging = self.extensions.display_root().join(&staging_relative);
        let mut guard = RemovePathOnDrop::new(self.extensions.clone(), staging_relative.clone());
        stage_source(
            &self.extensions,
            self.process.as_ref(),
            self.http.as_ref(),
            &self.cwd,
            &entry.repo,
            &staging_relative,
        )
        .await
        .with_context(|| {
            format!(
                "staging plugin {name} from {}",
                redact_url_credentials(&entry.repo)
            )
        })?;
        let manifest = validate_package(&staging).with_context(|| {
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
        directory_swap(&self.extensions, &staging_relative, &target_relative)
            .with_context(|| format!("replacing plugin {name}"))?;
        guard.disarm();
        let trusted = self.plugin_trusted(trust_store, &target);
        Ok(plugin_info(&target, &manifest, trusted))
    }

    fn plugin_trusted(&self, trust_store: &TrustStore, path: &Path) -> bool {
        trust_store
            .resolve_exact(path)
            .is_ok_and(|decision| decision == TrustDecision::Trusted)
    }

    fn scan_installed(&self) -> Result<(Vec<PluginInfo>, Vec<String>)> {
        let mut plugins = Vec::new();
        let mut problems = Vec::new();
        let mut entries = self.extensions.read_dir(Path::new(""))?;
        while let Some(entry) = entries.next()? {
            let name = entry.name().to_os_string();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            let CapabilityEntry::Directory(directory) = entry.into_entry() else {
                continue;
            };
            let path = directory.display_path();
            match read_plugin_manifest_from(directory) {
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
/// The trust store gates loading by exact canonical plugin directory: anything
/// other than that directory's own stored `Trusted` decision is skipped (it
/// stays visible in `rpi plugin list` but is not loadable). Each included resource
/// is a `PackageScope::Global` extension root, so the normal
/// [`crate::extension_spec_from_package_resource`] pipeline re-validates the
/// manifest and runtime path before launch.
pub fn marketplace_extension_resources(
    agent_dir: &Path,
    trust_store: &TrustStore,
) -> Result<Vec<PackageResourceSpec>> {
    let marketplace = PluginMarketplace::new(agent_dir)?;
    let mut resources = Vec::new();
    let mut entries = marketplace.extensions.read_dir(Path::new(""))?;
    while let Some(entry) = entries.next()? {
        let name = entry.name().to_os_string();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let CapabilityEntry::Directory(directory) = entry.into_entry() else {
            continue;
        };
        let path = directory.display_path();
        if read_plugin_manifest_from(directory).is_err() {
            continue;
        }
        let trusted = trust_store.resolve_exact(&path)? == TrustDecision::Trusted;
        if !trusted {
            continue;
        }
        resources.push(PackageResourceSpec {
            kind: PackageResourceKind::Extension,
            path,
            managed_root: None,
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
        managed_root: None,
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
    let capabilities = CapabilityFs::new().context("creating plugin source capabilities")?;
    let absolute = AbsolutePath::parse(root.to_path_buf()).map_err(|_| FsError::InvalidPath)?;
    let lease = capabilities
        .open_root(
            CapabilityRootPurpose::OperatorImport,
            absolute,
            CapabilityAccess::ReadOnly,
            RootCreation::Existing,
        )
        .context("opening plugin package")?;
    read_plugin_manifest_from(lease.open_directory(Path::new(""))?)
}

fn read_plugin_manifest_from(root: CapabilityDirectory) -> Result<ProcessExtensionManifest> {
    let manifest_path = root.display_path().join(PROCESS_EXTENSION_MANIFEST_FILE);
    let mut entries = root.read_dir()?;
    let mut manifest_file = None;
    while let Some(entry) = entries.next()? {
        if entry.name() != PROCESS_EXTENSION_MANIFEST_FILE {
            continue;
        }
        manifest_file = match entry.into_entry() {
            CapabilityEntry::File(file) => Some(file),
            CapabilityEntry::Directory(_) => bail!(
                "plugin manifest path is not a regular file: {}",
                manifest_path.display()
            ),
        };
        break;
    }
    let file = manifest_file.ok_or_else(|| {
        anyhow!(
            "plugin package must contain a {PROCESS_EXTENSION_MANIFEST_FILE} manifest at its root: {}",
            manifest_path.display()
        )
    })?;
    let bytes = file
        .read_bounded(MAX_MANIFEST_BYTES)
        .with_context(|| format!("reading plugin manifest {}", manifest_path.display()))?;
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
        version: manifest
            .version
            .clone()
            .unwrap_or_else(|| "0.0.0".to_owned()),
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
    if name.chars().any(|character| {
        !(character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.'))
    }) {
        bail!("plugin name {name:?} contains unsupported characters");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Staging
// ---------------------------------------------------------------------------

/// Materialize `source` into the staging directory as a plugin package root.
///
/// Local directory and archive sources are resolved against `cwd` (the
/// operator working directory) and opened through capability authority, so
/// relative paths behave exactly like the operator typed them. Git-shaped
/// sources (`git+`, `ssh`, scp-like, and `file:` URLs whose final segment
/// ends in `.git`) are resolved by clone *before* the local-open attempt, so
/// a `file://.../repo.git` that exists on disk is still cloned (bare
/// repositories have no working tree to copy) instead of copied.
async fn stage_source(
    staging_store: &CapabilityRootLease,
    process: Option<&GovernedProcessContext>,
    http: Option<&PluginHttpExecutors>,
    cwd: &Path,
    source: &str,
    staging_relative: &Path,
) -> Result<()> {
    let staging = staging_store.display_root().join(staging_relative);
    let trimmed = source.trim();
    if trimmed.is_empty() {
        bail!("plugin source must not be empty");
    }
    let redacted = redact_url_credentials(trimmed);
    if let Some(clone_url) = parse_git_reference(trimmed) {
        return stage_git_source(
            staging_store,
            process.ok_or_else(|| anyhow!("plugin process generation is unavailable"))?,
            &clone_url,
            staging_relative,
        )
        .await;
    }
    if looks_like_git_url(trimmed) {
        bail!("unsupported plugin git source {redacted}");
    }
    if let Some(reference) = parse_npm_reference(trimmed) {
        return stage_npm_source(
            &reference,
            staging_store,
            staging_relative,
            http.ok_or_else(|| anyhow!("plugin npm HTTP generation is unavailable"))?,
        )
        .await;
    }
    if !trimmed.starts_with("file://") {
        if let Some(url) = parse_http_url(trimmed) {
            if !is_archive_path(Path::new(url.path())) {
                bail!("unsupported plugin archive URL {redacted}");
            }
            let executor = http
                .map(|http| &http.archive)
                .ok_or_else(|| anyhow!("plugin archive HTTP generation is unavailable"))?;
            let bytes = fetch_bytes(executor, url.as_str(), MAX_PACKAGE_BYTES).await?;
            return extract_tar_from_bytes(&bytes, staging_store, staging_relative)
                .with_context(|| format!("extracting plugin archive {url}"));
        }
        if trimmed.contains("://") {
            bail!("unsupported plugin source {redacted}");
        }
    }
    match open_operator_source(cwd, trimmed) {
        Ok((source, source_name)) => {
            match source.metadata(Path::new(&source_name))?.identity.kind() {
                EntryKind::Directory => {
                    return copy_tree(
                        source.open_directory(Path::new(&source_name))?,
                        staging_store,
                        staging_relative,
                    );
                }
                EntryKind::RegularFile if is_archive_path(Path::new(trimmed)) => {
                    let bytes = source.read_bounded(Path::new(&source_name), MAX_PACKAGE_BYTES)?;
                    return extract_tar_from_bytes(&bytes, staging_store, staging_relative)
                        .with_context(|| {
                            format!("extracting plugin archive {}", Path::new(trimmed).display())
                        });
                }
                EntryKind::RegularFile => bail!("unsupported plugin source {redacted}"),
            }
        }
        Err(error) if is_local_source_spelling(trimmed) => {
            return Err(error).with_context(|| format!("opening local plugin source {redacted}"));
        }
        Err(_) => {}
    }
    if let Some(reference) = parse_github_reference(trimmed) {
        let git_ref = reference.git_ref.as_deref().unwrap_or("HEAD");
        let url = format!(
            "https://codeload.github.com/{}/{}/tar.gz/{git_ref}",
            reference.owner, reference.repo
        );
        let executor = http
            .map(|http| &http.archive)
            .ok_or_else(|| anyhow!("plugin archive HTTP generation is unavailable"))?;
        let bytes = fetch_bytes(executor, &url, MAX_PACKAGE_BYTES).await?;
        return extract_tar_from_bytes(&bytes, staging_store, staging_relative)
            .with_context(|| format!("extracting GitHub tarball {url}"));
    }
    bail!("unsupported plugin source {redacted}")
}

fn is_local_source_spelling(value: &str) -> bool {
    value.starts_with("file://")
        || value.starts_with('.')
        || Path::new(value).is_absolute()
        || value.starts_with("plugins/")
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
    static CREDENTIALS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
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

/// Parse a GitHub-style `owner/repo` or `owner/repo@ref` reference (exactly
/// two path segments, both valid GitHub names). `@ref` pins the codeload
/// tarball; omitting it selects `HEAD`. Returns `None` for URLs, extra
/// segments, empty pins, or names that could never exist on GitHub.
fn parse_github_reference(value: &str) -> Option<GitHubReference> {
    if value.contains("://") || value.starts_with('.') {
        return None;
    }
    let (spec, git_ref) = match value.rsplit_once('@') {
        Some((spec, git_ref)) if !spec.is_empty() && !git_ref.is_empty() => {
            (spec, Some(git_ref))
        }
        Some(_) => return None,
        None => (value, None),
    };
    if git_ref.is_some_and(|git_ref| !valid_git_ref(git_ref)) {
        return None;
    }
    let mut parts = spec.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if parts.next().is_some() || owner.is_empty() || repo.is_empty() {
        return None;
    }
    let repo_name = repo.strip_suffix(".git").unwrap_or(repo);
    (valid_repo_name(owner) && valid_repo_name(repo_name)).then(|| GitHubReference {
        owner: owner.to_owned(),
        repo: repo_name.to_owned(),
        git_ref: git_ref.map(str::to_owned),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitHubReference {
    owner: String,
    repo: String,
    git_ref: Option<String>,
}

/// GitHub codeload ref: tag, branch, or commit-ish. Rejects traversal and
/// URL-breaking characters.
fn valid_git_ref(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && !value.starts_with('.')
        && !value.contains("..")
        && !value.contains("//")
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | '/'))
}

/// Whether `part` could be a GitHub owner or repository name: ASCII
/// alphanumerics, `_`, `-`, `.`, at most 100 bytes, and no leading/trailing
/// dot.
fn valid_repo_name(part: &str) -> bool {
    !part.is_empty()
        && part.len() <= 100
        && !part.starts_with('.')
        && !part.ends_with('.')
        && part.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        })
}

/// A git URL plugin source, normalized to a clone URL. Accepts:
/// - `http://host/owner/repo.git` / `https://host/owner/repo.git`;
/// - `git+https://host/owner/repo[.git]` (the `.git` suffix is optional);
/// - `git+http://host/owner/repo[.git]`;
/// - `git+ssh://git@host/owner/repo.git`, `ssh://git@host/owner/repo.git`;
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
        '"', '\'', '\\', '`', '$', '&', '|', ';', '<', '>', '(', ')', '*', '?', '[', ']', '{', '}',
        '!', '#',
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
    if !matches!(url.scheme(), "https" | "http" | "ssh" | "file") {
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
        return None; // http/https/ssh repos are exactly host/owner/repo
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
            && part.chars().all(|character| {
                character.is_ascii_lowercase()
                    || character.is_ascii_digit()
                    || matches!(character, '-' | '_' | '.')
            })
    })
}

/// Clone a git repository into `staging` (`git clone --depth 1 <url>
/// <staging>`, argv-built so no shell string is ever constructed) with a
/// hard timeout, then drop the cloned `.git` metadata so credentials that
/// may be embedded in the source URL never persist in the installed package.
/// Failures are actionable — missing git binary, repository not found, auth
/// failure — and never echo URL credentials.
async fn stage_git_source(
    staging_store: &CapabilityRootLease,
    process: &GovernedProcessContext,
    clone_url: &str,
    staging_relative: &Path,
) -> Result<()> {
    let staging = staging_store.display_root().join(staging_relative);
    let output = process
        .run(
            pi_sandbox::SpawnFamily::PluginHelper,
            GovernedProcessSpec {
                program: process.git_program()?,
                args: vec![
                    "-c".into(),
                    "color.ui=false".into(),
                    "clone".into(),
                    "--depth".into(),
                    "1".into(),
                    clone_url.into(),
                    staging.to_string_lossy().into_owned(),
                ],
                cwd: staging
                    .parent()
                    .ok_or_else(|| anyhow!("plugin staging path has no parent"))?
                    .to_path_buf(),
                environment: std::collections::BTreeMap::from([(
                    "GIT_TERMINAL_PROMPT".to_owned(),
                    Some("0".to_owned()),
                )]),
                timeout: std::time::Duration::from_secs(GIT_CLONE_TIMEOUT_SECONDS),
                stdout_bytes: 64 * 1024,
                stderr_bytes: 64 * 1024,
                #[cfg(unix)]
                mount_sources: Vec::new(),
                #[cfg(unix)]
                retained_executable: None,
                #[cfg(unix)]
                retained_working_directory: None,
            },
        )
        .await?;
    let url = redact_url_credentials(clone_url);
    if output.cancelled {
        bail!("git clone of {url} was cancelled");
    }
    if output.timed_out {
        bail!(
            "git clone timed out after {GIT_CLONE_TIMEOUT_SECONDS}s (clone URL: {url}); check the repository URL and network access"
        );
    }
    if output.stdout_capped || output.stderr_capped {
        bail!("git clone of {url} exceeded its output limit");
    }
    if output.status != Some(0) {
        let detail = redact_url_credentials(&String::from_utf8_lossy(&output.stderr));
        let detail = detail.trim();
        if detail.is_empty() {
            bail!("git clone of {url} failed with status {:?}", output.status);
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
    remove_git_metadata(staging_store, staging_relative)?;
    Ok(())
}

fn remove_git_metadata(store: &CapabilityRootLease, staging_relative: &Path) -> Result<()> {
    let git = staging_relative.join(".git");
    match store.metadata(&git) {
        Ok(metadata) if metadata.identity.kind() == EntryKind::Directory => store
            .remove_tree(&git)
            .context("removing plugin git metadata"),
        Ok(_) => store
            .remove_file(&git)
            .context("removing plugin git metadata"),
        Err(error) if fs_error_kind(&error) == Some(std::io::ErrorKind::NotFound) => Ok(()),
        Err(error) => Err(error).context("inspecting plugin git metadata"),
    }
}
/// Resolve `npm:<name>[@<version>]` through the npm registry and stage the
/// published tarball. The registry metadata must carry `dist.tarball` and a
/// `sha512` `dist.integrity`; the fetched tarball is verified against that
/// digest before extraction (and an https registry serving plain-http
/// tarballs is refused: the integrity check would otherwise authenticate
/// content fetched over an unauthenticated channel).
async fn stage_npm_source(
    reference: &NpmReference,
    staging_store: &CapabilityRootLease,
    staging_relative: &Path,
    http: &PluginHttpExecutors,
) -> Result<()> {
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
    let metadata = fetch_bytes(&http.npm_registry, &metadata_url, MAX_NPM_METADATA_BYTES)
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
    let document: serde_json::Value = serde_json::from_slice(&metadata).with_context(|| {
        format!(
            "parsing npm metadata for {} from {metadata_url}",
            reference.name
        )
    })?;
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
    let bytes = fetch_bytes(&http.npm_tarball, tarball_url.as_str(), MAX_PACKAGE_BYTES)
        .await
        .with_context(|| format!("downloading npm tarball {tarball_url}"))?;
    verify_npm_integrity(&bytes, &integrity).with_context(|| {
        format!(
            "npm package {} tarball failed integrity verification",
            reference.name
        )
    })?;
    extract_tar_from_bytes(&bytes, staging_store, staging_relative)
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

/// Recursively copy a plugin directory. Hidden entries and `node_modules` are skipped.
fn copy_tree(
    source: CapabilityDirectory,
    target: &CapabilityRootLease,
    target_relative: &Path,
) -> Result<()> {
    target.create_dir_all(target_relative, FileMode::Private)?;
    let mut copied = 0_u64;
    copy_opened_directory(source, target, target_relative, &mut copied)
}

fn copy_opened_directory(
    source: CapabilityDirectory,
    target: &CapabilityRootLease,
    target_relative: &Path,
    copied: &mut u64,
) -> Result<()> {
    let mut entries = source.read_dir()?;
    while let Some(entry) = entries.next()? {
        let name = entry.name().to_os_string();
        if name.to_string_lossy().starts_with('.') || name == "node_modules" {
            continue;
        }
        let destination = target_relative.join(&name);
        match entry.into_entry() {
            CapabilityEntry::Directory(directory) => {
                target.create_dir_all(&destination, FileMode::Private)?;
                copy_opened_directory(directory, target, &destination, copied)?;
            }
            CapabilityEntry::File(file) => {
                *copied = copied
                    .checked_add(file.metadata()?.len)
                    .ok_or_else(|| anyhow!("plugin package size overflow"))?;
                if *copied > MAX_PACKAGE_BYTES {
                    bail!("plugin package exceeds {MAX_PACKAGE_BYTES} bytes");
                }
                let bytes = file.read_bounded(MAX_PACKAGE_BYTES)?;
                let mut output = target.create_new(&destination, FileMode::Private)?;
                output.write_all(&bytes)?;
                output.finish()?;
            }
        }
    }
    Ok(())
}

struct PlannedEntry {
    path: PathBuf,
    is_dir: bool,
    data: Vec<u8>,
}

fn extract_tar_from_bytes(
    bytes: &[u8],
    target: &CapabilityRootLease,
    target_relative: &Path,
) -> Result<()> {
    if bytes.is_empty() {
        bail!("plugin archive is empty");
    }
    let reader: Box<dyn Read> = if bytes.starts_with(&[0x1f, 0x8b]) {
        Box::new(flate2::read::GzDecoder::new(std::io::Cursor::new(bytes)))
    } else {
        Box::new(std::io::Cursor::new(bytes))
    };
    let mut archive = tar::Archive::new(reader);
    let entries = archive
        .entries()
        .context("reading plugin archive entries")?;
    let mut planned = Vec::new();
    let mut total_bytes = 0_u64;
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
        total_bytes = total_bytes
            .checked_add(entry.size())
            .ok_or_else(|| anyhow!("plugin archive size overflow"))?;
        if total_bytes > MAX_PACKAGE_BYTES {
            bail!("plugin archive exceeds {MAX_PACKAGE_BYTES} bytes");
        }
        let mut data = Vec::with_capacity(usize::try_from(entry.size()).unwrap_or(0));
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
        if relative.as_os_str().is_empty() {
            continue;
        }
        let destination = target_relative.join(relative);
        if entry.is_dir {
            target.create_dir_all(&destination, FileMode::Private)?;
        } else {
            if let Some(parent) = destination.parent() {
                target.create_dir_all(parent, FileMode::Private)?;
            }
            let mut file = target.create_new(&destination, FileMode::Private)?;
            file.write_all(&entry.data)?;
            file.finish()?;
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

/// Bounded HTTP fetch through one already-classed, revision-bound executor.
async fn fetch_bytes(executor: &pi_ai::SharedHttpExecutor, url: &str, max: u64) -> Result<Vec<u8>> {
    let request = pi_ai::request_from_url(
        pi_ai::HttpMethod::Get,
        url,
        &http::HeaderMap::new(),
        Vec::new(),
        &pi_ai::StreamOptions {
            timeout_ms: Some(FETCH_TIMEOUT_SECONDS * 1_000),
            ..Default::default()
        },
        max,
    )?;
    let response = executor
        .execute(request)
        .await
        .with_context(|| format!("fetching {url}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("fetching {url} failed: HTTP {status}");
    }
    pi_ai::collect_response_body(response)
        .await
        .with_context(|| format!("reading response from {url}"))
}
#[cfg(test)]
mod remove_test_hook {
    use std::cell::Cell;

    use anyhow::{Result, bail};

    thread_local! {
        static FAIL_NEXT_CLEANUP: Cell<bool> = const { Cell::new(false) };
    }

    pub(super) fn fail_next_cleanup() {
        FAIL_NEXT_CLEANUP.set(true);
    }

    pub(super) fn before_cleanup() -> Result<()> {
        if FAIL_NEXT_CLEANUP.replace(false) {
            bail!("injected trust cleanup failure")
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Filesystem helpers
// ---------------------------------------------------------------------------

fn unique_name(label: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    PathBuf::from(format!(
        ".{label}-{}-{millis}-{counter}",
        std::process::id()
    ))
}

struct RemovePathOnDrop {
    store: CapabilityRootLease,
    relative: Option<PathBuf>,
}

impl RemovePathOnDrop {
    fn new(store: CapabilityRootLease, relative: PathBuf) -> Self {
        Self {
            store,
            relative: Some(relative),
        }
    }

    fn disarm(&mut self) {
        self.relative = None;
    }
}

impl Drop for RemovePathOnDrop {
    fn drop(&mut self) {
        if let Some(relative) = &self.relative {
            let _ = self.store.remove_tree(relative);
        }
    }
}

/// Atomically replace the live plugin at `target` with `staged`, preserving
/// the previous version in a hidden backup until the swap is complete.
///
/// The result is always truthful about the live state:
/// - a failure to move the live plugin aside leaves it untouched at `target`;
/// - a failure to activate `staged` restores the previous version to
///   `target`, or — if that restore also fails — reports exactly where the
///   previous version was preserved;
/// - a failure to delete the backup *after* activation does not fail the
///   swap: the new version is live at `target`, so the swap reports success
///   and warns about the leftover hidden backup instead.
fn directory_swap(store: &CapabilityRootLease, staged: &Path, target: &Path) -> Result<()> {
    let backup = unique_name("plugin-backup");
    store.rename(target, &backup).with_context(|| {
        format!(
            "isolating the installed plugin for replacement; the previous version remains in place at {}",
            store.display_root().join(target).display()
        )
    })?;
    if let Err(error) = store.rename(staged, target) {
        return match store.rename(&backup, target) {
            Ok(()) => Err(error)
                .context("installing the plugin replacement; the previous version was restored"),
            Err(restore_error) => Err(error).context(format!(
                "installing the plugin replacement; restoring the previous version also failed \
                 ({restore_error:#}); the previous version is preserved at {}",
                store.display_root().join(&backup).display()
            )),
        };
    }
    let cleanup = (|| -> Result<()> {
        #[cfg(test)]
        swap_test_hook::before_backup_cleanup()?;
        Ok(store.remove_tree(&backup)?)
    })();
    if let Err(cleanup_error) = cleanup {
        // The staged version is live at `target` — the swap succeeded. Deleting
        // the previous version's backup is best-effort cleanup, so surface the
        // failure as a warning and report the swap truthfully as successful.
        eprintln!(
            "plugin: replaced the plugin at {} but could not delete its previous-version \
             backup at {} ({cleanup_error:#}); the hidden backup is ignored by plugin listing \
             and can be removed manually",
            store.display_root().join(target).display(),
            store.display_root().join(&backup).display(),
        );
    }
    Ok(())
}

/// Test seam: inject a failure into [`directory_swap`]'s post-activation
/// backup cleanup so tests can pin the truthful "swap succeeded" result.
#[cfg(test)]
mod swap_test_hook {
    use std::cell::Cell;

    use anyhow::{Result, bail};

    thread_local! {
        static FAIL_NEXT_BACKUP_CLEANUP: Cell<bool> = const { Cell::new(false) };
    }

    pub(super) fn fail_next_backup_cleanup() {
        FAIL_NEXT_BACKUP_CLEANUP.set(true);
    }

    pub(super) fn before_backup_cleanup() -> Result<()> {
        if FAIL_NEXT_BACKUP_CLEANUP.replace(false) {
            bail!("injected backup cleanup failure")
        }
        Ok(())
    }
}

/// Open an operator-supplied local plugin source or marketplace index file
/// through capability authority. `file://` prefixes are accepted and
/// stripped; relative paths are resolved against `cwd` (the operator working
/// directory) and lexically normalized, so `./src`, `plugins/demo`, and
/// `../mirrors/demo.tgz` all resolve exactly where the operator ran the
/// command. The returned lease is the source's parent directory and the name
/// is its final component, so every later read goes through the retained
/// directory handle rather than a re-parsed path string. Sources that do not
/// exist (or escape the resolution root) fail here and callers fall through
/// to their ordinary "source does not exist" handling.
fn open_operator_source(
    cwd: &Path,
    source: &str,
) -> Result<(CapabilityRootLease, std::ffi::OsString)> {
    let capabilities = CapabilityFs::new().context("creating plugin import capabilities")?;
    let raw = Path::new(source.strip_prefix("file://").unwrap_or(source));
    let absolute = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };
    let absolute = lexical_normalize(&absolute).with_context(|| {
        format!("resolving plugin source {source} against the operator working directory")
    })?;
    let absolute = AbsolutePath::parse(absolute).map_err(|_| FsError::InvalidPath)?;
    capabilities
        .open_exact_parent(
            CapabilityRootPurpose::OperatorImport,
            absolute,
            CapabilityAccess::ReadOnly,
            RootCreation::Existing,
        )
        .with_context(|| format!("opening plugin source {source}"))
}

/// Lexically resolve `.` and `..` components in `path` without touching the
/// filesystem, so the result parses through [`AbsolutePath`] (which rejects
/// such components). Traversal that would escape the path root fails closed.
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

fn fs_error_kind(error: &FsError) -> Option<std::io::ErrorKind> {
    match error {
        FsError::System { kind, .. } => Some(*kind),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::{ExtensionPermissionSet, ExtensionSpec, ResourceManagerOptions};
    use pi_ai::providers::{FauxProviderOptions, FauxProviderRegistration, register_faux_provider};
    use serde_json::json;
    use tempfile::TempDir;

    struct PluginApplicationFixture {
        _agent: TempDir,
        _cwd: TempDir,
        _sessions: TempDir,
        _faux: FauxProviderRegistration,
        application: crate::Application,
    }

    impl PluginApplicationFixture {
        fn marketplace(&self, agent: &Path) -> Result<PluginMarketplace> {
            Ok(PluginMarketplace::new(agent)?
                .with_process_context(self.application.governed_process_context())
                .with_http_executors(PluginHttpExecutors::new(
                    self.application
                        .http_executor(pi_sandbox::HostHttpClass::PluginMarketplace),
                    self.application
                        .http_executor(pi_sandbox::HostHttpClass::PluginArchive),
                    self.application
                        .http_executor(pi_sandbox::HostHttpClass::NpmRegistry),
                    self.application
                        .http_executor(pi_sandbox::HostHttpClass::NpmTarball),
                )))
        }
    }

    async fn plugin_application(settings: &str) -> Result<PluginApplicationFixture> {
        let agent = TempDir::new()?;
        let cwd = TempDir::new()?;
        let sessions = TempDir::new()?;
        fs::write(agent.path().join("settings.json"), settings)?;
        let mut options = ResourceManagerOptions::new(cwd.path());
        options.agent_dir = agent.path().to_path_buf();
        options.project_trust_override = Some(true);
        options.disable_extensions = true;
        options.disable_skills = true;
        options.disable_prompt_templates = true;
        options.disable_themes = true;
        options.disable_context_files = true;
        let resources = crate::ResourceManager::new(options)?;
        let suffix = uuid::Uuid::now_v7();
        let api = format!("plugin-test-api-{suffix}");
        let provider = format!("plugin-test-provider-{suffix}");
        let model = pi_ai::Model {
            id: format!("plugin-test-model-{suffix}"),
            name: "Plugin Test Model".to_owned(),
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
        let workspace = crate::WorkspaceRoots::new(cwd.path(), Vec::<PathBuf>::new())?;
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
            )?;
        session.set_session_dir(sessions.path().to_path_buf());
        session.attach_resources(resources).await?;
        let application = crate::Application::from_runtime_candidate(
            crate::ApplicationRuntimeCandidate::new(session),
        )
        .await?;
        Ok(PluginApplicationFixture {
            _agent: agent,
            _cwd: cwd,
            _sessions: sessions,
            _faux: faux,
            application,
        })
    }

    async fn governed_plugin_application() -> Result<PluginApplicationFixture> {
        plugin_application(r#"{"sandbox":{"mode":"danger-full-access"}}"#).await
    }

    async fn governed_network_marketplace(
        agent: &Path,
    ) -> Result<(
        PluginApplicationFixture,
        PluginMarketplace,
        std::sync::Arc<std::sync::Mutex<Vec<(TestHttpClass, String)>>>,
    )> {
        let application = governed_plugin_application().await?;
        let (http, observed) = test_http_executors(&application.application);
        let marketplace = PluginMarketplace::new(agent)?
            .with_process_context(application.application.governed_process_context())
            .with_http_executors(http);
        Ok((application, marketplace, observed))
    }

    const QUICKJS_MANIFEST: &str = r#"{
        "schemaVersion": 1,
        "id": "demo",
        "version": "1.0.0",
        "runtime": "quickjs",
        "entry": "index.mjs",
        "capabilities": ["commands"]
    }"#;

    fn write_quickjs_plugin(root: &Path, id: &str, version: &str) -> Result<()> {
        fs::create_dir_all(root)?;
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

    fn writable_test_root(root: &Path) -> Result<CapabilityRootLease> {
        let capabilities = CapabilityFs::new().context("creating test filesystem capabilities")?;
        let absolute = AbsolutePath::parse(root.to_path_buf())
            .map_err(|_| anyhow!("invalid plugin extraction test root"))?;
        capabilities
            .open_root(
                CapabilityRootPurpose::AgentStore,
                absolute,
                CapabilityAccess::PrivateReadWrite,
                RootCreation::Existing,
            )
            .context("opening plugin extraction test root")
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
            managed_root: None,
            package_id: "marketplace:test".to_owned(),
            scope: PackageScope::Global,
            trusted: true,
        })
    }

    fn resources_for(agent_dir: &Path, trust_store: &TrustStore) -> Vec<PackageResourceSpec> {
        marketplace_extension_resources(agent_dir, trust_store).expect("scan resources")
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum TestHttpClass {
        Marketplace,
        Archive,
        NpmRegistry,
        NpmTarball,
    }

    #[derive(Clone)]
    struct TestHttpExecutor {
        class: TestHttpClass,
        observed: std::sync::Arc<std::sync::Mutex<Vec<(TestHttpClass, String)>>>,
        delegate: pi_ai::SharedHttpExecutor,
    }

    impl pi_ai::HttpExecutor for TestHttpExecutor {
        fn execute<'a>(
            &'a self,
            request: pi_ai::HttpRequest,
        ) -> futures_util::future::BoxFuture<'a, anyhow::Result<pi_ai::HttpResponse>> {
            use futures_util::FutureExt as _;

            async move {
                let parts = request.into_parts();
                let scheme = match parts.authority.scheme() {
                    pi_ai::ResolvedHttpScheme::Http => "http",
                    pi_ai::ResolvedHttpScheme::Https => "https",
                };
                let host = match parts.authority.host() {
                    pi_ai::ResolvedHttpHost::Dns(name) => name.clone(),
                    pi_ai::ResolvedHttpHost::Ip(address) => address.to_string(),
                };
                let url = format!(
                    "{scheme}://{host}:{}{}",
                    parts.authority.port(),
                    parts.path_and_query
                );
                self.observed
                    .lock()
                    .expect("plugin HTTP observations mutex poisoned")
                    .push((self.class, url));
                let request = pi_ai::HttpRequest::new(
                    parts.method,
                    parts.authority,
                    parts.path_and_query,
                    parts.headers,
                    parts.body,
                    parts.timeout_ms,
                    parts.max_response_bytes.get(),
                    parts.abort_signal,
                )?;
                self.delegate.execute(request).await
            }
            .boxed()
        }
    }

    fn test_http_executors(
        application: &crate::Application,
    ) -> (
        PluginHttpExecutors,
        std::sync::Arc<std::sync::Mutex<Vec<(TestHttpClass, String)>>>,
    ) {
        let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = |class, host_class| {
            std::sync::Arc::new(TestHttpExecutor {
                class,
                observed: observed.clone(),
                delegate: application.http_executor(host_class),
            }) as pi_ai::SharedHttpExecutor
        };
        (
            PluginHttpExecutors::new(
                executor(
                    TestHttpClass::Marketplace,
                    pi_sandbox::HostHttpClass::PluginMarketplace,
                ),
                executor(
                    TestHttpClass::Archive,
                    pi_sandbox::HostHttpClass::PluginArchive,
                ),
                executor(
                    TestHttpClass::NpmRegistry,
                    pi_sandbox::HostHttpClass::NpmRegistry,
                ),
                executor(
                    TestHttpClass::NpmTarball,
                    pi_sandbox::HostHttpClass::NpmTarball,
                ),
            ),
            observed,
        )
    }

    #[tokio::test]
    async fn remote_marketplace_and_archive_use_exact_executors() -> Result<()> {
        let archive = side_chat_tarball()?;
        let index = serde_json::to_vec(&[json!({
            "name": "side-chat",
            "repo": "owner/side-chat",
            "version": "1.0.0",
        })])?;
        let base = spawn_registry_server(move |_port| {
            vec![
                ("/index.json".to_owned(), RouteResponse::Ok(index)),
                ("/side-chat.tgz".to_owned(), RouteResponse::Ok(archive)),
            ]
        })
        .await;
        let agent = TempDir::new()?;
        let (_application, marketplace, observed) =
            governed_network_marketplace(agent.path()).await?;
        let fetched = marketplace
            .fetch_index(&format!("{base}/index.json"))
            .await?;
        assert_eq!(
            fetched
                .entry("side-chat")
                .expect("marketplace entry")
                .version,
            "1.0.0"
        );
        let trust_store = TrustStore::new(agent.path());
        let installed = marketplace
            .install(&format!("{base}/side-chat.tgz"), &trust_store)
            .await?;
        assert_eq!(installed.name, "side-chat");
        assert_eq!(
            *observed
                .lock()
                .expect("plugin HTTP observations mutex poisoned"),
            vec![
                (TestHttpClass::Marketplace, format!("{base}/index.json")),
                (TestHttpClass::Archive, format!("{base}/side-chat.tgz")),
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn remote_plugin_sources_fail_closed_without_captured_http() -> Result<()> {
        let agent = TempDir::new()?;
        let marketplace = PluginMarketplace::new(agent.path())?;
        let trust_store = TrustStore::new(agent.path());
        let index_error = marketplace
            .fetch_index("https://plugins.example/index.json")
            .await
            .expect_err("remote index without captured authority must fail");
        assert!(
            index_error
                .to_string()
                .contains("plugin marketplace HTTP generation is unavailable"),
            "{index_error:#}"
        );
        let archive_error = marketplace
            .install("https://plugins.example/demo.tgz", &trust_store)
            .await
            .expect_err("remote archive without captured authority must fail");
        assert!(
            format!("{archive_error:#}").contains("plugin archive HTTP generation is unavailable"),
            "{archive_error:#}"
        );
        let npm_error = marketplace
            .install("npm:demo", &trust_store)
            .await
            .expect_err("npm without captured authority must fail");
        assert!(
            format!("{npm_error:#}").contains("plugin npm HTTP generation is unavailable"),
            "{npm_error:#}"
        );
        assert!(marketplace.list(&trust_store)?.0.is_empty());
        Ok(())
    }
    #[tokio::test]
    async fn install_from_local_directory_lands_in_root_and_is_loadable() -> Result<()> {
        let agent = TempDir::new()?;
        let source = TempDir::new()?;
        write_quickjs_plugin(source.path(), "demo", "1.0.0")?;

        let marketplace = PluginMarketplace::new(agent.path())?;
        let trust_store = TrustStore::new(agent.path());
        let installed = marketplace
            .install(source.path().to_str().unwrap(), &trust_store)
            .await?;

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
    async fn install_in_git_worktree_trusts_only_exact_plugin_directory() -> Result<()> {
        let agent = TempDir::new()?;
        fs::create_dir_all(agent.path().join(".git"))?;
        fs::write(agent.path().join(".git/HEAD"), "ref: refs/heads/main\n")?;
        let source = TempDir::new()?;
        write_quickjs_plugin(source.path(), "inside", "1.0.0")?;
        let sibling_source = TempDir::new()?;
        write_quickjs_plugin(sibling_source.path(), "sibling", "1.0.0")?;

        let marketplace = PluginMarketplace::new(agent.path())?;
        let trust_store = TrustStore::new(agent.path());
        marketplace
            .install(source.path().to_str().unwrap(), &trust_store)
            .await?;

        let target = agent.path().join("extensions/inside");
        let sibling = agent.path().join("extensions/sibling");
        fs::create_dir_all(&sibling)?;
        fs::copy(
            sibling_source.path().join(PROCESS_EXTENSION_MANIFEST_FILE),
            sibling.join(PROCESS_EXTENSION_MANIFEST_FILE),
        )?;
        fs::copy(
            sibling_source.path().join("index.mjs"),
            sibling.join("index.mjs"),
        )?;

        assert_eq!(trust_store.resolve_exact(&target)?, TrustDecision::Trusted);
        assert_eq!(trust_store.resolve_exact(agent.path())?, TrustDecision::Ask);
        assert_eq!(trust_store.resolve_exact(&sibling)?, TrustDecision::Ask);
        let project_resolution = trust_store.resolve(&target)?;
        assert_eq!(project_resolution.decision, TrustDecision::Ask);
        assert_eq!(
            project_resolution.project_path,
            fs::canonicalize(agent.path())?
        );

        let (plugins, problems) = marketplace.list(&trust_store)?;
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(plugins.len(), 2);
        assert!(
            plugins
                .iter()
                .any(|plugin| plugin.name == "inside" && plugin.trusted)
        );
        assert!(
            plugins
                .iter()
                .any(|plugin| plugin.name == "sibling" && !plugin.trusted)
        );
        let resources = resources_for(agent.path(), &trust_store);
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].path, target);

        marketplace.remove("inside", &trust_store)?;
        assert_eq!(trust_store.resolve_exact(&target)?, TrustDecision::Ask);
        assert!(sibling.is_dir());
        assert!(resources_for(agent.path(), &trust_store).is_empty());
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
                (
                    "tar-demo-v2/src/index.mjs",
                    b"export default function (pi) {}\n",
                ),
            ],
        )?;

        let marketplace = PluginMarketplace::new(agent.path())?;
        let trust_store = TrustStore::new(agent.path());
        let installed = marketplace
            .install(archive.to_str().unwrap(), &trust_store)
            .await?;
        assert_eq!(installed.name, "tar-demo");
        assert_eq!(installed.version, "2.0.0");

        let target = agent.path().join("extensions/tar-demo");
        assert!(target.join("pi-extension.json").is_file());
        assert!(target.join("src/index.mjs").is_file());
        assert!(
            !target.join("tar-demo-v2").exists(),
            "top directory must be stripped"
        );
        assert!(loadable_spec(&target).is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn install_removes_files_when_trust_recording_fails() -> Result<()> {
        let agent = TempDir::new()?;
        let source = TempDir::new()?;
        write_quickjs_plugin(source.path(), "rejected", "1.0.0")?;
        let marketplace = PluginMarketplace::new(agent.path())?;
        let trust_store = TrustStore::new(agent.path());
        fs::write(trust_store.path(), "{invalid trust document")?;

        let error = marketplace
            .install(source.path().to_str().unwrap(), &trust_store)
            .await
            .expect_err("trust write failure must fail installation");
        assert!(format!("{error:#}").contains("recording trust for plugin rejected"));
        assert!(!agent.path().join("extensions/rejected").exists());
        assert!(
            fs::read_dir(agent.path())?.all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".plugin-staging")
            }),
            "failed trust recording must clean the staged package"
        );
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

        let marketplace = PluginMarketplace::new(agent.path())?;
        let trust_store = TrustStore::new(agent.path());
        let installed = marketplace
            .install(archive.to_str().unwrap(), &trust_store)
            .await?;
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

        let marketplace = PluginMarketplace::new(agent.path())?;
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
        let marketplace = PluginMarketplace::new(agent.path())?;
        let trust_store = TrustStore::new(agent.path());
        let error = marketplace
            .install(source.path().to_str().unwrap(), &trust_store)
            .await
            .expect_err("traversal must fail install");
        assert!(
            format!("{error:#}").contains("must remain inside"),
            "{error:#}"
        );
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

        let marketplace = PluginMarketplace::new(agent.path())?;
        let trust_store = TrustStore::new(agent.path());
        let installed = marketplace
            .install(source.path().to_str().unwrap(), &trust_store)
            .await?;
        assert_eq!(installed.name, "payload");

        let target = agent.path().join("extensions/payload");
        assert!(
            !target.join("marker.txt").exists(),
            "install must not run payload.sh"
        );
        assert!(
            !target.join("EXECUTED").exists(),
            "install must not run payload.sh"
        );
        // The payload itself is only validated for existence/containment.
        assert!(target.join("payload.sh").is_file());
        Ok(())
    }

    #[tokio::test]
    async fn remove_cleans_up_and_clears_trust() -> Result<()> {
        let agent = TempDir::new()?;
        let source = TempDir::new()?;
        write_quickjs_plugin(source.path(), "gone", "1.0.0")?;
        let marketplace = PluginMarketplace::new(agent.path())?;
        let trust_store = TrustStore::new(agent.path());
        marketplace
            .install(source.path().to_str().unwrap(), &trust_store)
            .await?;
        let target = agent.path().join("extensions/gone");
        assert!(target.is_dir());

        marketplace.remove("gone", &trust_store)?;
        assert!(!target.exists(), "plugin directory must be removed");
        assert!(marketplace.list(&trust_store)?.0.is_empty());
        assert!(
            resources_for(agent.path(), &trust_store).is_empty(),
            "removed plugin must not be loadable"
        );
        assert_eq!(
            trust_store.resolve_exact(&target)?,
            TrustDecision::Ask,
            "the plugin's exact trust decision must be cleared"
        );
        Ok(())
    }

    #[tokio::test]
    async fn remove_restores_plugin_when_trust_cleanup_fails() -> Result<()> {
        let agent = TempDir::new()?;
        let source = TempDir::new()?;
        write_quickjs_plugin(source.path(), "kept", "1.0.0")?;
        let marketplace = PluginMarketplace::new(agent.path())?;
        let trust_store = TrustStore::new(agent.path());
        marketplace
            .install(source.path().to_str().unwrap(), &trust_store)
            .await?;
        let target = agent.path().join("extensions/kept");
        remove_test_hook::fail_next_cleanup();

        let error = marketplace
            .remove("kept", &trust_store)
            .expect_err("trust cleanup failure must abort removal");
        assert!(format!("{error:#}").contains("clearing trust for plugin kept"));
        assert_eq!(trust_store.resolve_exact(&target)?, TrustDecision::Trusted);
        assert!(target.join(PROCESS_EXTENSION_MANIFEST_FILE).is_file());
        assert!(target.join("index.mjs").is_file());
        assert!(
            fs::read_dir(agent.path())?.all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".plugin-removing")
            }),
            "failed trust cleanup must restore the original plugin directory"
        );
        Ok(())
    }

    #[tokio::test]
    async fn install_from_relative_directory_resolves_against_operator_cwd() -> Result<()> {
        let agent = TempDir::new()?;
        let operator = TempDir::new()?;
        let plugins = operator.path().join("plugins");
        fs::create_dir_all(&plugins)?;
        write_quickjs_plugin(&plugins.join("demo"), "demo", "1.0.0")?;

        let marketplace = PluginMarketplace::new(agent.path())?.with_operator_cwd(operator.path());
        let trust_store = TrustStore::new(agent.path());
        let target = agent.path().join("extensions/demo");

        // A plain relative directory source resolves against the operator cwd.
        let installed = marketplace.install("plugins/demo", &trust_store).await?;
        assert_eq!(installed.name, "demo");
        assert_eq!(installed.version, "1.0.0");
        assert!(installed.trusted);
        assert!(target.join(PROCESS_EXTENSION_MANIFEST_FILE).is_file());

        // An explicit `./`-prefixed spelling resolves to the same directory.
        marketplace.remove("demo", &trust_store)?;
        let dot_slash = marketplace.install("./plugins/demo", &trust_store).await?;
        assert_eq!(dot_slash.name, "demo");

        // A `..`-containing spelling is lexically normalized against the
        // operator cwd instead of being rejected outright.
        marketplace.remove("demo", &trust_store)?;
        let traversing = marketplace
            .install("plugins/../plugins/demo", &trust_store)
            .await?;
        assert_eq!(traversing.name, "demo");
        let (plugins, problems) = marketplace.list(&trust_store)?;
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].version, "1.0.0");
        assert!(loadable_spec(&target).is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn install_from_relative_and_file_archive_sources_resolve_against_operator_cwd()
    -> Result<()> {
        let agent = TempDir::new()?;
        let operator = TempDir::new()?;
        let plugins = operator.path().join("plugins");
        fs::create_dir_all(&plugins)?;
        let archive = plugins.join("demo-1.0.0.tgz");
        let manifest = serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": "demo",
            "version": "1.0.0",
            "runtime": "quickjs",
            "entry": "index.mjs",
            "capabilities": [],
        }))?;
        write_tarball(
            &archive,
            &[
                ("demo/pi-extension.json", manifest.as_slice()),
                ("demo/index.mjs", b"export default function (pi) {}\n"),
            ],
        )?;

        let marketplace = PluginMarketplace::new(agent.path())?.with_operator_cwd(operator.path());
        let trust_store = TrustStore::new(agent.path());
        let target = agent.path().join("extensions/demo");

        // A relative archive path resolves against the operator cwd, strips the
        // top directory, and installs.
        let installed = marketplace
            .install("plugins/demo-1.0.0.tgz", &trust_store)
            .await?;
        assert_eq!(installed.name, "demo");
        assert_eq!(installed.version, "1.0.0");
        assert!(
            !target.join("demo").exists(),
            "top directory must be stripped"
        );

        // `file://` sources (absolute and relative) open through the same
        // capability path; a relative `file:` component resolves against the
        // operator cwd too.
        marketplace.remove("demo", &trust_store)?;
        let absolute_url = format!("file://{}", archive.display());
        let via_url = marketplace.install(&absolute_url, &trust_store).await?;
        assert_eq!(via_url.name, "demo");
        marketplace.remove("demo", &trust_store)?;
        let relative_url = marketplace
            .install("file://plugins/demo-1.0.0.tgz", &trust_store)
            .await?;
        assert_eq!(relative_url.name, "demo");
        assert!(loadable_spec(&target).is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn fetch_index_resolves_relative_and_file_local_sources_against_operator_cwd()
    -> Result<()> {
        let agent = TempDir::new()?;
        let operator = TempDir::new()?;
        let market = operator.path().join("market");
        fs::create_dir_all(&market)?;
        let index_path = market.join("index.json");
        fs::write(
            &index_path,
            serde_json::to_vec_pretty(&[json!({
                "name": "demo",
                "repo": "owner/demo",
                "version": "1.0.0",
            })])?,
        )?;

        let marketplace = PluginMarketplace::new(agent.path())?.with_operator_cwd(operator.path());

        // Plain relative local index path.
        let index = marketplace.fetch_index("market/index.json").await?;
        assert_eq!(index.entry("demo").expect("entry").version, "1.0.0");

        // Absolute `file://` local index.
        let index = marketplace
            .fetch_index(&format!("file://{}", index_path.display()))
            .await?;
        assert_eq!(index.entry("demo").expect("entry").version, "1.0.0");

        // Relative `file://` component resolves against the operator cwd.
        let index = marketplace.fetch_index("file://market/index.json").await?;
        assert_eq!(index.entry("demo").expect("entry").version, "1.0.0");
        Ok(())
    }

    #[tokio::test]
    async fn missing_relative_source_fails_actionably() -> Result<()> {
        let agent = TempDir::new()?;
        let operator = TempDir::new()?;
        let marketplace = PluginMarketplace::new(agent.path())?.with_operator_cwd(operator.path());
        let trust_store = TrustStore::new(agent.path());

        // A nonexistent relative source is reported as such (after cwd
        // resolution), never misread as a network or git source.
        let error = marketplace
            .install("plugins/ghost", &trust_store)
            .await
            .expect_err("missing relative source must fail install");
        let message = format!("{error:#}");
        assert!(
            message.contains("opening local plugin source") && message.contains("plugins/ghost"),
            "{message}"
        );
        assert!(marketplace.list(&trust_store)?.0.is_empty());

        // Traversal that would escape the operator cwd fails closed.
        let escaping = marketplace
            .install("../../../../../../etc", &trust_store)
            .await
            .expect_err("escaping traversal must fail install");
        assert!(
            format!("{escaping:#}").contains("path traversal escapes its root"),
            "{escaping:#}"
        );
        assert!(marketplace.list(&trust_store)?.0.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn update_reports_live_swap_when_backup_cleanup_fails() -> Result<()> {
        let agent = TempDir::new()?;
        let source = TempDir::new()?;
        write_quickjs_plugin(source.path(), "demo", "1.0.0")?;
        let marketplace = PluginMarketplace::new(agent.path())?;
        let trust_store = TrustStore::new(agent.path());
        marketplace
            .install(source.path().to_str().unwrap(), &trust_store)
            .await?;

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
            })])?,
        )?;
        let index = marketplace
            .fetch_index(index_path.to_str().unwrap())
            .await?;

        // The staged version is already live when backup cleanup runs, so a
        // cleanup failure must not turn the successful swap into a reported
        // failure: update succeeds, the new version is installed, and the
        // hidden backup (still holding the previous version) is left behind.
        swap_test_hook::fail_next_backup_cleanup();
        let updated = marketplace.update("demo", &index, &trust_store).await?;
        assert_eq!(updated.version, "2.0.0", "a live swap must report success");

        let target = agent.path().join("extensions/demo");
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(target.join(PROCESS_EXTENSION_MANIFEST_FILE))?)?;
        assert_eq!(manifest["version"], "2.0.0", "the new version must be live");

        // Listing stays clean: the leftover backup is hidden and ignored, and
        // trust is preserved for the live directory.
        let (plugins, problems) = marketplace.list(&trust_store)?;
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].version, "2.0.0");
        assert!(plugins[0].trusted);

        let backups: Vec<_> = fs::read_dir(agent.path().join("extensions"))?
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".plugin-backup")
            })
            .collect();
        assert_eq!(
            backups.len(),
            1,
            "failed backup cleanup must leave the hidden backup behind"
        );
        Ok(())
    }

    #[test]
    fn directory_swap_restores_original_when_activation_fails() -> Result<()> {
        let root = TempDir::new()?;
        let target = Path::new("demo");
        fs::create_dir_all(root.path().join(&target))?;
        fs::write(root.path().join(target).join("marker.txt"), b"original")?;
        let store = writable_test_root(root.path())?;

        // The staged directory does not exist, so the activation rename fails
        // after the original was moved aside: the swap must restore it.
        let error = directory_swap(&store, Path::new("staged"), target)
            .expect_err("activation failure must fail the swap");
        let message = format!("{error:#}");
        assert!(
            message.contains("previous version was restored"),
            "{message}"
        );
        assert!(
            root.path().join(target).join("marker.txt").is_file(),
            "the original must be restored to the live path"
        );
        assert!(
            fs::read_dir(root.path())?.all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".plugin-backup")
            }),
            "a restored original must leave no hidden backup behind"
        );
        Ok(())
    }

    #[test]
    fn directory_swap_reports_untouched_original_when_move_aside_fails() -> Result<()> {
        let root = TempDir::new()?;
        let store = writable_test_root(root.path())?;
        // `target` does not exist: the move-aside rename fails before anything
        // is touched, and the error says the previous (missing) version is
        // still in place rather than implying a swap happened.
        let error = directory_swap(&store, Path::new("staged"), Path::new("ghost"))
            .expect_err("move-aside failure must fail the swap");
        let message = format!("{error:#}");
        assert!(
            message.contains("previous version remains in place"),
            "{message}"
        );
        assert!(
            fs::read_dir(root.path())?.all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".plugin-backup")
            }),
            "a failed move-aside must leave no hidden backup behind"
        );
        Ok(())
    }

    #[tokio::test]
    async fn update_uses_local_index_swaps_version_and_keeps_trust() -> Result<()> {
        let agent = TempDir::new()?;
        let source = TempDir::new()?;
        write_quickjs_plugin(source.path(), "demo", "1.0.0")?;
        let marketplace = PluginMarketplace::new(agent.path())?;
        let trust_store = TrustStore::new(agent.path());
        marketplace
            .install(source.path().to_str().unwrap(), &trust_store)
            .await?;

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

        let index = marketplace
            .fetch_index(index_path.to_str().unwrap())
            .await?;
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
        marketplace
            .install(loner.path().to_str().unwrap(), &trust_store)
            .await?;
        let error = marketplace
            .update("loner", &index, &trust_store)
            .await
            .expect_err("not in index");
        assert!(format!("{error:#}").contains("not listed"), "{error:#}");
        Ok(())
    }

    #[tokio::test]
    async fn list_and_index_report_available_versions() -> Result<()> {
        let agent = TempDir::new()?;
        let source = TempDir::new()?;
        write_quickjs_plugin(source.path(), "demo", "1.0.0")?;
        let marketplace = PluginMarketplace::new(agent.path())?;
        let trust_store = TrustStore::new(agent.path());
        marketplace
            .install(source.path().to_str().unwrap(), &trust_store)
            .await?;

        let index_dir = TempDir::new()?;
        let index_path = index_dir.path().join("index.json");
        fs::write(
            &index_path,
            serde_json::to_vec_pretty(&[
                json!({ "name": "demo", "repo": "owner/demo", "version": "1.0.10" }),
                json!({ "name": "other", "repo": "owner/other", "version": "9.0.0" }),
            ])?,
        )?;
        let index = marketplace
            .fetch_index(index_path.to_str().unwrap())
            .await?;
        let (installed, problems) = marketplace.list(&trust_store)?;
        assert!(problems.is_empty(), "{problems:?}");
        let updates: Vec<_> = installed
            .iter()
            .filter_map(|plugin| {
                let entry = index.entry(&plugin.name)?;
                compare_versions(&entry.version, &plugin.version)
                    .is_gt()
                    .then_some((plugin, entry))
            })
            .collect();
        assert_eq!(updates.len(), 1);
        let (installed, available) = updates[0];
        assert_eq!(installed.name, "demo");
        assert_eq!(installed.version, "1.0.0");
        assert_eq!(available.version, "1.0.10");
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
        fs::copy(
            source.path().join("pi-extension.json"),
            target.join("pi-extension.json"),
        )?;
        fs::copy(source.path().join("index.mjs"), target.join("index.mjs"))?;

        let trust_store = TrustStore::new(agent.path());
        let marketplace = PluginMarketplace::new(agent.path())?;
        let (plugins, problems) = marketplace.list(&trust_store)?;
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(plugins.len(), 1);
        assert!(!plugins[0].trusted, "dropped-in plugin starts untrusted");
        assert!(
            resources_for(agent.path(), &trust_store).is_empty(),
            "untrusted plugin must not be loadable"
        );

        // Storing a Trusted decision at the exact plugin path makes it loadable.
        trust_store.set_exact(&target, TrustDecision::Trusted)?;
        assert!(plugins_are_trusted(&marketplace, &trust_store));
        let resources = resources_for(agent.path(), &trust_store);
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].path, target);

        // A stored exact Untrusted decision blocks loading again.
        trust_store.set_exact(&target, TrustDecision::Untrusted)?;
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
        fs::write(
            agent.path().join("settings.json"),
            r#"{"sandbox":{"mode":"danger-full-access"}}"#,
        )?;

        let marketplace = PluginMarketplace::new(agent.path())?;
        let trust_store = TrustStore::new(agent.path());
        marketplace
            .install(source.path().to_str().unwrap(), &trust_store)
            .await?;
        let suffix = uuid::Uuid::now_v7();
        let api = format!("plugin-resource-test-api-{suffix}");
        let provider = format!("plugin-resource-test-provider-{suffix}");
        let model = pi_ai::Model {
            id: format!("plugin-resource-test-model-{suffix}"),
            name: "Plugin Resource Test Model".to_owned(),
            api: api.clone(),
            provider: provider.clone(),
            ..pi_ai::Model::default()
        };
        let _faux = register_faux_provider(FauxProviderOptions {
            api,
            provider,
            models: vec![model.clone()],
            chunk_size: 8,
        });
        let workspace = crate::WorkspaceRoots::new(cwd.path(), Vec::<PathBuf>::new())?;

        let mut options = ResourceManagerOptions::new(cwd.path());
        options.agent_dir = agent.path().to_path_buf();
        options.project_trust_override = Some(true);
        options.disable_skills = true;
        options.disable_prompt_templates = true;
        options.disable_themes = true;
        options.disable_context_files = true;
        options.headless = true;
        let resources = crate::ResourceManager::new(options)?;
        let sessions = TempDir::new()?;
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
            )?;
        session.set_session_dir(sessions.path().to_path_buf());
        session.attach_resources(resources).await?;
        let application = crate::Application::from_runtime_candidate(
            crate::ApplicationRuntimeCandidate::new(session),
        )
        .await?;
        let resources = application.capture_generation().resource().clone();
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
        let target_root = writable_test_root(target.path())?;
        let error = extract_tar_from_bytes(
            &raw_tar_entry("../escape.txt", b"evil\n", b'0'),
            &target_root,
            Path::new(""),
        )
        .expect_err("traversal entry must be rejected");
        assert!(format!("{error:#}").contains("unsafe path"), "{error:#}");
        assert!(!target.path().join("escape.txt").exists());
        assert!(!target.path().parent().unwrap().join("escape.txt").exists());
        Ok(())
    }

    #[test]
    fn archive_absolute_and_symlink_entries_are_rejected() -> Result<()> {
        // Absolute entries: the tar crate normalizes a leading slash, so an
        // absolute path in the archive lands inside the staging directory —
        // it must never escape it (defense in depth: our own is_absolute
        // check would also reject it if the crate ever stopped normalizing).
        let target = TempDir::new()?;
        let target_root = writable_test_root(target.path())?;
        match extract_tar_from_bytes(
            &raw_tar_entry("/etc/evil", b"evil\n", b'0'),
            &target_root,
            Path::new(""),
        ) {
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
        let symlink_root = writable_test_root(symlink.path())?;
        let error = extract_tar_from_bytes(
            &raw_tar_entry("pkg/evil", b"target", b'2'),
            &symlink_root,
            Path::new(""),
        )
        .expect_err("symlink entry must be rejected");
        assert!(
            format!("{error:#}").contains("not a regular file"),
            "{error:#}"
        );
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
        assert_eq!(
            index.entry("demo").expect("entry").description.as_deref(),
            Some("a demo")
        );
        assert!(index.entry("missing").is_none());

        let duplicates = serde_json::to_vec_pretty(&[
            json!({ "name": "demo", "repo": "a/demo", "version": "1.0.0" }),
            json!({ "name": "demo", "repo": "b/demo", "version": "1.0.1" }),
        ])?;
        let error = MarketplaceIndex::parse(&duplicates).expect_err("duplicates rejected");
        assert!(format!("{error:#}").contains("twice"), "{error:#}");

        let empty_repo = serde_json::to_vec_pretty(&[
            json!({ "name": "demo", "repo": "", "version": "1.0.0" }),
        ])?;
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
            Some(GitHubReference {
                owner: "owner".to_owned(),
                repo: "repo".to_owned(),
                git_ref: None,
            })
        );
        assert_eq!(
            parse_github_reference("owner/repo.git"),
            Some(GitHubReference {
                owner: "owner".to_owned(),
                repo: "repo".to_owned(),
                git_ref: None,
            })
        );
        assert_eq!(
            parse_github_reference("owner/repo@v1.2.3"),
            Some(GitHubReference {
                owner: "owner".to_owned(),
                repo: "repo".to_owned(),
                git_ref: Some("v1.2.3".to_owned()),
            })
        );
        assert_eq!(parse_github_reference("a/b/c"), None, "too many segments");
        assert_eq!(parse_github_reference("/repo"), None);
        assert_eq!(parse_github_reference("owner/"), None);
        assert_eq!(parse_github_reference("https://x/y"), None);
        assert_eq!(parse_github_reference("../relative/path"), None);
        assert_eq!(parse_github_reference(".hidden/repo"), None);
        assert_eq!(parse_github_reference("owner/repo!"), None);
        assert_eq!(parse_github_reference("owner/repo@"), None, "empty pin");
        assert_eq!(parse_github_reference("owner/repo@.."), None, "ref traversal");
    }

    #[test]
    fn git_http_clone_urls_parse_like_https() {
        let http = parse_git_reference("http://127.0.0.1:3010/owner/repo.git")
            .expect("explicit http git URL");
        assert!(http.starts_with("http://"), "{http}");
        let git_plus = parse_git_reference("git+http://127.0.0.1:3010/owner/repo")
            .expect("git+http bare repo");
        assert!(git_plus.starts_with("http://"), "{git_plus}");
        assert!(parse_git_reference("https://example.com/owner/repo.git").is_some());
        assert_eq!(
            parse_git_reference("https://example.com/owner/repo"),
            None,
            "https without .git still requires git+"
        );
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
            managed_root: None,
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
        for bad in [
            "",
            ".",
            "..",
            "../x",
            "a/b",
            "a\\b",
            "a b",
            "a\x00b",
            long.as_str(),
        ] {
            assert!(
                validate_plugin_name(bad).is_err(),
                "{bad:?} must be rejected"
            );
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
        let marketplace = PluginMarketplace::new(agent.path())?;
        let trust_store = TrustStore::new(agent.path());
        // The valid plugin installs normally; listing still surfaces the
        // corrupt directory as a problem instead of failing.
        marketplace
            .install(source.path().to_str().unwrap(), &trust_store)
            .await?;
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
        set_npm_registry_base_for_tests(base.as_str());

        let agent = TempDir::new()?;
        let (_application, marketplace, observed) =
            governed_network_marketplace(agent.path()).await?;
        let trust_store = TrustStore::new(agent.path());
        let installed = marketplace
            .install("npm:pi-side-chat", &trust_store)
            .await?;
        assert_eq!(
            *observed
                .lock()
                .expect("plugin HTTP observations mutex poisoned"),
            vec![
                (TestHttpClass::NpmRegistry, format!("{base}/pi-side-chat")),
                (
                    TestHttpClass::NpmTarball,
                    format!("{base}/pi-side-chat-1.0.0.tgz"),
                ),
            ],
            "npm metadata and tarball must use their exact captured classes"
        );
        assert_eq!(installed.name, "side-chat");
        assert_eq!(installed.version, "1.0.0");
        assert!(installed.trusted);

        // The `package/` top directory npm tarballs use is stripped; the
        // manifest and its entry point land at the installed extension root
        // and the runtime path resolves relative to it.
        let target = agent.path().join("extensions/side-chat");
        assert!(target.join("pi-extension.json").is_file());
        assert!(target.join("dist/index.mjs").is_file());
        assert!(
            !target.join("package").exists(),
            "npm top directory must be stripped"
        );
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
        let (_application, marketplace, _) = governed_network_marketplace(agent.path()).await?;
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
        let (_application, marketplace, _) = governed_network_marketplace(agent.path()).await?;
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
            vec![("/no-such-package".to_owned(), RouteResponse::Err(404))]
        })
        .await;
        set_npm_registry_base_for_tests(base);

        let agent = TempDir::new()?;
        let (_application, marketplace, _) = governed_network_marketplace(agent.path()).await?;
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
        let (_application, marketplace, _) = governed_network_marketplace(agent.path()).await?;
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
        let (_application, marketplace, _) = governed_network_marketplace(agent.path()).await?;
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
        let (_application, marketplace, _) = governed_network_marketplace(agent.path()).await?;
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

        let application = governed_plugin_application().await?;
        let marketplace = application.marketplace(agent.path())?;
        let trust_store = TrustStore::new(agent.path());
        let installed = marketplace.install(&url, &trust_store).await?;
        assert_eq!(installed.name, "gitdemo");

        let target = agent.path().join("extensions/gitdemo");
        assert!(
            target.join("pi-extension.json").is_file(),
            "manifest must be cloned in"
        );
        assert!(
            target.join("index.mjs").is_file(),
            "entry point must be cloned in"
        );
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
        fs::write(
            repo_dir.join("README.md"),
            "this repo has no plugin manifest\n",
        )?;
        init_git_repo(&repo_dir)?;
        // The `git+` prefix exercises the bare (`.git`-less) repo path.
        let url = format!("git+file://{}", repo_dir.display());

        let application = governed_plugin_application().await?;
        let marketplace = application.marketplace(agent.path())?;
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
        let marketplace = PluginMarketplace::new(agent.path())?;
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
