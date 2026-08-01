//! Two-phase global/project settings loading with atomic scoped persistence.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use parking_lot::RwLock;
use pi_agent::{QueueMode, ThinkingLevel};
use pi_ai::{CacheRetention, SimpleStreamOptions, ThinkingBudgets, Transport};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::resources::{CONFIG_DIR_NAME, agent_dir_path};

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
    pub max_concurrency: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_recursion_depth: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mailbox_capacity: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tools_per_agent: Option<usize>,
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
    pub fn merged(&self, overrides: &Self) -> Self {
        let base = serde_json::to_value(self).unwrap_or_else(|_| Value::Object(Map::new()));
        let overlay = serde_json::to_value(overrides).unwrap_or_else(|_| Value::Object(Map::new()));
        serde_json::from_value(deep_merge(base, overlay)).unwrap_or_else(|_| self.clone())
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
    let settings: Settings = serde_json::from_str(&content).with_context(|| {
        format!("Failed to parse settings.json\nFile: {}", path.display())
    })?;
    validate_settings(&settings, scope, path)?;
    Ok(settings)
}

fn validate_settings(settings: &Settings, scope: SettingsScope, path: &Path) -> Result<()> {
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
