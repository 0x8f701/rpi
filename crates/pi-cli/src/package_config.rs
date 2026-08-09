//! `rpi config [-l]` — package-resource configuration surface.
//!
//! Discovers installed/configured package resources from each package's
//! `package.json#pi` manifest, groups them by kind (extension/skill/prompt/
//! theme), shows enabled state for the selected scope, and lets the user toggle
//! individual resources on or off. Changes persist as one atomic settings write
//! only on Apply; Cancel and parse/manifest failures write nothing.
//!
//! The persisted representation matches the original product: a package entry's
//! per-kind filter list holds `+<rel>`/`-<rel>` tokens where `<rel>` is the
//! resource path relative to the package install root (POSIX). `+` force-includes
//! a resource and `-` force-excludes it (exact match, last write wins); the base
//! state is the package default (`autoload != false`). When no kind array
//! carries any token the entry collapses back to a bare source string. Other
//! tokens (manual globs / `!` excludes) are preserved verbatim.
//!
//! Project scope is refused when the project is not trusted, both at discovery
//! and at the atomic write. Headless invocation (non-TTY stdout) prints the
//! current scope/resource state as deterministic JSON and never blocks.

use std::io::{self, IsTerminal, Stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use crossterm::{
    cursor::{Hide, Show},
    event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use pi_coding::{
    DefaultProjectTrust, PackageManager, PackageManifest, PackageResourceKind, PackageResources,
    PackageScope, PackageSource, ResourcePackage, ScopePackage, SettingsManager, TrustStore,
    resolve_project_trust,
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use serde_json::{Value, json};

const KINDS: [PackageResourceKind; 4] = [
    PackageResourceKind::Extension,
    PackageResourceKind::Skill,
    PackageResourceKind::Prompt,
    PackageResourceKind::Theme,
];

fn kind_label(kind: PackageResourceKind) -> &'static str {
    match kind {
        PackageResourceKind::Extension => "extensions",
        PackageResourceKind::Skill => "skills",
        PackageResourceKind::Prompt => "prompts",
        PackageResourceKind::Theme => "themes",
    }
}

fn kind_plural(kind: PackageResourceKind) -> &'static str {
    match kind {
        PackageResourceKind::Extension => "Extensions",
        PackageResourceKind::Skill => "Skills",
        PackageResourceKind::Prompt => "Prompts",
        PackageResourceKind::Theme => "Themes",
    }
}

/// POSIX path of `path` relative to `root`. Falls back to the file name when the
/// resource is not lexically below the root (it always is for discovered
/// resources, but the guard keeps output deterministic on edge inputs).
fn relative_posix(root: &Path, path: &Path) -> String {
    if let Ok(rel) = path.strip_prefix(root) {
        return rel.to_string_lossy().replace('\\', "/");
    }
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The settings source string carried by either [`PackageSource`] variant.
fn source_of(entry: &PackageSource) -> &str {
    match entry {
        PackageSource::All(source) => source,
        PackageSource::Filtered(pkg) => &pkg.source,
    }
}
/// JSON representation of one toggleable resource for the headless snapshot.
fn resource_json(res: &ConfigResource) -> Value {
    json!({
        "name": res.name,
        "path": res.path.to_string_lossy(),
        "enabled": res.enabled,
    })
}
/// Strip a leading `+`/`-`/`!` force/include/exclude prefix from a filter
/// token, returning the bare pattern used for identity comparison.
fn strip_token_prefix(token: &str) -> &str {
    token.strip_prefix(['+', '-', '!']).unwrap_or(token)
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// One toggleable package resource.
#[derive(Debug, Clone)]
pub struct ConfigResource {
    /// POSIX path relative to the package install root (the filter-list key).
    pub name: String,
    /// Absolute on-disk path of the resource.
    pub path: PathBuf,
    /// Enabled state shown to the user (current, post-toggle).
    pub enabled: bool,
    original_enabled: bool,
}

/// Resources for one package, grouped by kind.
#[derive(Debug, Clone, Default)]
pub struct ConfigGroups {
    pub extensions: Vec<ConfigResource>,
    pub skills: Vec<ConfigResource>,
    pub prompts: Vec<ConfigResource>,
    pub themes: Vec<ConfigResource>,
}

impl ConfigGroups {
    fn get(&self, kind: PackageResourceKind) -> &[ConfigResource] {
        match kind {
            PackageResourceKind::Extension => &self.extensions,
            PackageResourceKind::Skill => &self.skills,
            PackageResourceKind::Prompt => &self.prompts,
            PackageResourceKind::Theme => &self.themes,
        }
    }

    fn get_mut(&mut self, kind: PackageResourceKind) -> &mut Vec<ConfigResource> {
        match kind {
            PackageResourceKind::Extension => &mut self.extensions,
            PackageResourceKind::Skill => &mut self.skills,
            PackageResourceKind::Prompt => &mut self.prompts,
            PackageResourceKind::Theme => &mut self.themes,
        }
    }

    fn is_empty(&self) -> bool {
        self.extensions.is_empty()
            && self.skills.is_empty()
            && self.prompts.is_empty()
            && self.themes.is_empty()
    }
}

/// One configured package as shown by `rpi config`.
#[derive(Debug, Clone)]
pub struct ConfigPackage {
    /// Settings source string (the `packages[].source` key).
    pub source: String,
    /// Package identity (`local:...` / `git:host/path`), or `None` for an
    /// unsupported (npm) or not-yet-installed package.
    pub identity: Option<String>,
    /// Whether the package checkout is present and supported.
    pub installed: bool,
    /// npm/unsupported sources are listed but never toggleable.
    pub unsupported: bool,
    /// Absolute install root for an installed package.
    pub root: Option<PathBuf>,
    pub manifest: PackageManifest,
    pub groups: ConfigGroups,
}

impl ConfigPackage {
    fn has_toggles(&self) -> bool {
        KINDS.iter().any(|kind| {
            self.groups
                .get(*kind)
                .iter()
                .any(|res| res.enabled != res.original_enabled)
        })
    }
}

/// The full `rpi config` view for one scope: pure data, no terminal. All
/// mutation (toggle, scope switch) and persistence (apply) happens here so the
/// surface is unit-testable without a PTY.
#[derive(Debug)]
pub struct PackageConfigModel {
    cwd: PathBuf,
    agent_dir: PathBuf,
    scope: PackageScope,
    project_trusted: bool,
    /// Settings `packages` entries in display order; `entries[i]` corresponds to
    /// `original[i]`.
    original: Vec<PackageSource>,
    pub entries: Vec<ConfigPackage>,
}

impl PackageConfigModel {
    #[must_use]
    pub fn scope(&self) -> PackageScope {
        self.scope
    }

    #[must_use]
    pub fn project_trusted(&self) -> bool {
        self.project_trusted
    }

    /// Toggle the enabled state of one resource. No-op for unsupported or
    /// not-installed packages.
    pub fn toggle(&mut self, package: usize, kind: PackageResourceKind, resource: usize) {
        let Some(pkg) = self.entries.get_mut(package) else {
            return;
        };
        if !pkg.installed || pkg.unsupported {
            return;
        }
        if let Some(res) = pkg.groups.get_mut(kind).get_mut(resource) {
            res.enabled = !res.enabled;
        }
    }

    /// Rebuild the model for a different scope. Refuses to switch to project
    /// scope when the project is not trusted (returning the original scope) so
    /// the UI can surface a denial without crashing.
    pub fn switch_scope(&mut self, scope: PackageScope) -> Result<()> {
        if scope == PackageScope::Project && !self.project_trusted {
            bail!("project is not trusted; refusing to access project package storage");
        }
        if scope == self.scope {
            return Ok(());
        }
        let cwd = self.cwd.clone();
        let agent_dir = self.agent_dir.clone();
        let project_trusted = self.project_trusted;
        let rebuilt = load_config_model(&cwd, &agent_dir, project_trusted, scope)?;
        *self = rebuilt;
        Ok(())
    }

    /// Persist toggles as one atomic settings write for the current scope.
    /// Non-toggled entries (including not-installed and npm packages) are
    /// preserved verbatim. Returns `Ok(())` with no write when nothing changed.
    pub fn apply(&self) -> Result<()> {
        let manager = SettingsManager::load_phase_one(&self.cwd, &self.agent_dir)
            .context("loading settings for apply")?;
        manager.load_project(self.project_trusted)?;

        let mut next = self.original.clone();
        let mut changed = false;
        for (index, entry) in self.entries.iter().enumerate() {
            if !entry.has_toggles() {
                continue;
            }
            changed = true;
            next[index] = apply_toggles(&next[index], entry);
        }
        if !changed {
            return Ok(());
        }
        match self.scope {
            PackageScope::Global => manager.update_global(|settings| {
                settings.packages = next;
            }),
            PackageScope::Project => manager.update_project(|settings| {
                settings.packages = next;
            }),
        }
        .context("applying package resource configuration")?;
        Ok(())
    }

    /// Deterministic JSON snapshot of the current scope/resource state.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "scope": self.scope.label(),
            "projectTrusted": self.project_trusted,
            "packages": self.entries.iter().map(|pkg| {
                json!({
                    "source": pkg.source,
                    "identity": pkg.identity,
                    "installed": pkg.installed,
                    "unsupported": pkg.unsupported,
                    "root": pkg.root.as_ref().map(|p| p.to_string_lossy().to_string()),
                    "resources": {
                        "extensions": pkg.groups.extensions.iter().map(resource_json).collect::<Vec<_>>(),
                        "skills": pkg.groups.skills.iter().map(resource_json).collect::<Vec<_>>(),
                        "prompts": pkg.groups.prompts.iter().map(resource_json).collect::<Vec<_>>(),
                        "themes": pkg.groups.themes.iter().map(resource_json).collect::<Vec<_>>(),
                    }
                })
            }).collect::<Vec<_>>(),
        })
    }
}

/// Build the configuration model for one scope. Refuses project scope when the
/// project is not trusted (via [`PackageManager::discover_scope_packages`]).
pub fn load_config_model(
    cwd: &Path,
    agent_dir: &Path,
    project_trusted: bool,
    scope: PackageScope,
) -> Result<PackageConfigModel> {
    let manager = PackageManager::with_agent_dir(cwd, agent_dir, project_trusted)
        .context("constructing package manager")?;
    let settings = SettingsManager::load_phase_one(cwd, agent_dir).context("loading settings")?;
    settings.load_project(project_trusted)?;

    let original = match scope {
        PackageScope::Global => settings.global_settings().packages,
        PackageScope::Project => settings.project_settings().packages,
    };

    let discovered = manager.discover_scope_packages(scope)?;
    let by_source: std::collections::HashMap<&str, &ScopePackage> = discovered
        .iter()
        .map(|pkg| (pkg.source.as_str(), pkg))
        .collect();

    let entries = original
        .iter()
        .map(|source_entry| build_entry(source_entry, &by_source))
        .collect::<Result<Vec<_>>>()?;

    Ok(PackageConfigModel {
        cwd: cwd.to_path_buf(),
        agent_dir: agent_dir.to_path_buf(),
        scope,
        project_trusted,
        original,
        entries,
    })
}

fn build_entry(
    source_entry: &PackageSource,
    by_source: &std::collections::HashMap<&str, &ScopePackage>,
) -> Result<ConfigPackage> {
    let source = source_of(source_entry);
    let unsupported = source.trim_start().starts_with("npm:");
    let installed = by_source.contains_key(source);
    if unsupported {
        return Ok(ConfigPackage {
            source: source.to_string(),
            identity: None,
            installed: false,
            unsupported: true,
            root: None,
            manifest: PackageManifest::default(),
            groups: ConfigGroups::default(),
        });
    }
    let Some(pkg) = by_source.get(source) else {
        // Configured but not installed (missing checkout): listed, not toggleable.
        return Ok(ConfigPackage {
            source: source.to_string(),
            identity: None,
            installed: false,
            unsupported: false,
            root: None,
            manifest: PackageManifest::default(),
            groups: ConfigGroups::default(),
        });
    };

    let mut groups = ConfigGroups::default();
    for kind in KINDS {
        let rows = resources_of_kind(&pkg.resources, kind)
            .into_iter()
            .map(|path| {
                let name = relative_posix(&pkg.root, &path);
                let enabled = enabled_for(source_entry, kind, &name);
                ConfigResource {
                    name,
                    path,
                    enabled,
                    original_enabled: enabled,
                }
            })
            .collect::<Vec<_>>();
        *groups.get_mut(kind) = rows;
    }

    Ok(ConfigPackage {
        source: source.to_string(),
        identity: Some(pkg.identity.clone()),
        installed: true,
        unsupported: false,
        root: Some(pkg.root.clone()),
        manifest: pkg.manifest.clone(),
        groups,
    })
}

fn resources_of_kind(resources: &PackageResources, kind: PackageResourceKind) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = match kind {
        PackageResourceKind::Extension => resources
            .extensions
            .iter()
            .map(|r| r.path.clone())
            .collect(),
        PackageResourceKind::Skill => resources.skills.iter().map(|r| r.path.clone()).collect(),
        PackageResourceKind::Prompt => resources.prompts.iter().map(|r| r.path.clone()).collect(),
        PackageResourceKind::Theme => resources.themes.iter().map(|r| r.path.clone()).collect(),
    };
    paths.sort();
    paths.dedup();
    paths
}

/// Compute the enabled state of one resource from its package's settings
/// entry.
///
/// The base enabled state is the package default (`autoload != false`, i.e.
/// all enabled unless `autoload` is explicitly `false`). `+<rel>` force-includes
/// a resource and `-<rel>` force-excludes it (exact match, last write wins).
/// Manual glob tokens are preserved on apply but not interpreted for display.
/// The typed settings model cannot distinguish a missing kind array from an
/// empty one (both deserialize to `Vec::new()`), so an empty array is treated
/// as the default rather than "disable all" — the config selector never emits
/// empty arrays, and the load side is what would honor an explicit `[]`.
fn enabled_for(entry: &PackageSource, kind: PackageResourceKind, rel: &str) -> bool {
    let (autoload, arrays) = unpack_entry(entry);
    let default = autoload != Some(false);
    let array = kind_array(&arrays, kind);
    let mut enabled = default;
    for token in array {
        if let Some(r) = token.strip_prefix('+') {
            if r == rel {
                enabled = true;
            }
        } else if let Some(r) = token.strip_prefix('-') {
            if r == rel {
                enabled = false;
            }
        }
    }
    enabled
}

fn kind_array(arrays: &[Vec<String>; 4], kind: PackageResourceKind) -> &Vec<String> {
    match kind {
        PackageResourceKind::Extension => &arrays[0],
        PackageResourceKind::Skill => &arrays[1],
        PackageResourceKind::Prompt => &arrays[2],
        PackageResourceKind::Theme => &arrays[3],
    }
}

/// Unpack a [`PackageSource`] into `(autoload, per-kind token arrays)`.
fn unpack_entry(entry: &PackageSource) -> (Option<bool>, [Vec<String>; 4]) {
    match entry {
        PackageSource::All(_) => (None, [Vec::new(), Vec::new(), Vec::new(), Vec::new()]),
        PackageSource::Filtered(pkg) => (
            pkg.autoload,
            [
                pkg.extensions.clone(),
                pkg.skills.clone(),
                pkg.prompts.clone(),
                pkg.themes.clone(),
            ],
        ),
    }
}

/// Apply a package's toggled resources to its settings entry, returning the new
/// [`PackageSource`]. Toggled resources emit `+<rel>`/`-<rel>` tokens; all other
/// tokens (manual globs, other resources) are preserved. Entries collapse to a
/// bare source string when no kind array carries any token.
fn apply_toggles(original: &PackageSource, entry: &ConfigPackage) -> PackageSource {
    let source = source_of(original).to_string();
    // Start from a mutable Filtered form; All is materialized with autoload
    // None so the original (which carries no autoload) is preserved on collapse.
    let mut pkg = match original {
        PackageSource::All(_) => ResourcePackage {
            source: source.clone(),
            autoload: None,
            extensions: Vec::new(),
            skills: Vec::new(),
            prompts: Vec::new(),
            themes: Vec::new(),
        },
        PackageSource::Filtered(filtered) => filtered.clone(),
    };

    for kind in KINDS {
        let toggled: Vec<&ConfigResource> = entry
            .groups
            .get(kind)
            .iter()
            .filter(|res| res.enabled != res.original_enabled)
            .collect();
        if toggled.is_empty() {
            continue;
        }
        let array = array_for_kind_mut(&mut pkg, kind);
        for res in toggled {
            array.retain(|token| strip_token_prefix(token) != res.name);
            array.push(if res.enabled {
                format!("+{}", res.name)
            } else {
                format!("-{}", res.name)
            });
        }
    }

    if pkg.extensions.is_empty()
        && pkg.skills.is_empty()
        && pkg.prompts.is_empty()
        && pkg.themes.is_empty()
    {
        PackageSource::All(source)
    } else {
        PackageSource::Filtered(pkg)
    }
}

fn array_for_kind_mut(pkg: &mut ResourcePackage, kind: PackageResourceKind) -> &mut Vec<String> {
    match kind {
        PackageResourceKind::Extension => &mut pkg.extensions,
        PackageResourceKind::Skill => &mut pkg.skills,
        PackageResourceKind::Prompt => &mut pkg.prompts,
        PackageResourceKind::Theme => &mut pkg.themes,
    }
}

// ---------------------------------------------------------------------------
// Command entry point
// ---------------------------------------------------------------------------

/// `rpi config [--local]` dispatch.
///
/// In an interactive terminal it opens the resource selector; with non-TTY
/// stdout it prints the current scope/resource state as deterministic JSON and
/// never blocks for input.
pub async fn config_command(
    cwd: &Path,
    local: bool,
    approve: bool,
    no_approve: bool,
) -> Result<()> {
    let agent_dir = pi_coding::agent_dir_path();
    let headless = !io::stdout().is_terminal();
    let trust = resolve_trust(cwd, &agent_dir, approve, no_approve, headless)?;
    let project_trusted = trust;
    let scope = if local {
        PackageScope::Project
    } else {
        PackageScope::Global
    };

    let model = load_config_model(cwd, &agent_dir, project_trusted, scope)?;

    if headless {
        let json = serde_json::to_string_pretty(&model.to_json())
            .context("serializing config snapshot")?;
        println!("{json}");
        return Ok(());
    }

    run_selector(model).await
}

/// Resolve project trust for a headless/config subcommand using the same
/// store, defaults, and one-run overrides as the package-resource selector.
pub(crate) fn resolve_trust(
    cwd: &Path,
    agent_dir: &Path,
    approve: bool,
    no_approve: bool,
    headless: bool,
) -> Result<bool> {
    let settings = SettingsManager::load_phase_one(cwd, agent_dir)
        .context("loading global settings for config trust policy")?;
    let default = settings
        .global_settings()
        .default_project_trust
        .unwrap_or(DefaultProjectTrust::Ask);
    let override_flag = if approve {
        Some(true)
    } else if no_approve {
        Some(false)
    } else {
        None
    };
    let trust = resolve_project_trust(
        &TrustStore::new(agent_dir),
        cwd,
        override_flag,
        default,
        headless,
    )
    .context("resolving project trust for config")?;
    Ok(trust.allows_project_resources(headless))
}

// ---------------------------------------------------------------------------
// Terminal selector (self-contained raw-mode/alt-screen lifecycle)
// ---------------------------------------------------------------------------

/// Whether the config selector currently owns the terminal. Set on enter,
/// cleared on restore. The config selector runs as a standalone subcommand (the
/// interactive TUI never runs concurrently), so its lifecycle never contends
/// with the TUI's own guard.
static CONFIG_TERM_ACTIVE: AtomicBool = AtomicBool::new(false);
static CONFIG_HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Restore the terminal to cooked mode + the main screen at most once per
/// active config epoch. The atomic swap is the idempotency latch shared by Drop
/// and the panic hook.
fn restore_terminal() {
    if CONFIG_TERM_ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, Show, LeaveAlternateScreen);
        let _ = stdout.flush();
    }
}

/// Install a process-wide panic hook that restores the terminal before the
/// panic message prints, so a panicking selector never strands the user in raw
/// mode. Idempotent. Mirrors the TUI's hook but keyed off the config latch.
fn install_config_panic_hook() {
    if CONFIG_HOOK_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        prev(info);
    }));
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        install_config_panic_hook();
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, Hide) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        let terminal = match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => terminal,
            Err(error) => {
                let mut stdout = io::stdout();
                let _ = execute!(stdout, Show, LeaveAlternateScreen);
                let _ = disable_raw_mode();
                return Err(error.into());
            }
        };
        CONFIG_TERM_ACTIVE.store(true, Ordering::SeqCst);
        Ok(Self { terminal })
    }

    fn draw(&mut self, render: impl FnOnce(&mut ratatui::Frame<'_>)) -> Result<()> {
        self.terminal.draw(render)?;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowKind {
    Package,
    Resource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RowId {
    package: usize,
    kind: Option<PackageResourceKind>,
    resource: Option<usize>,
}

struct FlatRow {
    id: RowId,
    row_kind: RowKind,
    label: String,
    toggleable: bool,
    enabled: bool,
}

fn flatten(model: &PackageConfigModel) -> Vec<FlatRow> {
    let mut rows = Vec::new();
    for (pidx, pkg) in model.entries.iter().enumerate() {
        let label = if let Some(identity) = &pkg.identity {
            format!("{}  ({identity})", pkg.source)
        } else if pkg.unsupported {
            format!("{}  (unsupported)", pkg.source)
        } else {
            format!("{}  (not installed)", pkg.source)
        };
        rows.push(FlatRow {
            id: RowId {
                package: pidx,
                kind: None,
                resource: None,
            },
            row_kind: RowKind::Package,
            label,
            toggleable: false,
            enabled: false,
        });
        if !pkg.installed || pkg.groups.is_empty() {
            continue;
        }
        for kind in KINDS {
            let group = pkg.groups.get(kind);
            if group.is_empty() {
                continue;
            }
            rows.push(FlatRow {
                id: RowId {
                    package: pidx,
                    kind: Some(kind),
                    resource: None,
                },
                row_kind: RowKind::Package,
                label: kind_plural(kind).to_string(),
                toggleable: false,
                enabled: false,
            });
            for (ridx, res) in group.iter().enumerate() {
                rows.push(FlatRow {
                    id: RowId {
                        package: pidx,
                        kind: Some(kind),
                        resource: Some(ridx),
                    },
                    row_kind: RowKind::Resource,
                    label: res.name.clone(),
                    toggleable: true,
                    enabled: res.enabled,
                });
            }
        }
    }
    rows
}

async fn run_selector(mut model: PackageConfigModel) -> Result<()> {
    // Same NO_COLOR override as the main TUI: this interactive editor renders
    // in full color on capable terminals regardless of NO_COLOR.
    crate::force_tui_color();
    let mut guard = TerminalGuard::enter()?;
    let mut input = EventStream::new();
    let mut list_state = ListState::default();
    list_state.select(Some(0));
    let mut status: Option<String> = None;
    let rows_empty_note = if model.entries.is_empty() {
        "No configured packages for this scope."
    } else {
        ""
    };

    loop {
        let rows = flatten(&model);
        let count = rows.len();
        guard.draw(|frame| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(3), Constraint::Length(3)])
                .split(frame.area());

            let scope_label = match model.scope() {
                PackageScope::Global => "global",
                PackageScope::Project => "project",
            };
            let trust_label = if model.project_trusted() {
                "trusted"
            } else {
                "untrusted"
            };
            let title = format!(" rpi config — {scope_label} scope ({trust_label}) ");

            let items: Vec<ListItem> = if rows.is_empty() {
                vec![ListItem::new(Line::from(Span::styled(
                    rows_empty_note.to_string(),
                    Style::default().fg(Color::DarkGray),
                )))]
            } else {
                rows.iter()
                    .map(|row| {
                        let indent = match (row.row_kind, row.id.kind) {
                            (RowKind::Package, None) => "",
                            (RowKind::Package, Some(_)) => "  ",
                            (RowKind::Resource, _) => "    ",
                        };
                        let marker = if row.toggleable {
                            if row.enabled { "[x]" } else { "[ ]" }
                        } else if row.id.kind.is_some() && row.row_kind == RowKind::Package {
                            "▸"
                        } else {
                            "•"
                        };
                        let style = if row.row_kind == RowKind::Package && row.id.kind.is_none() {
                            Style::default().add_modifier(Modifier::BOLD)
                        } else if row.toggleable && row.enabled {
                            Style::default().fg(Color::Green)
                        } else if row.toggleable {
                            Style::default().fg(Color::DarkGray)
                        } else {
                            Style::default()
                        };
                        ListItem::new(Line::from(Span::styled(
                            format!("{indent}{marker} {}", row.label),
                            style,
                        )))
                    })
                    .collect()
            };

            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(title))
                .highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::REVERSED),
                );
            frame.render_stateful_widget(list, chunks[0], &mut list_state);

            let status_text = status.clone().unwrap_or_else(|| {
                "↑↓ move · space toggle · tab switch scope · a apply · esc/q cancel".to_string()
            });
            let footer = Paragraph::new(status_text)
                .alignment(Alignment::Left)
                .block(Block::default().borders(Borders::ALL).title(" help "));
            frame.render_widget(footer, chunks[1]);
        })?;

        let Some(event) = input.next().await else {
            return Ok(());
        };
        let event = event?;
        let key = match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => key,
            Event::Resize(_, _) => continue,
            _ => continue,
        };

        match key.code {
            KeyCode::Up | KeyCode::Char('k') => move_cursor(&mut list_state, count, -1),
            KeyCode::Down | KeyCode::Char('j') => move_cursor(&mut list_state, count, 1),
            KeyCode::Char(' ') | KeyCode::Enter => {
                let id = current_id(&list_state, &rows);
                if let Some(kind) = id.kind
                    && let Some(ridx) = id.resource
                {
                    model.toggle(id.package, kind, ridx);
                    status = None;
                }
            }
            KeyCode::Tab | KeyCode::Char('\t') => {
                let target = match model.scope() {
                    PackageScope::Global => PackageScope::Project,
                    PackageScope::Project => PackageScope::Global,
                };
                match model.switch_scope(target) {
                    Ok(()) => {
                        list_state.select(Some(0));
                        status = None;
                    }
                    Err(error) => {
                        status = Some(format!("cannot switch: {}", error));
                    }
                }
            }
            KeyCode::Char('a') => match model.apply() {
                Ok(()) => {
                    restore_terminal();
                    println!(
                        "applied package resource configuration ({} scope)",
                        model.scope().label()
                    );
                    return Ok(());
                }
                Err(error) => {
                    status = Some(format!("apply failed: {error}"));
                }
            },
            KeyCode::Esc | KeyCode::Char('q') => {
                restore_terminal();
                return Ok(());
            }
            _ => {}
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            restore_terminal();
            return Ok(());
        }
    }
}

fn current_id(state: &ListState, rows: &[FlatRow]) -> RowId {
    state
        .selected()
        .and_then(|i| rows.get(i).map(|r| r.id))
        .unwrap_or(RowId {
            package: 0,
            kind: None,
            resource: None,
        })
}

fn move_cursor(state: &mut ListState, count: usize, delta: i32) {
    if count == 0 {
        state.select(None);
        return;
    }
    let current = state.selected().unwrap_or(0) as i32;
    let mut next = current + delta;
    if next < 0 {
        next = count as i32 - 1;
    } else if next >= count as i32 {
        next = 0;
    }
    state.select(Some(next as usize));
}

#[cfg(test)]
mod tests {
    // Tests live in the integration test crate; this module is kept for any
    // future unit-level assertions that do not require a filesystem fixture.
}
