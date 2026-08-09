//! OMP-like `/agents` management panel.
//!
//! Lists discovered agent definitions with source, model resolution, thinking,
//! tools, and skills. Edits per-agent enablement and model overrides through
//! atomic settings writes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use pi_ai::Model;
use pi_coding::{
    AgentDefinition, AgentDefinitionSource, AgentModelSource, AgentRuntimeSettings, Application,
    ResolvedAgentModel, SettingsManager, model_id, resolve_agent_model,
};

/// Focus target inside the agents panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentsPanelFocus {
    List,
    ModelPicker,
}

/// One row in the agents list.
#[derive(Clone, Debug)]
pub(crate) struct AgentPanelRow {
    pub name: String,
    pub description: String,
    pub source: AgentDefinitionSource,
    pub enabled: bool,
    pub thinking: String,
    pub tools: String,
    pub skills: String,
    pub definition_models: String,
    pub effective_model: String,
    pub model_source: AgentModelSource,
    pub override_model: Option<String>,
    /// Present when settings model resolution failed (invalid override).
    pub model_error: Option<String>,
    pub definition: AgentDefinition,
    /// Whether this row is a durable persona (vs an ordinary agent).
    pub is_persona: bool,
    /// Bounded one-line persona contract summary (empty for agents).
    pub persona_summary: String,
    /// Persona local-memory entry count (None for agents or unreadable state).
    pub memory_entries: Option<usize>,
    /// Persona transcript archive count (None for agents or unreadable state).
    pub transcript_count: Option<usize>,
    /// Set when persona state counting hit a symlink/non-regular file.
    pub state_error: Option<String>,
    /// Whether this row is the currently preferred agent for unnamed spawns.
    pub preferred: bool,
}

/// Result of a key press handled by the panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AgentsPanelAction {
    /// Keep the panel open; optional status toast.
    Continue(Option<String>),
    /// Settings persisted; caller must live-reload orchestration before toasting success.
    Saved,
    /// Close the panel; optional status toast.
    Close(Option<String>),
    /// Persist failed with an error message (panel stays open).
    Error(String),
}

/// Stateful `/agents` overlay.
pub(crate) struct AgentsPanel {
    rows: Vec<AgentPanelRow>,
    selected: usize,
    focus: AgentsPanelFocus,
    model_choices: Vec<Model>,
    model_selected: usize,
    model_query: String,
    dirty: bool,
    /// Pending global-scope edits keyed by agent name (not yet saved).
    edits: BTreeMap<String, AgentRuntimeSettings>,
    /// Effective (global+project) agents map used only for live display resolution
    /// when an agent has no pending global edit.
    effective_baseline: BTreeMap<String, AgentRuntimeSettings>,
    parent_model: Model,
    status: String,
    /// Currently preferred agent name (for marking persona/agent selection).
    preferred: Option<String>,
}

impl AgentsPanel {
    pub(crate) fn new(
        definitions: Vec<AgentDefinition>,
        global_agents: &BTreeMap<String, AgentRuntimeSettings>,
        parent_model: Model,
        available_models: Vec<Model>,
    ) -> Self {
        Self::new_with_effective(definitions, global_agents, global_agents, parent_model, available_models)
    }

    pub(crate) fn new_with_effective(
        definitions: Vec<AgentDefinition>,
        global_agents: &BTreeMap<String, AgentRuntimeSettings>,
        effective_agents: &BTreeMap<String, AgentRuntimeSettings>,
        parent_model: Model,
        available_models: Vec<Model>,
    ) -> Self {
        let mut edits = BTreeMap::new();
        for (name, entry) in global_agents {
            edits.insert(name.clone(), entry.clone());
        }
        let mut panel = Self {
            rows: Vec::new(),
            selected: 0,
            focus: AgentsPanelFocus::List,
            model_choices: available_models,
            model_selected: 0,
            model_query: String::new(),
            dirty: false,
            edits,
            effective_baseline: effective_agents.clone(),
            parent_model,
            status: "Global settings · Enter toggle · m model · Ctrl+S save · Esc close".to_owned(),
            preferred: None,
        };
        panel.rebuild_rows(definitions);
        panel
    }

    pub(crate) fn from_application(
        application: &Application,
        available_models: Vec<Model>,
    ) -> Result<Self> {
        let snapshot = application
            .resource_snapshot()
            .context("session has no resource snapshot")?;
        let parent_model = application.session().model().unwrap_or_default();
        // Panel edits global settings only (OMP dashboard scope). Do not seed
        // from effective/project-merged agents or a save would promote project
        // overrides into the user-global map.
        let global_agents = application
            .session()
            .resource_manager()
            .map(|resources| resources.settings_manager().global_settings().agents)
            .unwrap_or_default();
        let mut panel = Self::new_with_effective(
            snapshot.agents.clone(),
            &global_agents,
            &snapshot.settings.agents,
            parent_model,
            available_models,
        );
        panel.set_preferred(preferred_agent(application));
        Ok(panel)
    }

    #[must_use]
    pub(crate) fn title(&self) -> &'static str {
        match self.focus {
            AgentsPanelFocus::List => "Agents",
            AgentsPanelFocus::ModelPicker => "Agent model override",
        }
    }

    #[must_use]
    pub(crate) fn help(&self) -> &str {
        match self.focus {
            AgentsPanelFocus::List => self.status.as_str(),
            AgentsPanelFocus::ModelPicker => {
                "Type to filter · Enter set · 0 clear override · Esc back"
            }
        }
    }

    #[must_use]
    pub(crate) fn focus(&self) -> AgentsPanelFocus {
        self.focus
    }

    #[must_use]
    pub(crate) fn dirty(&self) -> bool {
        self.dirty
    }

    #[must_use]
    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    #[must_use]
    pub(crate) fn rows(&self) -> &[AgentPanelRow] {
        &self.rows
    }

    #[must_use]
    pub(crate) fn selected_row(&self) -> Option<&AgentPanelRow> {
        self.rows.get(self.selected)
    }

    #[must_use]
    pub(crate) fn model_query(&self) -> &str {
        &self.model_query
    }

    #[must_use]
    pub(crate) fn model_selected(&self) -> usize {
        self.model_selected
    }

    #[must_use]
    pub(crate) fn visible_model_ids(&self) -> Vec<String> {
        self.model_choices
            .iter()
            .map(model_id)
            .filter(|id| fuzzy_match(id, &self.model_query))
            .collect()
    }

    pub(crate) fn move_list(&mut self, delta: isize) {
        if self.rows.is_empty() {
            self.selected = 0;
            return;
        }
        let count = self.rows.len();
        self.selected = (self.selected as isize + delta).rem_euclid(count as isize) as usize;
    }

    pub(crate) fn move_models(&mut self, delta: isize) {
        let count = self.visible_model_ids().len();
        if count == 0 {
            self.model_selected = 0;
            return;
        }
        self.model_selected = (self.model_selected as isize + delta).rem_euclid(count as isize) as usize;
    }

    pub(crate) fn toggle_selected_enabled(&mut self) {
        let Some(name) = self.selected_row().map(|row| row.name.clone()) else {
            return;
        };
        // Global-only draft: never copy project-effective model into global edits.
        let global = self.edits.get(&name).cloned().unwrap_or_default();
        let display_enabled = self
            .edits
            .get(&name)
            .map(AgentRuntimeSettings::is_enabled)
            .or_else(|| {
                self.effective_baseline
                    .get(&name)
                    .map(AgentRuntimeSettings::is_enabled)
            })
            .unwrap_or(true);
        let enabled = !display_enabled;
        self.upsert_edit(
            name.clone(),
            AgentRuntimeSettings {
                enabled: Some(enabled),
                // Preserve only an existing *global* model/tombstone/tools.
                model: global.model,
                tools: global.tools,
            },
        );
        self.status = format!("{name} {}", if enabled { "enabled" } else { "disabled" });
        self.rebuild_rows_from_existing();
    }

    pub(crate) fn open_model_picker(&mut self) {
        if self.selected_row().is_none() {
            return;
        }
        self.focus = AgentsPanelFocus::ModelPicker;
        self.model_query.clear();
        self.model_selected = 0;
        if let Some(row) = self.selected_row() {
            let target = row
                .override_model
                .clone()
                .unwrap_or_else(|| row.effective_model.clone());
            if let Some(index) = self.visible_model_ids().iter().position(|id| id == &target) {
                self.model_selected = index;
            }
        }
    }

    pub(crate) fn push_model_query(&mut self, character: char) {
        self.model_query.push(character);
        self.model_selected = 0;
    }

    pub(crate) fn pop_model_query(&mut self) {
        self.model_query.pop();
        self.model_selected = 0;
    }

    pub(crate) fn clear_model_override(&mut self) {
        let Some(name) = self.selected_row().map(|row| row.name.clone()) else {
            return;
        };
        let enabled = self
            .edits
            .get(&name)
            .and_then(|entry| entry.enabled)
            .or_else(|| self.effective_baseline.get(&name).and_then(|entry| entry.enabled));
        let tools = self.edits.get(&name).and_then(|entry| entry.tools.clone());
        // Persist an empty-model tombstone so a global clear shadows project-effective models.
        self.upsert_edit(
            name.clone(),
            AgentRuntimeSettings {
                enabled,
                model: Some(String::new()),
                tools,
            },
        );
        self.focus = AgentsPanelFocus::List;
        self.status = format!("{name} global model override cleared");
        self.rebuild_rows_from_existing();
    }

    pub(crate) fn apply_selected_model(&mut self) {
        let Some(name) = self.selected_row().map(|row| row.name.clone()) else {
            return;
        };
        let Some(id) = self.visible_model_ids().get(self.model_selected).cloned() else {
            return;
        };
        let enabled = self
            .edits
            .get(&name)
            .and_then(|entry| entry.enabled)
            .or_else(|| self.effective_baseline.get(&name).and_then(|entry| entry.enabled));
        let tools = self.edits.get(&name).and_then(|entry| entry.tools.clone());
        self.upsert_edit(
            name.clone(),
            AgentRuntimeSettings {
                enabled,
                model: Some(id.clone()),
                tools,
            },
        );
        self.focus = AgentsPanelFocus::List;
        self.status = format!("{name} model → {id}");
        self.rebuild_rows_from_existing();
    }

    pub(crate) fn cancel_model_picker(&mut self) {
        self.focus = AgentsPanelFocus::List;
        self.model_query.clear();
    }

    /// Handle a raw key when the panel is focused.
    pub(crate) fn handle_key(
        &mut self,
        key: KeyEvent,
        settings: Option<&SettingsManager>,
    ) -> AgentsPanelAction {
        if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
            return AgentsPanelAction::Continue(None);
        }

        match self.focus {
            AgentsPanelFocus::ModelPicker => match key.code {
                KeyCode::Esc => {
                    self.cancel_model_picker();
                    AgentsPanelAction::Continue(None)
                }
                KeyCode::Up => {
                    self.move_models(-1);
                    AgentsPanelAction::Continue(None)
                }
                KeyCode::Down => {
                    self.move_models(1);
                    AgentsPanelAction::Continue(None)
                }
                KeyCode::Backspace => {
                    self.pop_model_query();
                    AgentsPanelAction::Continue(None)
                }
                KeyCode::Char('0')
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.clear_model_override();
                    AgentsPanelAction::Continue(Some(self.status.clone()))
                }
                KeyCode::Enter => {
                    self.apply_selected_model();
                    AgentsPanelAction::Continue(Some(self.status.clone()))
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.push_model_query(character);
                    AgentsPanelAction::Continue(None)
                }
                _ => AgentsPanelAction::Continue(None),
            },
            AgentsPanelFocus::List => {
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S'))
                {
                    return self.save(settings);
                }
                match key.code {
                    KeyCode::Esc => AgentsPanelAction::Close(if self.dirty {
                        Some("Discarded unsaved agent changes".to_owned())
                    } else {
                        None
                    }),
                    KeyCode::Up => {
                        self.move_list(-1);
                        AgentsPanelAction::Continue(None)
                    }
                    KeyCode::Down => {
                        self.move_list(1);
                        AgentsPanelAction::Continue(None)
                    }
                    KeyCode::Enter | KeyCode::Char(' ') => {
                        self.toggle_selected_enabled();
                        AgentsPanelAction::Continue(Some(self.status.clone()))
                    }
                    KeyCode::Char('m') | KeyCode::Char('M') => {
                        self.open_model_picker();
                        AgentsPanelAction::Continue(None)
                    }
                    _ => AgentsPanelAction::Continue(None),
                }
            }
        }
    }

    pub(crate) fn save(&mut self, settings: Option<&SettingsManager>) -> AgentsPanelAction {
        let Some(manager) = settings else {
            return AgentsPanelAction::Error("session has no settings manager".to_owned());
        };
        if let Some(row) = self.rows.iter().find(|row| row.model_error.is_some()) {
            return AgentsPanelAction::Error(format!(
                "Cannot save: agent `{}` has an invalid model override ({})",
                row.name,
                row.override_model.as_deref().unwrap_or("?")
            ));
        }
        let edits = self.edits.clone();
        match manager.update_global(|settings| {
            // Replace the entire agents map with the edited view so cleared
            // overrides disappear from disk.
            settings.agents = edits
                .into_iter()
                .filter(|(_, entry)| !entry.is_default())
                .collect();
        }) {
            Ok(()) => {
                self.dirty = false;
                // Success toast is owned by the TUI after live orchestration reload.
                self.status = "Applying agent settings…".to_owned();
                AgentsPanelAction::Saved
            }
            Err(error) => {
                AgentsPanelAction::Error(format!("Failed to save agent settings: {error:#}"))
            }
        }
    }

    /// Replace the model picker catalog (e.g. after `/reload` or reopen).
    pub(crate) fn set_model_choices(&mut self, available_models: Vec<Model>) {
        self.model_choices = available_models;
        if self.focus == AgentsPanelFocus::ModelPicker {
            let count = self.visible_model_ids().len();
            if count == 0 {
                self.model_selected = 0;
            } else {
                self.model_selected = self.model_selected.min(count - 1);
            }
        }
    }

    /// Update the currently preferred agent (e.g. after `/role --select` or a
    /// reload) and refresh the per-row selection markers.
    pub(crate) fn set_preferred(&mut self, preferred: Option<String>) {
        self.preferred = preferred;
        self.rebuild_rows_from_existing();
    }

    /// Refresh definition metadata after `/reload` while preserving pending edits.
    ///
    /// `disk_settings` must be the **global** agents map (not project-merged).
    pub(crate) fn reload_definitions(
        &mut self,
        definitions: Vec<AgentDefinition>,
        parent_model: Model,
        global_settings: &BTreeMap<String, AgentRuntimeSettings>,
        effective_settings: &BTreeMap<String, AgentRuntimeSettings>,
        available_models: Option<Vec<Model>>,
    ) {
        self.parent_model = parent_model;
        self.effective_baseline = effective_settings.clone();
        if let Some(models) = available_models {
            self.set_model_choices(models);
        }
        if !self.dirty {
            self.edits = global_settings.clone();
        }
        self.rebuild_rows(definitions);
        self.status = "Agent definitions reloaded".to_owned();
    }

    fn upsert_edit(&mut self, name: String, settings: AgentRuntimeSettings) {
        if settings.is_default() {
            self.edits.remove(&name);
        } else {
            self.edits.insert(name, settings);
        }
        self.dirty = true;
    }

    fn rebuild_rows_from_existing(&mut self) {
        let definitions = self
            .rows
            .iter()
            .map(|row| row.definition.clone())
            .collect::<Vec<_>>();
        self.rebuild_rows(definitions);
    }

    fn rebuild_rows(&mut self, definitions: Vec<AgentDefinition>) {
        let selected_name = self.rows.get(self.selected).map(|row| row.name.clone());
        self.rows = definitions
            .into_iter()
            .map(|definition| {
                let entry = self
                    .edits
                    .get(&definition.name)
                    .or_else(|| self.effective_baseline.get(&definition.name));
                let enabled = entry.map_or(true, AgentRuntimeSettings::is_enabled);
                let (effective_model, model_source, model_error) = match resolve_agent_model(
                    &definition,
                    entry,
                    &self.parent_model,
                    &self.model_choices,
                ) {
                    Ok(resolved) => (model_id(&resolved.model), resolved.source, None),
                    Err(error) => {
                        let override_pat = entry
                            .and_then(AgentRuntimeSettings::model_override)
                            .unwrap_or("?");
                        (
                            format!("INVALID({override_pat})"),
                            AgentModelSource::SettingsOverride,
                            Some(error.to_string()),
                        )
                    }
                };
                let is_persona = definition.is_persona();
                let (persona_summary, memory_entries, transcript_count, state_error) =
                    if is_persona {
                        let summary = persona_contract_summary(&definition);
                        let counts = persona_state_counts(definition.persona_root());
                        (summary, counts.memory_entries, counts.transcript_count, counts.error)
                    } else {
                        (String::new(), None, None, None)
                    };
                let preferred = self.preferred.as_deref() == Some(definition.name.as_str());
                AgentPanelRow {
                    name: definition.name.clone(),
                    description: definition.description.clone(),
                    source: definition.source,
                    enabled,
                    thinking: definition
                        .thinking_level
                        .map_or_else(|| "inherit".to_owned(), thinking_label),
                    tools: format_list(definition.tools.as_deref()),
                    skills: format_list(Some(definition.autoload_skills.as_slice())),
                    definition_models: format_list(definition.model.as_deref()),
                    effective_model,
                    model_source,
                    override_model: match self.edits.get(&definition.name) {
                        Some(edit) if edit.clears_model() => None,
                        Some(edit) => edit.model_override().map(str::to_owned),
                        None => self
                            .effective_baseline
                            .get(&definition.name)
                            .and_then(|value| value.model_override().map(str::to_owned)),
                    },
                    model_error,
                    definition,
                    is_persona,
                    persona_summary,
                    memory_entries,
                    transcript_count,
                    state_error,
                    preferred,
                }
            })
            .collect();
        self.selected = selected_name
            .and_then(|name| self.rows.iter().position(|row| row.name == name))
            .unwrap_or(0)
            .min(self.rows.len().saturating_sub(1));
        if let Some(row) = self.rows.iter().find(|row| row.model_error.is_some()) {
            if let Some(error) = &row.model_error {
                self.status = format!("{}: {error}", row.name);
            }
        }
    }

    #[must_use]
    pub(crate) fn view_lines(&self) -> Vec<AgentsPanelViewLine> {
        match self.focus {
            AgentsPanelFocus::List => self
                .rows
                .iter()
                .enumerate()
                .map(|(index, row)| {
                    let text = if row.is_persona {
                        let selection = if row.preferred { "*" } else { " " };
                        let counts = match &row.state_error {
                            Some(error) => format!("state=ERR({})", truncate_count_error(error)),
                            None => format!(
                                "mem={} sessions={}",
                                row.memory_entries.unwrap_or(0),
                                row.transcript_count.unwrap_or(0)
                            ),
                        };
                        format!(
                            "{selection} P {:<16} {:<8} {} · {}",
                            row.name,
                            source_label(row.source),
                            if row.persona_summary.is_empty() {
                                "(default contract)"
                            } else {
                                row.persona_summary.as_str()
                            },
                            counts,
                        )
                    } else {
                        let marker = if row.enabled { "ON " } else { "OFF" };
                        let source_tier = if row.model_error.is_some() {
                            "INVALID"
                        } else {
                            model_source_label(row.model_source)
                        };
                        format!(
                            "{marker} {:<16} {:<8} model={} ({}) think={} tools={} skills={}",
                            row.name,
                            source_label(row.source),
                            row.effective_model,
                            source_tier,
                            row.thinking,
                            row.tools,
                            row.skills,
                        )
                    };
                    AgentsPanelViewLine {
                        selected: index == self.selected,
                        text,
                    }
                })
                .collect(),
            AgentsPanelFocus::ModelPicker => {
                let mut lines = vec![AgentsPanelViewLine {
                    selected: false,
                    text: format!(
                        "Agent: {} · filter: {}",
                        self.selected_row()
                            .map(|row| row.name.as_str())
                            .unwrap_or("?"),
                        self.model_query
                    ),
                }];
                for (index, id) in self.visible_model_ids().into_iter().enumerate() {
                    lines.push(AgentsPanelViewLine {
                        selected: index == self.model_selected,
                        text: id,
                    });
                }
                if lines.len() == 1 {
                    lines.push(AgentsPanelViewLine {
                        selected: false,
                        text: "(no models match)".to_owned(),
                    });
                }
                lines
            }
        }
    }
}

/// Render helper data for TUI — one display line per agent plus detail.
#[derive(Clone, Debug)]
pub(crate) struct AgentsPanelViewLine {
    pub selected: bool,
    pub text: String,
}

fn format_list(values: Option<&[String]>) -> String {
    match values {
        None => "default".to_owned(),
        Some([]) => "none".to_owned(),
        Some(values) => values.join(", "),
    }
}

fn thinking_label(level: pi_agent::ThinkingLevel) -> String {
    match level {
        pi_agent::ThinkingLevel::Off => "off".to_owned(),
        pi_agent::ThinkingLevel::Minimal => "minimal".to_owned(),
        pi_agent::ThinkingLevel::Low => "low".to_owned(),
        pi_agent::ThinkingLevel::Medium => "medium".to_owned(),
        pi_agent::ThinkingLevel::High => "high".to_owned(),
        pi_agent::ThinkingLevel::Xhigh => "xhigh".to_owned(),
        pi_agent::ThinkingLevel::Max => "max".to_owned(),
    }
}

fn fuzzy_match(candidate: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let candidate = candidate.to_ascii_lowercase();
    let mut rest = candidate.as_str();
    for character in query.chars().flat_map(char::to_lowercase) {
        match rest.find(character) {
            Some(index) => rest = &rest[index + character.len_utf8()..],
            None => return false,
        }
    }
    true
}

fn source_label(source: AgentDefinitionSource) -> &'static str {
    match source {
        AgentDefinitionSource::Project => "project",
        AgentDefinitionSource::User => "user",
        AgentDefinitionSource::Bundled => "bundled",
    }
}

fn model_source_label(source: AgentModelSource) -> &'static str {
    match source {
        AgentModelSource::SettingsOverride => "settings",
        AgentModelSource::DefinitionFallback => "definition",
        AgentModelSource::Parent => "parent",
    }
}

/// Upper bound for persona state counts so a runaway dir never stalls the panel.
const PERSONA_COUNT_BOUND: usize = 9_999;

/// Bounded persona contract summary for one panel row.
fn persona_contract_summary(definition: &AgentDefinition) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(
        if definition
            .personality
            .as_ref()
            .is_some_and(|value| !value.is_empty())
        {
            "personality:present".to_owned()
        } else {
            "personality:absent".to_owned()
        },
    );
    if let Some(budget) = &definition.soft_budget {
        let mut budget_parts: Vec<String> = Vec::new();
        if let Some(value) = budget.max_requests {
            budget_parts.push(format!("maxRequests:{value}"));
        }
        if let Some(value) = budget.max_tokens {
            budget_parts.push(format!("maxTokens:{value}"));
        }
        if let Some(value) = budget.yield_after {
            budget_parts.push(format!("yieldAfter:{value}"));
        }
        if !budget_parts.is_empty() {
            parts.push(format!("softBudget:{}", budget_parts.join(",")));
        }
    }
    if let Some(value) = definition.max_turns {
        parts.push(format!("maxTurns:{value}"));
    }
    if let Some(value) = definition.max_tool_calls {
        parts.push(format!("maxToolCalls:{value}"));
    }
    if let Some(value) = definition.timeout_secs {
        parts.push(format!("timeoutSecs:{value}"));
    }
    if let Some(models) = &definition.model {
        if !models.is_empty() {
            parts.push(format!("model:{}", models.join(",")));
        }
    }
    parts.join(" ")
}

/// Persona local-memory and transcript counts. Missing state is tolerated
/// (returns 0); symlinked or non-regular state reports an error instead of
/// being followed.
#[derive(Clone, Debug, Default)]
struct PersonaStateCounts {
    memory_entries: Option<usize>,
    transcript_count: Option<usize>,
    error: Option<String>,
}

fn persona_state_counts(root: Option<PathBuf>) -> PersonaStateCounts {
    let Some(root) = root else {
        return PersonaStateCounts::default();
    };
    let memory = count_persona_memory(&root.join("memory").join("entries.jsonl"));
    let (memory_entries, error) = match memory {
        Ok(count) => (Some(count), None),
        Err(error) => (None, Some(error.to_string())),
    };
    if error.is_some() {
        return PersonaStateCounts {
            memory_entries,
            transcript_count: None,
            error,
        };
    }
    let transcripts = count_persona_transcripts(&root.join("sessions"));
    let (transcript_count, error) = match transcripts {
        Ok(count) => (Some(count), None),
        Err(error) => (None, Some(error.to_string())),
    };
    PersonaStateCounts {
        memory_entries,
        transcript_count,
        error,
    }
}
fn count_persona_memory(path: &Path) -> Result<usize> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                anyhow::bail!("persona memory is a symlink");
            }
            if !meta.is_file() {
                anyhow::bail!("persona memory is not a regular file");
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    }
    let file = std::fs::File::open(path).with_context(|| format!("opening persona memory {}", path.display()))?;
    let reader = std::io::BufReader::new(file);
    use std::io::BufRead;
    let mut count = 0;
    for line in reader.lines() {
        let line = line.with_context(|| format!("reading persona memory {}", path.display()))?;
        if !line.trim().is_empty() {
            count += 1;
            if count >= PERSONA_COUNT_BOUND {
                break;
            }
        }
    }
    Ok(count)
}

fn count_persona_transcripts(dir: &Path) -> Result<usize> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let mut count = 0;
    for entry in entries {
        let entry = entry?;
        let meta = std::fs::symlink_metadata(entry.path())?;
        if meta.file_type().is_symlink() {
            anyhow::bail!("persona transcript is a symlink");
        }
        if !meta.is_file() {
            anyhow::bail!("persona transcript is not a regular file");
        }
        if entry.file_name().to_string_lossy().ends_with(".jsonl") {
            count += 1;
            if count >= PERSONA_COUNT_BOUND {
                break;
            }
        }
    }
    Ok(count)
}

fn truncate_count_error(error: &str) -> String {
    let bound = 48;
    if error.len() <= bound {
        error.to_owned()
    } else {
        format!("{}…", &error[..bound])
    }
}

/// Resolve the currently preferred agent name (runtime preference, then the
/// persisted settings value). `None` when nothing is preferred.
pub(crate) fn preferred_agent(application: &Application) -> Option<String> {
    if let Some(name) = application
        .orchestration_runtime()
        .and_then(|runtime| runtime.preferred_agent())
    {
        return Some(name);
    }
    application
        .settings_manager()
        .ok()
        .and_then(|manager| {
            manager
                .settings()
                .orchestration
                .as_ref()
                .and_then(|orchestration| orchestration.preferred_agent.clone())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_agent::ThinkingLevel;
    use pi_coding::AgentDefinitionSource;
    use tempfile::tempdir;

    fn model(provider: &str, id: &str) -> Model {
        Model {
            provider: provider.to_owned(),
            id: id.to_owned(),
            name: id.to_owned(),
            ..Model::default()
        }
    }

    fn definition(name: &str, models: Option<Vec<&str>>) -> AgentDefinition {
        AgentDefinition { name: name.to_owned(),
        description: format!("{name} does work"),
        system_prompt: "prompt".to_owned(),
        tools: Some(vec!["read".to_owned(), "bash".to_owned()]),
        autoload_skills: vec!["rust".to_owned()],
        model: models.map(|values| values.into_iter().map(str::to_owned).collect()),
        thinking_level: Some(ThinkingLevel::Medium),
        max_turns: None,
        max_tool_calls: None,
        timeout_secs: None,
        disallowed_tools: Vec::new(),
        capability_ceiling: None,
        source: AgentDefinitionSource::User,
        path: None, trusted: true, kind: pi_coding::AgentDefinitionKind::Agent, personality: None, soft_budget: None }
    }

    #[test]
    fn lists_source_model_thinking_tools_skills() {
        let parent = model("openai", "gpt-4.1");
        let available = vec![
            model("openai", "gpt-4.1"),
            model("anthropic", "claude-sonnet-4-5"),
        ];
        let panel = AgentsPanel::new(
            vec![definition(
                "reviewer",
                Some(vec!["anthropic/claude-sonnet-4-5"]),
            )],
            &BTreeMap::new(),
            parent,
            available,
        );
        let row = panel.selected_row().expect("row");
        assert_eq!(row.name, "reviewer");
        assert_eq!(source_label(row.source), "user");
        assert_eq!(row.effective_model, "anthropic/claude-sonnet-4-5");
        assert_eq!(row.model_source, AgentModelSource::DefinitionFallback);
        assert_eq!(row.thinking, "medium");
        assert_eq!(row.tools, "read, bash");
        assert_eq!(row.skills, "rust");
        assert!(row.enabled);
    }

    #[test]
    fn toggle_and_model_override_mark_dirty_and_resolve() {
        let parent = model("openai", "gpt-4.1");
        let available = vec![
            model("openai", "gpt-4.1"),
            model("anthropic", "claude-sonnet-4-5"),
        ];
        let mut panel = AgentsPanel::new(
            vec![definition("task", None)],
            &BTreeMap::new(),
            parent,
            available,
        );
        panel.toggle_selected_enabled();
        assert!(!panel.selected_row().unwrap().enabled);
        assert!(panel.dirty());

        panel.open_model_picker();
        assert_eq!(panel.focus(), AgentsPanelFocus::ModelPicker);
        if let Some(index) = panel
            .visible_model_ids()
            .iter()
            .position(|id| id == "anthropic/claude-sonnet-4-5")
        {
            panel.model_selected = index;
        }
        panel.apply_selected_model();
        let row = panel.selected_row().unwrap();
        assert_eq!(row.effective_model, "anthropic/claude-sonnet-4-5");
        assert_eq!(row.model_source, AgentModelSource::SettingsOverride);
        assert_eq!(
            row.override_model.as_deref(),
            Some("anthropic/claude-sonnet-4-5")
        );
    }

    #[test]
    fn save_persists_enablement_and_model_override() {
        let agent_dir = tempdir().unwrap();
        let cwd = tempdir().unwrap();
        let manager =
            SettingsManager::load_phase_one(cwd.path(), agent_dir.path()).expect("settings");
        let parent = model("openai", "gpt-4.1");
        let available = vec![
            model("openai", "gpt-4.1"),
            model("anthropic", "claude-sonnet-4-5"),
        ];
        let mut panel = AgentsPanel::new(
            vec![definition("task", None)],
            &BTreeMap::new(),
            parent,
            available,
        );
        panel.toggle_selected_enabled();
        panel.open_model_picker();
        if let Some(index) = panel
            .visible_model_ids()
            .iter()
            .position(|id| id == "anthropic/claude-sonnet-4-5")
        {
            panel.model_selected = index;
        }
        panel.apply_selected_model();
        let action = panel.save(Some(&manager));
        assert!(matches!(action, AgentsPanelAction::Saved));
        assert!(!panel.dirty());

        let reloaded =
            SettingsManager::load_phase_one(cwd.path(), agent_dir.path()).expect("reload");
        let entry = reloaded
            .settings()
            .agent_settings("task")
            .cloned()
            .expect("task settings");
        assert_eq!(entry.enabled, Some(false));
        assert_eq!(entry.model.as_deref(), Some("anthropic/claude-sonnet-4-5"));
    }

    #[test]
    fn reload_definitions_preserves_dirty_edits() {
        let parent = model("openai", "gpt-4.1");
        let available = vec![model("openai", "gpt-4.1")];
        let mut panel = AgentsPanel::new(
            vec![definition("task", None)],
            &BTreeMap::new(),
            parent.clone(),
            available.clone(),
        );
        panel.toggle_selected_enabled();
        assert!(panel.dirty());
        panel.reload_definitions(
            vec![definition("task", Some(vec!["openai/gpt-4.1"]))],
            parent,
            &BTreeMap::new(),
            &BTreeMap::new(),
            None,
        );
        assert!(!panel.selected_row().unwrap().enabled);
        assert!(panel.dirty());
    }

    #[test]
    fn save_writes_only_edited_global_map() {
        let agent_dir = tempdir().unwrap();
        let cwd = tempdir().unwrap();
        let manager =
            SettingsManager::load_phase_one(cwd.path(), agent_dir.path()).expect("settings");
        let parent = model("openai", "gpt-4.1");
        let available = vec![model("openai", "gpt-4.1")];
        // Constructed with global-only map (empty), not project-merged entries.
        let mut panel = AgentsPanel::new(
            vec![definition("task", None)],
            &BTreeMap::new(),
            parent,
            available,
        );
        panel.toggle_selected_enabled();
        let _ = panel.save(Some(&manager));
        let reloaded =
            SettingsManager::load_phase_one(cwd.path(), agent_dir.path()).expect("reload");
        assert!(reloaded.settings().agent_settings("task").is_some());
        assert_eq!(reloaded.settings().agents.len(), 1);
    }

    #[test]
    fn invalid_override_surfaces_error_not_parent() {
        let parent = model("openai", "gpt-4.1");
        let available = vec![
            model("openai", "gpt-4.1"),
            model("anthropic", "claude-sonnet-4-5"),
        ];
        let mut settings = BTreeMap::new();
        settings.insert(
            "task".to_owned(),
            AgentRuntimeSettings {
                enabled: Some(true),
                model: Some("totally-missing/model".to_owned()),
                tools: None,
            },
        );
        let panel = AgentsPanel::new(vec![definition("task", None)], &settings, parent, available);
        let row = panel.selected_row().unwrap();
        assert!(row.model_error.is_some(), "expected model_error");
        assert!(
            row.effective_model.contains("INVALID"),
            "{}",
            row.effective_model
        );
        assert_ne!(row.effective_model, "openai/gpt-4.1");
        let line = panel
            .view_lines()
            .into_iter()
            .find(|line| line.selected)
            .unwrap();
        assert!(line.text.contains("INVALID"), "{}", line.text);
    }

    #[test]
    fn toggle_does_not_promote_project_model() {
        let parent = model("openai", "gpt-4.1");
        let available = vec![
            model("openai", "gpt-4.1"),
            model("anthropic", "claude-sonnet-4-5"),
        ];
        let global = BTreeMap::new();
        let mut effective = BTreeMap::new();
        effective.insert(
            "task".to_owned(),
            AgentRuntimeSettings {
                enabled: Some(true),
                model: Some("anthropic/claude-sonnet-4-5".to_owned()),
                tools: None,
            },
        );
        let mut panel = AgentsPanel::new_with_effective(
            vec![definition("task", None)],
            &global,
            &effective,
            parent,
            available,
        );
        // Live display shows project model...
        assert_eq!(
            panel.selected_row().unwrap().model_source,
            AgentModelSource::SettingsOverride
        );
        panel.toggle_selected_enabled();
        let edit = panel.edits.get("task").expect("global edit after toggle");
        assert_eq!(edit.enabled, Some(false));
        assert!(
            edit.model.is_none(),
            "toggle must not copy project model into global draft: {:?}",
            edit.model
        );
    }

    #[test]
    fn clear_model_override_shadows_project_effective() {
        let parent = model("openai", "gpt-4.1");
        let available = vec![
            model("openai", "gpt-4.1"),
            model("anthropic", "claude-sonnet-4-5"),
        ];
        let global = BTreeMap::new();
        let mut effective = BTreeMap::new();
        effective.insert(
            "task".to_owned(),
            AgentRuntimeSettings {
                enabled: Some(true),
                model: Some("anthropic/claude-sonnet-4-5".to_owned()),
                tools: None,
            },
        );
        let mut panel = AgentsPanel::new_with_effective(
            vec![definition("task", None)],
            &global,
            &effective,
            parent,
            available,
        );
        assert_eq!(
            panel.selected_row().unwrap().model_source,
            AgentModelSource::SettingsOverride
        );
        panel.open_model_picker();
        panel.clear_model_override();
        assert_eq!(
            panel.selected_row().unwrap().model_source,
            AgentModelSource::Parent,
            "global clear must not fall through to project model"
        );
        assert!(panel.selected_row().unwrap().override_model.is_none());
        assert!(
            panel.edits.get("task").is_some_and(|entry| entry.clears_model()),
            "tombstone must remain in draft edits"
        );
    }

    #[test]
    fn clear_model_override_falls_back_to_definition_or_parent() {
        let parent = model("openai", "gpt-4.1");
        let available = vec![
            model("openai", "gpt-4.1"),
            model("anthropic", "claude-sonnet-4-5"),
        ];
        let mut settings = BTreeMap::new();
        settings.insert(
            "task".to_owned(),
            AgentRuntimeSettings {
                enabled: Some(true),
                model: Some("anthropic/claude-sonnet-4-5".to_owned()),
                tools: None,
            },
        );
        let mut panel =
            AgentsPanel::new(vec![definition("task", None)], &settings, parent, available);
        assert_eq!(
            panel.selected_row().unwrap().model_source,
            AgentModelSource::SettingsOverride
        );
        panel.open_model_picker();
        panel.clear_model_override();
        assert_eq!(
            panel.selected_row().unwrap().model_source,
            AgentModelSource::Parent
        );
        assert!(panel.selected_row().unwrap().override_model.is_none());
    }

    #[test]
    fn save_returns_saved_not_success_toast() {
        let agent_dir = tempdir().unwrap();
        let cwd = tempdir().unwrap();
        let manager =
            SettingsManager::load_phase_one(cwd.path(), agent_dir.path()).expect("settings");
        let mut panel = AgentsPanel::new(
            vec![definition("task", None)],
            &BTreeMap::new(),
            model("openai", "gpt-4.1"),
            vec![model("openai", "gpt-4.1")],
        );
        panel.toggle_selected_enabled();
        let action = panel.save(Some(&manager));
        assert_eq!(action, AgentsPanelAction::Saved);
        assert!(
            !panel.status.to_lowercase().contains("saved"),
            "panel must not claim Saved before live reload: {}",
            panel.status
        );
    }

    #[test]
    fn clear_tombstone_survives_save_to_disk() {
        let agent_dir = tempdir().unwrap();
        let cwd = tempdir().unwrap();
        let manager =
            SettingsManager::load_phase_one(cwd.path(), agent_dir.path()).expect("settings");
        let mut global = BTreeMap::new();
        global.insert(
            "task".to_owned(),
            AgentRuntimeSettings {
                enabled: Some(true),
                model: Some("anthropic/claude-sonnet-4-5".to_owned()),
                tools: None,
            },
        );
        let mut panel = AgentsPanel::new(
            vec![definition("task", None)],
            &global,
            model("openai", "gpt-4.1"),
            vec![
                model("openai", "gpt-4.1"),
                model("anthropic", "claude-sonnet-4-5"),
            ],
        );
        panel.open_model_picker();
        panel.clear_model_override();
        assert!(matches!(panel.save(Some(&manager)), AgentsPanelAction::Saved));
        let reloaded =
            SettingsManager::load_phase_one(cwd.path(), agent_dir.path()).expect("reload");
        let settings = reloaded.settings();
        let entry = settings
            .agent_settings("task")
            .expect("tombstone persisted");
        assert!(entry.clears_model(), "{entry:?}");
        assert!(entry.model_override().is_none());
    }

    #[test]
    fn set_model_choices_refreshes_picker_catalog() {
        let mut panel = AgentsPanel::new(
            vec![definition("task", None)],
            &BTreeMap::new(),
            model("openai", "gpt-4.1"),
            vec![model("openai", "gpt-4.1")],
        );
        assert_eq!(panel.visible_model_ids(), vec!["openai/gpt-4.1".to_owned()]);
        panel.set_model_choices(vec![
            model("openai", "gpt-4.1"),
            model("anthropic", "claude-sonnet-4-5"),
        ]);
        let ids = panel.visible_model_ids();
        assert!(
            ids.iter().any(|id| id == "anthropic/claude-sonnet-4-5"),
            "{ids:?}"
        );
        panel.reload_definitions(
            vec![definition("task", None)],
            model("openai", "gpt-4.1"),
            &BTreeMap::new(),
            &BTreeMap::new(),
            Some(vec![model("google", "gemini-2.5-pro")]),
        );
        assert_eq!(
            panel.visible_model_ids(),
            vec!["google/gemini-2.5-pro".to_owned()]
        );
    }

    #[test]
    fn move_list_and_models_wrap_around() {
        let parent = model("openai", "gpt-4.1");
        let available = vec![
            model("openai", "gpt-4.1"),
            model("anthropic", "claude-sonnet-4-5"),
            model("google", "gemini-2.5-pro"),
        ];
        let mut panel = AgentsPanel::new(
            vec![
                definition("one", None),
                definition("two", None),
                definition("three", None),
            ],
            &BTreeMap::new(),
            parent,
            available,
        );
        assert_eq!(panel.rows().len(), 3);
        // Down past the end returns to the first row.
        panel.move_list(3);
        assert_eq!(panel.selected(), 0);
        assert_eq!(panel.selected_row().unwrap().name, "one");
        panel.move_list(1);
        assert_eq!(panel.selected(), 1);
        assert_eq!(panel.selected_row().unwrap().name, "two");
        // Up from the first row wraps to the last.
        panel.move_list(-2);
        assert_eq!(panel.selected(), 2);
        assert_eq!(panel.selected_row().unwrap().name, "three");
        // Model picker wraps within its visible choices.
        panel.open_model_picker();
        assert_eq!(panel.visible_model_ids().len(), 3);
        panel.move_models(3);
        assert_eq!(panel.model_selected(), 0);
        panel.move_models(-1);
        assert_eq!(panel.model_selected(), 2);
        // A single-row list stays put in both directions.
        let mut single = AgentsPanel::new(
            vec![definition("solo", None)],
            &BTreeMap::new(),
            model("openai", "gpt-4.1"),
            vec![model("openai", "gpt-4.1")],
        );
        single.move_list(5);
        assert_eq!(single.selected(), 0);
        single.move_list(-5);
        assert_eq!(single.selected(), 0);
        single.open_model_picker();
        single.move_models(5);
        assert_eq!(single.model_selected(), 0);
        single.move_models(-5);
        assert_eq!(single.model_selected(), 0);
    }
    fn persona_definition(
        name: &str,
        root: &Path,
        personality: Option<&str>,
        soft_budget: Option<pi_coding::JobSoftBudget>,
        max_turns: Option<usize>,
    ) -> AgentDefinition {
        AgentDefinition {
            name: name.to_owned(),
            description: format!("{name} persona"),
            system_prompt: "prompt".to_owned(),
            tools: None,
            autoload_skills: Vec::new(),
            model: None,
            thinking_level: None,
            max_turns,
            max_tool_calls: None,
            timeout_secs: None,
            disallowed_tools: Vec::new(),
            capability_ceiling: None,
            source: AgentDefinitionSource::User,
            path: Some(root.join("persona.md")),
            trusted: true,
            kind: pi_coding::AgentDefinitionKind::Persona,
            personality: personality.map(str::to_owned),
            soft_budget,
        }
    }

    fn persona_panel_with(definition: AgentDefinition) -> AgentsPanel {
        AgentsPanel::new(
            vec![definition],
            &BTreeMap::new(),
            model("openai", "gpt-4.1"),
            vec![model("openai", "gpt-4.1")],
        )
    }

    fn persona_view_line(panel: &AgentsPanel, name: &str) -> String {
        panel
            .view_lines()
            .into_iter()
            .find(|line| line.text.contains(name))
            .unwrap_or_else(|| panic!("no panel line for {name}"))
            .text
    }

    #[test]
    fn persona_panel_row_renders_source_summary_and_counts() {
        let root = tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("memory")).unwrap();
        std::fs::create_dir_all(root.path().join("sessions")).unwrap();
        std::fs::write(
            root.path().join("memory").join("entries.jsonl"),
            "{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n",
        )
        .unwrap();
        std::fs::write(root.path().join("sessions").join("a.jsonl"), "{}\n").unwrap();
        std::fs::write(root.path().join("sessions").join("b.jsonl"), "{}\n").unwrap();
        let def = persona_definition(
            "mentor",
            root.path(),
            Some("steady mentor"),
            Some(pi_coding::JobSoftBudget {
                max_requests: Some(4),
                ..Default::default()
            }),
            Some(8),
        );
        let panel = persona_panel_with(def);
        let line = persona_view_line(&panel, "mentor");
        assert!(line.contains("P "), "persona marker: {line}");
        assert!(line.contains("user"), "source marker: {line}");
        assert!(line.contains("personality:present"), "{line}");
        assert!(line.contains("softBudget:maxRequests:4"), "{line}");
        assert!(line.contains("maxTurns:8"), "{line}");
        assert!(line.contains("mem=3"), "memory count: {line}");
        assert!(line.contains("sessions=2"), "transcript count: {line}");
    }

    #[test]
    fn persona_panel_counts_tolerate_missing_state() {
        let root = tempdir().unwrap();
        let def = persona_definition("scout", root.path(), None, None, None);
        let panel = persona_panel_with(def);
        let line = persona_view_line(&panel, "scout");
        assert!(line.contains("mem=0"), "missing memory tolerated: {line}");
        assert!(line.contains("sessions=0"), "missing sessions tolerated: {line}");
        assert!(line.contains("personality:absent"), "{line}");
    }

    #[test]
    fn persona_panel_counts_bounded_for_large_state() {
        let root = tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("memory")).unwrap();
        std::fs::create_dir_all(root.path().join("sessions")).unwrap();
        let big = "{\"x\":1}\n".repeat(20_000);
        std::fs::write(root.path().join("memory").join("entries.jsonl"), big).unwrap();
        let def = persona_definition("big", root.path(), None, None, None);
        let panel = persona_panel_with(def);
        let line = persona_view_line(&panel, "big");
        assert!(
            line.contains(&format!("mem={}", PERSONA_COUNT_BOUND)),
            "count is bounded: {line}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn persona_panel_counts_error_on_symlinked_memory() {
        let root = tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("memory")).unwrap();
        let outside = tempdir().unwrap();
        std::fs::write(outside.path().join("leak.jsonl"), "secret\n").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("leak.jsonl"),
            root.path().join("memory").join("entries.jsonl"),
        )
        .unwrap();
        let def = persona_definition("sneaky", root.path(), None, None, None);
        let panel = persona_panel_with(def);
        let line = persona_view_line(&panel, "sneaky");
        assert!(line.contains("state=ERR"), "symlinked memory reported: {line}");
        assert!(!line.contains("secret"), "no leak via symlink: {line}");
    }

    #[cfg(unix)]
    #[test]
    fn persona_panel_counts_error_on_symlinked_transcript() {
        let root = tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("sessions")).unwrap();
        let outside = tempdir().unwrap();
        std::fs::write(outside.path().join("leak.jsonl"), "secret\n").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("leak.jsonl"),
            root.path().join("sessions").join("run-1.jsonl"),
        )
        .unwrap();
        let def = persona_definition("sneaky", root.path(), None, None, None);
        let panel = persona_panel_with(def);
        let line = persona_view_line(&panel, "sneaky");
        assert!(line.contains("state=ERR"), "symlinked transcript reported: {line}");
        assert!(!line.contains("secret"), "no leak via symlink: {line}");
    }

    #[test]
    fn persona_panel_counts_error_on_nonregular_transcript() {
        let root = tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("sessions")).unwrap();
        std::fs::create_dir_all(root.path().join("sessions").join("stray")).unwrap();
        let def = persona_definition("odd", root.path(), None, None, None);
        let panel = persona_panel_with(def);
        let line = persona_view_line(&panel, "odd");
        assert!(line.contains("state=ERR"), "non-regular transcript reported: {line}");
    }

    #[test]
    fn persona_panel_selection_marker_shows_preferred() {
        let root = tempdir().unwrap();
        let def = persona_definition("mentor", root.path(), Some("steady"), None, None);
        let mut panel = persona_panel_with(def);
        assert!(!persona_view_line(&panel, "mentor").starts_with('*'));
        panel.set_preferred(Some("mentor".to_owned()));
        let line = persona_view_line(&panel, "mentor");
        assert!(line.starts_with("* P"), "preferred persona marked: {line}");
        panel.set_preferred(None);
        assert!(!persona_view_line(&panel, "mentor").starts_with('*'));
    }
}
