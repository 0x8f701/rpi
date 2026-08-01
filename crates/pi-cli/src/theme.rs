//! Semantic TUI themes, terminal background detection, and live theme reloads.
//!
//! Colors are resolved before they become active. Custom files extend either
//! built-in palette and may use snake_case (Rust-native) or camelCase (upstream)
//! role names. Reloads are validate-then-apply: malformed or temporarily missing
//! files never replace the last successfully loaded palette.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ratatui::style::Color;
use serde::Deserialize;

/// The terminal background family used for the automatic built-in selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalTheme {
    Dark,
    Light,
}

/// Evidence used to choose a terminal theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalThemeSource {
    Osc11,
    ColorFgBg,
    Fallback,
}

/// Result of safe terminal background detection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalThemeDetection {
    pub theme: TerminalTheme,
    pub source: TerminalThemeSource,
    pub confident: bool,
}

/// Complete semantic palette used by the TUI renderer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub accent: Color,
    pub border: Color,
    pub border_accent: Color,
    pub border_muted: Color,
    pub success: Color,
    pub error: Color,
    pub warning: Color,
    pub muted: Color,
    pub dim: Color,
    pub text: Color,
    pub thinking_text: Color,
    pub user_message_text: Color,
    pub custom_message_text: Color,
    pub custom_message_label: Color,
    pub tool_title: Color,
    pub tool_output: Color,
    pub md_heading: Color,
    pub md_link: Color,
    pub md_link_url: Color,
    pub md_code: Color,
    pub md_code_block: Color,
    pub md_code_block_border: Color,
    pub md_quote: Color,
    pub md_quote_border: Color,
    pub md_hr: Color,
    pub md_list_bullet: Color,
    pub tool_diff_added: Color,
    pub tool_diff_removed: Color,
    pub tool_diff_context: Color,
    pub syntax_comment: Color,
    pub syntax_keyword: Color,
    pub syntax_function: Color,
    pub syntax_variable: Color,
    pub syntax_string: Color,
    pub syntax_number: Color,
    pub syntax_type: Color,
    pub syntax_operator: Color,
    pub syntax_punctuation: Color,
    pub thinking_off: Color,
    pub thinking_minimal: Color,
    pub thinking_low: Color,
    pub thinking_medium: Color,
    pub thinking_high: Color,
    pub thinking_xhigh: Color,
    pub thinking_max: Color,
    pub bash_mode: Color,
    pub selected_bg: Color,
    pub user_message_bg: Color,
    pub custom_message_bg: Color,
    pub tool_pending_bg: Color,
    pub tool_success_bg: Color,
    pub tool_error_bg: Color,
    /// Optional HTML export backgrounds (`export` in upstream theme JSON).
    pub export: ThemeExport,
}

/// Resolved optional colors for HTML session export.
///
/// Upstream themes may set `export.pageBg` / `cardBg` / `infoBg`. When absent,
/// HTML export derives defaults from the TUI palette (typically `userMessageBg`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ThemeExport {
    pub page_bg: Option<Color>,
    pub card_bg: Option<Color>,
    pub info_bg: Option<Color>,
}

/// Built-in dark palette matching OMP's observed titanium interaction theme.
pub const DARK: Theme = Theme {
    accent: rgb(0x00, 0xb4, 0xff), border: rgb(0x2a, 0x30, 0x38), border_accent: rgb(0x00, 0xb4, 0xff), border_muted: rgb(0x1f, 0x25, 0x2d),
    success: rgb(0x00, 0xff, 0x88), error: rgb(0xff, 0x47, 0x57), warning: rgb(0xff, 0xb3, 0x47), muted: rgb(0x9c, 0xa3, 0xb0), dim: rgb(0x6b, 0x72, 0x80), text: Color::Reset,
    thinking_text: rgb(0x9c, 0xa3, 0xb0), user_message_text: Color::Reset, custom_message_text: Color::Reset, custom_message_label: rgb(0xd4, 0xc0, 0x90), tool_title: Color::Reset, tool_output: rgb(0x9c, 0xa3, 0xb0),
    md_heading: rgb(0x00, 0xb4, 0xff), md_link: rgb(0x00, 0xb4, 0xff), md_link_url: rgb(0x00, 0x82, 0xb3), md_code: rgb(0x00, 0xff, 0x88), md_code_block: rgb(0x9c, 0xa3, 0xb0), md_code_block_border: rgb(0x2a, 0x30, 0x38), md_quote: rgb(0x9c, 0xa3, 0xb0), md_quote_border: rgb(0x2a, 0x30, 0x38), md_hr: rgb(0x2a, 0x30, 0x38), md_list_bullet: rgb(0x00, 0xb4, 0xff),
    tool_diff_added: rgb(0x00, 0xff, 0x88), tool_diff_removed: rgb(0xff, 0x47, 0x57), tool_diff_context: rgb(0x9c, 0xa3, 0xb0),
    syntax_comment: rgb(0x6b, 0x72, 0x80), syntax_keyword: rgb(0x00, 0xb4, 0xff), syntax_function: rgb(0x00, 0xff, 0x88), syntax_variable: rgb(0xe8, 0xec, 0xf4), syntax_string: rgb(0xd4, 0xc0, 0x90), syntax_number: rgb(0xff, 0xb3, 0x47), syntax_type: rgb(0x00, 0xb4, 0xff), syntax_operator: rgb(0x00, 0xb4, 0xff), syntax_punctuation: rgb(0x9c, 0xa3, 0xb0),
    thinking_off: rgb(0x4a, 0x50, 0x58), thinking_minimal: rgb(0x5a, 0x60, 0x68), thinking_low: rgb(0x6a, 0x70, 0x78), thinking_medium: rgb(0x9c, 0xa3, 0xb0), thinking_high: rgb(0x00, 0xb4, 0xff), thinking_xhigh: rgb(0xd4, 0xc0, 0x90), thinking_max: rgb(0xd4, 0xc0, 0x90), bash_mode: rgb(0x00, 0xff, 0x88),
    selected_bg: rgb(0x00, 0x82, 0xb3), user_message_bg: rgb(0x0f, 0x12, 0x16), custom_message_bg: rgb(0x2a, 0x30, 0x38), tool_pending_bg: rgb(0x0f, 0x12, 0x16), tool_success_bg: rgb(0x0f, 0x12, 0x16), tool_error_bg: rgb(0x1a, 0x0f, 0x10),
    export: ThemeExport { page_bg: Some(rgb(0x15, 0x18, 0x20)), card_bg: Some(rgb(0x0f, 0x12, 0x16)), info_bg: Some(rgb(0x2a, 0x30, 0x38)) },
};

/// Built-in light palette, faithful to the installed Oh My Pi 17.1.8 `light`
/// theme (`src/modes/theme/light.json`). Upstream variable references are
/// resolved to RGB for ratatui. Upstream empty-string roles (`text`,
/// `userMessageText`, `customMessageText`, `toolTitle`) mean "default
/// terminal color" and map to `Color::Reset`, matching the live TUI rendering
/// path. `thinkingMax` is optional upstream and falls back to `thinkingXhigh`
/// (`#8b008b`) when omitted.
pub const LIGHT: Theme = Theme {
    accent: rgb(0x5a, 0x80, 0x80),
    border: rgb(0x54, 0x7d, 0xa7),
    border_accent: rgb(0x5a, 0x80, 0x80),
    border_muted: rgb(0xb0, 0xb0, 0xb0),
    success: rgb(0x58, 0x84, 0x58),
    error: rgb(0xaa, 0x55, 0x55),
    warning: rgb(0x9a, 0x73, 0x26),
    muted: rgb(0x6c, 0x6c, 0x6c),
    dim: rgb(0x76, 0x76, 0x76),
    text: Color::Reset,
    thinking_text: rgb(0x6c, 0x6c, 0x6c),
    user_message_text: Color::Reset,
    custom_message_text: Color::Reset,
    custom_message_label: rgb(0x7e, 0x57, 0xc2),
    tool_title: Color::Reset,
    tool_output: rgb(0x6c, 0x6c, 0x6c),
    md_heading: rgb(0x9a, 0x73, 0x26),
    md_link: rgb(0x54, 0x7d, 0xa7),
    md_link_url: rgb(0x76, 0x76, 0x76),
    md_code: rgb(0x5a, 0x80, 0x80),
    md_code_block: rgb(0x58, 0x84, 0x58),
    md_code_block_border: rgb(0x6c, 0x6c, 0x6c),
    md_quote: rgb(0x6c, 0x6c, 0x6c),
    md_quote_border: rgb(0x6c, 0x6c, 0x6c),
    md_hr: rgb(0x6c, 0x6c, 0x6c),
    md_list_bullet: rgb(0x58, 0x84, 0x58),
    tool_diff_added: rgb(0x58, 0x84, 0x58),
    tool_diff_removed: rgb(0xaa, 0x55, 0x55),
    tool_diff_context: rgb(0x6c, 0x6c, 0x6c),
    syntax_comment: rgb(0x00, 0x80, 0x00),
    syntax_keyword: rgb(0x00, 0x00, 0xff),
    syntax_function: rgb(0x79, 0x5e, 0x26),
    syntax_variable: rgb(0x00, 0x10, 0x80),
    syntax_string: rgb(0xa3, 0x15, 0x15),
    syntax_number: rgb(0x09, 0x86, 0x58),
    syntax_type: rgb(0x26, 0x7f, 0x99),
    syntax_operator: rgb(0x00, 0x00, 0x00),
    syntax_punctuation: rgb(0x00, 0x00, 0x00),
    thinking_off: rgb(0xb0, 0xb0, 0xb0),
    thinking_minimal: rgb(0x76, 0x76, 0x76),
    thinking_low: rgb(0x54, 0x7d, 0xa7),
    thinking_medium: rgb(0x5a, 0x80, 0x80),
    thinking_high: rgb(0x87, 0x5f, 0x87),
    thinking_xhigh: rgb(0x8b, 0x00, 0x8b),
    thinking_max: rgb(0x8b, 0x00, 0x8b),
    bash_mode: rgb(0x58, 0x84, 0x58),
    selected_bg: rgb(0xd0, 0xd0, 0xe0),
    user_message_bg: rgb(0xe8, 0xe8, 0xe8),
    custom_message_bg: rgb(0xed, 0xe7, 0xf6),
    tool_pending_bg: rgb(0xe8, 0xe8, 0xf0),
    tool_success_bg: rgb(0xe8, 0xf0, 0xe8),
    tool_error_bg: rgb(0xf0, 0xe8, 0xe8),
    export: ThemeExport {
        page_bg: None,
        card_bg: None,
        info_bg: None,
    },
};

const fn rgb(red: u8, green: u8, blue: u8) -> Color {
    Color::Rgb(red, green, blue)
}

#[derive(Clone)]
struct NamedTheme {
    name: String,
    theme: Theme,
    source: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

/// Result of polling the watched theme resources.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ThemeReload {
    pub changed: bool,
    pub diagnostics: Vec<String>,
}

/// Catalog and live-reload state for built-in and custom themes.
pub struct ThemeManager {
    themes: Vec<NamedTheme>,
    active: String,
    diagnostics: Vec<String>,
    directories: Vec<PathBuf>,
    explicit_paths: Vec<PathBuf>,
    observed: BTreeMap<PathBuf, Option<FileStamp>>,
}

impl Default for ThemeManager {
    fn default() -> Self {
        Self {
            themes: vec![
                NamedTheme {
                    name: "dark".to_owned(),
                    theme: DARK,
                    source: None,
                },
                NamedTheme {
                    name: "light".to_owned(),
                    theme: LIGHT,
                    source: None,
                },
            ],
            active: "dark".to_owned(),
            diagnostics: Vec::new(),
            directories: Vec::new(),
            explicit_paths: Vec::new(),
            observed: BTreeMap::new(),
        }
    }
}

impl ThemeManager {
    /// Loads themes from directories and chooses a built-in from safe terminal
    /// background hints. Directory order remains global-first/project-last.
    pub fn load(directories: Vec<PathBuf>) -> Self {
        Self::load_sources(directories, Vec::new())
    }

    /// Loads directory themes plus explicitly registered theme files.
    pub fn load_sources(directories: Vec<PathBuf>, explicit_paths: Vec<PathBuf>) -> Self {
        let mut manager = Self::default();
        manager.directories = directories;
        manager.explicit_paths = explicit_paths;
        manager.active = match detect_terminal_background(None).theme {
            TerminalTheme::Dark => "dark",
            TerminalTheme::Light => "light",
        }
        .to_owned();
        for path in manager.discover_files() {
            manager.observe_initial(path);
        }
        manager
    }

    /// The active palette.
    pub fn theme(&self) -> Theme {
        self.themes
            .iter()
            .find(|theme| theme.name == self.active)
            .map_or(DARK, |theme| theme.theme)
    }

    /// The active display name.
    pub fn active_name(&self) -> &str {
        &self.active
    }

    /// Theme names in deterministic catalog order.
    pub fn names(&self) -> Vec<String> {
        self.themes.iter().map(|theme| theme.name.clone()).collect()
    }

    /// Initial per-file load diagnostics.
    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    /// Switches to an already validated theme.
    pub fn switch_by_name(&mut self, name: &str) -> Result<(), String> {
        if self.themes.iter().any(|theme| theme.name == name) {
            self.active = name.to_owned();
            Ok(())
        } else {
            Err(format!(
                "unknown theme `{name}`; available: {}",
                self.names().join(", ")
            ))
        }
    }

    /// Cycles through the validated catalog.
    pub fn cycle(&mut self, delta: i32) {
        let Some(current) = self
            .themes
            .iter()
            .position(|theme| theme.name == self.active)
        else {
            self.active = "dark".to_owned();
            return;
        };
        let next = (current as i32 + delta.signum()).rem_euclid(self.themes.len() as i32) as usize;
        self.active.clone_from(&self.themes[next].name);
    }

    /// Polls theme sources. Successfully parsed files replace their prior
    /// generation atomically. Invalid and missing files retain the prior theme.
    pub fn reload_if_changed(&mut self) -> ThemeReload {
        let mut reload = ThemeReload::default();
        let discovered = self.discover_files();
        let mut candidates = discovered;
        let mut seen = candidates.iter().cloned().collect::<BTreeSet<_>>();
        for path in self.observed.keys() {
            if seen.insert(path.clone()) {
                candidates.push(path.clone());
            }
        }
        for path in candidates {
            let stamp = file_stamp(&path);
            if self
                .observed
                .get(&path)
                .is_some_and(|observed| *observed == stamp)
            {
                continue;
            }
            self.observed.insert(path.clone(), stamp);
            let Some(_) = stamp else {
                continue;
            };
            match load_theme_file(&path) {
                Ok((name, theme)) => {
                    self.replace_source(&path, name, theme);
                    reload.changed = true;
                }
                Err(error) => reload
                    .diagnostics
                    .push(format!("{}: {error}", path.display())),
            }
        }
        reload
    }

    fn observe_initial(&mut self, path: PathBuf) {
        self.observed.insert(path.clone(), file_stamp(&path));
        match load_theme_file(&path) {
            Ok((name, theme)) => self.replace_source(&path, name, theme),
            Err(error) => self
                .diagnostics
                .push(format!("{}: {error}", path.display())),
        }
    }

    fn replace_source(&mut self, path: &Path, name: String, theme: Theme) {
        let previous = self
            .themes
            .iter()
            .position(|candidate| candidate.source.as_deref() == Some(path));
        if let Some(index) = previous {
            let old_name = self.themes[index].name.clone();
            self.themes.remove(index);
            if self.active == old_name {
                self.active.clone_from(&name);
            }
        }
        let named = NamedTheme {
            name: name.clone(),
            theme,
            source: Some(path.to_path_buf()),
        };
        if let Some(index) = self
            .themes
            .iter()
            .position(|candidate| candidate.name == name)
        {
            self.themes[index] = named;
        } else {
            self.themes.push(named);
        }
    }

    fn discover_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let mut seen = BTreeSet::new();
        for directory in &self.directories {
            for path in list_theme_files(directory) {
                if seen.insert(path.clone()) {
                    files.push(path);
                }
            }
        }
        for explicit in &self.explicit_paths {
            let paths = if explicit.is_dir() {
                list_theme_files(explicit)
            } else {
                vec![explicit.clone()]
            };
            for path in paths {
                if seen.insert(path.clone()) {
                    files.push(path);
                }
            }
        }
        files
    }
}

fn list_theme_files(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let metadata = fs::metadata(path).ok()?;
    Some(FileStamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ColorValue {
    String(String),
    Index(u16),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeExportFile {
    #[serde(default, rename = "pageBg")]
    page_bg: Option<ColorValue>,
    #[serde(default, rename = "cardBg")]
    card_bg: Option<ColorValue>,
    #[serde(default, rename = "infoBg")]
    info_bg: Option<ColorValue>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeFile {
    #[serde(default, rename = "$schema")]
    _schema: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    extends: Option<String>,
    #[serde(default)]
    vars: BTreeMap<String, ColorValue>,
    #[serde(default)]
    colors: BTreeMap<String, ColorValue>,
    /// Optional HTML export palette (`pageBg` / `cardBg` / `infoBg`).
    #[serde(default)]
    export: Option<ThemeExportFile>,
}

fn load_theme_file(path: &Path) -> Result<(String, Theme), String> {
    let data = fs::read_to_string(path).map_err(|error| format!("read failed: {error}"))?;
    let parsed: ThemeFile =
        serde_json::from_str(&data).map_err(|error| format!("invalid JSON: {error}"))?;
    let ThemeFile {
        _schema: _,
        name,
        extends,
        vars,
        colors,
        export,
    } = parsed;
    let name = name
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("theme")
                .to_owned()
        });
    if name.contains('/') || name.contains('\\') {
        return Err("theme name must not contain path separators".to_owned());
    }
    let mut theme = match extends.as_deref().map(str::trim) {
        None | Some("dark") => DARK,
        Some("light") => LIGHT,
        Some(other) => {
            return Err(format!(
                "`extends` must be \"dark\" or \"light\", got `{other}`"
            ));
        }
    };
    for (role, value) in &colors {
        let color = resolve_color(value, &vars, &mut BTreeSet::new())?;
        theme.apply_role(role, color)?;
    }
    if let Some(export) = export {
        theme.export = ThemeExport {
            page_bg: export
                .page_bg
                .as_ref()
                .map(|value| resolve_color(value, &vars, &mut BTreeSet::new()))
                .transpose()?,
            card_bg: export
                .card_bg
                .as_ref()
                .map(|value| resolve_color(value, &vars, &mut BTreeSet::new()))
                .transpose()?,
            info_bg: export
                .info_bg
                .as_ref()
                .map(|value| resolve_color(value, &vars, &mut BTreeSet::new()))
                .transpose()?,
        };
    }
    Ok((name, theme))
}

fn resolve_color(
    value: &ColorValue,
    vars: &BTreeMap<String, ColorValue>,
    visited: &mut BTreeSet<String>,
) -> Result<Color, String> {
    match value {
        ColorValue::Index(index) if *index <= 255 => Ok(Color::Indexed(*index as u8)),
        ColorValue::Index(index) => Err(format!("color index {index} is outside 0..255")),
        ColorValue::String(spec) => {
            let spec = spec.trim();
            if spec.is_empty() {
                return Ok(Color::Reset);
            }
            if let Some(color) = parse_color(spec) {
                return Ok(color);
            }
            if !visited.insert(spec.to_owned()) {
                return Err(format!("circular color variable `{spec}`"));
            }
            let variable = vars
                .get(spec)
                .ok_or_else(|| format!("unknown color or variable `{spec}`"))?;
            let resolved = resolve_color(variable, vars, visited);
            visited.remove(spec);
            resolved
        }
    }
}

impl Theme {
    fn apply_role(&mut self, role: &str, color: Color) -> Result<(), String> {
        let normalized = role
            .chars()
            .filter(|character| *character != '_' && *character != '-')
            .flat_map(char::to_lowercase)
            .collect::<String>();
        let slot = match normalized.as_str() {
            "accent" | "userlabel" | "headerlabelbg" => &mut self.accent,
            "border" => &mut self.border,
            "borderaccent" => &mut self.border_accent,
            "bordermuted" => &mut self.border_muted,
            "success" | "assistantlabel" => &mut self.success,
            "error" | "statuserror" => &mut self.error,
            "warning" => &mut self.warning,
            "muted" | "completionitem" => &mut self.muted,
            "dim" | "statusinfo" | "statusbar" => &mut self.dim,
            "text" | "transcripttext" | "completionselectedfg" | "headerlabelfg" => &mut self.text,
            "thinkingtext" => &mut self.thinking_text,
            "usermessagetext" => &mut self.user_message_text,
            "custommessagetext" => &mut self.custom_message_text,
            "custommessagelabel" => &mut self.custom_message_label,
            "tooltitle" => &mut self.tool_title,
            "tooloutput" => &mut self.tool_output,
            "mdheading" => &mut self.md_heading,
            "mdlink" => &mut self.md_link,
            "mdlinkurl" => &mut self.md_link_url,
            "mdcode" => &mut self.md_code,
            "mdcodeblock" => &mut self.md_code_block,
            "mdcodeblockborder" => &mut self.md_code_block_border,
            "mdquote" => &mut self.md_quote,
            "mdquoteborder" => &mut self.md_quote_border,
            "mdhr" => &mut self.md_hr,
            "mdlistbullet" => &mut self.md_list_bullet,
            "tooldiffadded" => &mut self.tool_diff_added,
            "tooldiffremoved" => &mut self.tool_diff_removed,
            "tooldiffcontext" => &mut self.tool_diff_context,
            "syntaxcomment" => &mut self.syntax_comment,
            "syntaxkeyword" => &mut self.syntax_keyword,
            "syntaxfunction" => &mut self.syntax_function,
            "syntaxvariable" => &mut self.syntax_variable,
            "syntaxstring" => &mut self.syntax_string,
            "syntaxnumber" => &mut self.syntax_number,
            "syntaxtype" => &mut self.syntax_type,
            "syntaxoperator" => &mut self.syntax_operator,
            "syntaxpunctuation" => &mut self.syntax_punctuation,
            "thinkingoff" => &mut self.thinking_off,
            "thinkingminimal" => &mut self.thinking_minimal,
            "thinkinglow" => &mut self.thinking_low,
            "thinkingmedium" => &mut self.thinking_medium,
            "thinkinghigh" => &mut self.thinking_high,
            "thinkingxhigh" => &mut self.thinking_xhigh,
            "thinkingmax" => &mut self.thinking_max,
            "bashmode" => &mut self.bash_mode,
            "selectedbg" | "completionselectedbg" => &mut self.selected_bg,
            "usermessagebg" => &mut self.user_message_bg,
            "custommessagebg" => &mut self.custom_message_bg,
            "toolpendingbg" => &mut self.tool_pending_bg,
            "toolsuccessbg" => &mut self.tool_success_bg,
            "toolerrorbg" => &mut self.tool_error_bg,
            _ => return Err(format!("unknown semantic color role `{role}`")),
        };
        *slot = color;
        Ok(())
    }
}

/// Parses named, indexed, or `#rgb`/`#rrggbb` colors.
pub(crate) fn parse_color(spec: &str) -> Option<Color> {
    let lower = spec.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix('#') {
        return parse_hex(rest);
    }
    Some(match lower.as_str() {
        "reset" => Color::Reset,
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        "white" => Color::White,
        _ => return None,
    })
}

fn parse_hex(rest: &str) -> Option<Color> {
    if !rest.chars().all(|character| character.is_ascii_hexdigit()) {
        return None;
    }
    let (red, green, blue) = match rest.len() {
        6 => (
            u8::from_str_radix(rest.get(0..2)?, 16).ok()?,
            u8::from_str_radix(rest.get(2..4)?, 16).ok()?,
            u8::from_str_radix(rest.get(4..6)?, 16).ok()?,
        ),
        3 => (
            u8::from_str_radix(rest.get(0..1)?, 16).ok()? * 0x11,
            u8::from_str_radix(rest.get(1..2)?, 16).ok()? * 0x11,
            u8::from_str_radix(rest.get(2..3)?, 16).ok()? * 0x11,
        ),
        _ => return None,
    };
    Some(Color::Rgb(red, green, blue))
}

/// Detects a light/dark background from an already captured OSC 11 response,
/// falling back to `COLORFGBG` and finally dark. This function never reads from
/// stdin, so it cannot race the crossterm event stream.
pub fn detect_terminal_background(osc11_response: Option<&str>) -> TerminalThemeDetection {
    if let Some((red, green, blue)) = osc11_response.and_then(parse_osc11_rgb) {
        return TerminalThemeDetection {
            theme: theme_for_rgb(red, green, blue),
            source: TerminalThemeSource::Osc11,
            confident: true,
        };
    }
    if let Some(index) = std::env::var("COLORFGBG")
        .ok()
        .as_deref()
        .and_then(colorfgbg_background)
    {
        let (red, green, blue) = ansi_256_rgb(index);
        return TerminalThemeDetection {
            theme: theme_for_rgb(red, green, blue),
            source: TerminalThemeSource::ColorFgBg,
            confident: true,
        };
    }
    TerminalThemeDetection {
        theme: TerminalTheme::Dark,
        source: TerminalThemeSource::Fallback,
        confident: false,
    }
}

fn colorfgbg_background(value: &str) -> Option<u8> {
    value
        .split(';')
        .rev()
        .find_map(|part| {
            part.trim()
                .parse::<u16>()
                .ok()
                .filter(|index| *index <= 255)
        })
        .map(|index| index as u8)
}

fn parse_osc11_rgb(response: &str) -> Option<(u8, u8, u8)> {
    let payload = response
        .split("11;")
        .nth(1)?
        .trim_end_matches(['\u{7}', '\u{1b}', '\\']);
    if let Some(hex) = payload.strip_prefix('#') {
        return match parse_hex(hex)? {
            Color::Rgb(red, green, blue) => Some((red, green, blue)),
            _ => None,
        };
    }
    let channels = payload.strip_prefix("rgb:")?.split('/').collect::<Vec<_>>();
    if channels.len() != 3 {
        return None;
    }
    Some((
        scale_osc_channel(channels[0])?,
        scale_osc_channel(channels[1])?,
        scale_osc_channel(channels[2])?,
    ))
}

fn scale_osc_channel(channel: &str) -> Option<u8> {
    if channel.is_empty()
        || channel.len() > 4
        || !channel.chars().all(|value| value.is_ascii_hexdigit())
    {
        return None;
    }
    let value = u32::from_str_radix(channel, 16).ok()?;
    let maximum = (1_u32 << (channel.len() * 4)) - 1;
    Some(((value * 255 + maximum / 2) / maximum) as u8)
}

fn theme_for_rgb(red: u8, green: u8, blue: u8) -> TerminalTheme {
    let linear = |channel: u8| {
        let value = f64::from(channel) / 255.0;
        if value <= 0.039_28 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    let luminance = 0.2126 * linear(red) + 0.7152 * linear(green) + 0.0722 * linear(blue);
    if luminance >= 0.5 {
        TerminalTheme::Light
    } else {
        TerminalTheme::Dark
    }
}

fn ansi_256_rgb(index: u8) -> (u8, u8, u8) {
    const BASIC: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    match index {
        0..=15 => BASIC[usize::from(index)],
        16..=231 => {
            let value = index - 16;
            let channel = |component: u8| {
                if component == 0 {
                    0
                } else {
                    55 + component * 40
                }
            };
            (
                channel(value / 36),
                channel((value / 6) % 6),
                channel(value % 6),
            )
        }
        232..=255 => {
            let gray = 8 + (index - 232) * 10;
            (gray, gray, gray)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc_detection_precedes_environment_fallback() {
        let detected = detect_terminal_background(Some("\u{1b}]11;rgb:ffff/ffff/ffff\u{7}"));
        assert_eq!(detected.theme, TerminalTheme::Light);
        assert_eq!(detected.source, TerminalThemeSource::Osc11);
    }

    #[test]
    fn custom_extends_can_override_every_role_and_use_variables() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("complete.json");
        let roles = [
            "accent",
            "border",
            "borderAccent",
            "borderMuted",
            "success",
            "error",
            "warning",
            "muted",
            "dim",
            "text",
            "thinkingText",
            "userMessageText",
            "customMessageText",
            "customMessageLabel",
            "toolTitle",
            "toolOutput",
            "mdHeading",
            "mdLink",
            "mdLinkUrl",
            "mdCode",
            "mdCodeBlock",
            "mdCodeBlockBorder",
            "mdQuote",
            "mdQuoteBorder",
            "mdHr",
            "mdListBullet",
            "toolDiffAdded",
            "toolDiffRemoved",
            "toolDiffContext",
            "syntaxComment",
            "syntaxKeyword",
            "syntaxFunction",
            "syntaxVariable",
            "syntaxString",
            "syntaxNumber",
            "syntaxType",
            "syntaxOperator",
            "syntaxPunctuation",
            "thinkingOff",
            "thinkingMinimal",
            "thinkingLow",
            "thinkingMedium",
            "thinkingHigh",
            "thinkingXhigh",
            "thinkingMax",
            "bashMode",
            "selectedBg",
            "userMessageBg",
            "customMessageBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ];
        let colors = roles
            .iter()
            .map(|role| format!("\"{role}\":\"all\""))
            .collect::<Vec<_>>()
            .join(",");
        fs::write(
            &path,
            format!("{{\"name\":\"complete\",\"extends\":\"light\",\"vars\":{{\"all\":\"#123456\"}},\"colors\":{{{colors}}}}}"),
        )
        .unwrap();
        let (_, theme) = load_theme_file(&path).unwrap();
        assert_eq!(theme.accent, Color::Rgb(0x12, 0x34, 0x56));
        assert_eq!(theme.tool_error_bg, Color::Rgb(0x12, 0x34, 0x56));
        assert_eq!(theme.thinking_max, Color::Rgb(0x12, 0x34, 0x56));
    }

    #[test]
    fn invalid_live_reload_keeps_the_last_good_theme() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("custom.json");
        fs::write(
            &path,
            r##"{"name":"custom","colors":{"accent":"#123456"}}"##,
        )
        .unwrap();
        let mut manager = ThemeManager::load(vec![directory.path().to_path_buf()]);
        manager.switch_by_name("custom").unwrap();
        let before = manager.theme();
        fs::write(&path, "{ invalid and deliberately longer JSON").unwrap();
        let reload = manager.reload_if_changed();
        assert!(!reload.diagnostics.is_empty());
        assert_eq!(manager.theme(), before);
        assert_eq!(manager.active_name(), "custom");
    }

    #[test]
    fn dark_palette_matches_observed_omp_titanium_values() {
        assert_eq!(DARK.accent, Color::Rgb(0x00, 0xb4, 0xff));
        assert_eq!(DARK.border, Color::Rgb(0x2a, 0x30, 0x38));
        assert_eq!(DARK.border_accent, Color::Rgb(0x00, 0xb4, 0xff));
        assert_eq!(DARK.muted, Color::Rgb(0x9c, 0xa3, 0xb0));
        assert_eq!(DARK.syntax_variable, Color::Rgb(0xe8, 0xec, 0xf4));
        assert_eq!(DARK.user_message_bg, Color::Rgb(0x0f, 0x12, 0x16));
        assert_eq!(DARK.user_message_text, Color::Reset);
        assert_eq!(DARK.custom_message_text, Color::Reset);
        assert_eq!(DARK.tool_title, Color::Reset);
        assert_eq!(DARK.custom_message_label, Color::Rgb(0xd4, 0xc0, 0x90));
        assert_eq!(DARK.custom_message_bg, Color::Rgb(0x2a, 0x30, 0x38));
        assert_eq!(DARK.md_heading, Color::Rgb(0x00, 0xb4, 0xff));
        assert_eq!(DARK.md_link, Color::Rgb(0x00, 0xb4, 0xff));
        assert_eq!(DARK.md_code, Color::Rgb(0x00, 0xff, 0x88));
        assert_eq!(DARK.tool_diff_added, Color::Rgb(0x00, 0xff, 0x88));
        assert_eq!(DARK.tool_diff_removed, Color::Rgb(0xff, 0x47, 0x57));
        assert_eq!(DARK.syntax_comment, Color::Rgb(0x6b, 0x72, 0x80));
        assert_eq!(DARK.syntax_keyword, Color::Rgb(0x00, 0xb4, 0xff));
        assert_eq!(DARK.syntax_function, Color::Rgb(0x00, 0xff, 0x88));
        assert_eq!(DARK.thinking_off, Color::Rgb(0x4a, 0x50, 0x58));
        assert_eq!(DARK.thinking_xhigh, Color::Rgb(0xd4, 0xc0, 0x90));
        assert_eq!(DARK.thinking_max, DARK.thinking_xhigh);
        assert_eq!(DARK.bash_mode, Color::Rgb(0x00, 0xff, 0x88));
        assert_eq!(DARK.selected_bg, Color::Rgb(0x00, 0x82, 0xb3));
        assert_eq!(DARK.tool_pending_bg, Color::Rgb(0x0f, 0x12, 0x16));
        assert_eq!(DARK.tool_success_bg, Color::Rgb(0x0f, 0x12, 0x16));
        assert_eq!(DARK.tool_error_bg, Color::Rgb(0x1a, 0x0f, 0x10));
    }

    #[test]
    fn light_palette_matches_omp_light_resolved_values() {
        // Source: @oh-my-pi/pi-coding-agent 17.1.8 src/modes/theme/light.json.
        // Variable refs (teal, blue, green, red, yellow, mediumGray, dimGray,
        // lightGray, selectedBg, userMsgBg, customMsgBg, tool{Pending,Success,
        // Error}Bg) resolved to RGB.
        // accent / borders
        assert_eq!(LIGHT.accent, Color::Rgb(0x5a, 0x80, 0x80));
        assert_eq!(LIGHT.border, Color::Rgb(0x54, 0x7d, 0xa7));
        assert_eq!(LIGHT.border_accent, Color::Rgb(0x5a, 0x80, 0x80));
        assert_eq!(LIGHT.border_muted, Color::Rgb(0xb0, 0xb0, 0xb0));
        // messages: empty upstream roles map to terminal default (Color::Reset)
        assert_eq!(LIGHT.user_message_text, Color::Reset);
        assert_eq!(LIGHT.custom_message_text, Color::Reset);
        assert_eq!(LIGHT.tool_title, Color::Reset);
        assert_eq!(LIGHT.custom_message_label, Color::Rgb(0x7e, 0x57, 0xc2));
        assert_eq!(LIGHT.user_message_bg, Color::Rgb(0xe8, 0xe8, 0xe8));
        assert_eq!(LIGHT.custom_message_bg, Color::Rgb(0xed, 0xe7, 0xf6));
        // markdown
        assert_eq!(LIGHT.md_heading, Color::Rgb(0x9a, 0x73, 0x26));
        assert_eq!(LIGHT.md_link, Color::Rgb(0x54, 0x7d, 0xa7));
        assert_eq!(LIGHT.md_code, Color::Rgb(0x5a, 0x80, 0x80));
        assert_eq!(LIGHT.md_code_block, Color::Rgb(0x58, 0x84, 0x58));
        // diff
        assert_eq!(LIGHT.tool_diff_added, Color::Rgb(0x58, 0x84, 0x58));
        assert_eq!(LIGHT.tool_diff_removed, Color::Rgb(0xaa, 0x55, 0x55));
        assert_eq!(LIGHT.tool_diff_context, Color::Rgb(0x6c, 0x6c, 0x6c));
        // syntax
        assert_eq!(LIGHT.syntax_comment, Color::Rgb(0x00, 0x80, 0x00));
        assert_eq!(LIGHT.syntax_keyword, Color::Rgb(0x00, 0x00, 0xff));
        assert_eq!(LIGHT.syntax_string, Color::Rgb(0xa3, 0x15, 0x15));
        assert_eq!(LIGHT.syntax_number, Color::Rgb(0x09, 0x86, 0x58));
        assert_eq!(LIGHT.syntax_type, Color::Rgb(0x26, 0x7f, 0x99));
        // thinking: thinkingMax is absent upstream and falls back to thinkingXhigh
        assert_eq!(LIGHT.thinking_off, Color::Rgb(0xb0, 0xb0, 0xb0));
        assert_eq!(LIGHT.thinking_xhigh, Color::Rgb(0x8b, 0x00, 0x8b));
        assert_eq!(LIGHT.thinking_max, LIGHT.thinking_xhigh);
        // bash
        assert_eq!(LIGHT.bash_mode, Color::Rgb(0x58, 0x84, 0x58));
        // backgrounds
        assert_eq!(LIGHT.selected_bg, Color::Rgb(0xd0, 0xd0, 0xe0));
        assert_eq!(LIGHT.tool_pending_bg, Color::Rgb(0xe8, 0xe8, 0xf0));
        assert_eq!(LIGHT.tool_success_bg, Color::Rgb(0xe8, 0xf0, 0xe8));
        assert_eq!(LIGHT.tool_error_bg, Color::Rgb(0xf0, 0xe8, 0xe8));
    }

    #[test]
    fn empty_color_role_resolves_to_terminal_default_like_upstream() {
        // OMP renders empty-string roles as the terminal default foreground
        // (\x1b[39m); pi-rs mirrors that with Color::Reset, so a custom theme
        // that leaves a role empty inherits Reset rather than an invented color.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("empty.json");
        fs::write(
            &path,
            r#"{"name":"empty","extends":"dark","colors":{"text":""}}"#,
        )
        .unwrap();
        let (_, theme) = load_theme_file(&path).unwrap();
        assert_eq!(theme.text, Color::Reset);
    }

    #[test]
    fn theme_export_object_parses_canonical_keys_and_rejects_unknown() {
        // Upstream @earendil-works/pi-coding-agent@0.82.1 theme-schema.json
        // defines optional top-level `export` with pageBg/cardBg/infoBg only
        // (additionalProperties: false). Canonical gruvbox-shaped themes ship
        // this block; unknown nested keys and unknown top-level fields stay
        // rejected via deny_unknown_fields.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("dark-gruvbox.json");
        fs::write(
            &path,
            r##"{
                "name": "dark-gruvbox",
                "extends": "dark",
                "vars": {
                    "bg0": "#282828",
                    "bg1": "#3c3836",
                    "userMsgBg": "#1d2021"
                },
                "colors": {
                    "accent": "bg1",
                    "userMessageBg": "userMsgBg"
                },
                "export": {
                    "pageBg": "userMsgBg",
                    "cardBg": "bg0",
                    "infoBg": "#3c3836"
                }
            }"##,
        )
        .unwrap();
        let (name, theme) = load_theme_file(&path).unwrap();
        assert_eq!(name, "dark-gruvbox");
        assert_eq!(theme.user_message_bg, Color::Rgb(0x1d, 0x20, 0x21));
        assert_eq!(
            theme.export,
            ThemeExport {
                page_bg: Some(Color::Rgb(0x1d, 0x20, 0x21)),
                card_bg: Some(Color::Rgb(0x28, 0x28, 0x28)),
                info_bg: Some(Color::Rgb(0x3c, 0x38, 0x36)),
            }
        );

        let bad_export = directory.path().join("bad-export.json");
        fs::write(
            &bad_export,
            r##"{"name":"bad","colors":{"accent":"#ffffff"},"export":{"pageBg":"#000000","mystery":"#ff0000"}}"##,
        )
        .unwrap();
        let error = load_theme_file(&bad_export).unwrap_err();
        assert!(
            error.contains("unknown field") && error.contains("mystery"),
            "expected unknown export key rejection, got: {error}"
        );

        let bad_top = directory.path().join("bad-top.json");
        fs::write(
            &bad_top,
            r##"{"name":"bad","colors":{"accent":"#ffffff"},"notAField":true}"##,
        )
        .unwrap();
        let error = load_theme_file(&bad_top).unwrap_err();
        assert!(
            error.contains("unknown field") && error.contains("notAField"),
            "expected unknown top-level rejection, got: {error}"
        );
    }
}
