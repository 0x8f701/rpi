//! Two-phase global/project settings loading with atomic scoped persistence.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use parking_lot::RwLock;
use pi_agent::{ApprovalMode, QueueMode, ThinkingLevel};
use pi_ai::{CacheRetention, SimpleStreamOptions, ThinkingBudgets, Transport};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::resources::{CONFIG_DIR_NAME, agent_dir_path};
use crate::session_catalog::SessionSourceKind;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultProjectTrust {
    Always,
    Never,
    #[default]
    Ask,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourcePackage {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autoload: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub themes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PackageSource {
    All(String),
    Filtered(ResourcePackage),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_recent_tokens: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_images: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_width_cells: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear_on_shrink: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_terminal_progress: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRetryConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retry_delay_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_fallback: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_chains: Option<crate::RetryFallbackChains>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderRetryConfig>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_prompt: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_resize: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_images: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingBudgetsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimal: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub medium: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub high: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum KeyBindingValue {
    One(String),
    Many(Vec<String>),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DoubleEscapeAction {
    Fork,
    #[default]
    Tree,
    None,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrchestrationSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tasks: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_recursion_depth: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mailbox_capacity: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tools_per_agent: Option<usize>,
}

/// Per-agent enablement and model override persisted under `settings.agents`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRuntimeSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Model spec override (`provider/id` or bare id). Empty clears the override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Optional tool allow-list override for the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
}

impl AgentRuntimeSettings {
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    #[must_use]
    pub fn model_override(&self) -> Option<&str> {
        self.model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    #[must_use]
    pub fn tools_override(&self) -> Option<&[String]> {
        self.tools.as_deref()
    }

    /// Explicit model clear tombstone (`Some("")`) is not default — it must persist
    /// so a global clear can shadow a project-level model override.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.enabled.is_none() && self.model.is_none() && self.tools.is_none()
    }

    /// Whether this entry clears any model override (including project-effective).
    #[must_use]
    pub fn clears_model(&self) -> bool {
        self.model
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectiveBranchSummarySettings {
    pub reserve_tokens: i64,
    pub skip_prompt: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TuiRuntimeSettings {
    pub show_images: bool,
    pub image_width_cells: u16,
    pub auto_resize_images: bool,
    pub block_images: bool,
    pub theme: Option<String>,
    pub keybindings: BTreeMap<String, KeyBindingValue>,
    pub quiet_startup: bool,
    pub show_thinking: bool,
    pub double_escape_action: DoubleEscapeAction,
    pub scoped_models: Option<Vec<String>>,
}

#[derive(Clone, Debug)]
pub struct RuntimeSettingsSnapshot {
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub retry: crate::RetrySettings,
    pub compaction: crate::CompactionSettings,
    pub stream_options: SimpleStreamOptions,
    pub branch_summary: EffectiveBranchSummarySettings,
    pub tui: TuiRuntimeSettings,
    pub expose_session_environment: bool,
    pub process_tool_enabled: bool,
    pub glob_tool_enabled: bool,
    pub orchestration_enabled: bool,
    pub todo_tool_enabled: bool,
    pub orchestration_max_concurrency: usize,
    pub orchestration_max_recursion_depth: usize,
    pub orchestration_mailbox_capacity: usize,
    pub orchestration_max_tools_per_agent: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSettingsState {
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub auto_retry: bool,
    pub max_retries: usize,
    pub base_delay_ms: u64,
    pub model_fallback: bool,
    pub fallback_chains: crate::RetryFallbackChains,
    pub compaction_enabled: bool,
    pub compaction_reserve_tokens: i64,
    pub compaction_keep_recent_tokens: i64,
    pub transport: Transport,
    pub timeout_ms: Option<u64>,
    pub websocket_connect_timeout_ms: Option<u64>,
    pub provider_max_retries: usize,
    pub max_retry_delay_ms: Option<u64>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<i64>,
    pub cache_retention: CacheRetention,
    pub thinking_budgets: Option<ThinkingBudgetsConfig>,
    pub branch_summary_reserve_tokens: i64,
    pub branch_summary_skip_prompt: bool,
    pub show_images: bool,
    pub image_width_cells: u16,
    pub auto_resize_images: bool,
    pub block_images: bool,
    pub theme: Option<String>,
    pub keybindings: BTreeMap<String, KeyBindingValue>,
    pub quiet_startup: bool,
    pub show_thinking: bool,
    pub double_escape_action: DoubleEscapeAction,
    pub scoped_models: Option<Vec<String>>,
    pub expose_session_environment: bool,
    pub process_tool_enabled: bool,
    pub glob_tool_enabled: bool,
    pub orchestration_enabled: bool,
    pub todo_tool_enabled: bool,
    pub orchestration_max_concurrency: usize,
    pub orchestration_max_recursion_depth: usize,
    pub orchestration_mailbox_capacity: usize,
    pub orchestration_max_tools_per_agent: usize,
}

impl RuntimeSettingsSnapshot {
    #[must_use]
    pub fn state(&self) -> RuntimeSettingsState {
        RuntimeSettingsState {
            steering_mode: self.steering_mode,
            follow_up_mode: self.follow_up_mode,
            auto_retry: self.retry.enabled,
            max_retries: self.retry.max_retries,
            base_delay_ms: self.retry.base_delay_ms,
            model_fallback: self.retry.model_fallback,
            fallback_chains: self.retry.fallback_chains.clone(),
            compaction_enabled: self.compaction.enabled,
            compaction_reserve_tokens: self.compaction.reserve_tokens,
            compaction_keep_recent_tokens: self.compaction.keep_recent_tokens,
            transport: self.stream_options.stream.transport,
            timeout_ms: self.stream_options.stream.timeout_ms,
            websocket_connect_timeout_ms: self.stream_options.stream.websocket_connect_timeout_ms,
            provider_max_retries: self.stream_options.stream.max_retries,
            max_retry_delay_ms: self.stream_options.stream.max_retry_delay_ms,
            temperature: self.stream_options.stream.temperature,
            max_tokens: self.stream_options.stream.max_tokens,
            cache_retention: self.stream_options.stream.cache_retention,
            thinking_budgets: self.stream_options.thinking_budgets.as_ref().map(|budgets| ThinkingBudgetsConfig {
                minimal: budgets.minimal,
                low: budgets.low,
                medium: budgets.medium,
                high: budgets.high,
            }),
            branch_summary_reserve_tokens: self.branch_summary.reserve_tokens,
            branch_summary_skip_prompt: self.branch_summary.skip_prompt,
            show_images: self.tui.show_images,
            image_width_cells: self.tui.image_width_cells,
            auto_resize_images: self.tui.auto_resize_images,
            block_images: self.tui.block_images,
            theme: self.tui.theme.clone(),
            keybindings: self.tui.keybindings.clone(),
            quiet_startup: self.tui.quiet_startup,
            show_thinking: self.tui.show_thinking,
            double_escape_action: self.tui.double_escape_action,
            scoped_models: self.tui.scoped_models.clone(),
            expose_session_environment: self.expose_session_environment,
            process_tool_enabled: self.process_tool_enabled,
            glob_tool_enabled: self.glob_tool_enabled,
            orchestration_enabled: self.orchestration_enabled,
            todo_tool_enabled: self.todo_tool_enabled,
            orchestration_max_concurrency: self.orchestration_max_concurrency,
            orchestration_max_recursion_depth: self.orchestration_max_recursion_depth,
            orchestration_mailbox_capacity: self.orchestration_mailbox_capacity,
            orchestration_max_tools_per_agent: self.orchestration_max_tools_per_agent,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SupportedSetting {
    pub key: &'static str,
    pub application: &'static str,
}

pub const SUPPORTED_SETTINGS_COVERAGE: &[SupportedSetting] = &[
    SupportedSetting { key: "approvalMode", application: "session_run_blueprint host approval hook" },
    SupportedSetting { key: "sessionDir", application: "effective_session_dir" },
    SupportedSetting { key: "sessionImportSources", application: "effective_session_import_sources" },
    SupportedSetting { key: "steeringMode", application: "steering_mode" },
    SupportedSetting { key: "followUpMode", application: "follow_up_mode" },
    SupportedSetting { key: "retry", application: "retry_settings/apply_session_options" },
    SupportedSetting { key: "compaction", application: "apply_session_options" },
    SupportedSetting { key: "transport", application: "apply_session_options" },
    SupportedSetting { key: "timeoutMs", application: "apply_session_options" },
    SupportedSetting { key: "maxRetryDelayMs", application: "apply_session_options" },
    SupportedSetting { key: "temperature", application: "apply_session_options" },
    SupportedSetting { key: "maxTokens", application: "apply_session_options" },
    SupportedSetting { key: "cacheRetention", application: "apply_session_options" },
    SupportedSetting { key: "thinkingBudgets", application: "apply_session_options" },
    SupportedSetting { key: "scopedModels", application: "scoped_model_patterns" },
    SupportedSetting { key: "enabledModels", application: "scoped_model_patterns" },
    SupportedSetting { key: "terminal", application: "tui_runtime" },
    SupportedSetting { key: "images", application: "tui_runtime" },
    SupportedSetting { key: "theme", application: "tui_runtime/resource validation" },
    SupportedSetting { key: "keybindings", application: "tui_runtime" },
    SupportedSetting { key: "branchSummary", application: "branch_summary_settings" },
    SupportedSetting { key: "quietStartup", application: "tui_runtime" },
    SupportedSetting { key: "hideThinkingBlock", application: "tui_runtime" },
    SupportedSetting { key: "showThinking", application: "tui_runtime" },
    SupportedSetting { key: "exposeSessionEnvironment", application: "expose_session_environment" },
    SupportedSetting { key: "doubleEscapeAction", application: "tui_runtime" },
    SupportedSetting { key: "orchestration", application: "orchestration tool gates" },
];

/// Product settings shared by global and trusted project scopes.
///
/// Known fields are typed and unknown fields are retained so updating one
/// setting never destroys configuration owned by another product module.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_thinking_level: Option<ThinkingLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_project_trust: Option<DefaultProjectTrust>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<ApprovalMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_import_sources: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<ImageSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_summary: Option<BranchSummaryConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steering_mode: Option<QueueMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up_mode: Option<QueueMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_retry: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<Transport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retry_delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_retention: Option<CacheRetention>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budgets: Option<ThinkingBudgetsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_images: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_width_cells: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_resize_images: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_idle_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub websocket_connect_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scoped_models: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keybindings: Option<BTreeMap<String, KeyBindingValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_startup: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hide_thinking_block: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expose_session_environment: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub double_escape_action: Option<DoubleEscapeAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestration: Option<OrchestrationSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<crate::SelectorSettings>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: BTreeMap<String, AgentRuntimeSettings>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<PackageSource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub themes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_models: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_skill_commands: Option<bool>,
    #[serde(default, flatten)]
    pub extra: Map<String, Value>,
}

impl Settings {
    #[must_use]
    pub fn approval_mode(&self) -> ApprovalMode {
        self.approval_mode.unwrap_or_default()
    }

    /// Session sources eligible for automatic discovery and resolution.
    ///
    /// Native Pi sessions are always included. Foreign sources are opt-in and
    /// retain their configured order.
    #[must_use]
    pub fn effective_session_import_sources(&self) -> Vec<SessionSourceKind> {
        let configured = self.session_import_sources.as_deref().unwrap_or_default();
        let mut sources = Vec::with_capacity(configured.len() + 1);
        sources.push(SessionSourceKind::NativePi);
        for source in configured {
            if let Some(kind) = session_import_source_kind(source)
                && !sources.contains(&kind)
            {
                sources.push(kind);
            }
        }
        sources
    }
}

impl Settings {
    #[must_use]
    pub fn merged(&self, overrides: &Self) -> Self {
        let base = serde_json::to_value(self).unwrap_or_else(|_| Value::Object(Map::new()));
        let overlay =
            serde_json::to_value(overrides).unwrap_or_else(|_| Value::Object(Map::new()));
        let mut merged = serde_json::from_value::<Settings>(deep_merge(base, overlay))
            .unwrap_or_else(|_| self.clone());
        // Agent entries need field-aware merge so a global model clear tombstone
        // (`Some("")`) is not overwritten by a project-level model promotion.
        merged.agents = merge_agent_settings_maps(&self.agents, &overrides.agents);
        merged
    }

    pub fn apply_session_options(&self, options: &mut crate::SessionOptions) -> Result<()> {
        validate_operational_settings(self)?;
        if let Some(compaction) = &self.compaction {
            let defaults = crate::DEFAULT_COMPACTION_SETTINGS;
            options.compaction = Some(crate::CompactionSettings {
                enabled: compaction.enabled.unwrap_or(defaults.enabled),
                reserve_tokens: compaction.reserve_tokens.unwrap_or(defaults.reserve_tokens),
                keep_recent_tokens: compaction
                    .keep_recent_tokens
                    .unwrap_or(defaults.keep_recent_tokens),
            });
        }
        let provider_retry = self.retry.as_ref().and_then(|retry| retry.provider.as_ref());
        if let Some(value) = self.transport {
            options.stream_options.stream.transport = value;
        }
        if let Some(value) = provider_retry
            .and_then(|retry| retry.timeout_ms)
            .or(self.timeout_ms)
            .or(self.http_idle_timeout_ms)
        {
            options.stream_options.stream.timeout_ms = Some(value);
        }
        if let Some(value) = self.websocket_connect_timeout_ms {
            options.stream_options.stream.websocket_connect_timeout_ms = Some(value);
        }
        if let Some(value) = provider_retry.and_then(|retry| retry.max_retries) {
            options.stream_options.stream.max_retries = value;
        }
        if let Some(value) = provider_retry
            .and_then(|retry| retry.max_retry_delay_ms)
            .or(self.max_retry_delay_ms)
        {
            options.stream_options.stream.max_retry_delay_ms = Some(value);
        }
        if let Some(value) = self.temperature {
            options.stream_options.stream.temperature = Some(value);
        }
        if let Some(value) = self.max_tokens {
            options.stream_options.stream.max_tokens = Some(value);
        }
        if let Some(value) = self.cache_retention {
            options.stream_options.stream.cache_retention = value;
        }
        if let Some(value) = &self.thinking_budgets {
            options.stream_options.thinking_budgets = Some(ThinkingBudgets {
                minimal: value.minimal,
                low: value.low,
                medium: value.medium,
                high: value.high,
            });
        }
        Ok(())
    }

    pub fn runtime_settings(&self) -> Result<RuntimeSettingsSnapshot> {
        validate_operational_settings(self)?;
        let mut session_options = crate::SessionOptions {
            model: pi_ai::Model::default(),
            cwd: PathBuf::new(),
            system_prompt: String::new(),
            thinking_level: ThinkingLevel::Off,
            api_key: String::new(),
            compaction: Some(crate::DEFAULT_COMPACTION_SETTINGS),
            stream_options: SimpleStreamOptions::default(),
            tools: None,
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
        };
        session_options.stream_options.stream.transport = Transport::Auto;
        session_options.stream_options.stream.max_retry_delay_ms = Some(60_000);
        self.apply_session_options(&mut session_options)?;
        let orchestration = self.orchestration.as_ref();
        Ok(RuntimeSettingsSnapshot {
            steering_mode: self.steering_mode(),
            follow_up_mode: self.follow_up_mode(),
            retry: self.retry_settings(),
            compaction: session_options
                .compaction
                .unwrap_or(crate::DEFAULT_COMPACTION_SETTINGS),
            stream_options: session_options.stream_options,
            branch_summary: self.branch_summary_settings(),
            tui: self.tui_runtime(),
            expose_session_environment: self.expose_session_environment(),
            process_tool_enabled: self.process_tool_enabled(),
            glob_tool_enabled: self.glob_tool_enabled(),
            orchestration_enabled: self.orchestration_enabled(),
            todo_tool_enabled: self.todo_tool_enabled(),
            orchestration_max_concurrency: orchestration
                .and_then(|settings| settings.max_concurrency)
                .unwrap_or(crate::DEFAULT_MAX_CONCURRENCY),
            orchestration_max_recursion_depth: orchestration
                .and_then(|settings| settings.max_recursion_depth)
                .unwrap_or(crate::DEFAULT_MAX_RECURSION_DEPTH),
            orchestration_mailbox_capacity: orchestration
                .and_then(|settings| settings.mailbox_capacity)
                .unwrap_or(crate::DEFAULT_MAILBOX_CAPACITY),
            orchestration_max_tools_per_agent: orchestration
                .and_then(|settings| settings.max_tools_per_agent)
                .unwrap_or(crate::DEFAULT_MAX_TOOLS_PER_AGENT),
        })
    }

    #[must_use]
    pub fn steering_mode(&self) -> QueueMode {
        self.steering_mode.unwrap_or_default()
    }

    #[must_use]
    pub fn follow_up_mode(&self) -> QueueMode {
        self.follow_up_mode.unwrap_or_default()
    }

    #[must_use]
    pub fn retry_settings(&self) -> crate::RetrySettings {
        let retry = self.retry.as_ref();
        crate::RetrySettings {
            enabled: retry
                .and_then(|value| value.enabled)
                .or(self.auto_retry)
                .unwrap_or(true),
            max_retries: retry
                .and_then(|value| value.max_retries)
                .or(self.max_retries)
                .unwrap_or(3),
            base_delay_ms: retry
                .and_then(|value| value.base_delay_ms)
                .or(self.base_delay_ms)
                .unwrap_or(2_000),
            model_fallback: retry.and_then(|value| value.model_fallback).unwrap_or(true),
            fallback_chains: retry
                .and_then(|value| value.fallback_chains.clone())
                .unwrap_or_default(),
        }
    }

    #[must_use]
    pub fn branch_summary_settings(&self) -> EffectiveBranchSummarySettings {
        EffectiveBranchSummarySettings {
            reserve_tokens: self
                .branch_summary
                .as_ref()
                .and_then(|value| value.reserve_tokens)
                .unwrap_or(16_384),
            skip_prompt: self
                .branch_summary
                .as_ref()
                .and_then(|value| value.skip_prompt)
                .unwrap_or(false),
        }
    }

    #[must_use]
    pub fn scoped_model_patterns(&self) -> Option<&[String]> {
        self.scoped_models.as_deref().or(self.enabled_models.as_deref())
    }

    /// Lookup per-agent runtime settings, if any override exists.
    #[must_use]
    pub fn agent_settings(&self, name: &str) -> Option<&AgentRuntimeSettings> {
        self.agents.get(name)
    }

    /// Whether the named agent may be spawned. Missing entries default to enabled.
    #[must_use]
    pub fn is_agent_enabled(&self, name: &str) -> bool {
        self.agents
            .get(name)
            .map_or(true, AgentRuntimeSettings::is_enabled)
    }

    /// Model override string for the named agent, if configured.
    #[must_use]
    pub fn agent_model_override(&self, name: &str) -> Option<&str> {
        self.agents.get(name).and_then(AgentRuntimeSettings::model_override)
    }

    /// Insert or update one agent entry, removing it when it collapses to defaults.
    pub fn set_agent_settings(&mut self, name: impl Into<String>, settings: AgentRuntimeSettings) {
        let name = name.into();
        if settings.is_default() {
            self.agents.remove(&name);
        } else {
            self.agents.insert(name, settings);
        }
    }

    #[must_use]
    pub fn tui_runtime(&self) -> TuiRuntimeSettings {
        TuiRuntimeSettings {
            show_images: self
                .show_images
                .or_else(|| self.terminal.as_ref().and_then(|value| value.show_images))
                .unwrap_or(true),
            image_width_cells: self
                .image_width_cells
                .or_else(|| self.terminal.as_ref().and_then(|value| value.image_width_cells))
                .unwrap_or(60),
            auto_resize_images: self
                .auto_resize_images
                .or_else(|| self.images.as_ref().and_then(|value| value.auto_resize))
                .unwrap_or(true),
            block_images: self
                .images
                .as_ref()
                .and_then(|value| value.block_images)
                .unwrap_or(false),
            theme: self.theme.clone(),
            keybindings: self.keybindings.clone().unwrap_or_default(),
            quiet_startup: self.quiet_startup.unwrap_or(false),
            show_thinking: self
                .show_thinking
                .unwrap_or_else(|| !self.hide_thinking_block.unwrap_or(false)),
            double_escape_action: self.double_escape_action.unwrap_or_default(),
            scoped_models: self.scoped_model_patterns().map(<[String]>::to_vec),
        }
    }

    #[must_use]
    pub fn expose_session_environment(&self) -> bool {
        self.expose_session_environment.unwrap_or(true)
    }

    #[must_use]
    pub fn process_tool_enabled(&self) -> bool {
        self.orchestration
            .as_ref()
            .and_then(|value| value.process)
            .unwrap_or(false)
    }

    /// When true, the main session catalog includes the native sandboxed `glob`
    /// tool. Default false — does not broaden the strict [read,bash,edit,write]
    /// baseline unless explicitly enabled via `settings.orchestration.glob` or
    /// an allow-list that names `glob`.
    #[must_use]
    pub fn glob_tool_enabled(&self) -> bool {
        self.orchestration
            .as_ref()
            .and_then(|value| value.glob)
            .unwrap_or(false)
    }

    #[must_use]
    pub fn orchestration_enabled(&self) -> bool {
        self.orchestration
            .as_ref()
            .and_then(|value| value.tasks)
            .unwrap_or(false)
    }

    #[must_use]
    pub fn todo_tool_enabled(&self) -> bool {
        self.orchestration
            .as_ref()
            .and_then(|value| value.todo)
            .unwrap_or(false)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingsScope {
    Global,
    Project,
}

#[derive(Clone, Debug)]
pub struct SettingsPaths {
    pub global: PathBuf,
    pub project: PathBuf,
}

impl SettingsPaths {
    #[must_use]
    pub fn new(cwd: impl AsRef<Path>, agent_dir: impl AsRef<Path>) -> Self {
        Self {
            global: agent_dir.as_ref().join("settings.json"),
            project: cwd.as_ref().join(CONFIG_DIR_NAME).join("settings.json"),
        }
    }
}

#[derive(Clone)]
pub struct SettingsManager {
    inner: Arc<RwLock<SettingsState>>,
}

#[derive(Clone, Debug)]
struct SettingsState {
    paths: SettingsPaths,
    global: Settings,
    project: Settings,
    overrides: Settings,
    effective: Settings,
    project_trusted: bool,
}

impl SettingsManager {
    /// Phase one: load only trusted user-global settings. Project settings are
    /// deliberately not opened until [`Self::load_project`] receives trust.
    pub fn load_phase_one(cwd: impl AsRef<Path>, agent_dir: impl AsRef<Path>) -> Result<Self> {
        let paths = SettingsPaths::new(cwd, agent_dir);
        let global = load_settings_file(&paths.global, SettingsScope::Global)?;
        let effective = global.clone();
        Ok(Self {
            inner: Arc::new(RwLock::new(SettingsState {
                paths,
                global,
                project: Settings::default(),
                overrides: Settings::default(),
                effective,
                project_trusted: false,
            })),
        })
    }

    pub fn load(cwd: impl AsRef<Path>, project_trusted: bool) -> Result<Self> {
        let manager = Self::load_phase_one(cwd.as_ref(), agent_dir_path())?;
        manager.load_project(project_trusted)?;
        Ok(manager)
    }

    /// Phase two: load project settings only when the project is trusted.
    /// Revoking trust immediately drops all project-derived values.
    pub fn load_project(&self, trusted: bool) -> Result<()> {
        let path = self.inner.read().paths.project.clone();
        let project = if trusted {
            load_settings_file(&path, SettingsScope::Project)?
        } else {
            Settings::default()
        };
        let mut state = self.inner.write();
        state.project_trusted = trusted;
        state.project = project;
        recompute(&mut state);
        Ok(())
    }

    /// Validate both eligible scopes before changing the active snapshot.
    pub fn reload(&self) -> Result<()> {
        let state = self.inner.read().clone();
        let global = load_settings_file(&state.paths.global, SettingsScope::Global)?;
        let project = if state.project_trusted {
            load_settings_file(&state.paths.project, SettingsScope::Project)?
        } else {
            Settings::default()
        };
        let mut active = self.inner.write();
        active.global = global;
        active.project = project;
        recompute(&mut active);
        Ok(())
    }

    #[must_use]
    pub fn settings(&self) -> Settings {
        self.inner.read().effective.clone()
    }

    #[must_use]
    pub fn global_settings(&self) -> Settings {
        self.inner.read().global.clone()
    }

    #[must_use]
    pub fn project_settings(&self) -> Settings {
        self.inner.read().project.clone()
    }
    #[must_use]
    pub fn session_overrides(&self) -> Settings {
        self.inner.read().overrides.clone()
    }


    #[must_use]
    pub fn paths(&self) -> SettingsPaths {
        self.inner.read().paths.clone()
    }

    #[must_use]
    pub fn is_project_trusted(&self) -> bool {
        self.inner.read().project_trusted
    }

    /// Session-only overrides are merged last and are never persisted.
    pub fn apply_overrides(&self, overrides: Settings) {
        let mut state = self.inner.write();
        state.overrides = state.overrides.merged(&overrides);
        recompute(&mut state);
    }

    pub fn clear_overrides(&self) {
        let mut state = self.inner.write();
        state.overrides = Settings::default();
        recompute(&mut state);
    }

    pub fn update_global(&self, update: impl FnOnce(&mut Settings)) -> Result<()> {
        let (path, mut candidate) = {
            let state = self.inner.read();
            (state.paths.global.clone(), state.global.clone())
        };
        update(&mut candidate);
        validate_settings(&candidate, SettingsScope::Global, &path)?;
        write_settings_file(&path, &candidate, SettingsScope::Global)?;
        let mut state = self.inner.write();
        state.global = candidate;
        recompute(&mut state);
        Ok(())
    }

    pub fn update_project(&self, update: impl FnOnce(&mut Settings)) -> Result<()> {
        let (trusted, path, mut candidate) = {
            let state = self.inner.read();
            (
                state.project_trusted,
                state.paths.project.clone(),
                state.project.clone(),
            )
        };
        if !trusted {
            bail!("project is not trusted; refusing to write project settings");
        }
        update(&mut candidate);
        validate_settings(&candidate, SettingsScope::Project, &path)?;
        write_settings_file(&path, &candidate, SettingsScope::Project)?;
        let mut state = self.inner.write();
        state.project = candidate;
        recompute(&mut state);
        Ok(())
    }
}

fn recompute(state: &mut SettingsState) {
    state.effective = state.global.merged(&state.project).merged(&state.overrides);
}

/// Merge per-agent maps with tombstone-aware field semantics.
///
/// Overlay fields win when present. An empty-string model in either layer is a
/// clear tombstone that must survive merge so a global clear continues to shadow
/// a project model promotion.
fn merge_agent_settings_maps(
    base: &BTreeMap<String, AgentRuntimeSettings>,
    overlay: &BTreeMap<String, AgentRuntimeSettings>,
) -> BTreeMap<String, AgentRuntimeSettings> {
    let mut names = base.keys().cloned().collect::<std::collections::BTreeSet<_>>();
    names.extend(overlay.keys().cloned());
    let mut out = BTreeMap::new();
    for name in names {
        let left = base.get(&name);
        let right = overlay.get(&name);
        let enabled = right
            .and_then(|entry| entry.enabled)
            .or_else(|| left.and_then(|entry| entry.enabled));
        // A clear tombstone in either layer shadows a concrete model in the other
        // so global clear remains effective against project promotions (and vice versa).
        let model = if left.is_some_and(AgentRuntimeSettings::clears_model)
            || right.is_some_and(AgentRuntimeSettings::clears_model)
        {
            Some(String::new())
        } else if let Some(value) = right.and_then(|entry| entry.model.as_ref()) {
            Some(value.clone())
        } else {
            left.and_then(|entry| entry.model.clone())
        };
        let tools = right
            .and_then(|entry| entry.tools.clone())
            .or_else(|| left.and_then(|entry| entry.tools.clone()));
        let entry = AgentRuntimeSettings {
            enabled,
            model,
            tools,
        };
        if !entry.is_default() {
            out.insert(name.clone(), entry);
        }
    }
    out
}

fn deep_merge(base: Value, overlay: Value) -> Value {
    match (base, overlay) {
        (Value::Object(mut base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                if value.is_null() {
                    continue;
                }
                let merged = base
                    .remove(&key)
                    .map_or(value.clone(), |current| deep_merge(current, value));
                base.insert(key, merged);
            }
            Value::Object(base)
        }
        (_, overlay) => overlay,
    }
}

const MAX_SETTINGS_FILE_BYTES: u64 = 1024 * 1024;

/// Result of migrating legacy `subagents.agentOverrides` into canonical `agents`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct LegacyAgentMigration {
    /// True when any legacy-only entry was merged into `agents`.
    merged: bool,
    /// True when at least one name existed in both layers (canonical kept).
    conflicts: usize,
    /// True when the on-disk shape still carries legacy overrides that should be rewritten.
    needs_rewrite: bool,
}

fn migrate_legacy_agent_overrides(settings: &mut Settings) -> Result<LegacyAgentMigration> {
    let Some(subagents_value) = settings.extra.get("subagents").cloned() else {
        return Ok(LegacyAgentMigration::default());
    };
    let Some(subagents) = subagents_value.as_object() else {
        // Preserve unknown non-object subagents payloads untouched.
        return Ok(LegacyAgentMigration::default());
    };
    let Some(overrides_value) = subagents.get("agentOverrides") else {
        return Ok(LegacyAgentMigration::default());
    };
    let Some(overrides) = overrides_value.as_object() else {
        bail!(
            "subagents.agentOverrides must be an object of per-agent overrides; \
             migrate entries to top-level agents or fix the settings file"
        );
    };

    let mut migration = LegacyAgentMigration {
        needs_rewrite: true,
        ..LegacyAgentMigration::default()
    };

    for (name, value) in overrides {
        let name = name.trim();
        if name.is_empty() {
            bail!("subagents.agentOverrides contains an empty agent name");
        }
        let legacy = parse_legacy_agent_override(name, value)?;
        if legacy.is_default() {
            continue;
        }
        if settings.agents.contains_key(name) {
            // Canonical top-level agents always win on conflict.
            migration.conflicts += 1;
            continue;
        }
        settings.agents.insert(name.to_owned(), legacy);
        migration.merged = true;
    }

    // Drop only agentOverrides; preserve any other unknown subagents fields.
    let mut remaining = subagents.clone();
    remaining.remove("agentOverrides");
    if remaining.is_empty() {
        settings.extra.remove("subagents");
    } else {
        settings
            .extra
            .insert("subagents".to_owned(), Value::Object(remaining));
    }
    Ok(migration)
}

fn parse_legacy_agent_override(name: &str, value: &Value) -> Result<AgentRuntimeSettings> {
    let object = value.as_object().ok_or_else(|| {
        anyhow!("subagents.agentOverrides.{name} must be an object with enabled/model/tools fields")
    })?;
    let enabled = match object.get("enabled") {
        None => None,
        Some(Value::Bool(value)) => Some(*value),
        Some(other) => bail!(
            "subagents.agentOverrides.{name}.enabled must be a boolean, got {other}"
        ),
    };
    let model = match object.get("model") {
        None => None,
        Some(Value::String(value)) => Some(value.clone()),
        Some(Value::Null) => Some(String::new()),
        Some(other) => bail!(
            "subagents.agentOverrides.{name}.model must be a string, got {other}"
        ),
    };
    let tools = match object.get("tools") {
        None => None,
        Some(Value::Array(items)) => {
            let mut tools = Vec::with_capacity(items.len());
            for item in items {
                let tool = item.as_str().ok_or_else(|| {
                    anyhow!("subagents.agentOverrides.{name}.tools entries must be strings")
                })?;
                if tool.trim().is_empty() {
                    bail!("subagents.agentOverrides.{name}.tools contains an empty tool name");
                }
                tools.push(tool.to_owned());
            }
            Some(tools)
        }
        Some(other) => bail!(
            "subagents.agentOverrides.{name}.tools must be an array of strings, got {other}"
        ),
    };
    // Unknown per-agent keys are intentionally ignored (not promoted into extra).
    Ok(AgentRuntimeSettings {
        enabled,
        model,
        tools,
    })
}

/// Process-wide dedupe keys for settings diagnostics (legacy migration path
/// strings, or `path\0subagents.<key>` for ignored agent-config fields).
static SETTINGS_DIAGNOSTIC_WARNED_KEYS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

thread_local! {
    /// When set, settings diagnostics from this thread are collected instead of
    /// printed. Thread-local so parallel settings tests cannot pollute each
    /// other's capture buffers or suppress foreign-thread diagnostics.
    /// Armed by `arm_settings_diagnostic_capture()` for interactive TUI startup
    /// so warnings surface in the UI instead of only on stderr.
    static SETTINGS_DIAGNOSTIC_WARN_CAPTURE: std::cell::RefCell<Option<Vec<String>>> =
        const { std::cell::RefCell::new(None) };
}

/// Emit a settings diagnostic once per process-wide dedupe key.
fn emit_settings_diagnostic(dedupe_key: String, message: String) {
    {
        let mut warned = SETTINGS_DIAGNOSTIC_WARNED_KEYS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !warned.insert(dedupe_key) {
            return;
        }
    }
    let captured = SETTINGS_DIAGNOSTIC_WARN_CAPTURE.with(|capture| {
        let mut capture = capture.borrow_mut();
        if let Some(log) = capture.as_mut() {
            log.push(message.clone());
            true
        } else {
            false
        }
    });
    if captured {
        return;
    }
    // Diagnostics go to stderr so structured stdout (JSON/RPC) stays clean.
    eprintln!("{message}");
}

fn warn_legacy_agent_migration(path: &Path, migration: &LegacyAgentMigration) {
    if !migration.needs_rewrite {
        return;
    }
    let detail = match (migration.merged, migration.conflicts) {
        (true, 0) => "legacy-only entries were merged into top-level agents".to_owned(),
        (true, n) => format!(
            "legacy-only entries were merged into top-level agents; {n} conflicting name(s) kept the canonical agents value"
        ),
        (false, n) if n > 0 => format!(
            "{n} conflicting name(s) kept the canonical agents value; redundant legacy overrides were dropped"
        ),
        _ => "legacy overrides were removed with no additional agent entries".to_owned(),
    };
    let message = format!(
        "warning: {} uses deprecated subagents.agentOverrides; {detail}. \
         Prefer settings.agents.<name>.{{enabled,model,tools}}. \
         The file will be rewritten in canonical form on the next settings save.",
        path.display()
    );
    // One deprecation notice per settings file path.
    emit_settings_diagnostic(path.to_string_lossy().into_owned(), message);
}

/// Known nested keys under `subagents` that look like agent configuration but
/// are not migrated (only `agentOverrides` is). Arbitrary metadata is retained
/// silently; these names always warn so mis-nested config is not silent.
const UNSUPPORTED_SUBAGENTS_AGENT_KEYS: &[&str] =
    &["agents", "models", "agentModels", "tools", "defaults"];

fn value_looks_like_agent_runtime_config(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.contains_key("enabled") || object.contains_key("model") || object.contains_key("tools")
}

/// True when a remaining `subagents.<key>` value looks like agent configuration:
/// a single agent runtime object, or a map of agent-name → runtime objects.
fn value_looks_like_agent_config_tree(value: &Value) -> bool {
    if value_looks_like_agent_runtime_config(value) {
        return true;
    }
    let Some(object) = value.as_object() else {
        return false;
    };
    !object.is_empty() && object.values().any(value_looks_like_agent_runtime_config)
}

fn is_unsupported_agentish_subagents_field(key: &str, value: &Value) -> bool {
    if UNSUPPORTED_SUBAGENTS_AGENT_KEYS.contains(&key) {
        return true;
    }
    // `agentOverrides` is handled by migration + its own deprecation warning.
    if key == "agentOverrides" {
        return false;
    }
    value_looks_like_agent_config_tree(value)
}

/// Warn once per settings path + field for remaining agent-config-looking keys
/// under `subagents`. Unknown non-agent metadata is preserved without warning.
fn warn_unsupported_subagents_fields(path: &Path, settings: &Settings) {
    let Some(subagents_value) = settings.extra.get("subagents") else {
        return;
    };
    let Some(subagents) = subagents_value.as_object() else {
        return;
    };
    let path_display = path.display().to_string();
    for (key, value) in subagents {
        if !is_unsupported_agentish_subagents_field(key, value) {
            continue;
        }
        let message = format!(
            "{path_display} contains unsupported subagents.{key}; this setting is ignored. \
             Move agent configuration to top-level agents.<name>.{{enabled,model,tools}}."
        );
        let dedupe_key = format!("{path_display}\0subagents.{key}");
        emit_settings_diagnostic(dedupe_key, message);
    }
}


/// Arm the process-wide settings diagnostic capture on the current thread.
///
/// When armed, subsequent calls to [`emit_settings_diagnostic`] on this thread
/// collect messages into a thread-local buffer instead of printing to stderr.
/// The capture is thread-local so production startup cannot accidentally
/// suppress diagnostics emitted by concurrent background work on other threads.
/// Drain with [`drain_settings_diagnostics`] to retrieve and clear the buffer.
pub fn arm_settings_diagnostic_capture() {
    SETTINGS_DIAGNOSTIC_WARN_CAPTURE.with(|capture| {
        *capture.borrow_mut() = Some(Vec::new());
    });
}

/// Drain captured settings diagnostics from the current thread.
///
/// Returns the messages collected since the last [`arm_settings_diagnostic_capture`]
/// on this thread, or an empty `Vec` when the capture was never armed. Draining
/// clears the buffer so a second drain without re-arming yields nothing.
#[must_use]
pub fn drain_settings_diagnostics() -> Vec<String> {
    SETTINGS_DIAGNOSTIC_WARN_CAPTURE.with(|capture| {
        capture.borrow_mut().take().unwrap_or_default()
    })
}
#[cfg(test)]
fn capture_settings_diagnostics<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    // Unique temp paths make process-wide path state safe without clearing:
    // each capture body only asserts on warnings for the paths it loads.
    arm_settings_diagnostic_capture();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    let messages = drain_settings_diagnostics();
    match result {
        Ok(value) => (value, messages),
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[cfg(test)]
fn capture_legacy_agent_migration_warnings<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    capture_settings_diagnostics(f)
}
#[cfg(test)]
fn capture_is_explicitly_armed_and_drained_once() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("settings.json");
    fs::write(
        &path,
        r#"{"subagents":{"agentOverrides":{"reviewer":{"enabled":true}}}}"#,
    )
    .expect("settings");

    arm_settings_diagnostic_capture();
    load_settings_file(&path, SettingsScope::Global).expect("load");
    let warnings = drain_settings_diagnostics();

    assert_eq!(warnings.len(), 1, "captured once: {warnings:?}");
    assert!(
        warnings[0].contains("deprecated subagents.agentOverrides"),
        "{warnings:?}"
    );
    assert!(
        drain_settings_diagnostics().is_empty(),
        "drain must clear the capture"
    );
}





fn load_settings_file(path: &Path, scope: SettingsScope) -> Result<Settings> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("opening {} settings {}", scope_name(scope), path.display())
            });
        }
    };
    let size = file
        .metadata()
        .with_context(|| format!("reading metadata for {} settings {}", scope_name(scope), path.display()))?
        .len();
    if size > MAX_SETTINGS_FILE_BYTES {
        bail!(
            "{} settings {} exceeds maximum size of {} bytes (found {} bytes)",
            scope_name(scope),
            path.display(),
            MAX_SETTINGS_FILE_BYTES,
            size,
        );
    }
    let mut content = String::with_capacity(usize::try_from(size).unwrap_or(0));
    file.take(MAX_SETTINGS_FILE_BYTES + 1)
        .read_to_string(&mut content)
        .with_context(|| format!("reading {} settings {}", scope_name(scope), path.display()))?;
    if content.len() as u64 > MAX_SETTINGS_FILE_BYTES {
        bail!(
            "{} settings {} exceeds maximum size of {} bytes",
            scope_name(scope),
            path.display(),
            MAX_SETTINGS_FILE_BYTES,
        );
    }
    let mut settings: Settings = serde_json::from_str(&content).with_context(|| {
        format!("Failed to parse settings.json\nFile: {}", path.display())
    })?;
    let migration = migrate_legacy_agent_overrides(&mut settings).with_context(|| {
        format!(
            "invalid {} settings {} while migrating subagents.agentOverrides",
            scope_name(scope),
            path.display()
        )
    })?;
    validate_settings(&settings, scope, path)?;
    warn_legacy_agent_migration(path, &migration);
    warn_unsupported_subagents_fields(path, &settings);
    Ok(settings)
}

pub(crate) const SESSION_IMPORT_SOURCE_VALUES: &[&str] =
    &["omp", "codex", "claude", "grok", "droid"];

fn session_import_source_kind(value: &str) -> Option<SessionSourceKind> {
    match value {
        "omp" => Some(SessionSourceKind::Omp),
        "codex" => Some(SessionSourceKind::Codex),
        "claude" => Some(SessionSourceKind::Claude),
        "grok" => Some(SessionSourceKind::Grok),
        "droid" => Some(SessionSourceKind::Droid),
        _ => None,
    }
}

pub(crate) fn validate_settings(settings: &Settings, scope: SettingsScope, path: &Path) -> Result<()> {
    if scope == SettingsScope::Project && settings.approval_mode.is_some() {
        bail!(
            "invalid project settings {}: approvalMode is global-only",
            path.display()
        );
    }
    for (field, entries) in [
        ("extensions", &settings.extensions),
        ("skills", &settings.skills),
        ("prompts", &settings.prompts),
        ("themes", &settings.themes),
    ] {
        if entries.iter().any(|entry| entry.trim().is_empty()) {
            bail!(
                "invalid {} settings {}: {field} contains an empty path",
                scope_name(scope),
                path.display()
            );
        }
    }
    if settings.enabled_models.as_ref().is_some_and(|patterns| {
        patterns.iter().any(|pattern| pattern.trim().is_empty())
    }) {
        bail!(
            "invalid {} settings {}: enabledModels contains an empty pattern",
            scope_name(scope),
            path.display()
        );
    }
    if let Some(selector) = &settings.selector {
        if selector.max_results == 0 || selector.max_results > 20 {
            bail!(
                "invalid {} settings {}: selector.maxResults must be between 1 and 20",
                scope_name(scope),
                path.display()
            );
        }
        if selector.min_score < 0
            || selector.autoload_threshold < 0
            || selector.auto_select_threshold < 0
            || selector.confidence_margin < 0
        {
            bail!(
                "invalid {} settings {}: selector score thresholds must not be negative",
                scope_name(scope),
                path.display()
            );
        }
        if selector.classifier.max_tokens <= 0 || selector.classifier.max_tokens > 256 {
            bail!(
                "invalid {} settings {}: selector.classifier.maxTokens must be between 1 and 256",
                scope_name(scope),
                path.display()
            );
        }
        if selector.classifier.timeout_ms == 0 || selector.classifier.timeout_ms > 60_000 {
            bail!(
                "invalid {} settings {}: selector.classifier.timeoutMs must be between 1 and 60000",
                scope_name(scope),
                path.display()
            );
        }
    }
    for package in &settings.packages {
        let source = match package {
            PackageSource::All(source) => source,
            PackageSource::Filtered(package) => &package.source,
        };
        if source.trim().is_empty() {
            bail!(
                "invalid {} settings {}: package source is empty",
                scope_name(scope),
                path.display()
            );
        }
    }
    validate_operational_settings(settings).with_context(|| {
        format!("invalid {} settings {}", scope_name(scope), path.display())
    })?;
    Ok(())
}

fn validate_operational_settings(settings: &Settings) -> Result<()> {
    if settings.compaction.as_ref().and_then(|value| value.reserve_tokens).is_some_and(|value| value <= 0) {
        bail!("compaction.reserveTokens must be greater than zero");
    }
    if settings.compaction.as_ref().and_then(|value| value.keep_recent_tokens).is_some_and(|value| value < 0) {
        bail!("compaction.keepRecentTokens must be non-negative");
    }
    if settings.branch_summary.as_ref().and_then(|value| value.reserve_tokens).is_some_and(|value| value <= 0) {
        bail!("branchSummary.reserveTokens must be greater than zero");
    }
    if settings.retry.as_ref().and_then(|value| value.base_delay_ms).or(settings.base_delay_ms).is_some_and(|value| value == 0) {
        bail!("retry.baseDelayMs must be greater than zero");
    }
    if let Some(chains) = settings.retry.as_ref().and_then(|value| value.fallback_chains.as_ref()) {
        for (key, chain) in chains {
            if key.trim().is_empty() {
                bail!("retry.fallbackChains contains an empty key");
            }
            if !chain.iter().any(|selector| !selector.trim().is_empty()) {
                bail!("retry.fallbackChains entry '{key}' must contain at least one selector");
            }
            for selector in chain {
                let trimmed = selector.trim();
                if trimmed.is_empty() {
                    bail!("retry.fallbackChains entry '{key}' contains an empty selector");
                }
                if crate::is_retry_fallback_wildcard_key(trimmed) {
                    continue;
                }
                if crate::parse_retry_fallback_selector(trimmed, None::<&crate::CatalogModelLookup>).is_none() {
                    bail!("retry.fallbackChains entry '{key}' has invalid selector '{trimmed}'");
                }
            }
        }
    }
    if settings.temperature.is_some_and(|value| !value.is_finite() || !(0.0..=2.0).contains(&value)) {
        bail!("temperature must be finite and between 0 and 2");
    }
    if settings.max_tokens.is_some_and(|value| value <= 0) {
        bail!("maxTokens must be greater than zero");
    }
    if settings.image_width_cells.or_else(|| settings.terminal.as_ref().and_then(|value| value.image_width_cells)).is_some_and(|value| value == 0) {
        bail!("imageWidthCells must be greater than zero");
    }
    if settings.theme.as_ref().is_some_and(|value| value.trim().is_empty()) {
        bail!("theme must not be empty");
    }
    if settings.session_dir.as_ref().is_some_and(|value| value.as_os_str().is_empty()) {
        bail!("sessionDir must not be empty");
    }
    if let Some(sources) = &settings.session_import_sources {
        let mut seen = HashSet::with_capacity(sources.len());
        for source in sources {
            if session_import_source_kind(source).is_none() {
                bail!(
                    "sessionImportSources contains unsupported source '{source}'; allowed sources: {}",
                    SESSION_IMPORT_SOURCE_VALUES.join(", ")
                );
            }
            if !seen.insert(source.as_str()) {
                bail!("sessionImportSources contains duplicate source '{source}'");
            }
        }
    }
    if settings.scoped_models.as_ref().is_some_and(|patterns| patterns.iter().any(|pattern| pattern.trim().is_empty())) {
        bail!("scopedModels contains an empty pattern");
    }
    if settings.thinking_budgets.as_ref().is_some_and(|budgets| {
        [budgets.minimal, budgets.low, budgets.medium, budgets.high]
            .into_iter()
            .flatten()
            .any(|value| value <= 0)
    }) {
        bail!("thinkingBudgets values must be greater than zero");
    }
    if settings.keybindings.as_ref().is_some_and(|bindings| bindings.iter().any(|(action, chords)| {
        action.trim().is_empty() || match chords {
            KeyBindingValue::One(chord) => chord.trim().is_empty(),
            KeyBindingValue::Many(chords) => chords.is_empty() || chords.iter().any(|chord| chord.trim().is_empty()),
        }
    })) {
        bail!("keybindings contains an empty action or chord");
    }
    if settings.agents.iter().any(|(name, _)| name.trim().is_empty()) {
        bail!("agents contains an empty name");
    }
    if let Some(orchestration) = &settings.orchestration {
        if orchestration.max_concurrency.is_some_and(|value| value == 0 || value > 64) {
            bail!("orchestration.maxConcurrency must be between 1 and 64");
        }
        if orchestration.max_recursion_depth.is_some_and(|value| value > 16) {
            bail!("orchestration.maxRecursionDepth must be at most 16");
        }
        if orchestration.mailbox_capacity.is_some_and(|value| value == 0 || value > 10_000) {
            bail!("orchestration.mailboxCapacity must be between 1 and 10000");
        }
        if orchestration.max_tools_per_agent.is_some_and(|value| value == 0 || value > 64) {
            bail!("orchestration.maxToolsPerAgent must be between 1 and 64");
        }
    }
    Ok(())
}

fn write_settings_file(path: &Path, settings: &Settings, scope: SettingsScope) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(settings).with_context(|| {
        format!("serializing {} settings {}", scope_name(scope), path.display())
    })?;
    bytes.push(b'\n');
    atomic_write(path, &bytes).with_context(|| {
        format!("writing {} settings {}", scope_name(scope), path.display())
    })
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating directory {}", parent.display()))?;
    let temp_path = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|name| name.to_str()).unwrap_or("pi"),
        Uuid::new_v4().simple()
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .with_context(|| format!("creating temporary file {}", temp_path.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("writing temporary file {}", temp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing temporary file {}", temp_path.display()))?;
        drop(file);
        fs::rename(&temp_path, path)
            .with_context(|| format!("replacing {} with {}", path.display(), temp_path.display()))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("syncing directory {}", parent.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

const fn scope_name(scope: SettingsScope) -> &'static str {
    match scope {
        SettingsScope::Global => "global",
        SettingsScope::Project => "project",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_models_persist_atomically_and_preserve_order() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("settings manager");
        manager.update_global(|settings| {
            settings.enabled_models = Some(vec!["provider/two".to_owned(), "provider/one".to_owned()]);
        }).expect("persist enabled models");

        let reloaded = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("reload settings");
        assert_eq!(reloaded.settings().enabled_models, Some(vec!["provider/two".to_owned(), "provider/one".to_owned()]));
        assert!(reloaded.paths().global.exists());
    }

    #[test]
    fn selector_settings_persist_and_validate_limits() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path())
            .expect("settings manager");
        manager
            .update_global(|settings| {
                settings.selector = Some(crate::SelectorSettings {
                    max_results: 3,
                    classifier: crate::SelectorClassifierSettings {
                        enabled: true,
                        max_tokens: 128,
                        timeout_ms: 2_000,
                        ..crate::SelectorClassifierSettings::default()
                    },
                    ..crate::SelectorSettings::default()
                });
            })
            .expect("persist selector settings");
        let reloaded = SettingsManager::load_phase_one(cwd.path(), agent.path())
            .expect("reload settings");
        assert_eq!(reloaded.settings().selector.unwrap().max_results, 3);

        let mut invalid = Settings::default();
        invalid.selector = Some(crate::SelectorSettings {
            max_results: 0,
            ..crate::SelectorSettings::default()
        });
        assert!(validate_settings(&invalid, SettingsScope::Global, &manager.paths().global)
            .expect_err("zero max results must fail")
            .to_string()
            .contains("selector.maxResults"));
    }
    #[test]
    fn approval_mode_has_documented_support_coverage() {
        let entry = SUPPORTED_SETTINGS_COVERAGE
            .iter()
            .find(|entry| entry.key == "approvalMode")
            .expect("approvalMode support coverage");
        assert!(entry.application.contains("host approval hook"));
    }

    #[test]
    fn approval_mode_defaults_to_yolo_and_round_trips_lowercase() {
        assert_eq!(ApprovalMode::default(), ApprovalMode::Yolo);

        let settings: Settings = serde_json::from_str(r#"{"approvalMode":"write"}"#)
            .expect("deserialize approval mode");
        assert_eq!(settings.approval_mode, Some(ApprovalMode::Write));
        let encoded = serde_json::to_value(&settings).expect("serialize settings");
        assert_eq!(encoded["approvalMode"], "write");
        assert!(serde_json::from_str::<Settings>(r#"{"approvalMode":"WRITE"}"#).is_err());
    }
    #[test]
    fn approval_mode_accessor_defaults_to_yolo() {
        assert_eq!(Settings::default().approval_mode(), ApprovalMode::Yolo);
        let settings = Settings {
            approval_mode: Some(ApprovalMode::Ask),
            ..Settings::default()
        };
        assert_eq!(settings.approval_mode(), ApprovalMode::Ask);
    }

    #[test]
    fn project_approval_mode_cannot_weaken_global_policy() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        fs::write(agent.path().join("settings.json"), r#"{"approvalMode":"ask"}"#)
            .expect("global settings");
        fs::create_dir_all(cwd.path().join(CONFIG_DIR_NAME)).expect("project settings dir");
        fs::write(
            cwd.path().join(CONFIG_DIR_NAME).join("settings.json"),
            r#"{"approvalMode":"yolo"}"#,
        )
        .expect("project settings");

        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("global");
        assert_eq!(manager.settings().approval_mode, Some(ApprovalMode::Ask));
        let error = manager.load_project(true).expect_err("project approval mode rejected");
        assert!(error.to_string().contains("approvalMode"), "{error:#}");
        assert_eq!(manager.settings().approval_mode, Some(ApprovalMode::Ask));
    }

    #[test]
    fn approval_mode_merge_and_persistence_remain_typed() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("settings");
        manager
            .update_global(|settings| settings.approval_mode = Some(ApprovalMode::Write))
            .expect("persist approval mode");
        let reloaded = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("reload");
        assert_eq!(reloaded.settings().approval_mode, Some(ApprovalMode::Write));

        let mut override_settings = Settings::default();
        override_settings.approval_mode = Some(ApprovalMode::Ask);
        assert_eq!(
            reloaded.settings().merged(&override_settings).approval_mode,
            Some(ApprovalMode::Ask)
        );
    }

    #[test]
    fn session_dir_is_typed_and_project_scope_overrides_global() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        fs::write(
            agent.path().join("settings.json"),
            r#"{"sessionDir":"global-sessions"}"#,
        )
        .expect("global settings");
        fs::create_dir_all(cwd.path().join(CONFIG_DIR_NAME)).expect("project settings dir");
        fs::write(
            cwd.path().join(CONFIG_DIR_NAME).join("settings.json"),
            r#"{"sessionDir":"project-sessions"}"#,
        )
        .expect("project settings");

        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("settings");
        assert_eq!(manager.settings().session_dir.as_deref(), Some(Path::new("global-sessions")));
        manager.load_project(true).expect("trusted project settings");
        assert_eq!(manager.settings().session_dir.as_deref(), Some(Path::new("project-sessions")));
    }

    #[test]
    fn session_import_sources_absent_empty_and_empty_override_are_native_only() {
        assert_eq!(
            Settings::default().effective_session_import_sources(),
            vec![SessionSourceKind::NativePi]
        );
        let absent = serde_json::to_value(Settings::default()).expect("serialize absent sources");
        assert!(absent.get("sessionImportSources").is_none());

        let empty: Settings = serde_json::from_str(r#"{"sessionImportSources":[]}"#)
            .expect("deserialize empty session import sources");
        assert_eq!(empty.session_import_sources, Some(Vec::new()));
        assert_eq!(
            empty.effective_session_import_sources(),
            vec![SessionSourceKind::NativePi]
        );
        let encoded_empty = serde_json::to_value(&empty).expect("serialize empty sources");
        assert_eq!(encoded_empty["sessionImportSources"], serde_json::json!([]));

        let configured: Settings =
            serde_json::from_str(r#"{"sessionImportSources":["codex"]}"#)
                .expect("deserialize configured source");
        let merged = configured.merged(&empty);
        assert_eq!(merged.session_import_sources, Some(Vec::new()));
        assert_eq!(
            merged.effective_session_import_sources(),
            vec![SessionSourceKind::NativePi]
        );
    }

    #[test]
    fn session_import_sources_allowed_union_round_trips_camel_case() {
        let settings: Settings = serde_json::from_str(
            r#"{"sessionImportSources":["omp","codex","claude","grok","droid"]}"#,
        )
        .expect("deserialize allowed session import sources");
        validate_settings(&settings, SettingsScope::Global, Path::new("settings.json"))
            .expect("allowed sources validate");
        assert_eq!(
            settings.effective_session_import_sources(),
            vec![
                SessionSourceKind::NativePi,
                SessionSourceKind::Omp,
                SessionSourceKind::Codex,
                SessionSourceKind::Claude,
                SessionSourceKind::Grok,
                SessionSourceKind::Droid,
            ]
        );

        let encoded = serde_json::to_value(&settings).expect("serialize settings");
        assert_eq!(
            encoded["sessionImportSources"],
            serde_json::json!(["omp", "codex", "claude", "grok", "droid"])
        );
        assert!(encoded.get("session_import_sources").is_none());
    }

    #[test]
    fn session_import_sources_reject_invalid_alias_and_duplicate_values() {
        assert!(
            serde_json::from_str::<Settings>(r#"{"sessionImportSources":["codex",1]}"#)
                .is_err(),
            "sessionImportSources must deserialize as a string list"
        );

        for (sources, expected) in [
            (vec![""], "unsupported source"),
            (vec!["   "], "unsupported source"),
            (vec!["pi"], "unsupported source"),
            (vec!["native"], "unsupported source"),
            (vec!["hyper"], "unsupported source"),
            (vec!["grok/hyper"], "unsupported source"),
            (vec!["OMP"], "unsupported source"),
            (vec!["unknown"], "unsupported source"),
            (vec!["omp", "omp"], "duplicate source"),
        ] {
            let settings = Settings {
                session_import_sources: Some(
                    sources.into_iter().map(str::to_owned).collect(),
                ),
                ..Settings::default()
            };
            let error = validate_settings(
                &settings,
                SettingsScope::Global,
                Path::new("settings.json"),
            )
            .expect_err("invalid session import sources must fail");
            let error = format!("{error:#}");
            assert!(error.contains("sessionImportSources"), "{error}");
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn agent_runtime_settings_persist_atomically() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("settings manager");
        manager
            .update_global(|settings| {
                settings.set_agent_settings(
                    "reviewer",
                    AgentRuntimeSettings {
                        enabled: Some(false),
                        model: Some("anthropic/claude-sonnet-4-5".to_owned()),
                tools: None,
            },
                );
            })
            .expect("persist agent settings");

        let reloaded = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("reload");
        let entry = reloaded
            .settings()
            .agent_settings("reviewer")
            .cloned()
            .expect("reviewer settings");
        assert_eq!(entry.enabled, Some(false));
        assert_eq!(entry.model.as_deref(), Some("anthropic/claude-sonnet-4-5"));
        assert!(!reloaded.settings().is_agent_enabled("reviewer"));
        assert!(reloaded.settings().is_agent_enabled("task"));
        assert_eq!(
            reloaded.settings().agent_model_override("reviewer"),
            Some("anthropic/claude-sonnet-4-5")
        );
    }

    #[test]
    fn empty_agent_model_override_is_clear_tombstone() {
        let mut settings = Settings::default();
        settings.agents.insert(
            "task".to_owned(),
            AgentRuntimeSettings {
                enabled: Some(true),
                model: Some("   ".to_owned()),
                tools: None,
            },
        );
        validate_operational_settings(&settings).expect("empty model is a clear tombstone");
        assert!(settings.agents["task"].clears_model());
        assert!(settings.agents["task"].model_override().is_none());
        assert!(!settings.agents["task"].is_default());
    }

    #[test]
    fn global_model_clear_tombstone_shadows_project_model() {
        let mut global = Settings::default();
        global.agents.insert(
            "task".to_owned(),
            AgentRuntimeSettings {
                enabled: Some(true),
                model: Some(String::new()),
                tools: None,
            },
        );
        let mut project = Settings::default();
        project.agents.insert(
            "task".to_owned(),
            AgentRuntimeSettings {
                enabled: None,
                model: Some("anthropic/claude-sonnet-4-5".to_owned()),
                tools: None,
            },
        );
        let effective = global.merged(&project);
        let entry = effective.agent_settings("task").expect("task settings");
        assert!(
            entry.clears_model(),
            "global clear must remain effective after project model merge: {entry:?}"
        );
        assert!(entry.model_override().is_none());
        assert_eq!(entry.enabled, Some(true));
    }

    #[test]
    fn project_enabled_override_still_merges_with_global_model() {
        let mut global = Settings::default();
        global.agents.insert(
            "task".to_owned(),
            AgentRuntimeSettings {
                enabled: Some(true),
                model: Some("openai/gpt-4.1".to_owned()),
                tools: None,
            },
        );
        let mut project = Settings::default();
        project.agents.insert(
            "task".to_owned(),
            AgentRuntimeSettings {
                enabled: Some(false),
                model: None,
                tools: None,
            },
        );
        let effective = global.merged(&project);
        let entry = effective.agent_settings("task").expect("task settings");
        assert_eq!(entry.enabled, Some(false));
        assert_eq!(entry.model.as_deref(), Some("openai/gpt-4.1"));
    }

    #[test]
    fn untrusted_project_agent_write_is_refused() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("settings manager");
        let error = manager
            .update_project(|settings| {
                settings.set_agent_settings(
                    "task",
                    AgentRuntimeSettings {
                        enabled: Some(false),
                        model: None,
                tools: None,
            },
                );
            })
            .expect_err("untrusted project write must fail")
            .to_string();
        assert!(error.contains("not trusted"), "{error}");
    }

    #[test]
    fn legacy_agent_overrides_merge_enabled_model_and_tools() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            r#"{
              "theme": "keep-me",
              "subagents": {
                "agentOverrides": {
                  "reviewer": {
                    "enabled": false,
                    "model": "anthropic/claude-sonnet-4-5",
                    "tools": ["read", "grep"],
                    "futureFlag": true
                  }
                },
                "otherLegacy": {"keep": true}
              }
            }"#,
        )
        .expect("write legacy settings");

        let loaded = load_settings_file(&path, SettingsScope::Global).expect("load");
        let entry = loaded.agent_settings("reviewer").expect("migrated reviewer");
        assert_eq!(entry.enabled, Some(false));
        assert_eq!(entry.model.as_deref(), Some("anthropic/claude-sonnet-4-5"));
        assert_eq!(
            entry.tools.as_deref(),
            Some(["read".to_owned(), "grep".to_owned()].as_slice())
        );
        let subagents = loaded.extra.get("subagents").expect("subagents retained");
        assert!(subagents.get("agentOverrides").is_none());
        assert_eq!(
            subagents.get("otherLegacy").and_then(|value| value.get("keep")),
            Some(&Value::Bool(true))
        );
        assert_eq!(loaded.theme.as_deref(), Some("keep-me"));
    }

    #[test]
    fn canonical_agents_win_conflicts_over_legacy_overrides() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            r#"{
              "agents": {
                "reviewer": {"enabled": true, "model": "openai/gpt-4.1"}
              },
              "subagents": {
                "agentOverrides": {
                  "reviewer": {
                    "enabled": false,
                    "model": "anthropic/claude-sonnet-4-5",
                    "tools": ["bash"]
                  },
                  "task": {"enabled": false, "tools": ["read"]}
                }
              }
            }"#,
        )
        .expect("write conflict settings");

        let loaded = load_settings_file(&path, SettingsScope::Global).expect("load");
        let reviewer = loaded.agent_settings("reviewer").expect("reviewer");
        assert_eq!(reviewer.enabled, Some(true), "canonical enabled must win");
        assert_eq!(reviewer.model.as_deref(), Some("openai/gpt-4.1"));
        assert!(
            reviewer.tools.is_none(),
            "legacy tools must not override canonical absence"
        );

        let task = loaded.agent_settings("task").expect("legacy-only task merged");
        assert_eq!(task.enabled, Some(false));
        assert_eq!(
            task.tools.as_deref(),
            Some(["read".to_owned()].as_slice())
        );
    }

    #[test]
    fn legacy_agent_overrides_rewrite_on_save_is_canonical() {
        let agent = tempfile::tempdir().expect("agent");
        let cwd = tempfile::tempdir().expect("cwd");
        let path = agent.path().join("settings.json");
        fs::write(
            &path,
            r#"{
              "subagents": {
                "agentOverrides": {
                  "reviewer": {
                    "enabled": false,
                    "model": "anthropic/claude-sonnet-4-5"
                  }
                },
                "keepMe": 1
              }
            }"#,
        )
        .expect("seed legacy");

        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("load");
        assert!(!manager.settings().is_agent_enabled("reviewer"));
        assert_eq!(
            manager.settings().agent_model_override("reviewer"),
            Some("anthropic/claude-sonnet-4-5")
        );

        manager
            .update_global(|settings| {
                settings.quiet_startup = Some(true);
            })
            .expect("save");

        let raw = fs::read_to_string(&path).expect("read rewritten");
        assert!(
            !raw.contains("agentOverrides"),
            "legacy key must be gone after save: {raw}"
        );
        assert!(raw.contains("\"agents\""), "canonical agents present: {raw}");
        assert!(raw.contains("\"reviewer\""), "migrated agent persisted: {raw}");
        assert!(
            raw.contains("\"keepMe\""),
            "unknown subagents fields preserved: {raw}"
        );

        let reloaded = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("reload");
        assert!(!reloaded.settings().is_agent_enabled("reviewer"));
        assert_eq!(
            reloaded.settings().agent_model_override("reviewer"),
            Some("anthropic/claude-sonnet-4-5")
        );
    }

    #[test]
    fn legacy_agent_overrides_warning_is_deduped_per_path() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            r#"{
              "subagents": {
                "agentOverrides": {
                  "reviewer": {
                    "enabled": false,
                    "model": "anthropic/claude-sonnet-4-5"
                  }
                }
              }
            }"#,
        )
        .expect("write legacy settings");

        let (loaded, warnings) = capture_legacy_agent_migration_warnings(|| {
            let first = load_settings_file(&path, SettingsScope::Global).expect("load 1");
            let second = load_settings_file(&path, SettingsScope::Global).expect("load 2");
            let third = load_settings_file(&path, SettingsScope::Global).expect("load 3");
            (first, second, third)
        });
        let (first, second, third) = loaded;

        assert_eq!(warnings.len(), 1, "same path warns once: {warnings:?}");
        assert!(
            warnings[0].contains("deprecated subagents.agentOverrides"),
            "{warnings:?}"
        );
        assert!(
            warnings[0].contains(&path.display().to_string()),
            "warning identifies settings path: {warnings:?}"
        );
        assert_eq!(first.agent_settings("reviewer"), second.agent_settings("reviewer"));
        assert_eq!(second.agent_settings("reviewer"), third.agent_settings("reviewer"));
        assert_eq!(
            first.agent_settings("reviewer").and_then(|entry| entry.model.clone()),
            Some("anthropic/claude-sonnet-4-5".to_owned())
        );
    }

    #[test]
    fn legacy_agent_overrides_warning_is_separate_per_settings_path() {
        let dir = tempfile::tempdir().expect("dir");
        let global = dir.path().join("global-settings.json");
        let project = dir.path().join("project-settings.json");
        let body = r#"{
          "subagents": {
            "agentOverrides": {
              "reviewer": {"enabled": false}
            }
          }
        }"#;
        fs::write(&global, body).expect("write global");
        fs::write(&project, body).expect("write project");

        let (_, warnings) = capture_legacy_agent_migration_warnings(|| {
            load_settings_file(&global, SettingsScope::Global).expect("global");
            load_settings_file(&project, SettingsScope::Project).expect("project");
            load_settings_file(&global, SettingsScope::Global).expect("global reload");
            load_settings_file(&project, SettingsScope::Project).expect("project reload");
        });

        assert_eq!(
            warnings.len(),
            2,
            "each distinct settings path warns once: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains(&global.display().to_string())),
            "global path warned: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains(&project.display().to_string())),
            "project path warned: {warnings:?}"
        );
    }

    #[test]
    fn unsupported_subagents_agent_keys_warn_once_per_path_key() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            r#"{
              "subagents": {
                "agents": {
                  "reviewer": {"enabled": false, "model": "openai/gpt-4.1"}
                },
                "models": {"reviewer": "anthropic/claude-sonnet-4-5"},
                "keepMe": {"x": 1}
              }
            }"#,
        )
        .expect("write unsupported nested agents");

        let (loaded, warnings) = capture_settings_diagnostics(|| {
            let first = load_settings_file(&path, SettingsScope::Global).expect("load 1");
            let second = load_settings_file(&path, SettingsScope::Global).expect("load 2");
            (first, second)
        });
        let (first, second) = loaded;

        assert_eq!(
            warnings.len(),
            2,
            "each unsupported key warns once: {warnings:?}"
        );
        assert!(
            warnings.iter().any(|warning| {
                warning.contains(&path.display().to_string())
                    && warning.contains("unsupported subagents.agents")
                    && warning.contains("top-level agents.<name>.{enabled,model,tools}")
            }),
            "agents key warned: {warnings:?}"
        );
        assert!(
            warnings.iter().any(|warning| {
                warning.contains(&path.display().to_string())
                    && warning.contains("unsupported subagents.models")
            }),
            "models key warned: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .all(|warning| !warning.contains("subagents.keepMe")),
            "arbitrary metadata must stay silent: {warnings:?}"
        );
        assert!(first.agents.is_empty(), "ignored nested agents must not migrate");
        assert_eq!(
            first.extra.get("subagents").and_then(|value| value.get("agents")),
            second.extra.get("subagents").and_then(|value| value.get("agents")),
            "unsupported fields remain in extra"
        );
        assert_eq!(
            first
                .extra
                .get("subagents")
                .and_then(|value| value.get("keepMe"))
                .and_then(|value| value.get("x")),
            Some(&Value::Number(1.into()))
        );
    }

    #[test]
    fn unsupported_subagents_agentish_maps_warn_and_preserve_unknowns_on_save() {
        let agent = tempfile::tempdir().expect("agent");
        let cwd = tempfile::tempdir().expect("cwd");
        let path = agent.path().join("settings.json");
        fs::write(
            &path,
            r#"{
              "subagents": {
                "agentModels": {"reviewer": "openai/gpt-4.1"},
                "tools": ["read"],
                "defaults": {"enabled": true},
                "reviewer": {"enabled": false, "tools": ["grep"]},
                "otherLegacy": {"keep": true}
              }
            }"#,
        )
        .expect("seed unsupported nested config");

        let (manager, warnings) = capture_settings_diagnostics(|| {
            SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("load")
        });

        let warned_keys = ["agentModels", "tools", "defaults", "reviewer"];
        for key in warned_keys {
            assert!(
                warnings.iter().any(|warning| {
                    warning.contains(&format!("unsupported subagents.{key}"))
                        && warning.contains("this setting is ignored")
                        && warning.contains("top-level agents")
                }),
                "expected warning for subagents.{key}: {warnings:?}"
            );
        }
        assert!(
            warnings
                .iter()
                .all(|warning| !warning.contains("subagents.otherLegacy")),
            "non-agent metadata must not warn: {warnings:?}"
        );
        assert_eq!(
            warnings.len(),
            warned_keys.len(),
            "exactly one warning per unsupported key: {warnings:?}"
        );
        assert!(manager.settings().agents.is_empty());

        manager
            .update_global(|settings| {
                settings.quiet_startup = Some(true);
            })
            .expect("save");

        let raw = fs::read_to_string(&path).expect("read rewritten");
        for key in ["agentModels", "tools", "defaults", "reviewer", "otherLegacy"] {
            assert!(
                raw.contains(&format!("\"{key}\"")),
                "canonical save must preserve unknown subagents field {key}: {raw}"
            );
        }
        assert!(
            !raw.contains("agentOverrides"),
            "no accidental agentOverrides injection: {raw}"
        );

        let (_, reload_warnings) = capture_settings_diagnostics(|| {
            SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("reload")
        });
        assert!(
            reload_warnings.is_empty(),
            "same path+key must not warn again after reload: {reload_warnings:?}"
        );
    }

    #[test]
    fn plain_subagents_metadata_does_not_warn() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            r#"{
              "subagents": {
                "keepMe": 1,
                "otherLegacy": {"keep": true},
                "note": "freeform"
              }
            }"#,
        )
        .expect("write metadata-only subagents");

        let (loaded, warnings) = capture_settings_diagnostics(|| {
            load_settings_file(&path, SettingsScope::Global).expect("load")
        });
        assert!(
            warnings.is_empty(),
            "non-agent metadata must stay silent: {warnings:?}"
        );
        let subagents = loaded.extra.get("subagents").expect("subagents retained");
        assert_eq!(subagents.get("keepMe"), Some(&Value::Number(1.into())));
        assert_eq!(
            subagents
                .get("otherLegacy")
                .and_then(|value| value.get("keep")),
            Some(&Value::Bool(true))
        );
    }



    #[test]
    fn settings_file_size_boundary_is_accepted() {
        let dir = tempfile::tempdir().expect("settings dir");
        let path = dir.path().join("settings.json");
        let padding = MAX_SETTINGS_FILE_BYTES as usize - 8;
        let content = format!("{{\"x\":\"{}\"}}", " ".repeat(padding));
        assert_eq!(content.len() as u64, MAX_SETTINGS_FILE_BYTES);
        fs::write(&path, content).expect("write boundary settings");
        load_settings_file(&path, SettingsScope::Global).expect("boundary settings accepted");
    }

    #[test]
    fn oversized_settings_file_is_rejected_before_parse() {
        let dir = tempfile::tempdir().expect("settings dir");
        let path = dir.path().join("settings.json");
        let file = File::create(&path).expect("create settings");
        file.set_len(MAX_SETTINGS_FILE_BYTES + 1).expect("extend settings");
        let error = load_settings_file(&path, SettingsScope::Global)
            .expect_err("oversized settings must fail")
            .to_string();
        assert!(error.contains("exceeds maximum size"), "{error}");
        assert!(error.contains(&path.display().to_string()), "{error}");
    }

}
