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

/// Nested field paths inside a setting row whose string values are secret and
/// must be redacted in every settings view (RPC/TUI), even though the
/// containing row stays editable. `[]` marks an array element. Covers MCP
/// server environment maps (`mcpServers[].env`), which routinely carry
/// credentials (API tokens, auth headers) — matching how the env values are
/// never echoed by the MCP tool itself.
const NESTED_SECRET_PATHS: &[&str] = &["mcpServers[].env"];

/// Redact the values reachable through [`NESTED_SECRET_PATHS`] for `definition`
/// inside `value`. Other content of the row (names, commands, args, urls) is
/// left intact so the row remains usable and editable.
/// Split a nested path into walkable segments, lifting a trailing `[]` array
/// marker off a segment into its own step (`mcpServers[].env` →
/// `["mcpServers", "[]", "env"]`).
fn normalize_nested_segments(path: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    for segment in path.split('.') {
        if let Some(key) = segment.strip_suffix("[]") {
            segments.push(key);
            segments.push("[]");
        } else {
            segments.push(segment);
        }
    }
    segments
}

fn redact_nested_secret_values(definition: &SettingDef, mut value: Value) -> Value {
    for path in NESTED_SECRET_PATHS {
        let segments = normalize_nested_segments(path);
        let Some((&row_key, relative)) = segments.split_first() else {
            continue;
        };
        if definition.key != row_key {
            continue;
        }
        // The remaining segments are relative to the row's value (the
        // mcpServers array), with `[]` iterating array elements.
        redact_segments(&mut value, relative);
    }
    value
}

/// Walk `segments` (`[]` iterates array elements) and replace every string
/// value found at the final segment with the redaction marker.
fn redact_segments(value: &mut Value, segments: &[&str]) {
    let Some((head, rest)) = segments.split_first() else {
        return;
    };
    if *head == "[]" {
        if let Value::Array(items) = value {
            for item in items {
                redact_segments(item, rest);
            }
        }
        return;
    }
    let Value::Object(map) = value else {
        return;
    };
    let Some(next) = map.get_mut(*head) else {
        return;
    };
    if rest.is_empty() {
        redact_string_leaves(next);
        return;
    }
    redact_segments(next, rest);
}

/// Replace every string leaf of `value` with the redaction marker (keeps
/// structure and keys, redacts values — the env-map shape).
fn redact_string_leaves(value: &mut Value) {
    match value {
        Value::String(text) => *text = REDACTED_VALUE.to_owned(),
        Value::Array(items) => {
            for item in items {
                redact_string_leaves(item);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                redact_string_leaves(item);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

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
    Live,
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
    setting!("authScope", Models, NON_EMPTY_STRING, "null", "Active credential scope used to select stored auth.json credentials; PI_AUTH_SCOPE overrides. Credentials logged in with `rpi login --scope <label>` are selected when the label matches.", NONE, ALL, LIVE, false, false),
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
    setting!("responsesStatefulChain", Models, BOOL, "false", "Opt-in stateful turn chaining for OpenAI Responses API models: carry previous_response_id across turns per session instead of resending full history.", NONE, ALL, LIVE, false, false),
    setting!("visionModel", Models, STRING, "null", "Model spec (provider/id or bare id) used to describe images when the active chat model does not support image input. Prompts containing image blocks are delegated to this model and replaced with its text description before reaching the main model.", NONE, ALL, LIVE, false, false),

    setting!("steeringMode", Session, SettingValueType::Enum, "\"one-at-a-time\"", "Queue behavior for steering messages.", &["all", "one-at-a-time"], ALL, LIVE, false, false),
    setting!("followUpMode", Session, SettingValueType::Enum, "\"one-at-a-time\"", "Queue behavior for follow-up messages.", &["all", "one-at-a-time"], ALL, LIVE, false, false),
    setting!("branchSummary.reserveTokens", Session, POSITIVE_I64, "16384", "Tokens reserved while generating branch summaries.", NONE, ALL, LIVE, false, false),
    setting!("branchSummary.skipPrompt", Session, BOOL, "false", "Skip the branch-summary prompt when possible.", NONE, ALL, LIVE, false, false),
    setting!("exposeSessionEnvironment", Session, BOOL, "true", "Expose the session environment to tools.", NONE, ALL, LIVE, false, true),
    setting!("sessionDir", Session, NON_EMPTY_STRING, "null", "Directory used for session storage and lookup.", NONE, ALL, RESTART, false, false),
    setting!("sessionImportSources", Session, STRING_LIST, "[]", "Foreign session sources eligible for automatic discovery and resume; native Pi sessions are always included.", SESSION_IMPORT_SOURCE_VALUES, ALL, RELOAD, false, false),
    setting!("sessionTtlDays", Session, POSITIVE_U64, "30", "Days after which an untouched native session file is pruned at startup.", NONE, ALL, RESTART, false, false),

    setting!("compaction.enabled", Compaction, BOOL, "true", "Enable automatic context compaction.", NONE, ALL, LIVE, false, false),
    setting!("compaction.reserveTokens", Compaction, POSITIVE_I64, "16384", "Tokens reserved for the next response during compaction.", NONE, ALL, LIVE, false, false),
    setting!("compaction.keepRecentTokens", Compaction, NON_NEGATIVE_I64, "20000", "Recent tokens retained verbatim during compaction.", NONE, ALL, LIVE, false, false),
    setting!("compaction.snapKeepTurns", Compaction, POSITIVE_I64, "10", "Recent user turns retained verbatim by /compact --snap (older turns are archived deterministically).", NONE, ALL, LIVE, false, false),

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
    setting!("images.genModel", TerminalUi, STRING, "null", "Model spec (provider/id or bare id) the generate_image tool uses instead of the active chat model. The resolved model must declare imageGeneration: true.", NONE, ALL, LIVE, false, false),
    setting!("images.genBaseUrl", TerminalUi, STRING, "\"\"", "Base URL of the user-configured OpenAI-compatible image endpoint (POST {base}/images/generations). Overrides the resolved model's baseUrl for the generate_image tool. No provider default is assumed.", NONE, ALL, LIVE, false, false),
    setting!("images.genApiKey", TerminalUi, SettingValueType::Secret, "null", "Bearer API key for images.genBaseUrl (or the resolved model's provider when genBaseUrl is absent). Secret material: redacted in every settings view and never writable through settings.json — edit the settings file directly (or use an environment reference). Never logged.", NONE, SettingScopeSupport::None, RESTART, true, false),
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
    setting!("orchestration.todo", Orchestration, BOOL, "true", "Expose the todo tool to the model (create/modify/clear todos in natural language; OMP parity).", NONE, ALL, RELOAD, false, true),
    setting!("orchestration.maxConcurrency", Orchestration, SettingValueType::UnsignedInteger { min: 1, max: 64 }, "4", "Maximum concurrent orchestration agents.", NONE, ALL, RELOAD, false, true),
    setting!("orchestration.maxRecursionDepth", Orchestration, SettingValueType::UnsignedInteger { min: 0, max: 16 }, "2", "Maximum orchestration recursion depth.", NONE, ALL, RELOAD, false, true),
    setting!("orchestration.mailboxCapacity", Orchestration, SettingValueType::UnsignedInteger { min: 1, max: 10000 }, "100", "Per-agent orchestration mailbox capacity.", NONE, ALL, RELOAD, false, true),
    setting!("orchestration.maxToolsPerAgent", Orchestration, SettingValueType::UnsignedInteger { min: 1, max: 64 }, "16", "Maximum tools exposed to each orchestration agent.", NONE, ALL, RELOAD, false, true),
    setting!("orchestration.softBudget.maxRequests", Orchestration, POSITIVE_U64, "null", "Maximum LLM turns a child orchestration job may use before yielding.", NONE, ALL, RELOAD, false, true),
    setting!("orchestration.softBudget.maxTokens", Orchestration, POSITIVE_U64, "null", "Maximum cumulative tokens a child orchestration job may consume before yielding.", NONE, ALL, RELOAD, false, true),
    setting!("orchestration.softBudget.yieldAfter", Orchestration, POSITIVE_U64, "null", "Yield to the parent after this many child requests regardless of remaining budget.", NONE, ALL, RELOAD, false, true),
    setting!("orchestration.sandboxed", Orchestration, BOOL, "false", "Run orchestration subagent children inside the Linux filesystem sandbox (opt-in; default off): every process a child spawns (its bash tool) is confined to the workspace, the agent directory, and sandbox.allowedPaths, with the same deny-by-default filesystem, fail-closed validation, and loopback-only network (unless sandbox.network is true) as the bash sandbox.", NONE, ALL, RELOAD, false, true),
    setting!("orchestration.isolation", Orchestration, SettingValueType::Enum, "\"worktree\"", "Workflow isolation backend: worktree (default) isolates each workflow in a git worktree; overlayfs uses an overlay (source repo as the read-only lower layer, private writable upper layer; kernel overlay with fuse-overlayfs and recursive-copy fallbacks) and integrates by copying the overlay tree back into the source repo; none disables isolation so workflows operate directly on the source working tree.", &["worktree", "overlayfs", "none"], ALL, RELOAD, false, true),
    setting!("orchestration.preferredAgent", Orchestration, STRING, "null", "Preferred agent or persona name for unnamed task spawns. Set by /role <name> --select or /persona <name> --select; cleared by --clear. A missing or disabled selection falls back to ranked/default agent selection at spawn.", NONE, ALL, RELOAD, false, true),
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
    setting!("selector.autoMode", Orchestration, SettingValueType::Enum, "\"suggest\"", "Automatic interaction-mode classification for user prompts: off disables it, suggest shows a status hint after a detected code task or long-running goal, auto additionally creates and starts a todo DAG for detected code tasks (only when orchestration is enabled and no todo list exists).", &["off", "suggest", "auto"], ALL, RELOAD, false, true),
    setting!("agents", Orchestration, OBJECT, "{}", "Per-agent enablement and model overrides.", NONE, ALL, RELOAD, false, true),

    setting!("packages", Resources, ARRAY, "[]", "Package sources supplying extensions and resources.", NONE, ALL, RELOAD, false, true),
    setting!("extensions", Resources, STRING_LIST, "[]", "Extension paths or identifiers to load.", NONE, ALL, RELOAD, false, true),
    setting!("skills", Resources, STRING_LIST, "[]", "Skill paths or identifiers to load.", NONE, ALL, RELOAD, false, true),
    setting!("prompts", Resources, STRING_LIST, "[]", "Prompt-template paths or identifiers to load.", NONE, ALL, RELOAD, false, true),
    setting!("themes", Resources, STRING_LIST, "[]", "Theme paths or identifiers to load.", NONE, ALL, RELOAD, false, true),
    setting!("enableSkillCommands", Resources, BOOL, "true", "Expose loaded skills as interactive commands.", NONE, ALL, RELOAD, false, true),

    setting!("hooks", TrustSecurity, ARRAY, "[]", "Host-level command hooks fired at session, turn, tool-call, and trust-decision events. Each entry: { event: pre_tool_call|pre_trust_decision|post_tool_call|session_start|session_end|turn_start|turn_end, matcher?, command: string[], timeoutMs?, enabled?, failClosed? }. pre_tool_call blocks a tool with {\"decision\":\"block\"}; pre_trust_decision receives the canonical project path, tentative decision, and isNew and may deny the trust decision the same way. Other events are advisory.", NONE, ALL, RELOAD, false, true),
    setting!("permissionRules", TrustSecurity, ARRAY, "[]", "Path-level permission rules evaluated before the capability-wide approval mode for read/write/edit/glob/grep. Each entry: { action: allow|deny|ask, path: string (path or prefix; a trailing '*' is accepted as an explicit prefix marker, relative paths resolve from the session working directory), tools?: [read|write|edit|glob|grep] }. deny blocks with an actionable message, ask forces interactive confirmation, allow bypasses the capability ask. The longest matching path wins; among equally specific matches deny > ask > allow. bash is not covered by path rules.", NONE, ALL, RELOAD, false, true),
    setting!("sandbox.enabled", TrustSecurity, BOOL, "false", "Run the bash tool inside an opt-in Linux filesystem sandbox (unshare mount/pid/net namespaces): only sandbox.allowedPaths are visible, everything else is denied, and network is loopback-only unless sandbox.network is true. Same-user confinement, not isolation; the per-call bash sandboxed parameter overrides this for one command.", NONE, ALL, RELOAD, false, true),
    setting!("sandbox.network", TrustSecurity, BOOL, "false", "Share the host network inside the bash sandbox (default: fresh network namespace with loopback only).", NONE, ALL, RELOAD, false, true),
    setting!("sandbox.readOnly", TrustSecurity, BOOL, "false", "Mount the sandbox allowed paths read-only (bind mounts with MS_RDONLY). Default: allowed paths are writable. The sandbox's private HOME/TMPDIR stay writable so commands can still cache and create temp files.", NONE, ALL, RELOAD, false, true),
    setting!("sandbox.allowedPaths", TrustSecurity, STRING_LIST, "[]", "Paths visible inside the bash sandbox (bind-mounted read-write). Defaults to the session working directory plus the agent directory. Relative paths resolve from the session working directory.", NONE, ALL, RELOAD, false, true),
    setting!("sandbox.deniedPaths", TrustSecurity, STRING_LIST, "[]", "Paths hidden inside the bash sandbox (empty overlay), even when nested under an allowed path. Relative paths resolve from the session working directory.", NONE, ALL, RELOAD, false, true),
    setting!("mcpServers", Resources, ARRAY, "[]", "MCP (Model Context Protocol) servers exposed to the model through the mcp tool. Each entry: { name, disabled?, transport: stdio|sse, command?, args?, url?, env? }. stdio servers run as child processes (command + args, extra env from env); sse servers connect to an http(s) url. sse entries parse and round-trip but the client transport in this build is stdio. Entries with disabled: true are never spawned and are omitted from the mcp tool's server list; import Claude Desktop / Cursor configs with `rpi mcp import`.", NONE, ALL, RELOAD, false, true),

    setting!("memory.backend", Resources, SettingValueType::Enum, "\"local\"", "Memory backend: off hides every memory tool, local uses the built-in JSONL store, hindsight uses the configured Hindsight HTTP API and exposes recall/retain/reflect.", &["off", "local", "hindsight"], ALL, RELOAD, false, true),
    setting!("memory.hindsightApiUrl", Resources, NON_EMPTY_STRING, "null", "Explicit Hindsight HTTP API base URL. Required when memory.backend is hindsight. HTTPS is required unless hindsightAllowInsecure is true.", NONE, ALL, RELOAD, false, true),
    setting!("memory.hindsightApiToken", Resources, SettingValueType::Secret, "null", "Optional Hindsight bearer token. Secret material: always redacted and not writable through catalog drafts.", NONE, SettingScopeSupport::None, RESTART, true, false),
    setting!("memory.hindsightAllowInsecure", Resources, BOOL, "false", "Explicitly allow plaintext HTTP for a trusted self-hosted Hindsight API. Off by default.", NONE, ALL, RELOAD, false, true),
    setting!("memory.hindsightInjection", Resources, BOOL, "false", "When true, each turn injects bounded, redacted recall output for the latest user ask as hidden context.", NONE, ALL, RELOAD, false, true),
    setting!("memory.hindsightBankId", Resources, NON_EMPTY_STRING, "\"rpi\"", "Base Hindsight memory bank id.", NONE, ALL, RELOAD, false, true),
    setting!("memory.hindsightBankIdPrefix", Resources, NON_EMPTY_STRING, "null", "Optional prefix prepended to the Hindsight bank id.", NONE, ALL, RELOAD, false, true),
    setting!("memory.hindsightScoping", Resources, SettingValueType::Enum, "\"per-project-tagged\"", "Hindsight namespace policy: global shares one bank, per-project appends the project label to the bank, per-project-tagged uses project tags while retaining untagged global memories.", &["global", "per-project", "per-project-tagged"], ALL, RELOAD, false, true),
    setting!("memory.hindsightBankMission", Resources, STRING, "null", "Optional reflect mission applied when ensuring the bank exists.", NONE, ALL, RELOAD, false, true),
    setting!("memory.hindsightRetainMission", Resources, STRING, "null", "Optional retain mission applied when ensuring the bank exists.", NONE, ALL, RELOAD, false, true),
    setting!("memory.hindsightRecallBudget", Resources, SettingValueType::Enum, "\"mid\"", "Hindsight recall and reflect reasoning budget.", &["low", "mid", "high"], ALL, RELOAD, false, true),
    setting!("memory.hindsightRecallMaxTokens", Resources, SettingValueType::UnsignedInteger { min: 1, max: 65536 }, "1024", "Maximum tokens requested from Hindsight recall.", NONE, ALL, RELOAD, false, true),
    setting!("memory.hindsightRecallTypes", Resources, STRING_LIST, "[\"world\",\"experience\"]", "Hindsight memory types included in recall.", NONE, ALL, RELOAD, false, true),
    setting!("memory.hindsightRequestTimeoutMs", Resources, SettingValueType::UnsignedInteger { min: 1, max: 600000 }, "30000", "Default Hindsight HTTP request timeout in milliseconds.", NONE, ALL, RELOAD, false, true),
    setting!("memory.hindsightRecallTimeoutMs", Resources, SettingValueType::UnsignedInteger { min: 1, max: 600000 }, "30000", "Hindsight recall timeout in milliseconds.", NONE, ALL, RELOAD, false, true),
    setting!("memory.hindsightRetainTimeoutMs", Resources, SettingValueType::UnsignedInteger { min: 1, max: 600000 }, "60000", "Hindsight retain timeout in milliseconds.", NONE, ALL, RELOAD, false, true),
    setting!("memory.hindsightReflectTimeoutMs", Resources, SettingValueType::UnsignedInteger { min: 1, max: 600000 }, "120000", "Hindsight reflect timeout in milliseconds.", NONE, ALL, RELOAD, false, true),

    setting!("live.enabled", Live, BOOL, "false", "Enable /live hold-to-talk voice. Off by default; /live refuses to arm until true.", NONE, ALL, LIVE, false, false),
    setting!("live.sttBaseUrl", Live, STRING, "\"\"", "Base URL of the user-configured OpenAI-compatible speech-to-text endpoint (POST {base}/v1/audio/transcriptions, e.g. a self-hosted whisper server). Must be https:// unless live.allowInsecure is true; ws:// and wss:// URLs are always rejected. No provider default is assumed.", NONE, ALL, LIVE, false, false),
    setting!("live.sttApiKey", Live, SettingValueType::Secret, "null", "Bearer API key for live.sttBaseUrl. Secret material: redacted in every settings view and never writable through settings.json — edit the settings file directly (or use an environment reference like $STT_API_KEY). Never logged.", NONE, SettingScopeSupport::None, RESTART, true, false),
    setting!("live.sttModel", Live, STRING, "\"whisper-1\"", "Model label sent as the multipart model field. 'whisper-1' is only a fallback label; the configured base URL serves or rejects it — this product never contacts OpenAI.", NONE, ALL, LIVE, false, false),
    setting!("live.language", Live, STRING, "null", "Optional BCP-47 language hint sent as the multipart language field.", NONE, ALL, LIVE, false, false),
    setting!("live.allowInsecure", Live, BOOL, "false", "Explicitly allow http:// STT endpoints (e.g. a loopback self-hosted whisper server). Default false: plaintext bearer credentials are rejected with an actionable error.", NONE, ALL, LIVE, false, false),
    setting!("live.mode", Live, SettingValueType::Enum, "\"stt\"", "Voice mode: `stt` drives TUI hold-to-talk (mic capture → speech-to-text → composer draft); `realtime` drives WebRTC realtime voice, which the TUI does not implement — `/live`/hold-to-talk report an actionable error directing the user to the Web listener (start it with `rpi --listen 127.0.0.1:8080` and open the /web page in a browser). The runtime trims and lowercases this value; the catalog draft accepts only the canonical lowercase forms.", &["stt", "realtime"], ALL, LIVE, false, false),
    setting!("live.realtimeBaseUrl", Live, STRING, "\"\"", "Base URL of the realtime voice endpoint (CLIProxyAPI's /v1/realtime/calls, e.g. http://localhost:8317) used by the Web listener when live.mode is `realtime`. The TUI never contacts this URL; it is read live by the Web listener with each realtime call.", NONE, ALL, LIVE, false, false),
    setting!("live.realtimeApiKey", Live, SettingValueType::Secret, "null", "Access key for live.realtimeBaseUrl (CLIProxyAPI realtime endpoint). Secret material: redacted in every settings view and never writable through settings.json — edit the settings file directly (or use an environment reference like $REALTIME_API_KEY). Never logged.", NONE, SettingScopeSupport::None, RESTART, true, false),
    setting!("live.realtimeModel", Live, STRING, "\"gpt-realtime-1.5\"", "Realtime model label sent in the realtime session payload. Used by the Web listener when live.mode is `realtime`; the runtime applies this default when the field is absent.", NONE, ALL, LIVE, false, false),
    setting!("live.voice", Live, STRING, "\"sol\"", "Voice for the realtime session (e.g. `sol`). Used by the Web listener when live.mode is `realtime`; the runtime applies this default when the field is absent.", NONE, ALL, LIVE, false, false),

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
    let global_value =
        value_at_settings(global, definition.key).map(|value| redact_nested_secret_values(definition, value));
    let project_value = project_trusted
        .then(|| value_at_settings(project, definition.key))
        .flatten()
        .map(|value| redact_nested_secret_values(definition, value));
    let override_value =
        value_at_settings(overrides, definition.key).map(|value| redact_nested_secret_values(definition, value));
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
            "defaultProvider", "defaultModel", "authScope", "defaultThinkingLevel", "defaultProjectTrust", "approvalMode", "sessionDir", "sessionImportSources", "sessionTtlDays", "theme",
            "compaction.enabled", "compaction.reserveTokens", "compaction.keepRecentTokens", "compaction.snapKeepTurns",
            "terminal.showImages", "terminal.imageWidthCells", "terminal.clearOnShrink", "terminal.showTerminalProgress",
            "images.autoResize", "images.blockImages", "images.genModel", "images.genBaseUrl", "retry.enabled", "retry.maxRetries", "retry.baseDelayMs",
            "retry.provider.timeoutMs", "retry.provider.maxRetries", "retry.provider.maxRetryDelayMs",
            "retry.modelFallback", "retry.fallbackChains",
            "branchSummary.reserveTokens", "branchSummary.skipPrompt", "steeringMode", "followUpMode", "autoRetry",
            "maxRetries", "baseDelayMs", "transport", "timeoutMs", "maxRetryDelayMs", "temperature", "maxTokens",
            "cacheRetention", "responsesStatefulChain", "visionModel", "thinkingBudgets.minimal", "thinkingBudgets.low", "thinkingBudgets.medium",
            "thinkingBudgets.high", "showImages", "imageWidthCells", "autoResizeImages", "httpIdleTimeoutMs",
            "websocketConnectTimeoutMs", "scopedModels", "keybindings", "quietStartup", "hideThinkingBlock",
            "showThinking", "exposeSessionEnvironment", "doubleEscapeAction", "orchestration.process",
            "orchestration.tasks", "orchestration.todo", "orchestration.maxConcurrency",
            "orchestration.maxRecursionDepth", "orchestration.mailboxCapacity", "orchestration.maxToolsPerAgent",
            "orchestration.softBudget.maxRequests", "orchestration.softBudget.maxTokens", "orchestration.softBudget.yieldAfter",
            "orchestration.sandboxed", "orchestration.isolation", "orchestration.preferredAgent",
            "selector.enabled", "selector.maxResults", "selector.minScore", "selector.autoloadThreshold",
            "selector.autoSelectThreshold", "selector.confidenceMargin", "selector.classifier.enabled",
            "selector.classifier.model", "selector.classifier.maxTokens", "selector.classifier.timeoutMs",
            "selector.autoMode", "agents",
            "packages", "extensions", "skills", "prompts", "themes", "enabledModels", "enableSkillCommands",
            "hooks", "permissionRules", "mcpServers",
            "sandbox.enabled", "sandbox.network", "sandbox.readOnly", "sandbox.allowedPaths", "sandbox.deniedPaths",
            "memory.backend", "memory.hindsightApiUrl", "memory.hindsightAllowInsecure", "memory.hindsightInjection",
            "memory.hindsightBankId", "memory.hindsightBankIdPrefix", "memory.hindsightScoping",
            "memory.hindsightBankMission", "memory.hindsightRetainMission", "memory.hindsightRecallBudget",
            "memory.hindsightRecallMaxTokens", "memory.hindsightRecallTypes", "memory.hindsightRequestTimeoutMs",
            "memory.hindsightRecallTimeoutMs", "memory.hindsightRetainTimeoutMs", "memory.hindsightReflectTimeoutMs",
            "live.enabled", "live.sttBaseUrl", "live.sttModel", "live.language", "live.allowInsecure", "live.mode", "live.realtimeBaseUrl", "live.realtimeModel", "live.voice",
        ]);
        let actual = SETTINGS_CATALOG.iter().filter(|definition| !definition.secret).map(|definition| definition.key).collect::<BTreeSet<_>>();
        assert_eq!(actual, expected);
        let secret_count = SETTINGS_CATALOG.iter().filter(|definition| definition.secret).count();
        assert_eq!(actual.len(), SETTINGS_CATALOG.len() - secret_count);
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
    fn mcp_server_env_values_are_redacted_in_views_but_preserved_on_write() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        fs::write(
            agent.path().join("settings.json"),
            r#"{"mcpServers":[{"name":"local","command":"npx","env":{"API_KEY":"sk-supersecret","STATIC":"plain"}}]}"#,
        )
        .expect("settings");
        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("manager");

        let view = manager.setting("mcpServers").expect("mcpServers view");
        let encoded = serde_json::to_string(&view.effective_value).expect("serialize");
        assert!(
            !encoded.contains("sk-supersecret"),
            "mcp env value must be redacted in views: {encoded}"
        );
        assert!(encoded.contains("[redacted]"), "env value redacted: {encoded}");
        // Non-secret parts of the row stay visible and editable.
        assert!(encoded.contains("local") && encoded.contains("npx"));
        assert!(view.editable_global, "the mcpServers row must stay editable");

        // The RPC/UI surfaces only the catalog views, so the raw secret never
        // leaves the manager; a persisted write keeps the raw value on disk
        // (the redacted view is never round-tripped into the file).
        let mut draft = manager.settings_draft(SettingsScope::Global).expect("draft");
        draft.set("theme", json!("nord")).expect("theme");
        draft.apply(&manager).expect("apply");
        let saved = fs::read_to_string(agent.path().join("settings.json")).expect("saved");
        assert!(
            saved.contains("sk-supersecret"),
            "raw env value must be preserved on disk:\n{saved}"
        );
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
    fn selector_auto_mode_round_trips_through_draft_and_persists() {
        let (_agent, _cwd, manager) = manager();
        let definition = SettingsCatalog::definition("selector.autoMode").expect("auto mode");
        assert_eq!(definition.category, SettingCategory::Orchestration);
        assert_eq!(definition.behavior, SettingApplyBehavior::Reload);
        assert_eq!(definition.default_value(), json!("suggest"));
        assert_eq!(
            definition.enum_values.iter().copied().collect::<Vec<_>>(),
            vec!["off", "suggest", "auto"]
        );
        assert!(definition.scopes.allows(SettingsScope::Global));
        assert!(definition.scopes.allows(SettingsScope::Project));

        let view = manager.setting("selector.autoMode").expect("default view");
        assert_eq!(view.effective_value, json!("suggest"));
        assert_eq!(view.source, SettingSource::Default);

        let mut draft = manager.settings_draft(SettingsScope::Global).expect("draft");
        draft.set("selector.autoMode", json!("auto")).expect("set auto");
        draft.validate().expect("auto mode validates");
        assert!(draft.get("selector.autoMode").expect("staged view").effective_value == json!("auto"));
        let writes = draft.apply(&manager).expect("apply");
        assert_eq!(writes.len(), 1);
        assert!(writes[0].needs_reload);
        assert!(!writes[0].needs_restart);

        // Persisted round-trip through a fresh manager.
        let paths = manager.paths();
        let fresh = SettingsManager::load_phase_one(
            paths.project.parent().expect("project config").parent().expect("cwd"),
            paths.global.parent().expect("global dir"),
        )
        .expect("fresh manager");
        let persisted = fresh.setting("selector.autoMode").expect("persisted view");
        assert_eq!(persisted.effective_value, json!("auto"));
        assert_eq!(persisted.source, SettingSource::Global);
        assert_eq!(
            fresh.settings().selector.expect("selector settings").auto_mode,
            crate::AutoMode::Auto
        );
    }

    #[test]
    fn selector_auto_mode_rejects_unknown_values() {
        let (_agent, _cwd, manager) = manager();
        let mut draft = manager.settings_draft(SettingsScope::Global).expect("draft");
        assert!(draft.set("selector.autoMode", json!("banana")).is_err());
        assert!(!draft.is_dirty());
        assert!(draft.set("selector.autoMode", json!(7)).is_err());
    }

    #[test]
    fn memory_settings_validate_and_round_trip_through_draft() {
        let (_agent, _cwd, manager) = manager();
        let definition = SettingsCatalog::definition("memory.backend").expect("memory.backend");
        assert_eq!(definition.category, SettingCategory::Resources);
        assert_eq!(definition.enum_values, &["off", "local", "hindsight"]);
        let mut draft = manager.settings_draft(SettingsScope::Global).expect("draft");
        assert!(draft.set("memory.backend", json!("banana")).is_err());
        draft.set("memory.backend", json!("hindsight")).expect("backend");
        draft.set("memory.hindsightApiUrl", json!("https://memory.example.test"))
            .expect("api url");
        draft.set("memory.hindsightInjection", json!(true)).expect("injection");
        draft.set("memory.hindsightBankId", json!("pi-main")).expect("bank");
        draft.set("memory.hindsightScoping", json!("per-project")).expect("scoping");
        draft.set("memory.hindsightRecallMaxTokens", json!(2048)).expect("max tokens");
        assert!(draft.set("memory.hindsightInjection", json!("yes")).is_err());
        assert!(draft.set("memory.hindsightApiUrl", json!("")).is_err());
        assert!(draft.set("memory.hindsightApiToken", json!("secret")).is_err());
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
    fn session_ttl_days_catalog_defaults_to_30_days_and_rejects_zero() {
        let definition = SettingsCatalog::definition("sessionTtlDays")
            .expect("session ttl days definition");
        assert_eq!(definition.category, SettingCategory::Session);
        assert_eq!(definition.value_type, POSITIVE_U64);
        assert_eq!(definition.default_value(), json!(30));
        assert_eq!(definition.behavior, SettingApplyBehavior::Restart);

        let (_agent, _cwd, manager) = manager();
        let mut draft = manager.settings_draft(SettingsScope::Global).expect("draft");
        draft.set("sessionTtlDays", json!(45)).expect("stage 45 days");
        draft.validate().expect("45 days validates");
        let mut zero = manager.settings_draft(SettingsScope::Global).expect("zero draft");
        let error = zero
            .set("sessionTtlDays", json!(0))
            .expect_err("catalog type POSITIVE_U64 must reject zero at stage time");
        assert!(format!("{error:#}").contains("sessionTtlDays"), "{error:#}");
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
    fn live_stt_api_key_is_secret_marked_redacted_and_never_editable() {
        let (agent, cwd, _) = manager();
        let secret = ["s", "k-", "live-secret-1234567890abcdef"].concat();
        fs::write(
            agent.path().join("settings.json"),
            format!(r#"{{"live":{{"enabled":true,"sttBaseUrl":"https://stt.example","sttApiKey":"{secret}"}}}}"#),
        )
        .expect("settings");
        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("reload");
        let definition = SettingsCatalog::definition("live.sttApiKey").expect("definition");
        assert!(definition.secret, "live.sttApiKey must be marked secret");
        assert_eq!(
            definition.value_type,
            SettingValueType::Secret,
            "live.sttApiKey must use the Secret value type"
        );
        let view = manager.setting("live.sttApiKey").expect("secret view");
        assert!(view.redacted);
        assert_eq!(view.effective_value, REDACTED_VALUE);
        let encoded = serde_json::to_string(&view).expect("serialize view");
        assert!(!encoded.contains(secret.as_str()), "view must not leak the key: {encoded}");
        assert!(encoded.contains(REDACTED_VALUE));
        assert!(!view.editable_global && !view.editable_project);

        // The draft API refuses to write the secret through settings.json
        // (the raw value is configured by editing the settings file directly).
        let mut draft = manager.settings_draft(SettingsScope::Global).expect("draft");
        let error = draft
            .set("live.sttApiKey", json!("replacement"))
            .expect_err("secret write")
            .to_string();
        assert!(!error.contains("replacement"));
        assert!(error.contains("secret material"), "{error}");

        // The runtime still sees the configured key (the redacted view is
        // never round-tripped into the persisted file).
        let runtime = manager.settings().live_runtime();
        assert_eq!(runtime.stt_api_key, secret);
        let persisted = fs::read_to_string(agent.path().join("settings.json")).expect("persisted");
        assert!(persisted.contains(secret.as_str()), "raw key preserved on disk");
    }

    #[test]
    fn live_realtime_api_key_is_secret_marked_redacted_and_never_editable() {
        let (agent, cwd, _) = manager();
        let secret = ["r", "t-", "realtime-secret-1234567890abcdef"].concat();
        fs::write(
            agent.path().join("settings.json"),
            format!(
                r#"{{"live":{{"mode":"realtime","realtimeBaseUrl":"http://localhost:8317","realtimeApiKey":"{secret}"}}}}"#
            ),
        )
        .expect("settings");
        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("reload");
        let definition = SettingsCatalog::definition("live.realtimeApiKey").expect("definition");
        assert!(definition.secret, "live.realtimeApiKey must be marked secret");
        assert_eq!(
            definition.value_type,
            SettingValueType::Secret,
            "live.realtimeApiKey must use the Secret value type"
        );
        // A secret is never writable through any scope.
        assert_eq!(definition.scopes, SettingScopeSupport::None);
        let view = manager.setting("live.realtimeApiKey").expect("secret view");
        assert!(view.redacted);
        assert_eq!(view.effective_value, REDACTED_VALUE);
        let encoded = serde_json::to_string(&view).expect("serialize view");
        assert!(!encoded.contains(secret.as_str()), "view must not leak the realtime key: {encoded}");
        assert!(encoded.contains(REDACTED_VALUE));
        assert!(!view.editable_global && !view.editable_project);

        // The draft API refuses to write the secret through settings.json.
        let mut draft = manager.settings_draft(SettingsScope::Global).expect("draft");
        let error = draft
            .set("live.realtimeApiKey", json!("replacement"))
            .expect_err("secret write")
            .to_string();
        assert!(!error.contains("replacement"));
        assert!(error.contains("secret material"), "{error}");

        // The runtime still sees the configured realtime key (redacted view is
        // never round-tripped into the persisted file).
        let runtime = manager.settings().live_runtime();
        assert_eq!(runtime.realtime_api_key, secret);
        assert_eq!(runtime.mode, "realtime");
        assert_eq!(runtime.realtime_base_url, "http://localhost:8317");
        let persisted = fs::read_to_string(agent.path().join("settings.json")).expect("persisted");
        assert!(persisted.contains(secret.as_str()), "raw realtime key preserved on disk");
    }

    #[test]
    fn live_realtime_catalog_entries_have_correct_semantics() {
        // live.mode: enum stt/realtime, default stt, live-reload behavior,
        // editable in both scopes.
        let mode = SettingsCatalog::definition("live.mode").expect("live.mode definition");
        assert_eq!(mode.category, SettingCategory::Live);
        assert_eq!(mode.value_type, SettingValueType::Enum);
        assert_eq!(
            mode.enum_values.iter().copied().collect::<Vec<_>>(),
            vec!["stt", "realtime"]
        );
        assert_eq!(mode.default_value(), json!("stt"));
        assert_eq!(mode.behavior, SettingApplyBehavior::Live);
        assert!(mode.scopes.allows(SettingsScope::Global));
        assert!(mode.scopes.allows(SettingsScope::Project));
        assert!(!mode.secret);

        // The catalog draft accepts the canonical lowercase values and
        // rejects anything else (the runtime validator is the lenient layer
        // that trims/lowercases file-edited values).
        let (_agent, _cwd, manager) = manager();
        let mut draft = manager.settings_draft(SettingsScope::Global).expect("draft");
        draft.set("live.mode", json!("realtime")).expect("stage realtime");
        assert!(draft.set("live.mode", json!("Realtime")).is_err(), "mixed-case rejected by catalog");
        assert!(draft.set("live.mode", json!("banana")).is_err());

        // live.realtimeBaseUrl / realtimeModel / voice: plain strings, live,
        // both scopes, not secret.
        for (key, default) in [
            ("live.realtimeBaseUrl", json!("")),
            ("live.realtimeModel", json!("gpt-realtime-1.5")),
            ("live.voice", json!("sol")),
        ] {
            let definition = SettingsCatalog::definition(key).expect("{key} definition");
            assert_eq!(definition.category, SettingCategory::Live, "{key}");
            assert_eq!(definition.value_type, STRING, "{key}");
            assert_eq!(definition.default_value(), default, "{key}");
            assert_eq!(definition.behavior, SettingApplyBehavior::Live, "{key}");
            assert!(definition.scopes.allows(SettingsScope::Global), "{key}");
            assert!(definition.scopes.allows(SettingsScope::Project), "{key}");
            assert!(!definition.secret, "{key}");
        }

        // live.realtimeApiKey follows the same secret shape as live.sttApiKey.
        let key_def = SettingsCatalog::definition("live.realtimeApiKey").expect("realtimeApiKey");
        assert_eq!(key_def.value_type, SettingValueType::Secret);
        assert_eq!(key_def.scopes, SettingScopeSupport::None);
        assert!(key_def.secret);
        assert_eq!(key_def.default_value(), Value::Null);
    }

    #[test]
    fn images_gen_api_key_is_secret_marked_redacted_and_never_editable() {
        let (agent, cwd, _) = manager();
        let secret = ["s", "k-", "gen-secret-1234567890abcdef"].concat();
        fs::write(
            agent.path().join("settings.json"),
            format!(
                r#"{{"images":{{"genModel":"openai/gpt-image-1","genBaseUrl":"https://images.example/v1","genApiKey":"{secret}"}}}}"#
            ),
        )
        .expect("settings");
        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("reload");
        let definition = SettingsCatalog::definition("images.genApiKey").expect("definition");
        assert!(definition.secret, "images.genApiKey must be marked secret");
        assert_eq!(
            definition.value_type,
            SettingValueType::Secret,
            "images.genApiKey must use the Secret value type"
        );
        let view = manager.setting("images.genApiKey").expect("secret view");
        assert!(view.redacted);
        assert_eq!(view.effective_value, REDACTED_VALUE);
        let encoded = serde_json::to_string(&view).expect("serialize view");
        assert!(!encoded.contains(secret.as_str()), "view must not leak the key: {encoded}");
        assert!(encoded.contains(REDACTED_VALUE));
        assert!(!view.editable_global && !view.editable_project);

        // The draft API refuses to write the secret through settings.json.
        let mut draft = manager.settings_draft(SettingsScope::Global).expect("draft");
        let error = draft
            .set("images.genApiKey", json!("replacement"))
            .expect_err("secret write")
            .to_string();
        assert!(!error.contains("replacement"));
        assert!(error.contains("secret material"), "{error}");

        // The runtime still sees the configured key and redacts it in Debug;
        // the redacted view is never round-tripped into the persisted file.
        let runtime = manager.settings().image_gen_runtime();
        assert_eq!(runtime.gen_model.as_deref(), Some("openai/gpt-image-1"));
        assert_eq!(runtime.gen_base_url, "https://images.example/v1");
        assert_eq!(runtime.gen_api_key, secret);
        assert!(
            !format!("{runtime:?}").contains(secret.as_str()),
            "runtime Debug must redact the key"
        );
        let persisted = fs::read_to_string(agent.path().join("settings.json")).expect("persisted");
        assert!(persisted.contains(secret.as_str()), "raw key preserved on disk");
    }

    #[test]
    fn image_gen_runtime_defaults_are_empty() {
        let (_, _, manager) = manager();
        let runtime = manager.settings().image_gen_runtime();
        assert_eq!(runtime.gen_model, None);
        assert_eq!(runtime.gen_base_url, "");
        assert_eq!(runtime.gen_api_key, "");
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
