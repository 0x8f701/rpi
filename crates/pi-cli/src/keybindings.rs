//! Configurable TUI keybindings routed through stable action IDs.
//!
//! The TUI never checks hard-coded key chords: [`KeyBindingsManager::resolve`]
//! maps an incoming crossterm [`KeyEvent`] to a stable [`Action`], and the
//! dispatch layer matches on `Action`. Bindings are loaded from JSON files
//! supplied by the caller in global-first, project-last order: built-in
//! defaults form the base layer, a global file overlays them, and a project
//! file overlays the result (project-overrides-global).
//!
//! Validation is validate-then-apply per file: a file with any malformed chord,
//! unknown action, or duplicate (canonicalized) chord is rejected in full with
//! an actionable diagnostic — the prior active map is never partially replaced.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Stable upstream action identifiers matched by the dispatch layer. Contextual
/// selector actions are only exposed when their product flow has an executable
/// handler; accepting a configured action and silently dropping it is forbidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    EditorSubmit,
    EditorNewline,
    EditorBackspace,
    EditorDelete,
    EditorLeft,
    EditorRight,
    EditorUp,
    EditorDown,
    EditorWordLeft,
    EditorWordRight,
    EditorHome,
    EditorEnd,
    EditorJumpForward,
    EditorJumpBackward,
    EditorPageUp,
    EditorPageDown,
    EditorDeleteWordBackward,
    EditorDeleteWordForward,
    EditorDeleteToLineStart,
    EditorDeleteToLineEnd,
    EditorYank,
    EditorYankPop,
    EditorUndo,
    Abort,
    ClearEditor,
    Quit,
    AcceptCompletion,
    ThemeNext,
    ThemePrev,
    ClipboardPaste,
    CopyLastAssistant,
    ExternalEditor,
    Suspend,
    ThinkingCycle,
    ThinkingToggle,
    ModelCycleForward,
    ModelSelect,

    ModelCycleBackward,
    ToolsExpand,
    FollowUp,
    Dequeue,
    SessionNew,
    SessionResume,
    SessionTree,
    SessionFork,
    SessionToggleNamedFilter,
    SessionTogglePath,
    SessionToggleSort,
    SessionRename,
    SessionDelete,
    SessionDeleteNoninvasive,
    ModelsSave,
    ModelsEnableAll,
    ModelsClearAll,
    ModelsToggleProvider,
    ModelsReorderUp,
    ModelsReorderDown,
    TreeFoldOrUp,
    TreeUnfoldOrDown,
    TreeEditLabel,
    TreeToggleLabelTimestamp,
    TreeFilterDefault,
    TreeFilterNoTools,
    TreeFilterUserOnly,
    TreeFilterLabeledOnly,
    TreeFilterAll,
    TreeFilterCycleForward,
    TreeFilterCycleBackward,
}

impl Action {
    /// Maps upstream namespaced IDs, plus the legacy snake_case IDs previously
    /// accepted by pi-rs, to an executable action.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name.trim() {
            "tui.input.submit" | "editor_submit" => Self::EditorSubmit,
            "tui.input.newLine" | "editor_newline" => Self::EditorNewline,
            "tui.editor.deleteCharBackward" | "editor_backspace" => Self::EditorBackspace,
            "tui.editor.deleteCharForward" | "editor_delete" => Self::EditorDelete,
            "tui.editor.cursorLeft" | "editor_left" => Self::EditorLeft,
            "tui.editor.cursorRight" | "editor_right" => Self::EditorRight,
            "tui.editor.cursorUp" | "editor_up" => Self::EditorUp,
            "tui.editor.cursorDown" | "editor_down" => Self::EditorDown,
            "tui.editor.cursorWordLeft" => Self::EditorWordLeft,
            "tui.editor.cursorWordRight" => Self::EditorWordRight,
            "tui.editor.cursorLineStart" | "editor_home" => Self::EditorHome,
            "tui.editor.cursorLineEnd" | "editor_end" => Self::EditorEnd,
            "tui.editor.jumpForward" => Self::EditorJumpForward,
            "tui.editor.jumpBackward" => Self::EditorJumpBackward,
            "tui.editor.pageUp" => Self::EditorPageUp,
            "tui.editor.pageDown" => Self::EditorPageDown,
            "tui.editor.deleteWordBackward" => Self::EditorDeleteWordBackward,
            "tui.editor.deleteWordForward" => Self::EditorDeleteWordForward,
            "tui.editor.deleteToLineStart" => Self::EditorDeleteToLineStart,
            "tui.editor.deleteToLineEnd" => Self::EditorDeleteToLineEnd,
            "tui.editor.yank" => Self::EditorYank,
            "tui.editor.yankPop" => Self::EditorYankPop,
            "tui.editor.undo" => Self::EditorUndo,
            "app.interrupt" | "abort" => Self::Abort,
            "app.clear" | "clear_editor" => Self::ClearEditor,
            "app.exit" | "quit" => Self::Quit,
            "tui.input.tab" | "accept_completion" => Self::AcceptCompletion,
            "theme_next" => Self::ThemeNext,
            "theme_prev" => Self::ThemePrev,
            "app.clipboard.pasteImage" | "clipboard_paste" => Self::ClipboardPaste,
            "app.message.copy" | "copy_last_assistant" => Self::CopyLastAssistant,
            "app.editor.external" => Self::ExternalEditor,
            "app.suspend" => Self::Suspend,
            "app.thinking.cycle" => Self::ThinkingCycle,
            "app.thinking.toggle" => Self::ThinkingToggle,
            "app.model.cycleForward" => Self::ModelCycleForward,
            "app.model.cycleBackward" => Self::ModelCycleBackward,
            "app.model.select" => Self::ModelSelect,

            "app.tools.expand" => Self::ToolsExpand,
            "app.message.followUp" => Self::FollowUp,
            "app.message.dequeue" => Self::Dequeue,
            "app.session.new" => Self::SessionNew,
            "app.session.resume" => Self::SessionResume,
            "app.session.tree" => Self::SessionTree,
            "app.session.fork" => Self::SessionFork,
            "app.session.toggleNamedFilter" => Self::SessionToggleNamedFilter,
            "app.session.togglePath" => Self::SessionTogglePath,
            "app.session.toggleSort" => Self::SessionToggleSort,
            "app.session.rename" => Self::SessionRename,
            "app.session.delete" => Self::SessionDelete,
            "app.session.deleteNoninvasive" => Self::SessionDeleteNoninvasive,
            "app.models.save" => Self::ModelsSave,
            "app.models.enableAll" => Self::ModelsEnableAll,
            "app.models.clearAll" => Self::ModelsClearAll,
            "app.models.toggleProvider" => Self::ModelsToggleProvider,
            "app.models.reorderUp" => Self::ModelsReorderUp,
            "app.models.reorderDown" => Self::ModelsReorderDown,
            "app.tree.foldOrUp" => Self::TreeFoldOrUp,
            "app.tree.unfoldOrDown" => Self::TreeUnfoldOrDown,
            "app.tree.editLabel" => Self::TreeEditLabel,
            "app.tree.toggleLabelTimestamp" => Self::TreeToggleLabelTimestamp,
            "app.tree.filter.default" => Self::TreeFilterDefault,
            "app.tree.filter.noTools" => Self::TreeFilterNoTools,
            "app.tree.filter.userOnly" => Self::TreeFilterUserOnly,
            "app.tree.filter.labeledOnly" => Self::TreeFilterLabeledOnly,
            "app.tree.filter.all" => Self::TreeFilterAll,
            "app.tree.filter.cycleForward" => Self::TreeFilterCycleForward,
            "app.tree.filter.cycleBackward" => Self::TreeFilterCycleBackward,

            _ => return None,
        })
    }
}

/// Canonical configurable action names. Every entry has observable dispatch.
pub const VALID_ACTION_NAMES: &[&str] = &[
    "tui.input.submit",
    "tui.input.newLine",
    "tui.editor.deleteCharBackward",
    "tui.editor.deleteCharForward",
    "tui.editor.cursorLeft",
    "tui.editor.cursorRight",
    "tui.editor.cursorUp",
    "tui.editor.cursorDown",
    "tui.editor.cursorWordLeft",
    "tui.editor.cursorWordRight",
    "tui.editor.cursorLineStart",
    "tui.editor.cursorLineEnd",
    "tui.editor.jumpForward",
    "tui.editor.jumpBackward",
    "tui.editor.pageUp",
    "tui.editor.pageDown",
    "tui.editor.deleteWordBackward",
    "tui.editor.deleteWordForward",
    "tui.editor.deleteToLineStart",
    "tui.editor.deleteToLineEnd",
    "tui.editor.yank",
    "tui.editor.yankPop",
    "tui.editor.undo",
    "app.interrupt",
    "app.clear",
    "app.exit",
    "tui.input.tab",
    "theme_next",
    "theme_prev",
    "app.clipboard.pasteImage",
    "app.message.copy",
    "app.editor.external",
    "app.suspend",
    "app.thinking.cycle",
    "app.thinking.toggle",
    "app.model.cycleForward",
    "app.model.cycleBackward",
    "app.model.select",
    "app.tools.expand",
    "app.message.followUp",
    "app.message.dequeue",
    "app.session.new",
    "app.session.resume",
    "app.session.tree",
    "app.session.fork",
    "app.session.toggleNamedFilter",
    "app.session.togglePath",
    "app.session.toggleSort",
    "app.session.rename",
    "app.session.delete",
    "app.session.deleteNoninvasive",
    "app.models.save",
    "app.models.enableAll",
    "app.models.clearAll",
    "app.models.toggleProvider",
    "app.models.reorderUp",
    "app.models.reorderDown",
    "app.tree.foldOrUp",
    "app.tree.unfoldOrDown",
    "app.tree.editLabel",
    "app.tree.toggleLabelTimestamp",
    "app.tree.filter.default",
    "app.tree.filter.noTools",
    "app.tree.filter.userOnly",
    "app.tree.filter.labeledOnly",
    "app.tree.filter.all",
    "app.tree.filter.cycleForward",
    "app.tree.filter.cycleBackward",
];

/// Owns the active chord→action map plus per-file load diagnostics. Files are
/// supplied by the caller in global-first, project-last order.
pub struct KeyBindingsManager {
    active: HashMap<String, Vec<Action>>,
    diagnostics: Vec<String>,
}

impl Default for KeyBindingsManager {
    fn default() -> Self {
        Self {
            active: default_bindings(),
            diagnostics: Vec::new(),
        }
    }
}

impl KeyBindingsManager {
    /// Loads built-in defaults, then applies each upstream action→chord overlay.
    /// Overriding an action replaces all of its earlier chords, matching pi.
    pub fn load(files: Vec<PathBuf>) -> Self {
        let mut active = default_bindings();
        let mut diagnostics = Vec::new();
        for file in &files {
            if !file.exists() {
                continue;
            }
            match load_file(file) {
                Ok(overrides) => apply_overrides(&mut active, overrides),
                Err(error) => diagnostics.push(format!("{}: {error}", file.display())),
            }
        }
        Self {
            active,
            diagnostics,
        }
    }

    /// Applies a validated in-memory upstream action→chord overlay atomically.
    pub fn apply_inline(
        &mut self,
        entries: &BTreeMap<String, pi_coding::KeyBindingValue>,
    ) -> Result<(), String> {
        let entries = entries
            .iter()
            .map(|(action, value)| {
                let chords = match value {
                    pi_coding::KeyBindingValue::One(chord) => vec![chord.clone()],
                    pi_coding::KeyBindingValue::Many(chords) => chords.clone(),
                };
                (action.clone(), chords)
            })
            .collect();
        let overrides = validate_entries(entries)?;
        let mut active = self.active.clone();
        apply_overrides(&mut active, overrides);
        self.active = active;
        Ok(())
    }

    /// Resolves a key event to its first globally active action. Contextual
    /// selectors use [`Self::resolve_in`] because upstream defaults deliberately
    /// reuse chords across mutually exclusive panels.
    pub fn resolve(&self, key: &KeyEvent) -> Option<Action> {
        let chord = normalize_key(key)?;
        self.active.get(&chord)?.iter().copied().find(|action| !is_selector_action(*action))
    }

    /// Resolves only within the caller's active UI scope. This intentionally
    /// takes precedence over a global action sharing the same chord (for
    /// example, Ctrl+D is Quit globally and TreeFilterDefault in the tree).
    pub fn resolve_in(&self, key: &KeyEvent, allowed: &[Action]) -> Option<Action> {
        let chord = normalize_key(key)?;
        self.active
            .get(&chord)?
            .iter()
            .copied()
            .find(|action| allowed.contains(action))
    }

    /// The active chord→actions map (canonical chords, registration order).
    pub fn bindings(&self) -> &HashMap<String, Vec<Action>> {
        &self.active
    }

    /// Per-file load failures (empty when all files validated).
    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }
}

fn is_selector_action(action: Action) -> bool {
    matches!(
        action,
        Action::SessionToggleNamedFilter
            | Action::SessionTogglePath
            | Action::SessionToggleSort
            | Action::SessionRename
            | Action::SessionDelete
            | Action::SessionDeleteNoninvasive
            | Action::ModelsSave
            | Action::ModelsEnableAll
            | Action::ModelsClearAll
            | Action::ModelsToggleProvider
            | Action::ModelsReorderUp
            | Action::ModelsReorderDown
            | Action::TreeFoldOrUp
            | Action::TreeUnfoldOrDown
            | Action::TreeEditLabel
            | Action::TreeToggleLabelTimestamp
            | Action::TreeFilterDefault
            | Action::TreeFilterNoTools
            | Action::TreeFilterUserOnly
            | Action::TreeFilterLabeledOnly
            | Action::TreeFilterAll
            | Action::TreeFilterCycleForward
            | Action::TreeFilterCycleBackward
    )
}

/// Built-in upstream-compatible bindings. Chords whose selectors are not yet
/// backed by a TUI flow are deliberately absent rather than accepted as no-ops.
fn default_bindings() -> HashMap<String, Vec<Action>> {
    let mut map: HashMap<String, Vec<Action>> = HashMap::new();
    let defaults: &[(&str, Action)] = &[
        ("enter", Action::EditorSubmit),
        ("shift+enter", Action::EditorNewline),
        ("ctrl+j", Action::EditorNewline),
        ("backspace", Action::EditorBackspace),
        ("delete", Action::EditorDelete),
        ("left", Action::EditorLeft),
        ("ctrl+b", Action::EditorLeft),
        ("right", Action::EditorRight),
        ("ctrl+f", Action::EditorRight),
        ("up", Action::EditorUp),
        ("down", Action::EditorDown),
        ("alt+left", Action::EditorWordLeft),
        ("ctrl+left", Action::EditorWordLeft),
        ("alt+b", Action::EditorWordLeft),
        ("alt+right", Action::EditorWordRight),
        ("ctrl+right", Action::EditorWordRight),
        ("alt+f", Action::EditorWordRight),
        ("home", Action::EditorHome),
        ("ctrl+a", Action::EditorHome),
        ("end", Action::EditorEnd),
        ("ctrl+e", Action::EditorEnd),
        ("ctrl+]", Action::EditorJumpForward),
        ("ctrl+alt+]", Action::EditorJumpBackward),
        ("pageup", Action::EditorPageUp),
        ("pagedown", Action::EditorPageDown),
        ("ctrl+w", Action::EditorDeleteWordBackward),
        ("alt+backspace", Action::EditorDeleteWordBackward),
        ("alt+d", Action::EditorDeleteWordForward),
        ("alt+delete", Action::EditorDeleteWordForward),
        ("ctrl+u", Action::EditorDeleteToLineStart),
        ("ctrl+k", Action::EditorDeleteToLineEnd),
        ("ctrl+y", Action::EditorYank),
        ("alt+y", Action::EditorYankPop),
        ("ctrl+-", Action::EditorUndo),
        ("esc", Action::Abort),
        ("ctrl+c", Action::ClearEditor),
        ("ctrl+d", Action::Quit),
        ("tab", Action::AcceptCompletion),
        ("ctrl+v", Action::ClipboardPaste),
        ("alt+v", Action::ClipboardPaste),
        ("ctrl+x", Action::CopyLastAssistant),
        ("ctrl+shift+c", Action::CopyLastAssistant),
        ("ctrl+g", Action::ExternalEditor),
        ("ctrl+z", Action::Suspend),
        ("shift+tab", Action::ThinkingCycle),
        ("ctrl+t", Action::ThinkingToggle),
        ("ctrl+p", Action::ModelCycleForward),
        ("ctrl+shift+p", Action::ModelCycleBackward),
        ("ctrl+l", Action::ModelSelect),
        ("ctrl+o", Action::ToolsExpand),
        ("alt+enter", Action::FollowUp),
        ("alt+up", Action::Dequeue),
        ("ctrl+n", Action::SessionToggleNamedFilter),
        ("ctrl+p", Action::SessionTogglePath),
        ("ctrl+s", Action::SessionToggleSort),
        ("ctrl+r", Action::SessionRename),
        ("ctrl+d", Action::SessionDelete),
        ("ctrl+backspace", Action::SessionDeleteNoninvasive),
        ("ctrl+s", Action::ModelsSave),
        ("ctrl+a", Action::ModelsEnableAll),
        ("ctrl+x", Action::ModelsClearAll),
        ("ctrl+p", Action::ModelsToggleProvider),
        ("alt+up", Action::ModelsReorderUp),
        ("alt+down", Action::ModelsReorderDown),
        ("ctrl+d", Action::TreeFilterDefault),
        ("ctrl+t", Action::TreeFilterNoTools),
        ("ctrl+o", Action::TreeFilterCycleForward),
        ("ctrl+shift+o", Action::TreeFilterCycleBackward),
        ("alt+shift+l", Action::TreeEditLabel),
        ("alt+shift+t", Action::TreeToggleLabelTimestamp),
        ("ctrl+u", Action::TreeFilterUserOnly),
        ("ctrl+l", Action::TreeFilterLabeledOnly),
        ("ctrl+a", Action::TreeFilterAll),
    ];
    for (chord, action) in defaults {
        map.entry((*chord).to_owned()).or_default().push(*action);
    }
    map
}

fn apply_overrides(
    active: &mut HashMap<String, Vec<Action>>,
    overrides: Vec<(Action, Vec<String>)>,
) {
    for (action, chords) in overrides {
        for actions in active.values_mut() {
            actions.retain(|existing| *existing != action);
        }
        active.retain(|_, actions| !actions.is_empty());
        for chord in chords {
            active.entry(chord).or_default().push(action);
        }
    }
}

/// Reads and validates one keybindings file in the upstream action→key(s)
/// shape. The legacy pi-rs `bindings` array remains readable for clean cutover.
fn load_file(path: &PathBuf) -> Result<Vec<(Action, Vec<String>)>, String> {
    let data = fs::read_to_string(path).map_err(|error| format!("read failed: {error}"))?;
    let parsed: KeyBindingFile =
        serde_json::from_str(&data).map_err(|error| format!("invalid JSON: {error}"))?;
    let entries = match parsed {
        KeyBindingFile::Upstream(entries) => entries
            .into_iter()
            .map(|(action, chords)| (action, chords.into_vec()))
            .collect::<Vec<_>>(),
        KeyBindingFile::Legacy { bindings } => bindings
            .into_iter()
            .map(|entry| (entry.action, vec![entry.chord]))
            .collect(),
    };
    validate_entries(entries)
}

fn validate_entries(entries: Vec<(String, Vec<String>)>) -> Result<Vec<(Action, Vec<String>)>, String> {
    let mut overrides = Vec::new();
    let mut claimed = HashMap::<String, String>::new();
    let mut errors = Vec::new();
    for (action_name, raw_chords) in entries {
        let Some(action) = Action::from_name(&action_name) else {
            errors.push(format!(
                "action `{action_name}` is unknown; valid actions: {}",
                VALID_ACTION_NAMES.join(", ")
            ));
            continue;
        };
        let mut chords = Vec::new();
        for raw_chord in raw_chords {
            match normalize_chord_str(&raw_chord) {
                Ok(chord) => {
                    if let Some(previous) = claimed.insert(chord.clone(), action_name.clone())
                        && previous != action_name
                    {
                        errors.push(format!("chord `{raw_chord}` is claimed by both `{previous}` and `{action_name}`"));
                        continue;
                    }
                    if !chords.contains(&chord) {
                        chords.push(chord);
                    }
                }
                Err(error) => errors.push(format!("chord `{raw_chord}`: {error}")),
            }
        }
        overrides.push((action, chords));
    }
    if errors.is_empty() {
        Ok(overrides)
    } else {
        Err(errors.join("; "))
    }
}

/// Canonicalizes a chord string from a config file: lowercases, splits on `+`,
/// orders modifiers as `ctrl`/`alt`/`shift`, accepts modifier aliases
/// (`control`→`ctrl`, `option`→`alt`), and validates the key token. Returns an
/// actionable error for unknown modifiers or key tokens.
fn normalize_chord_str(input: &str) -> Result<String, String> {
    let lower = input.to_ascii_lowercase();
    let parts: Vec<&str> = lower
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        return Err("empty chord".to_owned());
    }
    let key_token = canonical_key_token(parts[parts.len() - 1]);
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    for modifier in &parts[..parts.len() - 1] {
        match *modifier {
            "ctrl" | "control" => ctrl = true,
            "alt" | "option" => alt = true,
            "shift" => shift = true,
            other => return Err(format!("unknown modifier `{other}`")),
        }
    }
    validate_key_token(key_token)?;
    let mut out = String::new();
    if ctrl {
        out.push_str("ctrl+");
    }
    if alt {
        out.push_str("alt+");
    }
    if shift {
        out.push_str("shift+");
    }
    out.push_str(key_token);
    Ok(out)
}

fn canonical_key_token(token: &str) -> &str {
    match token {
        "escape" => "esc",
        "return" => "enter",
        other => other,
    }
}

/// Validates that a key token is one of the recognized special keys, a single
/// character, or an `f<n>` function key.
fn validate_key_token(token: &str) -> Result<(), String> {
    const SPECIAL: &[&str] = &[
        "enter",
        "tab",
        "backspace",
        "delete",
        "esc",
        "escape",
        "left",
        "right",
        "up",
        "down",
        "home",
        "end",
        "pageup",
        "pagedown",
        "space",
    ];
    if SPECIAL.contains(&token) {
        return Ok(());
    }
    if let Some(rest) = token.strip_prefix('f') {
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            return Ok(());
        }
    }
    // A single Unicode scalar (e.g. "a", "@", "/"). `chars().count == 1` guards
    // against multi-character garbage like "ab".
    if token.chars().count() == 1 {
        return Ok(());
    }
    Err(format!("unknown key `{token}`"))
}

/// Canonicalizes a live crossterm key event to the same chord form used by the
/// config files. Returns `None` for unrepresentable keys (e.g. media keys).
pub(crate) fn normalize_key(key: &KeyEvent) -> Option<String> {
    let mut mods = key.modifiers;
    let key_part: String = match key.code {
        KeyCode::Char(' ') => "space".to_owned(),
        KeyCode::Char(ch) if ch.is_ascii_alphabetic() => {
            if ch.is_ascii_uppercase() {
                mods |= KeyModifiers::SHIFT;
            }
            ch.to_ascii_lowercase().to_string()
        }
        KeyCode::Char(ch) => ch.to_string(),
        KeyCode::Enter => "enter".to_owned(),
        KeyCode::Tab => "tab".to_owned(),
        KeyCode::BackTab => {
            mods |= KeyModifiers::SHIFT;
            "tab".to_owned()
        }
        KeyCode::Backspace => "backspace".to_owned(),
        KeyCode::Delete => "delete".to_owned(),
        KeyCode::Esc => "esc".to_owned(),
        KeyCode::Left => "left".to_owned(),
        KeyCode::Right => "right".to_owned(),
        KeyCode::Up => "up".to_owned(),
        KeyCode::Down => "down".to_owned(),
        KeyCode::Home => "home".to_owned(),
        KeyCode::End => "end".to_owned(),
        KeyCode::PageUp => "pageup".to_owned(),
        KeyCode::PageDown => "pagedown".to_owned(),
        KeyCode::F(number) => format!("f{number}"),

        _ => return None,
    };
    let mut out = String::new();
    if mods.contains(KeyModifiers::CONTROL) {
        out.push_str("ctrl+");
    }
    if mods.contains(KeyModifiers::ALT) {
        out.push_str("alt+");
    }
    if mods.contains(KeyModifiers::SHIFT) {
        out.push_str("shift+");
    }
    out.push_str(&key_part);
    Some(out)
}

/// Upstream files are JSON objects whose keys are stable action IDs and whose
/// values are one chord or an array. Legacy array files remain readable.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum KeyBindingFile {
    Upstream(HashMap<String, ChordList>),
    Legacy { bindings: Vec<BindingEntry> },
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum ChordList {
    One(String),
    Many(Vec<String>),
}

impl ChordList {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(chord) => vec![chord],
            Self::Many(chords) => chords,
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BindingEntry {
    chord: String,
    action: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_resolve_upstream_editor_chords() {
        let bindings = KeyBindingsManager::default();
        let cases = [
            (
                KeyEvent::new(KeyCode::Left, KeyModifiers::ALT),
                Action::EditorWordLeft,
            ),
            (
                KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL),
                Action::EditorWordRight,
            ),
            (
                KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
                Action::EditorDeleteWordBackward,
            ),
            (
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT),
                Action::EditorDeleteWordForward,
            ),
            (
                KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
                Action::ThinkingCycle,
            ),
            (
                KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL),
                Action::ModelSelect,
            ),
        ];
        for (key, expected) in cases {
            assert_eq!(bindings.resolve(&key), Some(expected));
        }
    }

    #[test]
    fn upstream_config_overrides_all_default_chords_for_action() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("keybindings.json");
        std::fs::write(&path, r#"{"tui.editor.cursorWordLeft":["ctrl+q"]}"#).unwrap();
        let bindings = KeyBindingsManager::load(vec![path]);
        assert_eq!(
            bindings.resolve(&KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Some(Action::EditorWordLeft)
        );
        assert_eq!(
            bindings.resolve(&KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)),
            None
        );
        assert!(bindings.diagnostics().is_empty());

        let configured = directory.path().join("dequeue-keybindings.json");
        std::fs::write(&configured, r#"{"app.message.dequeue":["ctrl+q"]}"#).unwrap();
        let configured_bindings = KeyBindingsManager::load(vec![configured]);
        assert_eq!(
            configured_bindings.resolve(&KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Some(Action::Dequeue)
        );
        assert_eq!(
            configured_bindings.resolve(&KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)),
            None
        );
        assert!(configured_bindings.diagnostics().is_empty());
    }

    #[test]
    fn action_registry_only_accepts_executable_dispatch_actions() {
        for name in VALID_ACTION_NAMES {
            assert!(Action::from_name(name).is_some(), "{name}");
        }
        assert_eq!(
            Action::from_name("app.session.tree"),
            Some(Action::SessionTree)
        );
        assert_eq!(
            Action::from_name("app.tree.foldOrUp"),
            Some(Action::TreeFoldOrUp)
        );
        assert_eq!(
            Action::from_name("app.tree.filter.all"),
            Some(Action::TreeFilterAll)
        );
        assert_eq!(
            Action::from_name("app.message.dequeue"),
            Some(Action::Dequeue)
        );
    }
    #[test]
    fn selector_defaults_resolve_contextual_action_collisions() {
        let bindings = KeyBindingsManager::default();
        let cases = [
            (
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
                Action::SessionToggleNamedFilter,
            ),
            (
                KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
                Action::SessionTogglePath,
            ),
            (
                KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
                Action::SessionToggleSort,
            ),
            (
                KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
                Action::SessionRename,
            ),
            (
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
                Action::SessionDelete,
            ),
            (
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL),
                Action::SessionDeleteNoninvasive,
            ),
            (
                KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
                Action::ModelsSave,
            ),
            (
                KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
                Action::ModelsEnableAll,
            ),
            (
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
                Action::ModelsClearAll,
            ),
            (
                KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
                Action::ModelsToggleProvider,
            ),
            (
                KeyEvent::new(KeyCode::Up, KeyModifiers::ALT),
                Action::ModelsReorderUp,
            ),
            (
                KeyEvent::new(KeyCode::Down, KeyModifiers::ALT),
                Action::ModelsReorderDown,
            ),
        ];
        for (key, action) in cases {
            assert_eq!(bindings.resolve_in(&key, &[action]), Some(action));
        }

        let tree_cases = [
            (KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL), Some(Action::Quit), Action::TreeFilterDefault),
            (KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL), Some(Action::ThinkingToggle), Action::TreeFilterNoTools),
            (KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL), Some(Action::ToolsExpand), Action::TreeFilterCycleForward),
            (KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL | KeyModifiers::SHIFT), None, Action::TreeFilterCycleBackward),
            (KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL), Some(Action::EditorDeleteToLineStart), Action::TreeFilterUserOnly),
            (KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL), Some(Action::ModelSelect), Action::TreeFilterLabeledOnly),
            (KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL), Some(Action::EditorHome), Action::TreeFilterAll),
        ];
        for (key, global, contextual) in tree_cases {
            assert_eq!(bindings.resolve(&key), global, "global precedence for {contextual:?}");
            assert_eq!(bindings.resolve_in(&key, &[contextual]), Some(contextual));
        }
    }
    #[test]
    fn inline_overlay_is_atomic_and_replaces_defaults() {
        let mut bindings = KeyBindingsManager::default();
        let mut valid = BTreeMap::new();
        valid.insert(
            "tui.editor.cursorWordLeft".to_owned(),
            pi_coding::KeyBindingValue::One("control+q".to_owned()),
        );
        bindings.apply_inline(&valid).expect("valid overlay");
        assert_eq!(
            bindings.resolve(&KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Some(Action::EditorWordLeft)
        );
        assert_eq!(bindings.resolve(&KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)), None);

        let before = bindings.bindings().clone();
        let mut invalid = BTreeMap::new();
        invalid.insert(
            "unknown.action".to_owned(),
            pi_coding::KeyBindingValue::One("ctrl+x".to_owned()),
        );
        assert!(bindings.apply_inline(&invalid).is_err());
        assert_eq!(bindings.bindings(), &before);
    }
}
