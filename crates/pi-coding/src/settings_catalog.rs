//! Schema-driven settings inspection and atomic draft persistence.
//!
//! UI and command adapters consume this module's metadata and value views;
//! they must not duplicate setting types, constraints, defaults, or scope rules.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::settings::{
    SESSION_IMPORT_SOURCE_VALUES, Settings, SettingsManager, SettingsScope, validate_settings,
};

const REDACTED_VALUE: &str = "[redacted]";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SettingCategory {
    Models,
    Session,
    Compaction,
    RetryTransport,
    TerminalUi,
    Orchestration,
    Resources,
    TrustSecurity,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SettingValueType {
    Boolean,
    String { non_empty: bool },
    Integer { min: i64, max: i64 },
    UnsignedInteger { min: u64, max: u64 },
    Number { min: f64, max: f64 },
    Enum,
    StringList { non_empty_items: bool },
    Array,
    Object,
    Secret,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SettingApplyBehavior {
    Live,
    Reload,
    Restart,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SettingSource {
    Default,
    Global,
    Project,
    SessionOverride,
    /// Live session value that is not a persisted settings override (e.g. clamped thinking).
    Runtime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SettingScopeSupport {
    None,
    GlobalOnly,
    GlobalAndProject,
}

impl SettingScopeSupport {
    #[must_use]
    pub const fn allows(self, scope: SettingsScope) -> bool {
        matches!(
            (self, scope),
            (Self::GlobalOnly | Self::GlobalAndProject, SettingsScope::Global)
                | (Self::GlobalAndProject, SettingsScope::Project)
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingDef {
    pub key: &'static str,
    pub category: SettingCategory,
    pub value_type: SettingValueType,
    pub default_json: &'static str,
    pub description: &'static str,
    pub enum_values: &'static [&'static str],
    pub scopes: SettingScopeSupport,
    pub behavior: SettingApplyBehavior,
    pub secret: bool,
    pub trust_sensitive: bool,
}

impl SettingDef {
    #[must_use]
    pub fn default_value(&self) -> Value {
        serde_json::from_str(self.default_json)
            .expect("settings catalog defaults must be valid JSON")
    }

    pub fn validate_value(&self, value: &Value) -> Result<()> {
        if self.secret || matches!(self.value_type, SettingValueType::Secret) {
            bail!(
                "{} is secret material and cannot be read or written through settings.json",
                self.key
            );
        }
        match self.value_type {
            SettingValueType::Boolean if !value.is_boolean() => {
                bail!("{} must be a boolean", self.key);
            }
            SettingValueType::String { non_empty } => {
                let Some(value) = value.as_str() else {
                    bail!("{} must be a string", self.key);
                };
                if non_empty && value.trim().is_empty() {
                    bail!("{} must not be empty", self.key);
                }
            }
            SettingValueType::Integer { min, max } => {
                let Some(value) = value.as_i64() else {
                    bail!("{} must be an integer", self.key);
                };
                if !(min..=max).contains(&value) {
                    bail!("{} must be between {min} and {max}", self.key);
                }
            }
            SettingValueType::UnsignedInteger { min, max } => {
                let Some(value) = value.as_u64() else {
                    bail!("{} must be a non-negative integer", self.key);
                };
                if !(min..=max).contains(&value) {
                    bail!("{} must be between {min} and {max}", self.key);
                }
            }
            SettingValueType::Number { min, max } => {
                let Some(value) = value.as_f64() else {
                    bail!("{} must be a number", self.key);
                };
                if !value.is_finite() || !(min..=max).contains(&value) {
                    bail!("{} must be finite and between {min} and {max}", self.key);
                }
            }
            SettingValueType::Enum => {
                let Some(value) = value.as_str() else {
                    bail!("{} must be one of: {}", self.key, self.enum_values.join(", "));
                };
                if !self.enum_values.contains(&value) {
                    bail!("{} must be one of: {}", self.key, self.enum_values.join(", "));
                }
            }
            SettingValueType::StringList { non_empty_items } => {
                let Some(values) = value.as_array() else {
                    bail!("{} must be an array of strings", self.key);
                };
                if values.iter().any(|value| {
                    value
                        .as_str()
                        .is_none_or(|value| non_empty_items && value.trim().is_empty())
                }) {
                    bail!("{} must contain only non-empty strings", self.key);
                }
            }
            SettingValueType::Array if !value.is_array() => {
                bail!("{} must be an array", self.key);
            }
            SettingValueType::Object if !value.is_object() => {
                bail!("{} must be an object", self.key);
            }
            SettingValueType::Secret => unreachable!("secret rejected above"),
            SettingValueType::Boolean
            | SettingValueType::Array
            | SettingValueType::Object => {}
        }
        Ok(())
    }
}

const ALL: SettingScopeSupport = SettingScopeSupport::GlobalAndProject;
const GLOBAL: SettingScopeSupport = SettingScopeSupport::GlobalOnly;
const LIVE: SettingApplyBehavior = SettingApplyBehavior::Live;
const RELOAD: SettingApplyBehavior = SettingApplyBehavior::Reload;
const RESTART: SettingApplyBehavior = SettingApplyBehavior::Restart;
const NONE: &[&str] = &[];
const BOOL: SettingValueType = SettingValueType::Boolean;
const STRING: SettingValueType = SettingValueType::String { non_empty: false };
const NON_EMPTY_STRING: SettingValueType = SettingValueType::String { non_empty: true };
const STRING_LIST: SettingValueType = SettingValueType::StringList { non_empty_items: true };
const OBJECT: SettingValueType = SettingValueType::Object;
const ARRAY: SettingValueType = SettingValueType::Array;
const POSITIVE_I64: SettingValueType = SettingValueType::Integer { min: 1, max: i64::MAX };
const NON_NEGATIVE_I64: SettingValueType = SettingValueType::Integer { min: 0, max: i64::MAX };
const NON_NEGATIVE_U64: SettingValueType = SettingValueType::UnsignedInteger { min: 0, max: u64::MAX };
const POSITIVE_U64: SettingValueType = SettingValueType::UnsignedInteger { min: 1, max: u64::MAX };

macro_rules! setting {
    ($key:literal, $category:ident, $ty:expr, $default:literal, $description:literal, $enum_values:expr, $scopes:expr, $behavior:expr, $secret:expr, $trust:expr) => {
        SettingDef {
            key: $key,
            category: SettingCategory::$category,
            value_type: $ty,
            default_json: $default,
            description: $description,
            enum_values: $enum_values,
            scopes: $scopes,
            behavior: $behavior,
            secret: $secret,
            trust_sensitive: $trust,
        }
    };
}

/// Exhaustive schema for every typed [`Settings`] field. Nested object fields
/// are cataloged individually. `apiKey` is a guarded sentinel for credentials
/// mistakenly placed in settings.json; its value is always redacted.
pub const SETTINGS_CATALOG: &[SettingDef] = &[
    setting!("defaultProvider", Models, STRING, "null", "Default provider for new sessions.", NONE, ALL, RESTART, false, false),
    setting!("defaultModel", Models, STRING, "null", "Default model for new sessions.", NONE, ALL, RESTART, false, false),
    setting!("defaultThinkingLevel", Models, SettingValueType::Enum, "\"medium\"", "Default reasoning level for new sessions.", &["off", "minimal", "low", "medium", "high", "xhigh"], ALL, RESTART, false, false),
    setting!("thinkingBudgets.minimal", Models, POSITIVE_I64, "null", "Token budget for minimal reasoning.", NONE, ALL, LIVE, false, false),
    setting!("thinkingBudgets.low", Models, POSITIVE_I64, "null", "Token budget for low reasoning.", NONE, ALL, LIVE, false, false),
    setting!("thinkingBudgets.medium", Models, POSITIVE_I64, "null", "Token budget for medium reasoning.", NONE, ALL, LIVE, false, false),
    setting!("thinkingBudgets.high", Models, POSITIVE_I64, "null", "Token budget for high reasoning.", NONE, ALL, LIVE, false, false),
    setting!("scopedModels", Models, STRING_LIST, "null", "Provider/model patterns visible to model selection.", NONE, ALL, LIVE, false, false),
    setting!("enabledModels", Models, STRING_LIST, "null", "Legacy alias for scopedModels.", NONE, ALL, LIVE, false, false),
    setting!("temperature", Models, SettingValueType::Number { min: 0.0, max: 2.0 }, "null", "Sampling temperature for model requests.", NONE, ALL, LIVE, false, false),
    setting!("maxTokens", Models, POSITIVE_I64, "null", "Maximum output tokens per model request.", NONE, ALL, LIVE, false, false),
    setting!("cacheRetention", Models, SettingValueType::Enum, "\"short\"", "Prompt-cache retention policy.", &["none", "short", "long"], ALL, LIVE, false, false),

    setting!("steeringMode", Session, SettingValueType::Enum, "\"one-at-a-time\"", "Queue behavior for steering messages.", &["all", "one-at-a-time"], ALL, LIVE, false, false),
    setting!("followUpMode", Session, SettingValueType::Enum, "\"one-at-a-time\"", "Queue behavior for follow-up messages.", &["all", "one-at-a-time"], ALL, LIVE, false, false),
    setting!("branchSummary.reserveTokens", Session, POSITIVE_I64, "16384", "Tokens reserved while generating branch summaries.", NONE, ALL, LIVE, false, false),
    setting!("branchSummary.skipPrompt", Session, BOOL, "false", "Skip the branch-summary prompt when possible.", NONE, ALL, LIVE, false, false),
    setting!("exposeSessionEnvironment", Session, BOOL, "true", "Expose the session environment to tools.", NONE, ALL, LIVE, false, true),
    setting!("sessionDir", Session, NON_EMPTY_STRING, "null", "Directory used for session storage and lookup.", NONE, ALL, RESTART, false, false),
    setting!("sessionImportSources", Session, STRING_LIST, "[]", "Foreign session sources eligible for automatic discovery and resume; native Pi sessions are always included.", SESSION_IMPORT_SOURCE_VALUES, ALL, RELOAD, false, false),

    setting!("compaction.enabled", Compaction, BOOL, "true", "Enable automatic context compaction.", NONE, ALL, LIVE, false, false),
    setting!("compaction.reserveTokens", Compaction, POSITIVE_I64, "16384", "Tokens reserved for the next response during compaction.", NONE, ALL, LIVE, false, false),
    setting!("compaction.keepRecentTokens", Compaction, NON_NEGATIVE_I64, "20000", "Recent tokens retained verbatim during compaction.", NONE, ALL, LIVE, false, false),

    setting!("retry.enabled", RetryTransport, BOOL, "true", "Enable automatic retry.", NONE, ALL, LIVE, false, false),
    setting!("retry.maxRetries", RetryTransport, NON_NEGATIVE_U64, "3", "Maximum automatic retry attempts.", NONE, ALL, LIVE, false, false),
    setting!("retry.baseDelayMs", RetryTransport, POSITIVE_U64, "2000", "Initial automatic retry delay in milliseconds.", NONE, ALL, LIVE, false, false),
    setting!("retry.provider.timeoutMs", RetryTransport, NON_NEGATIVE_U64, "null", "Provider request timeout override in milliseconds.", NONE, ALL, LIVE, false, false),
    setting!("retry.provider.maxRetries", RetryTransport, NON_NEGATIVE_U64, "0", "Provider transport retry count.", NONE, ALL, LIVE, false, false),
    setting!("retry.provider.maxRetryDelayMs", RetryTransport, NON_NEGATIVE_U64, "null", "Provider transport retry-delay cap in milliseconds.", NONE, ALL, LIVE, false, false),
    setting!("retry.modelFallback", RetryTransport, BOOL, "true", "Allow retry recovery to switch to configured fallback models.", NONE, ALL, LIVE, false, false),
    setting!("retry.fallbackChains", RetryTransport, OBJECT, "{}", "Ordered model/provider fallback chains keyed by role, model selector, or provider wildcard.", NONE, ALL, LIVE, false, false),
    setting!("autoRetry", RetryTransport, BOOL, "true", "Legacy alias for retry.enabled.", NONE, ALL, LIVE, false, false),
    setting!("maxRetries", RetryTransport, NON_NEGATIVE_U64, "3", "Legacy alias for retry.maxRetries.", NONE, ALL, LIVE, false, false),
    setting!("baseDelayMs", RetryTransport, POSITIVE_U64, "2000", "Legacy alias for retry.baseDelayMs.", NONE, ALL, LIVE, false, false),
    setting!("transport", RetryTransport, SettingValueType::Enum, "\"auto\"", "Streaming transport selection.", &["auto", "sse", "web-socket", "web-socket-cached"], ALL, LIVE, false, false),
    setting!("timeoutMs", RetryTransport, NON_NEGATIVE_U64, "null", "HTTP or stream timeout in milliseconds.", NONE, ALL, LIVE, false, false),
    setting!("httpIdleTimeoutMs", RetryTransport, NON_NEGATIVE_U64, "null", "Idle HTTP timeout in milliseconds.", NONE, ALL, LIVE, false, false),
    setting!("websocketConnectTimeoutMs", RetryTransport, NON_NEGATIVE_U64, "null", "WebSocket connection timeout in milliseconds.", NONE, ALL, LIVE, false, false),
    setting!("maxRetryDelayMs", RetryTransport, NON_NEGATIVE_U64, "60000", "Maximum transport retry delay in milliseconds.", NONE, ALL, LIVE, false, false),

    setting!("theme", TerminalUi, NON_EMPTY_STRING, "null", "Initial terminal theme name.", NONE, ALL, LIVE, false, false),
    setting!("terminal.showImages", TerminalUi, BOOL, "true", "Display images in supported terminals.", NONE, ALL, LIVE, false, false),
    setting!("terminal.imageWidthCells", TerminalUi, SettingValueType::UnsignedInteger { min: 1, max: u16::MAX as u64 }, "60", "Maximum image width in terminal cells.", NONE, ALL, LIVE, false, false),
    setting!("terminal.clearOnShrink", TerminalUi, BOOL, "false", "Clear terminal content when its viewport shrinks.", NONE, ALL, LIVE, false, false),
    setting!("terminal.showTerminalProgress", TerminalUi, BOOL, "true", "Display terminal progress indicators.", NONE, ALL, LIVE, false, false),
    setting!("images.autoResize", TerminalUi, BOOL, "true", "Resize images to fit the terminal.", NONE, ALL, LIVE, false, false),
    setting!("images.blockImages", TerminalUi, BOOL, "false", "Block image rendering.", NONE, ALL, LIVE, false, false),
    setting!("showImages", TerminalUi, BOOL, "true", "Legacy alias for terminal.showImages.", NONE, ALL, LIVE, false, false),
    setting!("imageWidthCells", TerminalUi, SettingValueType::UnsignedInteger { min: 1, max: u16::MAX as u64 }, "60", "Legacy alias for terminal.imageWidthCells.", NONE, ALL, LIVE, false, false),
    setting!("autoResizeImages", TerminalUi, BOOL, "true", "Legacy alias for images.autoResize.", NONE, ALL, LIVE, false, false),
    setting!("keybindings", TerminalUi, OBJECT, "{}", "Inline action-to-key-chord mappings.", NONE, ALL, RELOAD, false, false),
    setting!("quietStartup", TerminalUi, BOOL, "false", "Suppress non-essential startup messages.", NONE, ALL, LIVE, false, false),
    setting!("hideThinkingBlock", TerminalUi, BOOL, "false", "Legacy inverse alias for showThinking.", NONE, ALL, LIVE, false, false),
    setting!("showThinking", TerminalUi, BOOL, "true", "Show model thinking blocks.", NONE, ALL, LIVE, false, false),
    setting!("doubleEscapeAction", TerminalUi, SettingValueType::Enum, "\"tree\"", "Action invoked by pressing Escape twice.", &["fork", "tree", "none"], ALL, LIVE, false, false),

    setting!("orchestration.process", Orchestration, BOOL, "false", "Enable the process orchestration tool.", NONE, ALL, RELOAD, false, true),
    setting!("orchestration.tasks", Orchestration, BOOL, "false", "Enable task-agent orchestration.", NONE, ALL, RELOAD, false, true),
    setting!("orchestration.todo", Orchestration, BOOL, "false", "Enable the todo orchestration tool.", NONE, ALL, RELOAD, false, true),
    setting!("orchestration.maxConcurrency", Orchestration, SettingValueType::UnsignedInteger { min: 1, max: 64 }, "4", "Maximum concurrent orchestration agents.", NONE, ALL, RELOAD, false, true),
    setting!("orchestration.maxRecursionDepth", Orchestration, SettingValueType::UnsignedInteger { min: 0, max: 16 }, "2", "Maximum orchestration recursion depth.", NONE, ALL, RELOAD, false, true),
    setting!("orchestration.mailboxCapacity", Orchestration, SettingValueType::UnsignedInteger { min: 1, max: 10000 }, "100", "Per-agent orchestration mailbox capacity.", NONE, ALL, RELOAD, false, true),
    setting!("orchestration.maxToolsPerAgent", Orchestration, SettingValueType::UnsignedInteger { min: 1, max: 64 }, "16", "Maximum tools exposed to each orchestration agent.", NONE, ALL, RELOAD, false, true),
    setting!("selector.enabled", Orchestration, BOOL, "true", "Enable automatic agent and skill selection.", NONE, ALL, RELOAD, false, true),
    setting!("selector.maxResults", Orchestration, SettingValueType::UnsignedInteger { min: 1, max: 20 }, "5", "Maximum deterministic selector results.", NONE, ALL, RELOAD, false, true),
    setting!("selector.minScore", Orchestration, NON_NEGATIVE_I64, "60", "Minimum deterministic selector score.", NONE, ALL, RELOAD, false, true),
    setting!("selector.autoloadThreshold", Orchestration, NON_NEGATIVE_I64, "900", "Score required to autoload a selection.", NONE, ALL, RELOAD, false, true),
    setting!("selector.autoSelectThreshold", Orchestration, NON_NEGATIVE_I64, "1600", "Score required to auto-select a result.", NONE, ALL, RELOAD, false, true),
    setting!("selector.confidenceMargin", Orchestration, NON_NEGATIVE_I64, "200", "Minimum lead over the next selector result.", NONE, ALL, RELOAD, false, true),
    setting!("selector.classifier.enabled", Orchestration, BOOL, "false", "Enable model-assisted selection.", NONE, ALL, RELOAD, false, true),
    setting!("selector.classifier.model", Orchestration, STRING, "null", "Optional model override for classification.", NONE, ALL, RELOAD, false, true),
    setting!("selector.classifier.maxTokens", Orchestration, SettingValueType::Integer { min: 1, max: 256 }, "256", "Maximum classifier output tokens.", NONE, ALL, RELOAD, false, true),
    setting!("selector.classifier.timeoutMs", Orchestration, SettingValueType::UnsignedInteger { min: 1, max: 60000 }, "4000", "Classifier timeout in milliseconds.", NONE, ALL, RELOAD, false, true),
    setting!("agents", Orchestration, OBJECT, "{}", "Per-agent enablement and model overrides.", NONE, ALL, RELOAD, false, true),

    setting!("packages", Resources, ARRAY, "[]", "Package sources supplying extensions and resources.", NONE, ALL, RELOAD, false, true),
    setting!("extensions", Resources, STRING_LIST, "[]", "Extension paths or identifiers to load.", NONE, ALL, RELOAD, false, true),
    setting!("skills", Resources, STRING_LIST, "[]", "Skill paths or identifiers to load.", NONE, ALL, RELOAD, false, true),
    setting!("prompts", Resources, STRING_LIST, "[]", "Prompt-template paths or identifiers to load.", NONE, ALL, RELOAD, false, true),
    setting!("themes", Resources, STRING_LIST, "[]", "Theme paths or identifiers to load.", NONE, ALL, RELOAD, false, true),
    setting!("enableSkillCommands", Resources, BOOL, "true", "Expose loaded skills as interactive commands.", NONE, ALL, RELOAD, false, true),

    setting!("defaultProjectTrust", TrustSecurity, SettingValueType::Enum, "\"ask\"", "Default trust for projects without a stored decision.", &["ask", "always", "never"], GLOBAL, RESTART, false, true),
    setting!("approvalMode", TrustSecurity, SettingValueType::Enum, "\"yolo\"", "Host tool approval policy.", &["yolo", "write", "ask"], GLOBAL, RESTART, false, true),
    setting!("apiKey", TrustSecurity, SettingValueType::Secret, "null", "Misplaced API credentials are redacted; use auth.json or environment variables.", NONE, SettingScopeSupport::None, RESTART, true, true),
];

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingValueView {
    pub definition: &'static SettingDef,
    pub effective_value: Value,
    pub source: SettingSource,
    pub global_value: Option<Value>,
    pub project_value: Option<Value>,
    pub session_override_value: Option<Value>,
    pub editable_global: bool,
    pub editable_project: bool,
    pub redacted: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsCatalogSnapshot {
    pub project_trusted: bool,
    pub global_path: PathBuf,
    pub project_path: PathBuf,
    pub values: Vec<SettingValueView>,
}

pub struct SettingsCatalog;

impl SettingsCatalog {
    #[must_use]
    pub const fn definitions() -> &'static [SettingDef] {
        SETTINGS_CATALOG
    }

    #[must_use]
    pub fn definition(key: &str) -> Option<&'static SettingDef> {
        SETTINGS_CATALOG.iter().find(|definition| definition.key == key)
    }

    pub fn get(manager: &SettingsManager, key: &str) -> Result<SettingValueView> {
        let definition = Self::definition(key).ok_or_else(|| anyhow!("unknown setting key {key:?}"))?;
        let runtime = BTreeMap::new();
        Ok(view_for(
            definition,
            &manager.global_settings(),
            &manager.project_settings(),
            &manager.session_overrides(),
            &runtime,
            manager.is_project_trusted(),
        ))
    }

    #[must_use]
    pub fn inspect(manager: &SettingsManager) -> SettingsCatalogSnapshot {
        let paths = manager.paths();
        let global = manager.global_settings();
        let project = manager.project_settings();
        let overrides = manager.session_overrides();
        let project_trusted = manager.is_project_trusted();
        let runtime = BTreeMap::new();
        SettingsCatalogSnapshot {
            project_trusted,
            global_path: paths.global,
            project_path: paths.project,
            values: SETTINGS_CATALOG
                .iter()
                .map(|definition| {
                    view_for(
                        definition,
                        &global,
                        &project,
                        &overrides,
                        &runtime,
                        project_trusted,
                    )
                })
                .collect(),
        }
    }

    #[must_use]
    pub fn search(manager: &SettingsManager, query: &str) -> Vec<SettingValueView> {
        let query = query.trim().to_ascii_lowercase();
        Self::inspect(manager)
            .values
            .into_iter()
            .filter(|view| {
                query.is_empty()
                    || view.definition.key.to_ascii_lowercase().contains(&query)
                    || view.definition.description.to_ascii_lowercase().contains(&query)
                    || format!("{:?}", view.definition.category)
                        .to_ascii_lowercase()
                        .contains(&query)
            })
            .collect()
    }

    pub fn draft(manager: &SettingsManager, scope: SettingsScope) -> Result<SettingsDraft> {
        SettingsDraft::new(manager, scope)
    }
}

impl SettingsManager {
    pub fn setting(&self, key: &str) -> Result<SettingValueView> {
        SettingsCatalog::get(self, key)
    }

    #[must_use]
    pub fn settings_catalog_snapshot(&self) -> SettingsCatalogSnapshot {
        SettingsCatalog::inspect(self)
    }

    pub fn settings_draft(&self, scope: SettingsScope) -> Result<SettingsDraft> {
        SettingsCatalog::draft(self, scope)
    }
}

#[derive(Clone, Debug, PartialEq)]
enum DraftEdit {
    Set(Value),
    Reset,
}

#[derive(Clone, Debug)]
pub struct SettingsDraft {
    scope: SettingsScope,
    global: Settings,
    project: Settings,
    overrides: Settings,
    project_trusted: bool,
    base_scope_settings: Settings,
    validation_path: PathBuf,
    edits: BTreeMap<&'static str, DraftEdit>,
    /// Display-only runtime values (not persisted). Highest precedence for views.
    runtime: BTreeMap<&'static str, Value>,
}

impl SettingsDraft {
    fn new(manager: &SettingsManager, scope: SettingsScope) -> Result<Self> {
        if scope == SettingsScope::Project && !manager.is_project_trusted() {
            bail!("project is not trusted; refusing to edit project settings");
        }
        let paths = manager.paths();
        Ok(Self {
            scope,
            global: manager.global_settings(),
            project: manager.project_settings(),
            overrides: manager.session_overrides(),
            project_trusted: manager.is_project_trusted(),
            base_scope_settings: match scope {
                SettingsScope::Global => manager.global_settings(),
                SettingsScope::Project => manager.project_settings(),
            },
            validation_path: match scope {
                SettingsScope::Global => paths.global,
                SettingsScope::Project => paths.project,
            },
            edits: BTreeMap::new(),
            runtime: BTreeMap::new(),
        })
    }

    /// Overlay a live session thinking level for display/provenance without dirtying the draft.
    pub fn overlay_runtime_thinking_level(&mut self, level: pi_agent::ThinkingLevel) {
        let name = match level {
            pi_agent::ThinkingLevel::Off => "off",
            pi_agent::ThinkingLevel::Minimal => "minimal",
            pi_agent::ThinkingLevel::Low => "low",
            pi_agent::ThinkingLevel::Medium => "medium",
            pi_agent::ThinkingLevel::High => "high",
            pi_agent::ThinkingLevel::Xhigh => "xhigh",
            pi_agent::ThinkingLevel::Max => "max",
        };
        self.runtime
            .insert("defaultThinkingLevel", Value::String(name.to_owned()));
    }

    #[must_use]
    pub const fn scope(&self) -> SettingsScope {
        self.scope
    }

    #[must_use]
    pub fn is_dirty(&self) -> bool {
        !self.edits.is_empty()
    }

    pub fn get(&self, key: &str) -> Result<SettingValueView> {
        let definition = SettingsCatalog::definition(key)
            .ok_or_else(|| anyhow!("unknown setting key {key:?}"))?;
        let (global, project) = self.staged_layers()?;
        Ok(view_for(
            definition,
            &global,
            &project,
            &self.overrides,
            &self.runtime,
            self.project_trusted,
        ))
    }

    pub fn set(&mut self, key: &str, value: Value) -> Result<()> {
        let definition = self.editable_definition(key)?;
        definition.validate_value(&value)?;
        self.edits.insert(definition.key, DraftEdit::Set(value));
        Ok(())
    }

    pub fn reset(&mut self, key: &str) -> Result<()> {
        let definition = self.editable_definition(key)?;
        self.edits.insert(definition.key, DraftEdit::Reset);
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        let candidate = self.staged_scope_settings()?;
        validate_candidate(&candidate, self.scope, &self.validation_path)
    }

    pub fn apply(self, manager: &SettingsManager) -> Result<Vec<SettingWriteResult>> {
        if self.edits.is_empty() {
            return Ok(Vec::new());
        }
        if self.scope == SettingsScope::Project && !manager.is_project_trusted() {
            bail!("project is not trusted; refusing to write project settings");
        }
        let current = match self.scope {
            SettingsScope::Global => manager.global_settings(),
            SettingsScope::Project => manager.project_settings(),
        };
        if current != self.base_scope_settings {
            bail!("settings changed after this draft was created; reopen the draft before applying");
        }
        let candidate = apply_edits(&current, &self.edits)?;
        validate_candidate(&candidate, self.scope, &self.validation_path)?;
        let edited_keys = self.edits.keys().copied().collect::<Vec<_>>();
        match self.scope {
            SettingsScope::Global => manager.update_global(|settings| *settings = candidate),
            SettingsScope::Project => manager.update_project(|settings| *settings = candidate),
        }?;
        edited_keys
            .into_iter()
            .map(|key| {
                let view = SettingsCatalog::get(manager, key)?;
                Ok(SettingWriteResult {
                    key: key.to_owned(),
                    scope: self.scope,
                    effective_value: view.effective_value,
                    source: view.source,
                    behavior: view.definition.behavior,
                    needs_reload: view.definition.behavior == SettingApplyBehavior::Reload,
                    needs_restart: view.definition.behavior == SettingApplyBehavior::Restart,
                })
            })
            .collect()
    }

    pub fn cancel(self) {}

    fn editable_definition(&self, key: &str) -> Result<&'static SettingDef> {
        let definition = SettingsCatalog::definition(key)
            .ok_or_else(|| anyhow!("unknown setting key {key:?}"))?;
        if definition.secret {
            bail!(
                "{} is secret material and cannot be read or written through settings.json",
                definition.key
            );
        }
        if !definition.scopes.allows(self.scope) {
            bail!("{} cannot be written in {:?} scope", definition.key, self.scope);
        }
        Ok(definition)
    }

    fn staged_scope_settings(&self) -> Result<Settings> {
        match self.scope {
            SettingsScope::Global => apply_edits(&self.global, &self.edits),
            SettingsScope::Project => apply_edits(&self.project, &self.edits),
        }
    }

    fn staged_layers(&self) -> Result<(Settings, Settings)> {
        match self.scope {
            SettingsScope::Global => Ok((self.staged_scope_settings()?, self.project.clone())),
            SettingsScope::Project => Ok((self.global.clone(), self.staged_scope_settings()?)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingWriteResult {
    pub key: String,
    pub scope: SettingsScope,
    pub effective_value: Value,
    pub source: SettingSource,
    pub behavior: SettingApplyBehavior,
    pub needs_reload: bool,
    pub needs_restart: bool,
}

fn validate_candidate(settings: &Settings, scope: SettingsScope, path: &Path) -> Result<()> {
    validate_settings(settings, scope, path)
}

fn view_for(
    definition: &'static SettingDef,
    global: &Settings,
    project: &Settings,
    overrides: &Settings,
    runtime: &BTreeMap<&'static str, Value>,
    project_trusted: bool,
) -> SettingValueView {
    let global_value = value_at_settings(global, definition.key);
    let project_value = project_trusted
        .then(|| value_at_settings(project, definition.key))
        .flatten();
    let override_value = value_at_settings(overrides, definition.key);
    let runtime_value = runtime.get(definition.key).cloned();
    let (source, effective_value) = if let Some(value) = runtime_value.clone() {
        (SettingSource::Runtime, value)
    } else if let Some(value) = override_value.clone() {
        (SettingSource::SessionOverride, value)
    } else if let Some(value) = project_value.clone() {
        (SettingSource::Project, value)
    } else if let Some(value) = global_value.clone() {
        (SettingSource::Global, value)
    } else {
        (SettingSource::Default, definition.default_value())
    };
    let redacted = definition.secret
        && (global_value.is_some() || project_value.is_some() || override_value.is_some());
    let redact = |value: Option<Value>| value.map(|_| Value::String(REDACTED_VALUE.to_owned()));
    SettingValueView {
        definition,
        effective_value: if redacted {
            Value::String(REDACTED_VALUE.to_owned())
        } else {
            effective_value
        },
        source,
        global_value: if definition.secret {
            redact(global_value)
        } else {
            global_value
        },
        project_value: if definition.secret {
            redact(project_value)
        } else {
            project_value
        },
        session_override_value: if definition.secret {
            redact(override_value)
        } else {
            override_value
        },
        editable_global: !definition.secret && definition.scopes.allows(SettingsScope::Global),
        editable_project: !definition.secret
            && project_trusted
            && definition.scopes.allows(SettingsScope::Project),
        redacted,
    }
}

fn value_at_settings(settings: &Settings, key: &str) -> Option<Value> {
    let value = serde_json::to_value(settings).ok()?;
    key.split('.')
        .try_fold(&value, |current, segment| current.as_object()?.get(segment))
        .cloned()
}

fn apply_edits(settings: &Settings, edits: &BTreeMap<&'static str, DraftEdit>) -> Result<Settings> {
    let mut value = serde_json::to_value(settings).context("serializing settings draft")?;
    for (key, edit) in edits {
        match edit {
            DraftEdit::Set(new_value) => set_value_at(&mut value, key, new_value.clone())?,
            DraftEdit::Reset => remove_value_at(&mut value, key)?,
        }
    }
    serde_json::from_value(value).context("deserializing settings draft")
}

fn set_value_at(root: &mut Value, key: &str, value: Value) -> Result<()> {
    let segments = key.split('.').collect::<Vec<_>>();
    let Some((last, parents)) = segments.split_last() else {
        bail!("setting key must not be empty");
    };
    let mut current = root;
    for segment in parents {
        let object = current.as_object_mut().ok_or_else(|| anyhow!("setting path {segment} is not an object"))?;
        current = object.entry((*segment).to_owned()).or_insert_with(|| Value::Object(Map::new()));
    }
    current
        .as_object_mut()
        .ok_or_else(|| anyhow!("setting parent for {key} is not an object"))?
        .insert((*last).to_owned(), value);
    Ok(())
}

fn remove_value_at(root: &mut Value, key: &str) -> Result<()> {
    let segments = key.split('.').collect::<Vec<_>>();
    if segments.is_empty() {
        bail!("setting key must not be empty");
    }
    remove_path(root, &segments);
    Ok(())
}

fn remove_path(current: &mut Value, segments: &[&str]) -> bool {
    let Some(object) = current.as_object_mut() else {
        return false;
    };
    if segments.len() == 1 {
        object.remove(segments[0]);
    } else if let Some(child) = object.get_mut(segments[0])
        && remove_path(child, &segments[1..])
    {
        object.remove(segments[0]);
    }
    object.is_empty()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;

    use serde_json::json;

    use super::*;

    fn manager() -> (tempfile::TempDir, tempfile::TempDir, SettingsManager) {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("settings manager");
        (agent, cwd, manager)
    }

    #[test]
    fn catalog_covers_all_typed_setting_paths() {
        let expected = BTreeSet::from([
            "defaultProvider", "defaultModel", "defaultThinkingLevel", "defaultProjectTrust", "approvalMode", "sessionDir", "sessionImportSources", "theme",
            "compaction.enabled", "compaction.reserveTokens", "compaction.keepRecentTokens",
            "terminal.showImages", "terminal.imageWidthCells", "terminal.clearOnShrink", "terminal.showTerminalProgress",
            "images.autoResize", "images.blockImages", "retry.enabled", "retry.maxRetries", "retry.baseDelayMs",
            "retry.provider.timeoutMs", "retry.provider.maxRetries", "retry.provider.maxRetryDelayMs",
            "retry.modelFallback", "retry.fallbackChains",
            "branchSummary.reserveTokens", "branchSummary.skipPrompt", "steeringMode", "followUpMode", "autoRetry",
            "maxRetries", "baseDelayMs", "transport", "timeoutMs", "maxRetryDelayMs", "temperature", "maxTokens",
            "cacheRetention", "thinkingBudgets.minimal", "thinkingBudgets.low", "thinkingBudgets.medium",
            "thinkingBudgets.high", "showImages", "imageWidthCells", "autoResizeImages", "httpIdleTimeoutMs",
            "websocketConnectTimeoutMs", "scopedModels", "keybindings", "quietStartup", "hideThinkingBlock",
            "showThinking", "exposeSessionEnvironment", "doubleEscapeAction", "orchestration.process",
            "orchestration.tasks", "orchestration.todo", "orchestration.maxConcurrency",
            "orchestration.maxRecursionDepth", "orchestration.mailboxCapacity", "orchestration.maxToolsPerAgent",
            "selector.enabled", "selector.maxResults", "selector.minScore", "selector.autoloadThreshold",
            "selector.autoSelectThreshold", "selector.confidenceMargin", "selector.classifier.enabled",
            "selector.classifier.model", "selector.classifier.maxTokens", "selector.classifier.timeoutMs", "agents",
            "packages", "extensions", "skills", "prompts", "themes", "enabledModels", "enableSkillCommands",
        ]);
        let actual = SETTINGS_CATALOG.iter().filter(|definition| !definition.secret).map(|definition| definition.key).collect::<BTreeSet<_>>();
        assert_eq!(actual, expected);
        assert_eq!(actual.len(), SETTINGS_CATALOG.len() - 1);
        for definition in SETTINGS_CATALOG {
            assert!(!definition.description.is_empty());
            let _ = definition.default_value();
            if matches!(definition.value_type, SettingValueType::Enum) {
                assert!(!definition.enum_values.is_empty(), "{}", definition.key);
            }
        }
    }

    #[test]
    fn provenance_prefers_session_then_project_then_global_then_default() {
        let (_agent, cwd, manager) = manager();
        manager.update_global(|settings| settings.compaction = Some(crate::CompactionConfig { reserve_tokens: Some(10), ..Default::default() })).expect("global");
        fs::create_dir_all(cwd.path().join(".pi")).expect("project dir");
        fs::write(cwd.path().join(".pi/settings.json"), r#"{"compaction":{"reserveTokens":20}}"#).expect("project settings");
        manager.load_project(true).expect("trusted project");
        let mut overrides = Settings::default();
        overrides.compaction = Some(crate::CompactionConfig { reserve_tokens: Some(30), ..Default::default() });
        manager.apply_overrides(overrides);
        let view = manager.setting("compaction.reserveTokens").expect("setting");
        assert_eq!(view.effective_value, json!(30));
        assert_eq!(view.source, SettingSource::SessionOverride);
        manager.clear_overrides();
        assert_eq!(manager.setting("compaction.reserveTokens").expect("project").source, SettingSource::Project);
        manager.load_project(false).expect("drop project");
        assert_eq!(manager.setting("compaction.reserveTokens").expect("global").source, SettingSource::Global);
        let pristine_agent = tempfile::tempdir().expect("pristine agent");
        let pristine = SettingsManager::load_phase_one(cwd.path(), pristine_agent.path()).expect("pristine");
        let default = pristine.setting("compaction.reserveTokens").expect("default");
        assert_eq!(default.source, SettingSource::Default);
        assert_eq!(default.effective_value, json!(16384));
    }

    #[test]
    fn approval_mode_is_global_only_restart_with_yolo_default() {
        let (_agent, _cwd, manager) = manager();
        let definition = SettingsCatalog::definition("approvalMode").expect("approval mode");
        assert_eq!(definition.scopes, SettingScopeSupport::GlobalOnly);
        assert_eq!(definition.behavior, SettingApplyBehavior::Restart);
        assert_eq!(definition.default_value(), json!("yolo"));

        let view = manager.setting("approvalMode").expect("approval mode view");
        assert_eq!(view.effective_value, json!("yolo"));
        assert_eq!(view.source, SettingSource::Default);
        assert!(view.editable_global);
        assert!(!view.editable_project);

        let paths = manager.paths();
        let cwd = paths.project.parent().expect("project config").parent().expect("cwd");
        fs::create_dir_all(cwd.join(".pi")).expect("project dir");
        manager.load_project(true).expect("trust empty project");
        let mut project = manager.settings_draft(SettingsScope::Project).expect("project draft");
        let error = project.set("approvalMode", json!("yolo")).expect_err("global only");
        assert!(error.to_string().contains("cannot be written"), "{error:#}");

        let mut global = manager.settings_draft(SettingsScope::Global).expect("global draft");
        global.set("approvalMode", json!("ask")).expect("set approval mode");
        let writes = global.apply(&manager).expect("apply");
        assert_eq!(writes.len(), 1);
        assert!(writes[0].needs_restart);
        assert!(!writes[0].needs_reload);
    }

    #[test]
    fn draft_rejects_wrong_types_ranges_and_enums() {
        let (_agent, _cwd, manager) = manager();
        let mut draft = manager.settings_draft(SettingsScope::Global).expect("draft");
        assert!(draft.set("compaction.enabled", json!("yes")).is_err());
        assert!(draft.set("temperature", json!(2.1)).is_err());
        assert!(draft.set("transport", json!("udp")).is_err());
        assert!(!draft.is_dirty());
    }

    #[test]
    fn session_import_sources_catalog_exposes_allowed_values_and_full_validation() {
        let definition = SettingsCatalog::definition("sessionImportSources")
            .expect("session import sources definition");
        assert_eq!(definition.category, SettingCategory::Session);
        assert_eq!(definition.value_type, STRING_LIST);
        assert_eq!(definition.default_value(), json!([]));
        assert_eq!(definition.enum_values, SESSION_IMPORT_SOURCE_VALUES);
        assert_eq!(definition.behavior, SettingApplyBehavior::Reload);

        let (_agent, _cwd, manager) = manager();
        let mut allowed = manager.settings_draft(SettingsScope::Global).expect("allowed draft");
        allowed
            .set("sessionImportSources", json!(["codex", "claude"]))
            .expect("stage allowed sources");
        allowed.validate().expect("allowed sources validate");

        for value in [json!(["pi"]), json!(["codex", "codex"])] {
            let mut invalid = manager.settings_draft(SettingsScope::Global)
                .expect("invalid draft");
            invalid
                .set("sessionImportSources", value)
                .expect("stage structurally valid list");
            assert!(invalid.validate().is_err());
        }
    }

    #[test]
    fn global_and_trusted_project_drafts_write_and_reset() {
        let (_agent, cwd, manager) = manager();
        let mut global = manager.settings_draft(SettingsScope::Global).expect("global draft");
        global.set("theme", json!("dark")).expect("theme");
        global.set("compaction.enabled", json!(true)).expect("compaction");
        assert_eq!(global.apply(&manager).expect("global apply").len(), 2);
        manager.load_project(true).expect("trust project");
        let mut project = manager.settings_draft(SettingsScope::Project).expect("project draft");
        project.set("theme", json!("light")).expect("project theme");
        project.apply(&manager).expect("project apply");
        assert_eq!(manager.setting("theme").expect("theme").source, SettingSource::Project);
        let mut reset = manager.settings_draft(SettingsScope::Project).expect("reset draft");
        reset.reset("theme").expect("reset theme");
        reset.apply(&manager).expect("reset apply");
        let view = manager.setting("theme").expect("fallback theme");
        assert_eq!(view.source, SettingSource::Global);
        assert_eq!(view.effective_value, json!("dark"));
        assert!(cwd.path().join(".pi/settings.json").exists());
    }

    #[test]
    fn untrusted_project_draft_is_rejected() {
        let (_agent, _cwd, manager) = manager();
        let error = manager.settings_draft(SettingsScope::Project).expect_err("untrusted draft").to_string();
        assert!(error.contains("not trusted"), "{error}");
    }

    #[test]
    fn cancel_writes_nothing() {
        let (_agent, _cwd, manager) = manager();
        let path = manager.paths().global;
        let mut draft = manager.settings_draft(SettingsScope::Global).expect("draft");
        draft.set("theme", json!("dark")).expect("theme");
        draft.cancel();
        assert!(!path.exists());
    }

    #[test]
    fn setting_write_preserves_unknown_fields() {
        let (agent, cwd, _) = manager();
        fs::write(agent.path().join("settings.json"), r#"{"theme":"old","future":{"nested":1}}"#).expect("settings");
        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("reload");
        let mut draft = manager.settings_draft(SettingsScope::Global).expect("draft");

        draft.set("theme", json!("new")).expect("theme");
        draft.apply(&manager).expect("apply");
        let saved: Value = serde_json::from_slice(&fs::read(agent.path().join("settings.json")).expect("read settings")).expect("json");
        assert_eq!(saved["theme"], "new");
        assert_eq!(saved["future"]["nested"], 1);
    }

    #[test]
    fn stale_draft_refuses_to_overwrite_newer_persistence() {
        let (_agent, _cwd, manager) = manager();
        let mut draft = manager.settings_draft(SettingsScope::Global).expect("draft");
        draft.set("theme", json!("stale")).expect("draft theme");
        manager
            .update_global(|settings| settings.quiet_startup = Some(true))
            .expect("concurrent write");

        let error = draft.apply(&manager).expect_err("stale draft").to_string();
        assert!(error.contains("settings changed"), "{error}");
        let settings = manager.settings();
        assert_eq!(settings.theme, None);
        assert_eq!(settings.quiet_startup, Some(true));
    }

    #[test]
    fn full_validation_failure_does_not_partially_write() {
        let (_agent, _cwd, manager) = manager();
        manager.update_global(|settings| settings.theme = Some("before".to_owned())).expect("seed");
        let path = manager.paths().global;
        let before = fs::read(&path).expect("before");
        let mut draft = manager.settings_draft(SettingsScope::Global).expect("draft");
        draft.set("theme", json!("after")).expect("theme");
        draft.set("agents", json!({"   ":{"enabled":false}})).expect("stage invalid agent");
        assert!(draft.validate().is_err());
        assert!(draft.apply(&manager).is_err());
        assert_eq!(fs::read(&path).expect("after"), before);
        assert_eq!(manager.settings().theme.as_deref(), Some("before"));
    }

    #[test]
    fn secret_values_are_redacted_and_never_editable() {
        let (agent, cwd, _) = manager();
        let secret = "do-not-leak-this-token";
        fs::write(agent.path().join("settings.json"), format!(r#"{{"apiKey":"{secret}"}}"#)).expect("settings");
        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("reload");
        let view = manager.setting("apiKey").expect("secret view");
        assert!(view.redacted);
        assert_eq!(view.effective_value, REDACTED_VALUE);
        let encoded = serde_json::to_string(&view).expect("serialize view");
        assert!(!encoded.contains(secret));
        assert!(encoded.contains(REDACTED_VALUE));
        let mut draft = manager.settings_draft(SettingsScope::Global).expect("draft");
        let error = draft.set("apiKey", json!("replacement")).expect_err("secret write").to_string();
        assert!(!error.contains("replacement"));
        assert!(error.contains("secret material"));
    }
}
