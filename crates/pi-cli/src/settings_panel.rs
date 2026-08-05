//! Schema-driven settings panel state shared by interactive product surfaces.
//!
//! This module derives controls, validation, scope access, and provenance from
//! `pi_coding::SettingsCatalog` and `SettingsDraft`. It owns no second schema.

use anyhow::{Result, bail};
use pi_coding::{
    Application, SettingApplyOutcome, SettingCategory, SettingSource, SettingValueType,
    SettingsCatalog, SettingsDraft, SettingsManager, SettingsScope,
};
use serde::Serialize;
use serde_json::{Map, Value};

const CATEGORIES: &[SettingCategory] = &[
    SettingCategory::Models,
    SettingCategory::Session,
    SettingCategory::Compaction,
    SettingCategory::RetryTransport,
    SettingCategory::TerminalUi,
    SettingCategory::Orchestration,
    SettingCategory::Resources,
    SettingCategory::TrustSecurity,
];

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SettingsControl {
    Boolean { value: Option<bool> },
    Enum { value: Option<String>, options: Vec<String> },
    String { value: Option<String>, non_empty: bool },
    Integer { value: Option<i64>, min: i64, max: i64 },
    UnsignedInteger { value: Option<u64>, min: u64, max: u64 },
    Number { value: Option<f64>, min: f64, max: f64 },
    StringList { value: Vec<String>, non_empty_items: bool },
    List { value: Vec<Value> },
    Object { value: Map<String, Value> },
    Secret { is_set: bool },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPanelRow {
    pub key: String,
    pub category: SettingCategory,
    pub description: String,
    pub control: SettingsControl,
    pub scope: SettingsScope,
    pub scope_value: Option<Value>,
    pub inherited: bool,
    pub effective_value: Value,
    pub source: SettingSource,
    pub global_value: Option<Value>,
    pub project_value: Option<Value>,
    pub session_override_value: Option<Value>,
    pub behavior: pi_coding::SettingApplyBehavior,
    pub writable: bool,
    pub trust_sensitive: bool,
    pub blocked_reason: Option<String>,
    pub redacted: bool,
}
impl SettingsPanelRow {
    #[must_use]
    pub fn section(&self) -> String {
        if self.key.starts_with("thinkingBudgets.") {
            return "Thinking token budgets".to_owned();
        }
        settings_key_section(&self.key)
    }

    #[must_use]
    pub fn display_key(&self) -> &str {
        if let Some(level) = self.key.strip_prefix("thinkingBudgets.") {
            return match level {
                "minimal" => "minimal token budget",
                "low" => "low token budget",
                "medium" => "medium token budget",
                "high" => "high token budget",
                _ => level,
            };
        }
        self.key
            .split_once('.')
            .map_or(self.key.as_str(), |(_, child)| child)
    }
}

fn settings_key_section(key: &str) -> String {
    let Some((prefix, _)) = key.split_once('.') else {
        return "General".to_owned();
    };
    let mut characters = prefix.chars();
    characters.next().map_or_else(
        || "General".to_owned(),
        |first| first.to_uppercase().chain(characters).collect(),
    )
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPanelSnapshot {
    pub scope: SettingsScope,
    pub project_trusted: bool,
    pub category: Option<SettingCategory>,
    pub search: String,
    pub cursor: usize,
    pub dirty: bool,
    pub rows: Vec<SettingsPanelRow>,
}

/// Stateful controller for a settings panel. Edits remain in an atomic draft
/// until apply, and cancel replaces the draft without touching persistence.
#[derive(Clone)]
pub struct SettingsPanel {
    manager: SettingsManager,
    draft: SettingsDraft,
    scope: SettingsScope,
    project_trusted: bool,
    category: Option<SettingCategory>,
    search: String,
    cursor: usize,
}

impl SettingsPanel {
    pub fn new(manager: SettingsManager, scope: SettingsScope) -> Result<Self> {
        let project_trusted = manager.is_project_trusted();
        let draft = SettingsCatalog::draft(&manager, scope)?;
        Ok(Self {
            manager,
            draft,
            scope,
            project_trusted,
            category: None,
            search: String::new(),
            cursor: 0,
        })
    }

    pub fn from_application(application: &Application, scope: SettingsScope) -> Result<Self> {
        let mut panel = Self::new(application.settings_manager()?, scope)?;
        panel
            .draft
            .overlay_runtime_thinking_level(application.session().thinking_level());
        Ok(panel)
    }

    #[must_use]
    pub const fn scope(&self) -> SettingsScope {
        self.scope
    }

    #[must_use]
    pub const fn category(&self) -> Option<SettingCategory> {
        self.category
    }

    #[must_use]
    pub fn search(&self) -> &str {
        &self.search
    }

    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.draft.is_dirty()
    }

    #[must_use]
    pub const fn categories() -> &'static [SettingCategory] {
        CATEGORIES
    }

    pub fn set_scope(&mut self, scope: SettingsScope) -> Result<()> {
        if scope == self.scope {
            return Ok(());
        }
        if self.draft.is_dirty() {
            bail!("cancel or apply the current settings draft before changing scope");
        }
        self.draft = SettingsCatalog::draft(&self.manager, scope)?;
        self.scope = scope;
        self.cursor = 0;
        Ok(())
    }

    pub fn set_category(&mut self, category: Option<SettingCategory>) {
        self.category = category;
        self.cursor = 0;
    }

    pub fn next_category(&mut self) {
        self.category = match self.category {
            None => CATEGORIES.first().copied(),
            Some(current) => CATEGORIES
                .iter()
                .position(|category| *category == current)
                .and_then(|index| CATEGORIES.get(index + 1).copied()),
        };
        self.cursor = 0;
    }

    pub fn previous_category(&mut self) {
        self.category = match self.category {
            None => CATEGORIES.last().copied(),
            Some(current) => CATEGORIES
                .iter()
                .position(|category| *category == current)
                .and_then(|index| index.checked_sub(1))
                .and_then(|index| CATEGORIES.get(index).copied()),
        };
        self.cursor = 0;
    }

    pub fn set_search(&mut self, query: impl Into<String>) {
        self.search = query.into();
        self.cursor = 0;
    }

    pub fn move_next(&mut self) -> Result<()> {
        let count = self.rows()?.len();
        if count > 0 {
            self.cursor = (self.cursor + 1) % count;
        }
        Ok(())
    }

    pub fn move_previous(&mut self) -> Result<()> {
        let count = self.rows()?.len();
        if count > 0 {
            self.cursor = self.cursor.checked_sub(1).unwrap_or(count - 1);
        }
        Ok(())
    }

    pub fn selected(&self) -> Result<Option<SettingsPanelRow>> {
        Ok(self.rows()?.into_iter().nth(self.cursor))
    }

    pub fn rows(&self) -> Result<Vec<SettingsPanelRow>> {
        let query = self.search.trim().to_ascii_lowercase();
        let mut rows = SettingsCatalog::definitions()
            .iter()
            .filter(|definition| self.category.is_none_or(|category| definition.category == category))
            .filter(|definition| {
                query.is_empty()
                    || definition.key.to_ascii_lowercase().contains(&query)
                    || definition.description.to_ascii_lowercase().contains(&query)
                    || format!("{:?}", definition.category)
                        .to_ascii_lowercase()
                        .contains(&query)
            })
            .map(|definition| {
                let view = self.draft.get(definition.key)?;
                let scope_value = match self.scope {
                    SettingsScope::Global => view.global_value.clone(),
                    SettingsScope::Project => view.project_value.clone(),
                };
                let control_value = scope_value.as_ref().unwrap_or(&view.effective_value);
                let writable = match self.scope {
                    SettingsScope::Global => view.editable_global,
                    SettingsScope::Project => view.editable_project,
                };
                let blocked_reason = if definition.secret {
                    Some("secret settings are managed through auth storage or environment variables".to_owned())
                } else if !definition.scopes.allows(self.scope) {
                    Some(format!("setting is unavailable in {:?} scope", self.scope))
                } else if self.scope == SettingsScope::Project && !self.project_trusted {
                    Some("project settings require a trusted project".to_owned())
                } else {
                    None
                };
                Ok(SettingsPanelRow {
                    key: definition.key.to_owned(),
                    category: definition.category,
                    description: definition.description.to_owned(),
                    control: control_for(
                        definition.value_type,
                        definition.enum_values,
                        control_value,
                        view.redacted,
                    ),
                    scope: self.scope,
                    scope_value: scope_value.clone(),
                    inherited: scope_value.is_none(),
                    effective_value: view.effective_value,
                    source: view.source,
                    global_value: view.global_value,
                    project_value: view.project_value,
                    session_override_value: view.session_override_value,
                    behavior: definition.behavior,
                    writable,
                    trust_sensitive: definition.trust_sensitive,
                    blocked_reason,
                    redacted: view.redacted,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if query.is_empty() {
            rows.sort_by_cached_key(SettingsPanelRow::section);
        } else {
            rows.sort_by_cached_key(|row| {
                let key = row.key.to_ascii_lowercase();
                let rank = if key == query {
                    0
                } else if key.starts_with(&query) {
                    1
                } else if key.contains(&query) {
                    2
                } else {
                    3
                };
                (rank, row.section(), key)
            });
        }
        Ok(rows)
    }

    pub fn snapshot(&self) -> Result<SettingsPanelSnapshot> {
        let rows = self.rows()?;
        let cursor = self.cursor.min(rows.len().saturating_sub(1));
        Ok(SettingsPanelSnapshot {
            scope: self.scope,
            project_trusted: self.project_trusted,
            category: self.category,
            search: self.search.clone(),
            cursor,
            dirty: self.draft.is_dirty(),
            rows,
        })
    }

    pub fn set_value(&mut self, key: &str, value: Value) -> Result<()> {
        self.draft.set(key, value)
    }

    pub fn set_boolean(&mut self, key: &str, value: bool) -> Result<()> {
        self.set_value(key, Value::Bool(value))
    }

    pub fn set_enum(&mut self, key: &str, value: impl Into<String>) -> Result<()> {
        self.set_value(key, Value::String(value.into()))
    }

    pub fn set_string(&mut self, key: &str, value: impl Into<String>) -> Result<()> {
        self.set_value(key, Value::String(value.into()))
    }

    pub fn set_integer(&mut self, key: &str, value: i64) -> Result<()> {
        self.set_value(key, Value::Number(value.into()))
    }

    pub fn set_unsigned_integer(&mut self, key: &str, value: u64) -> Result<()> {
        self.set_value(key, Value::Number(value.into()))
    }

    pub fn set_list(&mut self, key: &str, value: Vec<Value>) -> Result<()> {
        self.set_value(key, Value::Array(value))
    }

    pub fn set_object(&mut self, key: &str, value: Map<String, Value>) -> Result<()> {
        self.set_value(key, Value::Object(value))
    }

    pub fn set_input(&mut self, key: &str, input: &str) -> Result<()> {
        let definition = SettingsCatalog::definition(key)
            .ok_or_else(|| anyhow::anyhow!("unknown setting key {key:?}"))?;
        let value = match definition.value_type {
            SettingValueType::Boolean => match input.trim() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => bail!("{key} must be true or false"),
            },
            SettingValueType::Enum | SettingValueType::String { .. } => {
                Value::String(input.to_owned())
            }
            SettingValueType::Integer { .. } => {
                let value = input
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| anyhow::anyhow!("{key} must be an integer"))?;
                Value::Number(value.into())
            }
            SettingValueType::UnsignedInteger { .. } => {
                let value = input
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| anyhow::anyhow!("{key} must be a non-negative integer"))?;
                Value::Number(value.into())
            }
            SettingValueType::Number { .. } => {
                let value = input
                    .trim()
                    .parse::<f64>()
                    .map_err(|_| anyhow::anyhow!("{key} must be a number"))?;
                let number = serde_json::Number::from_f64(value)
                    .ok_or_else(|| anyhow::anyhow!("{key} must be a finite number"))?;
                Value::Number(number)
            }
            SettingValueType::StringList { .. } => {
                let value: Value = serde_json::from_str(input).map_err(|error| {
                    anyhow::anyhow!("{key} must be a valid JSON array of strings: {error}")
                })?;
                if value
                    .as_array()
                    .is_none_or(|values| values.iter().any(|value| !value.is_string()))
                {
                    bail!("{key} must be a JSON array of strings");
                }
                value
            }
            SettingValueType::Array | SettingValueType::Object => {
                serde_json::from_str(input).map_err(|error| {
                    anyhow::anyhow!("{key} must be valid JSON: {error}")
                })?
            }
            SettingValueType::Secret => {
                bail!("{key} is secret material and cannot be edited here")
            }
        };
        self.set_value(key, value)
    }

    pub fn input_hint(&self, key: &str) -> Result<&'static str> {
        let definition = SettingsCatalog::definition(key)
            .ok_or_else(|| anyhow::anyhow!("unknown setting key {key:?}"))?;
        if key.starts_with("thinkingBudgets.") {
            return Ok("integer token override; inherited when unset");
        }
        Ok(match definition.value_type {
            SettingValueType::Boolean => "true or false",
            SettingValueType::Enum => "allowed value",
            SettingValueType::String { .. } => "text",
            SettingValueType::Integer { .. } => "integer",
            SettingValueType::UnsignedInteger { .. } => "non-negative integer",
            SettingValueType::Number { .. } => "number",
            SettingValueType::StringList { .. } => "JSON array of strings",
            SettingValueType::Array => "JSON array",
            SettingValueType::Object => "JSON object",
            SettingValueType::Secret => "managed through authentication storage",
        })
    }

    pub fn input_value(&self, key: &str) -> Result<String> {
        let definition = SettingsCatalog::definition(key)
            .ok_or_else(|| anyhow::anyhow!("unknown setting key {key:?}"))?;
        let view = self.draft.get(key)?;
        let scope_value = match self.scope {
            SettingsScope::Global => view.global_value.as_ref(),
            SettingsScope::Project => view.project_value.as_ref(),
        };
        let value = scope_value.unwrap_or(&view.effective_value);
        match definition.value_type {
            SettingValueType::Boolean => Ok(value.as_bool().map(|value| value.to_string()).unwrap_or_default()),
            SettingValueType::Enum | SettingValueType::String { .. } => {
                Ok(value.as_str().unwrap_or_default().to_owned())
            }
            SettingValueType::Integer { .. }
            | SettingValueType::UnsignedInteger { .. }
            | SettingValueType::Number { .. } => {
                Ok(value.is_number().then(|| value.to_string()).unwrap_or_default())
            }
            SettingValueType::StringList { .. } => Ok(if value
                .as_array()
                .is_some_and(|values| values.iter().all(Value::is_string))
            {
                value.to_string()
            } else {
                "[]".to_owned()
            }),
            SettingValueType::Array => {
                Ok(if value.is_array() { value.to_string() } else { "[]".to_owned() })
            }
            SettingValueType::Object => {
                Ok(if value.is_object() { value.to_string() } else { "{}".to_owned() })
            }
            SettingValueType::Secret => {
                bail!("{key} is secret material and cannot be edited here")
            }
        }
    }

    pub fn reset(&mut self, key: &str) -> Result<()> {
        self.draft.reset(key)
    }

    pub fn validate(&self) -> Result<()> {
        self.draft.validate()
    }

    pub fn cancel(&mut self) -> Result<()> {
        let replacement = SettingsCatalog::draft(&self.manager, self.scope)?;
        let cancelled = std::mem::replace(&mut self.draft, replacement);
        cancelled.cancel();
        self.cursor = self.cursor.min(self.rows()?.len().saturating_sub(1));
        Ok(())
    }

    pub async fn apply(&mut self, application: &Application) -> Result<SettingApplyOutcome> {
        let outcome = application.apply_settings_draft(self.draft.clone()).await?;
        self.manager = application.settings_manager()?;
        self.project_trusted = self.manager.is_project_trusted();
        self.draft = SettingsCatalog::draft(&self.manager, self.scope)?;
        self.cursor = self.cursor.min(self.rows()?.len().saturating_sub(1));
        Ok(outcome)
    }
}

fn control_for(
    value_type: SettingValueType,
    enum_values: &[&str],
    value: &Value,
    redacted: bool,
) -> SettingsControl {
    if redacted || matches!(value_type, SettingValueType::Secret) {
        return SettingsControl::Secret { is_set: redacted };
    }
    match value_type {
        SettingValueType::Boolean => SettingsControl::Boolean { value: value.as_bool() },
        SettingValueType::Enum => SettingsControl::Enum {
            value: value.as_str().map(str::to_owned),
            options: enum_values.iter().map(|value| (*value).to_owned()).collect(),
        },
        SettingValueType::String { non_empty } => SettingsControl::String {
            value: value.as_str().map(str::to_owned),
            non_empty,
        },
        SettingValueType::Integer { min, max } => SettingsControl::Integer {
            value: value.as_i64(),
            min,
            max,
        },
        SettingValueType::UnsignedInteger { min, max } => SettingsControl::UnsignedInteger {
            value: value.as_u64(),
            min,
            max,
        },
        SettingValueType::Number { min, max } => SettingsControl::Number {
            value: value.as_f64(),
            min,
            max,
        },
        SettingValueType::StringList { non_empty_items } => SettingsControl::StringList {
            value: value
                .as_array()
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            non_empty_items,
        },
        SettingValueType::Array => SettingsControl::List {
            value: value.as_array().cloned().unwrap_or_default(),
        },
        SettingValueType::Object => SettingsControl::Object {
            value: value.as_object().cloned().unwrap_or_default(),
        },
        SettingValueType::Secret => SettingsControl::Secret { is_set: redacted },
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;

    fn manager(project_trusted: bool) -> (tempfile::TempDir, tempfile::TempDir, SettingsManager) {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("manager");
        manager.load_project(project_trusted).expect("project trust");
        (agent, cwd, manager)
    }

    #[test]
    fn projects_every_control_type_from_catalog() {
        let (_agent, _cwd, manager) = manager(true);
        let panel = SettingsPanel::new(manager, SettingsScope::Global).expect("panel");
        let rows = panel.rows().expect("rows");
        assert!(rows.iter().any(|row| matches!(row.control, SettingsControl::Boolean { .. })));
        assert!(rows.iter().any(|row| matches!(row.control, SettingsControl::Enum { ref options, .. } if !options.is_empty())));
        assert!(rows.iter().any(|row| matches!(row.control, SettingsControl::String { .. })));
        assert!(rows.iter().any(|row| matches!(row.control, SettingsControl::Integer { .. } | SettingsControl::UnsignedInteger { .. })));
        assert!(rows.iter().any(|row| matches!(row.control, SettingsControl::StringList { .. } | SettingsControl::List { .. })));
        assert!(rows.iter().any(|row| matches!(row.control, SettingsControl::Object { .. })));
        assert!(rows.iter().any(|row| matches!(row.control, SettingsControl::Secret { .. })));
    }

    #[test]
    fn dotted_keys_keep_exact_identity_with_grouped_labels() {
        let (_agent, _cwd, manager) = manager(true);
        let mut panel = SettingsPanel::new(manager, SettingsScope::Global).expect("panel");
        panel.set_category(Some(SettingCategory::RetryTransport));
        panel.set_search("retry.enabled");
        let rows = panel.rows().expect("rows");
        let row = rows
            .iter()
            .find(|row| row.key == "retry.enabled")
            .expect("retry.enabled row");
        assert_eq!(row.section(), "Retry");
        assert_eq!(row.display_key(), "enabled");
        panel.cursor = rows
            .iter()
            .position(|row| row.key == "retry.enabled")
            .expect("retry.enabled position");
        assert_eq!(panel.selected().expect("selected").expect("row").key, "retry.enabled");
    }

    #[test]
    fn thinking_budget_rows_are_labeled_as_numeric_overrides_without_mutation() {
        let (_agent, _cwd, manager) = manager(true);
        let mut panel = SettingsPanel::new(manager, SettingsScope::Global).expect("panel");
        panel.set_search("thinkingBudgets.medium");
        let row = panel.selected().expect("selected").expect("medium row");
        assert_eq!(row.key, "thinkingBudgets.medium");
        assert_eq!(row.section(), "Thinking token budgets");
        assert_eq!(row.display_key(), "medium token budget");
        assert_eq!(panel.input_hint(&row.key).expect("hint"), "integer token override; inherited when unset");
        assert!(row.inherited);
        assert!(!panel.is_dirty(), "selecting the override row must not stage a value");
    }

    #[test]
    fn category_search_navigation_and_scope_provenance_are_staged() {
        let (_agent, cwd, manager) = manager(true);
        manager
            .update_global(|settings| settings.theme = Some("global-theme".to_owned()))
            .expect("global theme");
        fs::create_dir_all(cwd.path().join(".pi")).expect("project dir");
        fs::write(
            cwd.path().join(".pi/settings.json"),
            r#"{"theme":"project-theme"}"#,
        )
        .expect("project settings");
        manager.load_project(true).expect("reload project");

        let mut panel = SettingsPanel::new(manager, SettingsScope::Project).expect("panel");
        panel.set_category(Some(SettingCategory::TerminalUi));
        panel.set_search("theme");
        let rows = panel.rows().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, SettingSource::Project);
        assert_eq!(rows[0].effective_value, json!("project-theme"));
        assert_eq!(rows[0].scope_value, Some(json!("project-theme")));
        panel.reset("theme").expect("reset");
        let row = panel.rows().expect("staged rows").remove(0);
        assert_eq!(row.source, SettingSource::Global);
        assert_eq!(row.effective_value, json!("global-theme"));
        assert!(row.inherited);
        panel.next_category();
        assert_eq!(panel.category(), Some(SettingCategory::Orchestration));
        panel.previous_category();
        assert_eq!(panel.category(), Some(SettingCategory::TerminalUi));
    }

    #[test]
    fn validation_trust_secret_cancel_and_unknown_fields_are_safe() {
        let (agent, cwd, _) = manager(false);
        fs::write(
            agent.path().join("settings.json"),
            r#"{"future":{"nested":1},"apiKey":"never-print-this"}"#,
        )
        .expect("seed settings");
        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("reload");
        assert!(SettingsPanel::new(manager.clone(), SettingsScope::Project).is_err());
        let mut panel = SettingsPanel::new(manager.clone(), SettingsScope::Global).expect("panel");
        assert!(panel.set_boolean("compaction.enabled", true).is_ok());
        assert!(panel.set_enum("transport", "udp").is_err());
        assert!(panel.set_integer("compaction.reserveTokens", 0).is_err());
        let secret = panel
            .rows()
            .expect("rows")
            .into_iter()
            .find(|row| row.key == "apiKey")
            .expect("secret row");
        assert!(secret.redacted);
        assert!(!secret.writable);
        assert!(!serde_json::to_string(&secret).expect("serialize").contains("never-print-this"));
        panel.cancel().expect("cancel");
        assert!(!panel.is_dirty());
        assert_eq!(manager.global_settings().compaction, None);
        let saved: Value = serde_json::from_slice(
            &fs::read(agent.path().join("settings.json")).expect("settings file"),
        )
        .expect("settings json");

        assert_eq!(saved["future"]["nested"], 1);
    }

    #[test]
    fn typed_input_preserves_strings_and_validates_numeric_and_collection_controls() {
        let (_agent, _cwd, manager) = manager(true);
        let mut panel = SettingsPanel::new(manager, SettingsScope::Global).expect("panel");

        panel.set_input("theme", "true").expect("string input");
        assert_eq!(
            panel
                .rows()
                .expect("rows")
                .into_iter()
                .find(|row| row.key == "theme")
                .expect("theme row")
                .scope_value,
            Some(json!("true"))
        );

        panel.set_input("maxTokens", "4096").expect("integer input");
        panel
            .set_input("terminal.imageWidthCells", "80")
            .expect("unsigned input");
        panel.set_input("temperature", "0.75").expect("number input");
        panel
            .set_input("enabledModels", r#"["first, model","  padded  "]"#)
            .expect("string list input");
        panel
            .set_input("keybindings", r#"{"submit":"enter"}"#)
            .expect("object input");
        panel
            .set_input("packages", r#"[{"source":"example"}]"#)
            .expect("array input");

        assert_eq!(panel.input_value("theme").expect("theme input"), "true");
        assert_eq!(
            panel.input_value("enabledModels").expect("list input"),
            r#"["first, model","  padded  "]"#
        );
        assert_eq!(
            panel.input_value("keybindings").expect("object input"),
            r#"{"submit":"enter"}"#
        );

        assert!(panel.set_input("maxTokens", "4.5").is_err());
        assert!(panel.set_input("terminal.imageWidthCells", "-1").is_err());
        assert!(panel.set_input("temperature", "NaN").is_err());
        assert!(panel.set_input("enabledModels", r#"["first",2]"#).is_err());
        assert!(panel.set_input("keybindings", "[]").is_err());
        assert!(panel.set_input("packages", "{}").is_err());
        assert!(panel.set_input("apiKey", "secret").is_err());

        assert_eq!(panel.input_hint("enabledModels").expect("list hint"), "JSON array of strings");
        assert_eq!(panel.input_hint("keybindings").expect("object hint"), "JSON object");
    }

    #[test]
    fn stale_panel_draft_refuses_to_overwrite_newer_settings() {
        let (_agent, _cwd, manager) = manager(false);
        let mut panel = SettingsPanel::new(manager.clone(), SettingsScope::Global).expect("panel");
        panel.set_string("theme", "stale").expect("staged theme");
        manager
            .update_global(|settings| settings.quiet_startup = Some(true))
            .expect("concurrent write");
        let error = panel.draft.clone().apply(&manager).expect_err("stale draft");
        assert!(error.to_string().contains("settings changed"));
        assert_eq!(manager.global_settings().theme, None);
    }
}
