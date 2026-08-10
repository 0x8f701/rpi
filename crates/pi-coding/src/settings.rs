//! Two-phase global/project settings loading with atomic scoped persistence.
//!
//! # File selection and format
//!
//! Each scope's settings file is chosen deterministically by name — never by
//! content sniffing: a `settings.toml` sitting next to the canonical
//! `settings.json` wins when present; otherwise the canonical JSON file is
//! used. A `.toml` extension selects TOML parsing/serialization; any other
//! extension (or none) selects JSON. TOML files round-trip through the typed
//! [`Settings`] struct, with unknown fields retained in the `extra` maps, and
//! settings writes target whichever file the loader would read.
//!
//! # Environment expansion
//!
//! `$NAME` and `${NAME}` references in every string value of the settings
//! document are replaced with the matching process-environment value,
//! recursively through nested objects and arrays (including retained unknown
//! fields). Expansion applies **only when projecting the effective runtime
//! view** ([`SettingsManager::settings`], consumed by sessions): the
//! persisted layers ([`SettingsManager::global_settings`] /
//! [`SettingsManager::project_settings`]) keep the pre-expansion document
//! literal, so writing settings re-serializes the raw references and never
//! persists expanded secrets (e.g. `mcpServers[].env` tokens) into the
//! on-disk `settings.json`. Names that are not set are left verbatim
//! (fail-open) and reported once per name through the settings diagnostic
//! channel. Keys and non-string values are never expanded. One consequence:
//! a value read through the persistence view (for example the `authScope`
//! label read by `auth::settings_auth_scope`) carries the literal
//! reference rather than the expanded value; runtime surfaces that need the
//! expanded value read [`SettingsManager::settings`]. Settings writes are
//! atomic (temp-file-in-same-dir + rename) with owner-only `0600`
//! permissions.

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
    /// Recent user turns retained verbatim by `/compact --snap` (default 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snap_keep_turns: Option<i64>,
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
    /// Model spec (`provider/id` or bare id) used by the `generate_image` tool
    /// instead of the active chat model. The resolved model must declare
    /// `imageGeneration: true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gen_model: Option<String>,
    /// Base URL of a user-configured OpenAI-compatible image endpoint
    /// (`POST {base}/images/generations`, e.g. a self-hosted server). Overrides
    /// the resolved model's `baseUrl` for the `generate_image` tool. No
    /// provider default is assumed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gen_base_url: Option<String>,
    /// Bearer API key for `images.genBaseUrl` (or the resolved model's
    /// provider when `genBaseUrl` is absent). Secret: marked in the settings
    /// catalog so the settings RPC/TUI redact it and refuse draft writes; the
    /// raw value lives only in the settings file. Never logged or rendered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gen_api_key: Option<String>,
}

/// Effective image-generation configuration for the `generate_image` tool
/// after defaults are applied. The API key is a plain field here because the
/// tool needs it for the bearer header, but [`Debug`] redacts it so no derived
/// logging/diagnostics surface leaks it.
#[derive(Clone, PartialEq, Eq)]
pub struct ImageGenRuntimeSettings {
    pub gen_model: Option<String>,
    pub gen_base_url: String,
    pub gen_api_key: String,
}

impl std::fmt::Debug for ImageGenRuntimeSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ImageGenRuntimeSettings")
            .field("gen_model", &self.gen_model)
            .field("gen_base_url", &self.gen_base_url)
            .field("gen_api_key", &"[REDACTED]")
            .finish()
    }
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

/// User-configurable soft budget for orchestration child jobs, mirroring
/// [`crate::orchestration::JobSoftBudget`]. All knobs optional and unlimited
/// by default; a configured limit makes a child yield after its current turn
/// with the `soft_budget_exhausted` marker instead of running to completion.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SoftBudgetSettings {
    /// Maximum assistant requests (LLM turns) a child job may make before it
    /// yields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_requests: Option<usize>,
    /// Maximum cumulative tokens a child job may consume before it yields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Yield-driving: return control to the parent after this many requests,
    /// regardless of any remaining budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yield_after: Option<usize>,
}

/// Opt-in Linux filesystem sandbox for the bash tool (`settings.sandbox`).
///
/// The sandbox confines bash commands to the configured allowed paths using
/// `unshare` mount/pid/net namespaces (network is loopback-only unless
/// `network` is true). It is same-user confinement, not isolation: the command
/// runs with the caller's uid and can write to the bind-mounted allowed paths.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxSettings {
    /// Run bash in the filesystem sandbox by default. The per-call
    /// `sandboxed` bash parameter overrides this for a single command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Share the host network inside the sandbox. Default (false): fresh
    /// network namespace with loopback only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<bool>,
    /// Mount the allowed paths read-only inside the sandbox (bind mounts with
    /// `MS_RDONLY`). Default (false): allowed paths stay writable. When true
    /// the deny-by-default filesystem still applies and every allowed path is
    /// read-only, while the sandbox's private HOME/TMPDIR remain writable so
    /// commands can cache and create temp files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    /// Paths visible inside the sandbox (bind-mounted read-write). Defaults to
    /// the session working directory plus the agent directory. Relative paths
    /// resolve from the session working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_paths: Option<Vec<String>>,
    /// Paths hidden inside the sandbox (empty overlay), even when nested under
    /// an allowed path. Relative paths resolve from the session working
    /// directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied_paths: Option<Vec<String>>,
    /// Unknown fields are preserved verbatim (round-trips through drafts).
    #[serde(default, flatten)]
    pub extra: Map<String, Value>,
}

/// Workflow/subagent isolation backend (`settings.orchestration.isolation`).
///
/// `worktree` (default) isolates each workflow in a git worktree; `overlayfs`
/// uses an overlay (kernel overlay → fuse-overlayfs → recursive-copy fallback)
/// with the source repo as the read-only lower layer and a private writable
/// upper layer; `none` disables isolation and workflows operate directly on
/// the source working tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowIsolationSetting {
    #[default]
    Worktree,
    Overlayfs,
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
    /// Run orchestration subagent children in the Linux filesystem sandbox
    /// (opt-in; default off). When true, every process a child spawns (its
    /// bash tool) is confined to the workspace, the agent directory, and
    /// `sandbox.allowedPaths`, with the same deny-by-default filesystem,
    /// fail-closed validation, and loopback-only network (unless
    /// `sandbox.network` is true) as the bash sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandboxed: Option<bool>,
    /// Workflow isolation backend used instead of a git worktree. Default
    /// `worktree`; `overlayfs` uses an overlay (source repo as the read-only
    /// lower layer) with a private writable upper layer; `none` runs workflows
    /// directly on the source working tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<WorkflowIsolationSetting>,
    /// Soft budget applied to orchestration child jobs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soft_budget: Option<SoftBudgetSettings>,
    /// Preferred agent/persona name for unnamed task spawns (`/role` or
    /// `/persona` select). Applied when a new orchestration runtime is built;
    /// a missing or disabled selection falls back to ranked/default behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_agent: Option<String>,
}

/// Memory backend selection (`settings.memory.backend`). `local` is the
/// built-in JSONL store exposed through the `memory` tool; `hindsight` swaps
/// the memory tools for the HTTP-backed `recall`/`retain`/`reflect` family;
/// `off` hides every memory tool.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryBackend {
    /// No memory tools are exposed.
    Off,
    /// Built-in JSONL store (`memory` tool: learn/recall/list/forget).
    #[default]
    Local,
    /// External Hindsight HTTP API (`recall`/`retain`/`reflect` tools).
    Hindsight,
}

/// Hindsight bank scoping policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HindsightScoping {
    Global,
    PerProject,
    #[default]
    PerProjectTagged,
}

/// Hindsight recall/reflect reasoning budget.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HindsightBudget {
    Low,
    #[default]
    Mid,
    High,
}

/// Selectable memory backend configuration (`settings.memory`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemorySettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<MemoryBackend>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hindsight_api_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hindsight_api_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hindsight_allow_insecure: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hindsight_bank_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hindsight_bank_id_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hindsight_scoping: Option<HindsightScoping>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hindsight_bank_mission: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hindsight_retain_mission: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hindsight_injection: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hindsight_recall_budget: Option<HindsightBudget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hindsight_recall_max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hindsight_recall_types: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hindsight_request_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hindsight_recall_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hindsight_retain_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hindsight_reflect_timeout_ms: Option<u64>,
}

/// Hold-to-talk realtime voice (`/live`) configuration persisted under
/// `settings.live`. The STT endpoint and API key are fully user-configured —
/// this product never hardcodes an OpenAI (or any other) default endpoint.
/// The endpoint must be an OpenAI-compatible `POST {base}/v1/audio/
/// transcriptions` service (e.g. a self-hosted whisper server); TLS is
/// mandatory unless [`Self::allow_insecure`] is explicitly set.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveSettings {
    /// Master switch for `/live`. Off by default; `/live` refuses to arm
    /// until this is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Voice mode: `"stt"` (simple STT via whisper-compatible endpoint) or
    /// `"realtime"` (Codex Live via CLIProxyAPI's /v1/live + /v1/realtime/calls).
    /// Default `"stt"` for backward compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Base URL of the OpenAI-compatible STT service. Empty until configured.
    /// `https://` is required unless `allowInsecure` is true; `ws://`/`wss://`
    /// URLs are always rejected (the client speaks HTTP multipart).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stt_base_url: Option<String>,
    /// Bearer token for the STT endpoint. Secret: marked in the settings
    /// catalog so the settings RPC/TUI redact it and refuse draft writes; the
    /// raw value lives only in the settings file. Never logged or rendered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stt_api_key: Option<String>,
    /// Model label sent as the multipart `model` field. `"whisper-1"` is a
    /// fallback label only — it is never resolved against OpenAI by this
    /// product; the configured base URL serves it or rejects it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stt_model: Option<String>,
    /// Base URL for the realtime (Codex Live) endpoint, typically
    /// CLIProxyAPI's address (e.g. `http://localhost:8317`). Used when
    /// `mode` is `"realtime"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realtime_base_url: Option<String>,
    /// API key for the realtime endpoint (CLIProxyAPI access key). Secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realtime_api_key: Option<String>,
    /// Realtime model label (e.g. `"gpt-realtime-1.5"`). Used when `mode` is
    /// `"realtime"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realtime_model: Option<String>,
    /// Voice for the realtime session (e.g. `"sol"`). Used when `mode` is
    /// `"realtime"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// Optional BCP-47 language hint sent as the multipart `language` field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Explicitly allow `http://` STT endpoints (loopback self-hosted
    /// whisper servers). Default false: plaintext bearer credentials are
    /// rejected with an actionable error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_insecure: Option<bool>,
}

/// Effective [`LiveSettings`] after defaults are applied. The API key is a
/// plain field here because the runtime needs it for the bearer header, but
/// [`Debug`] redacts it and serialization skips it (`#[serde(skip)]`), so
/// neither derived logging nor the `get_state` RPC response surfaces it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveRuntimeSettings {
    pub enabled: bool,
    pub mode: String,
    pub stt_base_url: String,
    #[serde(skip)]
    pub stt_api_key: String,
    pub stt_model: String,
    pub realtime_base_url: String,
    #[serde(skip)]
    pub realtime_api_key: String,
    pub realtime_model: String,
    pub voice: String,
    pub language: Option<String>,
    pub allow_insecure: bool,
}

impl std::fmt::Debug for LiveRuntimeSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LiveRuntimeSettings")
            .field("enabled", &self.enabled)
            .field("mode", &self.mode)
            .field("stt_base_url", &self.stt_base_url)
            .field("stt_api_key", &"[REDACTED]")
            .field("stt_model", &self.stt_model)
            .field("realtime_base_url", &self.realtime_base_url)
            .field("realtime_api_key", &"[REDACTED]")
            .field("realtime_model", &self.realtime_model)
            .field("voice", &self.voice)
            .field("language", &self.language)
            .field("allow_insecure", &self.allow_insecure)
            .finish()
    }
}

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
    /// Effective hold-to-talk voice configuration for `/live`. The STT
    /// endpoint/key are user-configured; Debug output redacts the key.
    pub live: LiveRuntimeSettings,
    pub expose_session_environment: bool,
    pub process_tool_enabled: bool,
    pub glob_tool_enabled: bool,
    pub orchestration_enabled: bool,
    pub todo_tool_enabled: bool,
    pub orchestration_max_concurrency: usize,
    pub orchestration_max_recursion_depth: usize,
    pub orchestration_mailbox_capacity: usize,
    pub orchestration_max_tools_per_agent: usize,
    /// Soft budget carried into [`crate::OrchestrationConfig::soft_budget`]
    /// when the orchestration runtime is built. Unlimited by default.
    pub orchestration_soft_budget: crate::JobSoftBudget,
    /// When true, orchestration subagent children run their process spawns
    /// inside the filesystem sandbox (workspace + agent dir +
    /// `sandbox.allowedPaths` visible; deny-by-default otherwise).
    pub orchestration_sandboxed: bool,
    /// Workflow isolation backend (`settings.orchestration.isolation`):
    /// `worktree` (default), `overlayfs`, or `none`.
    pub orchestration_isolation: crate::WorkflowIsolationSetting,
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
    pub compaction_snap_keep_turns: i64,
    pub transport: Transport,
    pub timeout_ms: Option<u64>,
    pub websocket_connect_timeout_ms: Option<u64>,
    pub provider_max_retries: usize,
    pub max_retry_delay_ms: Option<u64>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<i64>,
    pub cache_retention: CacheRetention,
    pub responses_stateful_chain: bool,
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
    pub orchestration_soft_budget: crate::JobSoftBudget,
    pub orchestration_sandboxed: bool,
    pub orchestration_isolation: crate::WorkflowIsolationSetting,
    /// Effective hold-to-talk voice configuration (`Settings.live`), including
    /// the realtime (Codex Live) base URL/model/voice the web frontend needs
    /// to drive `realtime_create_call`/`realtime_create_session`. Secret keys
    /// are omitted from serialization; `Debug` redacts them.
    pub live: LiveRuntimeSettings,
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
            compaction_snap_keep_turns: self.compaction.snap_keep_turns,
            transport: self.stream_options.stream.transport,
            timeout_ms: self.stream_options.stream.timeout_ms,
            websocket_connect_timeout_ms: self.stream_options.stream.websocket_connect_timeout_ms,
            provider_max_retries: self.stream_options.stream.max_retries,
            max_retry_delay_ms: self.stream_options.stream.max_retry_delay_ms,
            temperature: self.stream_options.stream.temperature,
            max_tokens: self.stream_options.stream.max_tokens,
            cache_retention: self.stream_options.stream.cache_retention,
            responses_stateful_chain: self.stream_options.responses_stateful_chain,
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
            orchestration_soft_budget: self.orchestration_soft_budget,
            orchestration_sandboxed: self.orchestration_sandboxed,
            orchestration_isolation: self.orchestration_isolation,
            live: self.live.clone(),
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
    SupportedSetting { key: "sessionTtlDays", application: "startup session TTL pruning" },
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
    SupportedSetting { key: "responsesStatefulChain", application: "apply_session_options" },
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
    SupportedSetting { key: "live", application: "live_runtime (/live hold-to-talk STT client)" },
    SupportedSetting { key: "hooks", application: "session host hooks (session/turn/tool-call firing points)" },
    SupportedSetting { key: "permissionRules", application: "host approval hook path-level rule evaluation" },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    PreToolCall,
    PostToolCall,
    SessionStart,
    SessionEnd,
    TurnStart,
    TurnEnd,
    PreTrustDecision,
}

impl HookEvent {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreToolCall => "pre_tool_call",
            Self::PostToolCall => "post_tool_call",
            Self::SessionStart => "session_start",
            Self::SessionEnd => "session_end",
            Self::TurnStart => "turn_start",
            Self::TurnEnd => "turn_end",
            Self::PreTrustDecision => "pre_trust_decision",
        }
    }
}

/// One ordered host-level hook entry.
///
/// Hooks are external commands run without a shell (`command[0]` is the
/// executable, the rest are argv). The event payload is written as JSON on
/// stdin; stdout (capped) is parsed as JSON for `pre_tool_call` and
/// `pre_trust_decision` decisions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookConfig {
    pub event: HookEvent,
    /// Exact or substring match on the event subject (tool name, message
    /// role, or canonical project path). Absent matchers fire for every
    /// subject of the event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    /// Command argv (no shell); must be non-empty.
    pub command: Vec<String>,
    /// Per-hook timeout in milliseconds. Defaults to 5000 and is capped at
    /// 60000; a timed-out hook's process group is killed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Set `false` to skip the entry without removing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Only meaningful for `pre_tool_call` and `pre_trust_decision`: when the
    /// hook errors or times out, fail closed (block the tool / deny the trust
    /// decision) instead of the default fail-open (allow).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_closed: Option<bool>,
    /// Unknown fields are retained so one product never destroys another's
    /// hook configuration during a settings round-trip.
    #[serde(default, flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionRuleAction {
    /// Allow without prompting, even when the capability-wide approval mode
    /// would ask.
    #[default]
    Allow,
    /// Force an interactive confirmation, even when the capability-wide
    /// approval mode would auto-allow.
    Ask,
    /// Block the tool call with an actionable message.
    Deny,
}

/// File-touching tool names addressable by a path-level permission rule.
///
/// `bash` is deliberately absent: its arbitrary commands cannot be reduced to
/// a reliable target path, so path rules never apply to it in the MVP. Bash
/// remains governed solely by the capability-wide approval mode.
///
/// `lsp` is addressable because its `rename` action applies a server-controlled
/// workspace edit to disk; the preflight in the lsp tool evaluates the same
/// rules against every workspace-edit target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionTool {
    Read,
    Write,
    Edit,
    Glob,
    Grep,
    Lsp,
}

impl PermissionTool {
    /// Map a tool name to its rule-addressable kind, if any.
    #[must_use]
    pub fn for_tool_name(name: &str) -> Option<Self> {
        match name {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "edit" => Some(Self::Edit),
            "glob" => Some(Self::Glob),
            "grep" => Some(Self::Grep),
            "lsp" => Some(Self::Lsp),
            _ => None,
        }
    }

    /// The rule-addressable tool name (wire form).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Edit => "edit",
            Self::Glob => "glob",
            Self::Grep => "grep",
            Self::Lsp => "lsp",
        }
    }
}

/// One path-level permission rule, evaluated before the capability-wide
/// approval mode for `read`/`write`/`edit`/`glob`/`grep` calls.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRule {
    /// What to do when the rule matches.
    pub action: PermissionRuleAction,
    /// Filesystem path or prefix to match. Relative paths resolve against the
    /// session working directory. A plain path matches the path itself and
    /// everything beneath it (component boundary); a trailing `*` is accepted
    /// as an explicit prefix marker with the same semantics. Matching is
    /// lexical: `.`, `..`, and duplicate separators are normalized but
    /// symlinks are not resolved.
    pub path: String,
    /// Optional tool allowlist; absent means all file-touching tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<PermissionTool>>,
    /// Unknown fields are retained so one product never destroys another's
    /// rule configuration during a settings round-trip.
    #[serde(default, flatten)]
    pub extra: Map<String, Value>,
}

/// Live source of path-level permission rules.
///
/// Consulted at execution time (per file-touching tool call), so
/// `permissionRules` changes apply without rebuilding tools (RELOAD apply
/// behavior in the settings catalog). The host approval hook and the `lsp`
/// tool's workspace-edit preflight consume the same source shape; the lsp
/// tool reads its source fresh on every call.
pub type PermissionRulesSource = Arc<dyn Fn() -> Vec<PermissionRule> + Send + Sync>;

/// A rules source that never matches; used where no path policy is configured.
#[must_use]
pub fn empty_permission_rules() -> PermissionRulesSource {
    Arc::new(Vec::new)
}

/// MCP transport: a child-process stdio server or an SSE HTTP endpoint
/// (Grok-compatible `transport` values).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    #[default]
    Stdio,
    Sse,
}

/// One configured MCP (Model Context Protocol) server, mirroring Grok's
/// `[mcp_servers.<name>]` shape: `transport` selects stdio (a child process
/// started from `command`/`args`/`env`) or sse (an http(s) endpoint at `url`).
///
/// `sse` entries parse and round-trip so Grok configs survive a settings
/// write, but the client transport in this build is stdio; `call`/`list_tools`
/// against an sse server report the limitation explicitly.
///
/// Disabling shape: a per-entry `disabled` boolean, matching Cursor's
/// `.cursor/mcp.json` entries. OMP's canonical shape is the inverse `enabled`
/// flag and Claude Desktop keeps a separate `disabledMCPServers` name list —
/// both are mapped onto this field by the config import (`mcp_import`). A
/// disabled server is never spawned: the registry filters it out at configure
/// time, so it has no session slot and never appears in `mcp list_servers`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConfig {
    /// Unique server name used by the `mcp` tool (`mcp call <name> ...`).
    pub name: String,
    /// Disabled servers are never spawned and are excluded from the registry
    /// and `mcp list_servers` output. Transport fields of a disabled entry are
    /// advisory: validation relaxes them, since the server never connects.
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
    #[serde(default)]
    pub transport: McpTransport,
    /// Executable for stdio servers, resolved from PATH; required for stdio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Command-line arguments for stdio servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// SSE endpoint URL for sse servers; required for sse and forbidden for
    /// stdio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Extra environment variables for the stdio child process. Never echoed
    /// back in tool output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    /// Unknown fields are retained so a Grok/Claude MCP config round-trips
    /// without data loss.
    #[serde(default, flatten)]
    pub extra: Map<String, Value>,
}

/// `skip_serializing_if` predicate for the [`McpServerConfig::disabled`] flag:
/// the field is omitted from settings files unless set.
fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionVerdict {
    /// No rule applies; the capability-wide approval mode decides.
    NoMatch,
    /// A matching rule allows the call without prompting.
    Allow,
    /// A matching rule forces interactive confirmation.
    Ask,
    /// A matching rule blocks the call; the payload is an actionable reason.
    Deny(String),
}

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
    /// Active credential scope used to select stored auth.json credentials.
    /// The `PI_AUTH_SCOPE` environment variable overrides this at runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_scope: Option<String>,
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
    /// Days after which an untouched native session file is pruned at startup.
    /// Missing falls back to [`crate::session_store::DEFAULT_SESSION_TTL_DAYS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_ttl_days: Option<u64>,
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
    pub responses_stateful_chain: Option<bool>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<Vec<HookConfig>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permission_rules: Vec<PermissionRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxSettings>,
    /// External memory backend configuration (`memory.backend`, `memory.hindsight*`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemorySettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<LiveSettings>,
    /// Model spec (provider/id or bare id) that describes images when the
    /// active chat model does not support image input. Prompts containing
    /// image blocks are delegated to this model and replaced with its text
    /// description before reaching the main model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision_model: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServerConfig>,
    #[serde(default, flatten)]
    pub extra: Map<String, Value>,
}

impl Settings {
    #[must_use]
    pub fn approval_mode(&self) -> ApprovalMode {
        self.approval_mode.unwrap_or_default()
    }

    /// Effective memory configuration with defaults applied. Missing
    /// `settings.memory` keeps the built-in `local` backend. Hindsight remains
    /// inert until an explicit API URL is configured.
    #[must_use]
    pub fn memory_config(&self) -> crate::MemoryConfig {
        let memory = self.memory.as_ref();
        crate::MemoryConfig {
            backend: memory.and_then(|value| value.backend).unwrap_or_default(),
            hindsight_api_url: memory.and_then(|value| value.hindsight_api_url.as_deref()).map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned),
            hindsight_api_token: memory.and_then(|value| value.hindsight_api_token.as_deref()).map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned),
            hindsight_allow_insecure: memory.and_then(|value| value.hindsight_allow_insecure).unwrap_or(false),
            hindsight_bank_id: memory.and_then(|value| value.hindsight_bank_id.as_deref()).map(str::trim).filter(|value| !value.is_empty()).unwrap_or(crate::memory::DEFAULT_HINDSIGHT_BANK_ID).to_owned(),
            hindsight_bank_id_prefix: memory.and_then(|value| value.hindsight_bank_id_prefix.as_deref()).map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned),
            hindsight_scoping: memory.and_then(|value| value.hindsight_scoping).unwrap_or_default(),
            hindsight_bank_mission: memory.and_then(|value| value.hindsight_bank_mission.as_deref()).map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned),
            hindsight_retain_mission: memory.and_then(|value| value.hindsight_retain_mission.as_deref()).map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned),
            hindsight_injection: memory.and_then(|value| value.hindsight_injection).unwrap_or(false),
            hindsight_recall_budget: memory.and_then(|value| value.hindsight_recall_budget).unwrap_or_default(),
            hindsight_recall_max_tokens: memory.and_then(|value| value.hindsight_recall_max_tokens).unwrap_or(crate::memory::DEFAULT_HINDSIGHT_RECALL_MAX_TOKENS),
            hindsight_recall_types: memory.and_then(|value| value.hindsight_recall_types.clone()).unwrap_or_else(|| crate::memory::DEFAULT_HINDSIGHT_RECALL_TYPES.iter().map(|value| (*value).to_owned()).collect()),
            hindsight_request_timeout_ms: memory.and_then(|value| value.hindsight_request_timeout_ms).unwrap_or(crate::memory::DEFAULT_HINDSIGHT_REQUEST_TIMEOUT_MS),
            hindsight_recall_timeout_ms: memory.and_then(|value| value.hindsight_recall_timeout_ms).unwrap_or(crate::memory::DEFAULT_HINDSIGHT_RECALL_TIMEOUT_MS),
            hindsight_retain_timeout_ms: memory.and_then(|value| value.hindsight_retain_timeout_ms).unwrap_or(crate::memory::DEFAULT_HINDSIGHT_RETAIN_TIMEOUT_MS),
            hindsight_reflect_timeout_ms: memory.and_then(|value| value.hindsight_reflect_timeout_ms).unwrap_or(crate::memory::DEFAULT_HINDSIGHT_REFLECT_TIMEOUT_MS),
        }
    }

    /// Path-level permission rules, in declaration order.
    #[must_use]
    pub fn permission_rules(&self) -> &[PermissionRule] {
        &self.permission_rules
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
                snap_keep_turns: compaction.snap_keep_turns.unwrap_or(defaults.snap_keep_turns),
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
        if let Some(value) = self.responses_stateful_chain {
            options.stream_options.responses_stateful_chain = value;
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
            live: self.live_runtime(),
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
            orchestration_soft_budget: orchestration
                .and_then(|settings| settings.soft_budget.as_ref())
                .map_or_else(crate::JobSoftBudget::default, |budget| crate::JobSoftBudget {
                    max_requests: budget.max_requests,
                    max_tokens: budget.max_tokens,
                    yield_after: budget.yield_after,
                }),
            orchestration_sandboxed: orchestration
                .and_then(|settings| settings.sandboxed)
                .unwrap_or(false),
            orchestration_isolation: orchestration
                .and_then(|settings| settings.isolation)
                .unwrap_or_default(),
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

    /// Effective hold-to-talk voice configuration (`/live`). The STT endpoint
    /// and API key are user-configured; `stt_model` defaults to the fallback
    /// label `whisper-1` and is never resolved by this product itself.
    #[must_use]
    pub fn live_runtime(&self) -> LiveRuntimeSettings {
        let live = self.live.as_ref();
        LiveRuntimeSettings {
            enabled: live.and_then(|value| value.enabled).unwrap_or(false),
            mode: live
                .and_then(|value| value.mode.clone())
                .unwrap_or_else(|| "stt".to_owned()),
            stt_base_url: live
                .and_then(|value| value.stt_base_url.clone())
                .unwrap_or_default(),
            stt_api_key: live
                .and_then(|value| value.stt_api_key.clone())
                .unwrap_or_default(),
            stt_model: live
                .and_then(|value| value.stt_model.clone())
                .unwrap_or_else(|| crate::live::DEFAULT_STT_MODEL.to_owned()),
            realtime_base_url: live
                .and_then(|value| value.realtime_base_url.clone())
                .unwrap_or_default(),
            realtime_api_key: live
                .and_then(|value| value.realtime_api_key.clone())
                .unwrap_or_default(),
            realtime_model: live
                .and_then(|value| value.realtime_model.clone())
                .unwrap_or_else(|| "gpt-realtime-1.5".to_owned()),
            voice: live
                .and_then(|value| value.voice.clone())
                .unwrap_or_else(|| "sol".to_owned()),
            language: live.and_then(|value| value.language.clone()),
            allow_insecure: live.and_then(|value| value.allow_insecure).unwrap_or(false),
        }
    }

    /// Effective image-generation configuration for the `generate_image` tool
    /// (`settings.images.genModel`/`genBaseUrl`/`genApiKey`). The endpoint and
    /// API key are user-configured; no provider default is assumed, and the
    /// key is redacted by the runtime settings' `Debug`.
    #[must_use]
    pub fn image_gen_runtime(&self) -> ImageGenRuntimeSettings {
        let images = self.images.as_ref();
        ImageGenRuntimeSettings {
            gen_model: images
                .and_then(|value| value.gen_model.clone())
                .filter(|model| !model.trim().is_empty()),
            gen_base_url: images
                .and_then(|value| value.gen_base_url.clone())
                .unwrap_or_default(),
            gen_api_key: images
                .and_then(|value| value.gen_api_key.clone())
                .unwrap_or_default(),
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
        // The todo tool is on by default so the model can create/modify/clear
        // todos in natural language (OMP parity); `orchestration.todo: false`
        // opts out.
        self.orchestration
            .as_ref()
            .and_then(|value| value.todo)
            .unwrap_or(true)
    }
}

/// Evaluate path-level permission rules for a file-touching tool call.
///
/// Precedence:
/// 1. Longest-prefix wins: the rule whose normalized path matches with the
///    most components governs.
/// 2. Among equally specific matches: `deny` > `ask` > `allow`.
/// 3. When a call has several targets (e.g. a semicolon-separated `glob`
///    path), the strongest verdict across targets wins.
///
/// Returns [`PermissionVerdict::NoMatch`] when no rule applies so the caller
/// can fall through to the capability-wide approval mode.
#[must_use]
pub fn permission_verdict(
    tool: &str,
    arguments: &Value,
    cwd: &Path,
    rules: &[PermissionRule],
) -> PermissionVerdict {
    if rules.is_empty() {
        return PermissionVerdict::NoMatch;
    }
    let Some(tool_kind) = PermissionTool::for_tool_name(tool) else {
        return PermissionVerdict::NoMatch;
    };
    let mut verdict = PermissionVerdict::NoMatch;
    for target in permission_targets(tool, arguments) {
        let Some(raw_target) = target else {
            continue;
        };
        if raw_target.contains("://") {
            // Internal URIs (skill://, agent://, history://, artifact://) are
            // not filesystem paths and are not addressable by path rules.
            continue;
        }
        let target = normalize_path(raw_target.trim(), cwd);
        if target.as_os_str().is_empty() {
            continue;
        }
        let Some((rule, _)) = best_rule_for_target(rules, tool_kind, &target, cwd) else {
            continue;
        };
        let target_verdict = match rule.action {
            PermissionRuleAction::Allow => PermissionVerdict::Allow,
            PermissionRuleAction::Ask => PermissionVerdict::Ask,
            PermissionRuleAction::Deny => PermissionVerdict::Deny(denied_reason(tool, &raw_target, rule)),
        };
        if verdict_rank(&target_verdict) > verdict_rank(&verdict) {
            verdict = target_verdict;
        }
        if matches!(verdict, PermissionVerdict::Deny(_)) {
            break;
        }
    }
    verdict
}

/// Evaluates path-level permission rules for an explicit set of absolute
/// target paths, using the same rule-matching machinery as
/// [`permission_verdict`]. Targets come from an external response rather than
/// the call arguments (e.g. LSP workspace-edit URIs resolved by the `lsp`
/// tool), so the strongest verdict across the given targets wins, matching
/// [`permission_verdict`] semantics (deny > ask > allow > no-match).
#[must_use]
pub fn permission_verdict_for_paths(
    tool: PermissionTool,
    targets: &[PathBuf],
    cwd: &Path,
    rules: &[PermissionRule],
) -> PermissionVerdict {
    if rules.is_empty() {
        return PermissionVerdict::NoMatch;
    }
    let mut verdict = PermissionVerdict::NoMatch;
    for target in targets {
        let Some((rule, _)) = best_rule_for_target(rules, tool, target, cwd) else {
            continue;
        };
        let target_verdict = match rule.action {
            PermissionRuleAction::Allow => PermissionVerdict::Allow,
            PermissionRuleAction::Ask => PermissionVerdict::Ask,
            PermissionRuleAction::Deny => {
                PermissionVerdict::Deny(denied_reason(tool.name(), &target.to_string_lossy(), rule))
            }
        };
        if verdict_rank(&target_verdict) > verdict_rank(&verdict) {
            verdict = target_verdict;
        }
        if matches!(verdict, PermissionVerdict::Deny(_)) {
            break;
        }
    }
    verdict
}

/// Extract the candidate filesystem targets a file-touching tool call may
/// touch, in argument order. `None` entries are calls that carry no path.
fn permission_targets(tool: &str, arguments: &Value) -> Vec<Option<String>> {
    let path = arguments.get("path").and_then(Value::as_str);
    match tool {
        // lsp's `path` argument is the primary file the call acts on (for
        // rename: the source document). Server-derived workspace-edit targets
        // are preflighted in the lsp tool itself via
        // [`permission_verdict_for_paths`], since they are only known after
        // the server responds.
        "read" | "write" | "edit" | "lsp" => vec![path.map(str::to_owned)],
        // glob's path is a semicolon-separated list of search targets; grep
        // falls back to the working directory when path is absent.
        "glob" => match path {
            Some(path) => path
                .split(';')
                .map(|target| {
                    let target = target.trim();
                    (!target.is_empty()).then(|| target.to_owned())
                })
                .collect(),
            None => vec![Some(".".to_owned())],
        },
        "grep" => vec![Some(path.map_or_else(|| ".".to_owned(), str::to_owned))],
        _ => Vec::new(),
    }
}

/// The most specific rule matching `target`, or `None`. Among equally specific
/// matches the stricter action (deny > ask > allow) wins.
fn best_rule_for_target<'a>(
    rules: &'a [PermissionRule],
    tool: PermissionTool,
    target: &Path,
    cwd: &Path,
) -> Option<(&'a PermissionRule, usize)> {
    let mut best: Option<(&'a PermissionRule, usize)> = None;
    for rule in rules {
        if let Some(tools) = &rule.tools
            && !tools.contains(&tool)
        {
            continue;
        }
        let pattern = normalized_rule_path(&rule.path, cwd);
        if !path_matches_prefix(target, &pattern) {
            continue;
        }
        let specificity = pattern.components().count();
        let candidate = (rule, specificity);
        let better = match best {
            None => true,
            Some((best_rule, best_specificity)) => {
                specificity > best_specificity
                    || (specificity == best_specificity
                        && action_rank(candidate.0.action) > action_rank(best_rule.action))
            }
        };
        if better {
            best = Some(candidate);
        }
    }
    best
}

/// Collapse a path into canonical lexical form: relative paths resolve against
/// `cwd`, `.` segments are dropped, and `..` segments pop one component — all
/// without touching the filesystem (the target may not exist yet, e.g. `write`
/// to a new file).
fn normalize_path(path: &str, cwd: &Path) -> PathBuf {
    use std::path::Component;
    let joined = Path::new(path);
    let joined = if joined.is_absolute() {
        joined.to_path_buf()
    } else {
        cwd.join(joined)
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Normalize a rule's path. A trailing `*` is accepted as an explicit prefix
/// marker and stripped; plain paths match their own subtree.
fn normalized_rule_path(path: &str, cwd: &Path) -> PathBuf {
    let base = path.trim_end().trim_end_matches('*').trim_end();
    normalize_path(base, cwd)
}

fn path_matches_prefix(target: &Path, pattern: &Path) -> bool {
    let target_components: Vec<_> = target.components().collect();
    let pattern_components: Vec<_> = pattern.components().collect();
    target_components.len() >= pattern_components.len()
        && target_components
            .iter()
            .zip(&pattern_components)
            .all(|(target, pattern)| target == pattern)
}

fn denied_reason(tool: &str, target: &str, rule: &PermissionRule) -> String {
    format!(
        "Tool execution denied by path permission rule: {tool} on '{target}' (matching rule path '{path}')",
        path = rule.path
    )
}

fn verdict_rank(verdict: &PermissionVerdict) -> u8 {
    match verdict {
        PermissionVerdict::NoMatch => 0,
        PermissionVerdict::Allow => 1,
        PermissionVerdict::Ask => 2,
        PermissionVerdict::Deny(_) => 3,
    }
}

fn action_rank(action: PermissionRuleAction) -> u8 {
    match action {
        PermissionRuleAction::Allow => 1,
        PermissionRuleAction::Ask => 2,
        PermissionRuleAction::Deny => 3,
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

#[derive(Clone)]
struct SettingsState {
    paths: SettingsPaths,
    /// Persisted global settings with environment references left **literal**
    /// (the pre-expansion document). Writing settings re-serializes this
    /// layer, so expanded secrets are never persisted.
    global: Settings,
    /// Persisted project settings with environment references left literal.
    project: Settings,
    overrides: Settings,
    /// The runtime view consumed by sessions: global/project are expanded
    /// (env references resolved) before the merge, overrides are not (they
    /// are concrete session-only values).
    effective: Settings,
    project_trusted: bool,
    /// Environment lookup used when projecting the runtime view.
    lookup: Arc<dyn Fn(&str) -> Option<String> + Send + Sync>,
}

impl SettingsManager {
    /// Phase one: load only trusted user-global settings. Project settings are
    /// deliberately not opened until [`Self::load_project`] receives trust.
    pub fn load_phase_one(cwd: impl AsRef<Path>, agent_dir: impl AsRef<Path>) -> Result<Self> {
        Self::load_phase_one_with(cwd, agent_dir, env_lookup)
    }

    /// [`Self::load_phase_one`] with an injectable environment lookup so
    /// env-expansion tests are deterministic without mutating the process
    /// environment.
    fn load_phase_one_with(
        cwd: impl AsRef<Path>,
        agent_dir: impl AsRef<Path>,
        lookup: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Result<Self> {
        let paths = SettingsPaths::new(cwd, agent_dir);
        // The persisted layers keep the pre-expansion document: env references
        // stay literal so writing settings never persists expanded secrets.
        // Expansion applies only when projecting the effective runtime view
        // (see `recompute`).
        let global = load_settings_file(&paths.global, SettingsScope::Global)?;
        let lookup: Arc<dyn Fn(&str) -> Option<String> + Send + Sync> = Arc::new(lookup);
        let mut state = SettingsState {
            paths,
            global,
            project: Settings::default(),
            overrides: Settings::default(),
            effective: Settings::default(),
            project_trusted: false,
            lookup,
        };
        recompute(&mut state);
        Ok(Self {
            inner: Arc::new(RwLock::new(state)),
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

    /// Path-level permission rules from the effective settings snapshot.
    /// Reads the live manager state so `permissionRules` changes apply without
    /// a session restart.
    #[must_use]
    pub fn permission_rules(&self) -> Vec<PermissionRule> {
        self.inner.read().effective.permission_rules.clone()
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
    // Project the runtime view: expand environment references in the
    // persisted layers, then merge. Overrides are concrete session-only
    // values and are merged unexpanded (they never came from a settings
    // file). Expansion never mutates the persisted layers.
    let lookup = state.lookup.as_ref();
    state.effective = expand_settings_with(&state.global, lookup)
        .merged(&expand_settings_with(&state.project, lookup))
        .merged(&state.overrides);
}

/// Expand environment references in a persisted settings layer for the
/// runtime view. The input layer is untouched (it stays literal so writes
/// never persist expanded secrets).
fn expand_settings_with(
    settings: &Settings,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Settings {
    let mut value =
        serde_json::to_value(settings).expect("settings serialize for env expansion");
    expand_env_in_value_with(&mut value, lookup);
    serde_json::from_value(value).expect("expanded settings deserialize")
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

/// Migration from `subagents.agentOverrides` to top-level `agents` is fully
/// automatic and silent. rpi keeps pi's config paths for compatibility, so
/// legacy config formats are supported without warning.
fn warn_legacy_agent_migration(_path: &Path, _migration: &LegacyAgentMigration) {}

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

/// Unsupported agent-config-looking keys under `subagents` are silently
/// ignored. rpi keeps pi's config format for compatibility; no warning.
fn warn_unsupported_subagents_fields(_path: &Path, _settings: &Settings) {}

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

    assert_eq!(warnings.len(), 0, "legacy migration is silent: {warnings:?}");
    assert!(
        drain_settings_diagnostics().is_empty(),
        "drain must clear the capture"
    );
}





fn load_settings_file(path: &Path, scope: SettingsScope) -> Result<Settings> {
    let path = resolve_settings_path(path);
    let file = match File::open(&path) {
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
    let file_label = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("settings");
    // Parse into a generic document and deserialize into the typed Settings
    // (whose `extra` maps retain unknown fields) WITHOUT expanding environment
    // references: the persisted layers stay literal, and expansion happens
    // only when projecting the effective runtime view (`expand_settings_with`
    // in `recompute`). Writing settings therefore never persists expanded
    // secrets.
    let document: Value = if is_toml_settings_path(&path) {
        let value: toml::Value = toml::from_str(&content).with_context(|| {
            format!("Failed to parse {file_label}\nFile: {}", path.display())
        })?;
        toml_to_json(value)
    } else {
        serde_json::from_str(&content).with_context(|| {
            format!("Failed to parse {file_label}\nFile: {}", path.display())
        })?
    };
    let mut settings: Settings = serde_json::from_value(document).with_context(|| {
        format!("Failed to deserialize {file_label}\nFile: {}", path.display())
    })?;
    let migration = migrate_legacy_agent_overrides(&mut settings).with_context(|| {
        format!(
            "invalid {} settings {} while migrating subagents.agentOverrides",
            scope_name(scope),
            path.display()
        )
    })?;
    validate_settings(&settings, scope, &path)?;
    warn_legacy_agent_migration(&path, &migration);
    warn_unsupported_subagents_fields(&path, &settings);
    Ok(settings)
}

/// Resolve the file actually read/written for a scope's canonical settings
/// path. A `settings.toml` next to the canonical `settings.json` wins when
/// present; otherwise the canonical JSON file is used. The preference is
/// purely name-based — file contents are never sniffed. Only the canonical
/// `settings.json` names participate; any other path is used verbatim.
fn resolve_settings_path(canonical: &Path) -> PathBuf {
    if canonical.file_name().and_then(|name| name.to_str()) == Some("settings.json") {
        let toml = canonical.with_extension("toml");
        if toml.is_file() {
            return toml;
        }
    }
    canonical.to_path_buf()
}

/// Deterministic format rule: a `.toml` extension selects TOML; any other
/// extension (`.json`, none, or anything else) selects JSON. Never
/// content-sniffed.
fn is_toml_settings_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("toml"))
}

/// Convert a parsed TOML value to its JSON equivalent. TOML has no null, so
/// the result contains none either; datetimes become their string form.
fn toml_to_json(value: toml::Value) -> Value {
    match value {
        toml::Value::String(string) => Value::String(string),
        toml::Value::Integer(integer) => Value::Number(integer.into()),
        toml::Value::Float(float) => {
            Value::Number(serde_json::Number::from_f64(float).unwrap_or_else(|| serde_json::Number::from(0)))
        }
        toml::Value::Boolean(boolean) => Value::Bool(boolean),
        toml::Value::Datetime(datetime) => Value::String(datetime.to_string()),
        toml::Value::Array(items) => Value::Array(items.into_iter().map(toml_to_json).collect()),
        toml::Value::Table(table) => Value::Object(
            table.into_iter().map(|(key, value)| (key, toml_to_json(value))).collect(),
        ),
    }
}

/// Convert a JSON value to a TOML value for serialization. JSON nulls inside
/// object values are dropped (TOML has no null); typed settings never produce
/// nulls outside retained `extra` maps.
fn json_to_toml(value: Value) -> Result<toml::Value> {
    Ok(match value {
        Value::Null => return Err(anyhow!("cannot serialize a null value as TOML")),
        Value::Bool(boolean) => toml::Value::Boolean(boolean),
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                toml::Value::Integer(integer)
            } else if let Some(integer) = number.as_u64() {
                toml::Value::Integer(i64::try_from(integer)?)
            } else if let Some(float) = number.as_f64() {
                toml::Value::Float(float)
            } else {
                return Err(anyhow!("unsupported JSON number {number}"));
            }
        }
        Value::String(string) => toml::Value::String(string),
        Value::Array(items) => {
            let items = items
                .into_iter()
                .map(json_to_toml)
                .collect::<Result<Vec<_>>>()?;
            toml::Value::Array(items)
        }
        Value::Object(map) => {
            let mut table = toml::map::Map::new();
            for (key, value) in map {
                if value.is_null() {
                    continue;
                }
                table.insert(key, json_to_toml(value)?);
            }
            toml::Value::Table(table)
        }
    })
}

/// Serialize settings as pretty TOML. The typed struct is round-tripped
/// through its JSON representation so retained unknown fields survive; JSON
/// nulls (possible only inside retained `extra` maps) are dropped because TOML
/// has no null.
fn toml_settings_string(settings: &Settings) -> Result<String> {
    let json = serde_json::to_value(settings)?;
    let value = json_to_toml(json)?;
    toml::to_string_pretty(&value).with_context(|| "serializing settings as TOML")
}

fn env_lookup(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Expand `$NAME` and `${NAME}` references in every string value of a parsed
/// settings document, recursively through objects and arrays. Keys and
/// non-string values are untouched.
fn expand_env_in_value_with(value: &mut Value, lookup: &dyn Fn(&str) -> Option<String>) {
    match value {
        Value::String(string) => *string = expand_env_string_with(string, lookup),
        Value::Array(items) => {
            for item in items {
                expand_env_in_value_with(item, lookup);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                expand_env_in_value_with(item, lookup);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn expand_env_string_with(input: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(input.len());
    let mut cursor = input;
    while let Some(dollar) = cursor.find('$') {
        out.push_str(&cursor[..dollar]);
        let tail = &cursor[dollar + 1..];
        // `${NAME}` takes precedence over bare `$NAME`.
        let (name, consumed_after_dollar) = if let Some(inner) = tail.strip_prefix('{') {
            match inner.find('}') {
                Some(end) if valid_env_name(&inner[..end]) => (&inner[..end], end + 2),
                _ => ("", 0),
            }
        } else {
            let name_len = tail
                .char_indices()
                .take_while(|(_, ch)| ch.is_ascii_alphanumeric() || *ch == '_')
                .map(|(index, _)| index)
                .last()
                .map_or(0, |last| last + 1)
                .min(tail.len());
            if name_len > 0 && valid_env_name(&tail[..name_len]) {
                (&tail[..name_len], name_len)
            } else {
                ("", 0)
            }
        };
        if name.is_empty() {
            // A `$` not followed by a valid name is a literal dollar.
            out.push('$');
            cursor = &cursor[dollar + 1..];
            continue;
        }
        match lookup(name) {
            Some(value) => {
                out.push_str(&value);
                cursor = &cursor[dollar + 1 + consumed_after_dollar..];
            }
            None => {
                // Fail-open: keep the literal reference, but report it once.
                emit_settings_diagnostic(
                    format!("settings-env\0{name}"),
                    format!(
                        "settings: environment variable {name} is not set; \
                         leaving the ${name} reference unchanged"
                    ),
                );
                out.push('$');
                cursor = &cursor[dollar + 1..];
            }
        }
    }
    out.push_str(cursor);
    out
}

/// POSIX-style environment variable name: `[A-Za-z_][A-Za-z0-9_]*`.
fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
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
    for (index, rule) in settings.permission_rules.iter().enumerate() {
        if rule.path.trim().is_empty() {
            bail!("permissionRules[{index}].path must not be empty");
        }
        if rule.tools.as_ref().is_some_and(Vec::is_empty) {
            bail!("permissionRules[{index}].tools must not be empty when present");
        }
    }
    if let Some(sandbox) = &settings.sandbox {
        for (field, paths) in [
            ("sandbox.allowedPaths", sandbox.allowed_paths.as_deref().unwrap_or_default()),
            ("sandbox.deniedPaths", sandbox.denied_paths.as_deref().unwrap_or_default()),
        ] {
            if paths.iter().any(|path| path.trim().is_empty()) {
                bail!("{field} must not contain empty paths");
            }
        }
    }
    if settings.compaction.as_ref().and_then(|value| value.reserve_tokens).is_some_and(|value| value <= 0) {
        bail!("compaction.reserveTokens must be greater than zero");
    }
    if settings.compaction.as_ref().and_then(|value| value.keep_recent_tokens).is_some_and(|value| value < 0) {
        bail!("compaction.keepRecentTokens must be non-negative");
    }
    if settings.compaction.as_ref().and_then(|value| value.snap_keep_turns).is_some_and(|value| value < 1) {
        bail!("compaction.snapKeepTurns must be at least 1");
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
    if settings.session_ttl_days.is_some_and(|days| days == 0) {
        bail!("sessionTtlDays must be greater than zero");
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
    if let Some(hooks) = &settings.hooks {
        for (index, hook) in hooks.iter().enumerate() {
            if hook.command.is_empty() {
                bail!("hooks[{index}] command must not be empty");
            }
            if hook.command[0].trim().is_empty() {
                bail!("hooks[{index}] command executable must not be empty");
            }
            if hook.timeout_ms.is_some_and(|timeout| timeout == 0) {
                bail!("hooks[{index}] timeoutMs must be greater than zero");
            }
        }
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
        if let Some(soft_budget) = orchestration.soft_budget.as_ref() {
            if soft_budget.max_requests.is_some_and(|value| value == 0) {
                bail!("orchestration.softBudget.maxRequests must be greater than zero");
            }
            if soft_budget.max_tokens.is_some_and(|value| value == 0) {
                bail!("orchestration.softBudget.maxTokens must be greater than zero");
            }
            if soft_budget.yield_after.is_some_and(|value| value == 0) {
                bail!("orchestration.softBudget.yieldAfter must be greater than zero");
            }
        }
    }
    let mut mcp_names = HashSet::with_capacity(settings.mcp_servers.len());
    for (index, server) in settings.mcp_servers.iter().enumerate() {
        if server.name.trim().is_empty() {
            bail!("mcpServers[{index}].name must not be empty");
        }
        if !mcp_names.insert(server.name.as_str()) {
            bail!("mcpServers[{index}] has duplicate name '{}'", server.name);
        }
        if server.disabled {
            // A disabled server never spawns, so its transport fields are
            // advisory; only the name rules above apply. (The mcp import
            // surface and Cursor/Claude configs routinely carry partial
            // entries for servers that are switched off.)
            continue;
        }
        match server.transport {
            McpTransport::Stdio => {
                if server.command.as_ref().is_none_or(|command| command.trim().is_empty()) {
                    bail!("mcpServers[{index}] (stdio) requires a non-empty command");
                }
                if server.url.is_some() {
                    bail!("mcpServers[{index}] (stdio) must not set url (url is for the sse transport)");
                }
            }
            McpTransport::Sse => {
                if server.url.as_ref().is_none_or(|url| url.trim().is_empty()) {
                    bail!("mcpServers[{index}] (sse) requires a non-empty url");
                }
                let Some(url) = server.url.as_deref() else {
                    bail!("mcpServers[{index}] (sse) requires a non-empty url");
                };
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    bail!("mcpServers[{index}] (sse) url must start with http:// or https://");
                }
                if server.command.is_some() {
                    bail!("mcpServers[{index}] (sse) must not set command (command is for the stdio transport)");
                }
            }
        }
    }
    Ok(())
}

fn write_settings_file(path: &Path, settings: &Settings, scope: SettingsScope) -> Result<()> {
    let path = resolve_settings_path(path);
    let mut bytes = if is_toml_settings_path(&path) {
        let mut bytes = toml_settings_string(settings)?.into_bytes();
        bytes.push(b'\n');
        bytes
    } else {
        let mut bytes = serde_json::to_vec_pretty(settings).with_context(|| {
            format!("serializing {} settings {}", scope_name(scope), path.display())
        })?;
        bytes.push(b'\n');
        bytes
    };
    atomic_write(&path, &bytes).with_context(|| {
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
        // Settings and trust-store files carry credentials, secret-bearing
        // env maps, and trust decisions: owner-only permissions so a shared
        // home directory cannot leak them. Set before the rename so the
        // visible file never has looser permissions.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = file.metadata()?.permissions();
            permissions.set_mode(0o600);
            file.set_permissions(permissions)?;
        }
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
    use serde_json::json;

    #[test]
    fn memory_config_applies_defaults_and_overrides() {
        let config = Settings::default().memory_config();
        assert_eq!(config.backend, MemoryBackend::Local);
        assert!(config.hindsight_api_url.is_none());
        assert!(config.hindsight_api_token.is_none());
        assert!(!config.hindsight_allow_insecure);
        assert_eq!(config.hindsight_bank_id, crate::memory::DEFAULT_HINDSIGHT_BANK_ID);
        assert_eq!(config.hindsight_scoping, HindsightScoping::PerProjectTagged);
        assert_eq!(config.hindsight_recall_budget, HindsightBudget::Mid);
        assert_eq!(config.hindsight_recall_types, crate::memory::DEFAULT_HINDSIGHT_RECALL_TYPES);

        let settings = Settings {
            memory: Some(MemorySettings {
                backend: Some(MemoryBackend::Hindsight),
                hindsight_api_url: Some("https://memory.example.test/".to_owned()),
                hindsight_api_token: Some("secret-value".to_owned()),
                hindsight_bank_id: Some("main".to_owned()),
                hindsight_bank_id_prefix: Some("team".to_owned()),
                hindsight_scoping: Some(HindsightScoping::PerProject),
                hindsight_injection: Some(true),
                hindsight_recall_budget: Some(HindsightBudget::High),
                hindsight_recall_max_tokens: Some(2048),
                hindsight_recall_types: Some(vec!["world".to_owned()]),
                hindsight_request_timeout_ms: Some(1_000),
                hindsight_recall_timeout_ms: Some(2_000),
                hindsight_retain_timeout_ms: Some(3_000),
                hindsight_reflect_timeout_ms: Some(4_000),
                ..Default::default()
            }),
            ..Default::default()
        };
        let config = settings.memory_config();
        assert_eq!(config.backend, MemoryBackend::Hindsight);
        assert_eq!(config.hindsight_api_url.as_deref(), Some("https://memory.example.test/"));
        assert_eq!(config.hindsight_api_token.as_deref(), Some("secret-value"));
        assert_eq!(config.hindsight_bank_id, "main");
        assert_eq!(config.hindsight_bank_id_prefix.as_deref(), Some("team"));
        assert_eq!(config.hindsight_scoping, HindsightScoping::PerProject);
        assert!(config.hindsight_injection);
        assert_eq!(config.hindsight_recall_budget, HindsightBudget::High);
        assert_eq!(config.hindsight_recall_max_tokens, 2048);
        assert_eq!(config.hindsight_recall_types, ["world"]);
        assert_eq!(config.hindsight_request_timeout_ms, 1_000);
        assert_eq!(config.hindsight_recall_timeout_ms, 2_000);
        assert_eq!(config.hindsight_retain_timeout_ms, 3_000);
        assert_eq!(config.hindsight_reflect_timeout_ms, 4_000);
        let encoded = serde_json::to_value(&settings).expect("serialize settings");
        let decoded: Settings = serde_json::from_value(encoded).expect("deserialize settings");
        assert_eq!(decoded, settings);
    }

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
    fn orchestration_isolation_parses_flows_to_runtime_and_persists() {
        // Every enum value parses and maps onto the runtime snapshot.
        for (raw, expected) in [
            ("worktree", crate::WorkflowIsolationSetting::Worktree),
            ("overlayfs", crate::WorkflowIsolationSetting::Overlayfs),
            ("none", crate::WorkflowIsolationSetting::None),
        ] {
            let settings: Settings =
                serde_json::from_str(&format!(r#"{{"orchestration":{{"isolation":"{raw}"}}}}"#))
                    .expect("parse isolation setting");
            let snapshot = settings.runtime_settings().expect("runtime settings");
            assert_eq!(snapshot.orchestration_isolation, expected);
            assert_eq!(snapshot.state().orchestration_isolation, expected);
        }

        // The default stays the git worktree.
        let default_snapshot = Settings::default().runtime_settings().expect("default runtime");
        assert_eq!(
            default_snapshot.orchestration_isolation,
            crate::WorkflowIsolationSetting::Worktree
        );

        // Persist/reload round trip preserves the selection.
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path())
            .expect("settings manager");
        manager
            .update_global(|settings| {
                settings.orchestration = Some(crate::OrchestrationSettings {
                    isolation: Some(crate::WorkflowIsolationSetting::Overlayfs),
                    ..crate::OrchestrationSettings::default()
                });
            })
            .expect("persist isolation setting");
        let reloaded = SettingsManager::load_phase_one(cwd.path(), agent.path())
            .expect("reload settings");
        let reloaded_isolation = reloaded
            .settings()
            .orchestration
            .as_ref()
            .and_then(|orchestration| orchestration.isolation)
            .expect("isolation survives reload");
        assert_eq!(reloaded_isolation, crate::WorkflowIsolationSetting::Overlayfs);

        // An unknown enum value fails deserialization (fail-closed), so the
        // backend can never be silently mistyped into the default.
        assert!(
            serde_json::from_str::<Settings>(r#"{"orchestration":{"isolation":"btrfs"}}"#)
                .is_err(),
            "unknown isolation values must be rejected"
        );
    }

    #[test]
    fn orchestration_soft_budget_parses_flows_to_runtime_and_persists() {
        let settings: Settings = serde_json::from_str(
            r#"{"orchestration":{"softBudget":{"maxRequests":12,"maxTokens":90000,"yieldAfter":3}}}"#,
        )
        .expect("parse soft budget settings");
        let budget = settings
            .orchestration
            .as_ref()
            .and_then(|orchestration| orchestration.soft_budget.as_ref())
            .expect("soft budget settings present");
        assert_eq!(budget.max_requests, Some(12));
        assert_eq!(budget.max_tokens, Some(90000));
        assert_eq!(budget.yield_after, Some(3));

        // The runtime snapshot maps the settings field onto JobSoftBudget so
        // orchestration_candidate can carry it into the runtime config.
        let snapshot = settings.runtime_settings().expect("runtime settings");
        assert_eq!(snapshot.orchestration_soft_budget.max_requests, Some(12));
        assert_eq!(snapshot.orchestration_soft_budget.max_tokens, Some(90000));
        assert_eq!(snapshot.orchestration_soft_budget.yield_after, Some(3));

        // A default (unset) soft budget stays unlimited.
        let default_snapshot = Settings::default().runtime_settings().expect("default runtime");
        assert_eq!(
            default_snapshot.orchestration_soft_budget,
            crate::JobSoftBudget::default()
        );

        // Persist/reload round trip preserves the knobs.
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path())
            .expect("settings manager");
        manager
            .update_global(|settings| {
                settings.orchestration = Some(crate::OrchestrationSettings {
                    soft_budget: Some(crate::SoftBudgetSettings {
                        max_requests: Some(5),
                        max_tokens: None,
                        yield_after: Some(2),
                    }),
                    ..crate::OrchestrationSettings::default()
                });
            })
            .expect("persist soft budget settings");
        let reloaded = SettingsManager::load_phase_one(cwd.path(), agent.path())
            .expect("reload settings");
        let reloaded_settings = reloaded.settings();
        let reloaded_budget = reloaded_settings
            .orchestration
            .as_ref()
            .and_then(|orchestration| orchestration.soft_budget.as_ref())
            .expect("soft budget survives reload");
        assert_eq!(reloaded_budget.max_requests, Some(5));
        assert_eq!(reloaded_budget.yield_after, Some(2));

        // Zero knobs are rejected: a soft budget must be positive or absent.
        for (key, json) in [
            ("maxRequests", r#"{"maxRequests":0}"#),
            ("maxTokens", r#"{"maxTokens":0}"#),
            ("yieldAfter", r#"{"yieldAfter":0}"#),
        ] {
            let zero: Settings = serde_json::from_str(&format!(
                r#"{{"orchestration":{{"softBudget":{json}}}}}"#
            ))
            .expect("zero soft budget parses");
            let error = validate_settings(&zero, SettingsScope::Global, Path::new("settings.json"))
                .expect_err("zero soft budget must fail validation");
            assert!(
                format!("{error:#}").contains(key),
                "{key} rejection: {error:#}"
            );
        }
    }

    #[test]
    fn orchestration_preferred_agent_parses_flows_and_persists() {
        let settings: Settings = serde_json::from_str(
            r#"{"orchestration":{"preferredAgent":"reviewer"}}"#,
        )
        .expect("parse preferred agent settings");
        assert_eq!(
            settings
                .orchestration
                .as_ref()
                .and_then(|orchestration| orchestration.preferred_agent.as_deref()),
            Some("reviewer")
        );

        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path())
            .expect("settings manager");
        manager
            .update_global(|settings| {
                settings.orchestration = Some(crate::OrchestrationSettings {
                    preferred_agent: Some("mentor".to_owned()),
                    ..crate::OrchestrationSettings::default()
                });
            })
            .expect("persist preferred agent");
        let reloaded = SettingsManager::load_phase_one(cwd.path(), agent.path())
            .expect("reload settings");
        assert_eq!(
            reloaded
                .settings()
                .orchestration
                .as_ref()
                .and_then(|orchestration| orchestration.preferred_agent.as_deref()),
            Some("mentor")
        );

        // Clearing persists as absent.
        manager
            .update_global(|settings| {
                if let Some(orchestration) = settings.orchestration.as_mut() {
                    orchestration.preferred_agent = None;
                }
            })
            .expect("clear preferred agent");
        let cleared = SettingsManager::load_phase_one(cwd.path(), agent.path())
            .expect("reload cleared settings");
        assert!(
            cleared
                .settings()
                .orchestration
                .as_ref()
                .and_then(|orchestration| orchestration.preferred_agent.as_ref())
                .is_none()
        );
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
    fn session_ttl_days_is_typed_optional_and_rejects_zero() {
        assert_eq!(Settings::default().session_ttl_days, None);
        let absent = serde_json::to_value(Settings::default()).expect("serialize absent ttl");
        assert!(absent.get("sessionTtlDays").is_none(), "absent ttl must not serialize");

        let typed: Settings = serde_json::from_str(r#"{"sessionTtlDays":45}"#)
            .expect("deserialize typed session ttl days");
        assert_eq!(typed.session_ttl_days, Some(45));

        let zero: Settings = serde_json::from_str(r#"{"sessionTtlDays":0}"#)
            .expect("zero deserializes as a typed u64");
        let error = validate_settings(&zero, SettingsScope::Global, Path::new("settings.json"))
            .expect_err("zero-day TTL must fail validation");
        assert!(format!("{error:#}").contains("sessionTtlDays"), "{error:#}");

        let negative = serde_json::from_str::<Settings>(r#"{"sessionTtlDays":-1}"#);
        assert!(negative.is_err(), "negative TTL must not deserialize into u64");
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

        assert_eq!(warnings.len(), 0, "legacy migration is silent: {warnings:?}");
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

        assert_eq!(warnings.len(), 0, "legacy migration is silent: {warnings:?}");
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

        assert_eq!(warnings.len(), 0, "unsupported subagents fields are silent: {warnings:?}");
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
        assert_eq!(
            warnings.len(),
            0,
            "unsupported subagents fields are silent: {warnings:?}"
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

    #[test]
    fn hooks_config_round_trips_with_unknown_field_preservation() {
        let raw = r#"{
            "hooks": [
                {
                    "event": "pre_tool_call",
                    "matcher": "read",
                    "command": ["/opt/hooks/guard", "--strict"],
                    "timeoutMs": 1200,
                    "enabled": false,
                    "failClosed": true,
                    "futureField": {"keep": true}
                },
                {
                    "event": "session_end",
                    "command": ["/opt/hooks/bye"]
                }
            ],
            "unknownTopLevel": {"keep": 1}
        }"#;
        let settings: Settings = serde_json::from_str(raw).expect("deserialize hooks");
        let hooks = settings.hooks.as_ref().expect("hooks present");
        assert_eq!(hooks.len(), 2);
        assert_eq!(hooks[0].event, HookEvent::PreToolCall);
        assert_eq!(hooks[0].matcher.as_deref(), Some("read"));
        assert_eq!(hooks[0].command, vec!["/opt/hooks/guard".to_owned(), "--strict".to_owned()]);
        assert_eq!(hooks[0].timeout_ms, Some(1200));
        assert_eq!(hooks[0].enabled, Some(false));
        assert_eq!(hooks[0].fail_closed, Some(true));
        assert_eq!(
            hooks[0].extra.get("futureField").and_then(|value| value.get("keep")),
            Some(&Value::Bool(true)),
            "unknown hook-entry fields must be retained"
        );
        assert_eq!(hooks[1].event, HookEvent::SessionEnd);
        assert!(hooks[1].matcher.is_none());
        assert_eq!(hooks[1].command, vec!["/opt/hooks/bye".to_owned()]);

        // Round-trip: serialize, re-deserialize, and compare with the original.
        let encoded = serde_json::to_value(&settings).expect("serialize settings");
        let decoded: Settings = serde_json::from_value(encoded).expect("re-deserialize");
        assert_eq!(decoded, settings, "hooks config must round-trip losslessly");
        let raw_again = serde_json::to_string(&decoded).expect("re-serialize");
        assert!(raw_again.contains("\"futureField\""), "unknown hook field survives: {raw_again}");
        assert!(raw_again.contains("\"unknownTopLevel\""), "unknown top-level field survives: {raw_again}");
        assert!(raw_again.contains("\"pre_tool_call\""));
        assert!(raw_again.contains("\"session_end\""));
    }

    #[test]
    fn hooks_events_use_snake_case_and_reject_unknown() {
        let settings: Settings =
            serde_json::from_str(r#"{"hooks":[{"event":"turn_start","command":["x"]}]}"#)
                .expect("turn_start accepted");
        assert_eq!(settings.hooks.as_ref().expect("hooks").len(), 1);
        assert_eq!(serde_json::from_str::<HookEvent>("\"post_tool_call\"").expect("post event"), HookEvent::PostToolCall);
        assert!(
            serde_json::from_str::<HookEvent>("\"mid_tool_call\"").is_err(),
            "unknown events must be rejected at deserialize time"
        );
    }

    #[test]
    fn hooks_validation_rejects_empty_command_and_zero_timeout() {
        let manager = SettingsManager::load_phase_one(
            std::env::temp_dir().join(format!("hooks-validate-{}", Uuid::now_v7())).as_path(),
            std::env::temp_dir().join(format!("hooks-validate-agent-{}", Uuid::now_v7())).as_path(),
        )
        .expect("settings manager");
        let path = manager.paths().global;

        let mut empty_command = Settings::default();
        empty_command.hooks = Some(vec![HookConfig {
            event: HookEvent::TurnStart,
            matcher: None,
            command: Vec::new(),
            timeout_ms: None,
            enabled: None,
            fail_closed: None,
            extra: Map::new(),
        }]);
        let error = validate_settings(&empty_command, SettingsScope::Global, &path)
            .expect_err("empty command must fail");
        assert!(
            format!("{error:#}").contains("hooks[0] command"),
            "error: {error}"
        );

        let mut zero_timeout = Settings::default();
        zero_timeout.hooks = Some(vec![HookConfig {
            event: HookEvent::PreToolCall,
            matcher: None,
            command: vec!["/bin/true".to_owned()],
            timeout_ms: Some(0),
            enabled: None,
            fail_closed: None,
            extra: Map::new(),
        }]);
        let error = validate_settings(&zero_timeout, SettingsScope::Global, &path)
            .expect_err("zero timeout must fail");
        assert!(
            format!("{error:#}").contains("timeoutMs"),
            "error: {error}"
        );

        let valid = Settings::default();
        validate_settings(&valid, SettingsScope::Global, &path).expect("defaults valid");
    }

    #[test]
    fn mcp_servers_round_trip_with_unknown_field_preservation() {
        let raw = r#"{
            "mcpServers": [
                {
                    "name": "filesystem",
                    "transport": "stdio",
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
                    "env": { "MCP_DEBUG": "1" },
                    "futureField": { "keep": true }
                },
                {
                    "name": "github",
                    "transport": "sse",
                    "url": "https://mcp.example.com/github"
                }
            ],
            "unknownTopLevel": {"keep": 1}
        }"#;
        let settings: Settings = serde_json::from_str(raw).expect("deserialize mcpServers");
        let servers = &settings.mcp_servers;
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "filesystem");
        assert_eq!(servers[0].transport, McpTransport::Stdio);
        assert_eq!(servers[0].command.as_deref(), Some("npx"));
        assert_eq!(
            servers[0].args.as_deref(),
            Some(&["-y".to_owned(), "@modelcontextprotocol/server-filesystem".to_owned(), "/tmp".to_owned()][..])
        );
        assert_eq!(
            servers[0].env.as_ref().and_then(|env| env.get("MCP_DEBUG")),
            Some(&"1".to_owned())
        );
        assert_eq!(
            servers[0].extra.get("futureField").and_then(|value| value.get("keep")),
            Some(&Value::Bool(true)),
            "unknown mcp server fields must be retained"
        );
        assert_eq!(servers[1].name, "github");
        assert_eq!(servers[1].transport, McpTransport::Sse);
        assert_eq!(servers[1].url.as_deref(), Some("https://mcp.example.com/github"));
        assert!(servers[1].command.is_none());

        // Round-trip: serialize, re-deserialize, and compare with the original.
        let encoded = serde_json::to_value(&settings).expect("serialize settings");
        let decoded: Settings = serde_json::from_value(encoded).expect("re-deserialize");
        assert_eq!(decoded, settings, "mcpServers config must round-trip losslessly");
        let raw_again = serde_json::to_string(&decoded).expect("re-serialize");
        assert!(raw_again.contains("\"futureField\""), "unknown server field survives: {raw_again}");
        assert!(raw_again.contains("\"unknownTopLevel\""), "unknown top-level field survives: {raw_again}");
        assert!(raw_again.contains("\"mcpServers\""), "mcpServers survives: {raw_again}");
        assert!(raw_again.contains("\"@modelcontextprotocol/server-filesystem\""));
    }

    #[test]
    fn mcp_servers_disabled_flag_round_trips() {
        // The per-entry `disabled` flag parses from both present and absent,
        // serializes only when true, and survives a full round-trip.
        let raw = r#"{
            "mcpServers": [
                { "name": "on", "command": "npx" },
                { "name": "off", "command": "npx", "disabled": true }
            ]
        }"#;
        let settings: Settings = serde_json::from_str(raw).expect("deserialize");
        assert!(!settings.mcp_servers[0].disabled);
        assert!(settings.mcp_servers[1].disabled);

        let serialized = serde_json::to_string(&settings).expect("serialize");
        assert!(
            !serialized.contains("\"disabled\":false"),
            "false must be omitted: {serialized}"
        );
        assert!(
            serialized.contains("\"disabled\":true"),
            "true must survive: {serialized}"
        );

        let decoded: Settings = serde_json::from_str(&serialized).expect("re-deserialize");
        assert_eq!(decoded, settings, "disabled flag must round-trip losslessly");
    }

    #[test]
    fn mcp_servers_validation_rejects_misconfiguration() {
        let manager = SettingsManager::load_phase_one(
            std::env::temp_dir().join(format!("mcp-validate-{}", Uuid::now_v7())).as_path(),
            std::env::temp_dir().join(format!("mcp-validate-agent-{}", Uuid::now_v7())).as_path(),
        )
        .expect("settings manager");
        let path = manager.paths().global;

        fn server(name: &str) -> McpServerConfig {
            McpServerConfig {
                name: name.to_owned(),
                disabled: false,
                transport: McpTransport::Stdio,
                command: Some("npx".to_owned()),
                args: None,
                url: None,
                env: None,
                extra: Map::new(),
            }
        }

        // Empty name.
        let mut empty_name = Settings::default();
        empty_name.mcp_servers = vec![McpServerConfig {
            name: "  ".to_owned(),
            ..server("x")
        }];
        let error = validate_settings(&empty_name, SettingsScope::Global, &path)
            .expect_err("empty name must fail");
        assert!(format!("{error:#}").contains("mcpServers[0].name"), "error: {error}");

        // Empty command (stdio).
        let mut empty_command = Settings::default();
        empty_command.mcp_servers = vec![McpServerConfig {
            command: Some(" ".to_owned()),
            ..server("x")
        }];
        let error = validate_settings(&empty_command, SettingsScope::Global, &path)
            .expect_err("empty command must fail");
        assert!(format!("{error:#}").contains("requires a non-empty command"), "error: {error}");

        // stdio + url conflict.
        let mut stdio_url = Settings::default();
        stdio_url.mcp_servers = vec![McpServerConfig {
            url: Some("https://example.com/mcp".to_owned()),
            ..server("x")
        }];
        let error = validate_settings(&stdio_url, SettingsScope::Global, &path)
            .expect_err("stdio+url must fail");
        assert!(format!("{error:#}").contains("must not set url"), "error: {error}");

        // sse without url.
        let mut sse_no_url = Settings::default();
        sse_no_url.mcp_servers = vec![McpServerConfig {
            transport: McpTransport::Sse,
            command: None,
            ..server("x")
        }];
        let error = validate_settings(&sse_no_url, SettingsScope::Global, &path)
            .expect_err("sse without url must fail");
        assert!(format!("{error:#}").contains("requires a non-empty url"), "error: {error}");

        // sse with a non-http url.
        let mut sse_bad_url = Settings::default();
        sse_bad_url.mcp_servers = vec![McpServerConfig {
            transport: McpTransport::Sse,
            command: None,
            url: Some("ftp://example.com/mcp".to_owned()),
            ..server("x")
        }];
        let error = validate_settings(&sse_bad_url, SettingsScope::Global, &path)
            .expect_err("non-http sse url must fail");
        assert!(format!("{error:#}").contains("http:// or https://"), "error: {error}");

        // sse + command conflict.
        let mut sse_command = Settings::default();
        sse_command.mcp_servers = vec![McpServerConfig {
            transport: McpTransport::Sse,
            url: Some("https://example.com/mcp".to_owned()),
            ..server("x")
        }];
        let error = validate_settings(&sse_command, SettingsScope::Global, &path)
            .expect_err("sse+command must fail");
        assert!(format!("{error:#}").contains("must not set command"), "error: {error}");

        // Duplicate names.
        let mut duplicates = Settings::default();
        duplicates.mcp_servers = vec![server("dup"), server("dup")];
        let error = validate_settings(&duplicates, SettingsScope::Global, &path)
            .expect_err("duplicate names must fail");
        assert!(format!("{error:#}").contains("duplicate name"), "error: {error}");

        // A well-formed stdio entry validates.
        let mut valid = Settings::default();
        valid.mcp_servers = vec![server("ok")];
        validate_settings(&valid, SettingsScope::Global, &path).expect("valid stdio entry");

        // A disabled entry is exempt from transport requirements (it never
        // spawns), but still must carry a non-empty, unique name.
        let mut disabled_partial = Settings::default();
        disabled_partial.mcp_servers = vec![McpServerConfig {
            disabled: true,
            command: None,
            transport: McpTransport::Stdio,
            ..server("off")
        }];
        validate_settings(&disabled_partial, SettingsScope::Global, &path)
            .expect("disabled entry needs no command");
        let mut disabled_duplicate = Settings::default();
        disabled_duplicate.mcp_servers = vec![
            McpServerConfig {
                disabled: true,
                ..server("dup")
            },
            server("dup"),
        ];
        let error = validate_settings(&disabled_duplicate, SettingsScope::Global, &path)
            .expect_err("duplicate names must fail even when disabled");
        assert!(format!("{error:#}").contains("duplicate name"), "error: {error}");
    }

    #[test]
    fn mcp_servers_persist_through_manager_round_trip() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("manager");
        manager
            .update_global(|settings| {
                settings.mcp_servers = vec![McpServerConfig {
                    name: "filesystem".to_owned(),
                    disabled: false,
                    transport: McpTransport::Stdio,
                    command: Some("npx".to_owned()),
                    args: Some(vec!["-y".to_owned(), "@modelcontextprotocol/server-filesystem".to_owned()]),
                    url: None,
                    env: Some(BTreeMap::from([("MCP_DEBUG".to_owned(), "1".to_owned())])),
                    extra: Map::new(),
                }];
            })
            .expect("persist mcpServers");
        let reloaded = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("reload");
        assert_eq!(reloaded.global_settings().mcp_servers.len(), 1);
        let server = &reloaded.global_settings().mcp_servers[0];
        assert_eq!(server.name, "filesystem");
        assert_eq!(server.transport, McpTransport::Stdio);
        assert_eq!(server.command.as_deref(), Some("npx"));
        assert_eq!(
            server.env.as_ref().and_then(|env| env.get("MCP_DEBUG")),
            Some(&"1".to_owned())
        );
    }

    #[test]
    fn hooks_settings_persist_through_manager_round_trip() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("manager");
        manager
            .update_global(|settings| {
                settings.hooks = Some(vec![HookConfig {
                    event: HookEvent::PreToolCall,
                    matcher: None,
                    command: vec!["/opt/hooks/guard".to_owned()],
                    timeout_ms: Some(1_000),
                    enabled: None,
                    fail_closed: None,
                    extra: Map::new(),
                }]);
            })
            .expect("persist hooks");
        let reloaded = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("reload");
        let settings = reloaded.settings();
        let hooks = settings.hooks.as_ref().expect("hooks persisted");
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].event, HookEvent::PreToolCall);
        assert_eq!(hooks[0].command, vec!["/opt/hooks/guard".to_owned()]);
        assert_eq!(hooks[0].timeout_ms, Some(1_000));
    }

    #[test]
    fn sandbox_settings_persist_through_manager_round_trip() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("manager");
        manager
            .update_global(|settings| {
                settings.sandbox = Some(SandboxSettings {
                    enabled: Some(true),
                    network: Some(false),
                    read_only: Some(true),
                    allowed_paths: Some(vec!["/work".to_owned(), "relative".to_owned()]),
                    denied_paths: Some(vec!["/work/secret".to_owned()]),
                    extra: Map::new(),
                });
            })
            .expect("persist sandbox");
        let reloaded = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("reload");
        let settings = reloaded.settings();
        let sandbox = settings.sandbox.as_ref().expect("sandbox persisted");
        assert_eq!(sandbox.enabled, Some(true));
        assert_eq!(sandbox.network, Some(false));
        assert_eq!(sandbox.read_only, Some(true));
        assert_eq!(
            sandbox.allowed_paths.as_deref(),
            Some(&["/work".to_owned(), "relative".to_owned()][..])
        );
        assert_eq!(sandbox.denied_paths.as_deref(), Some(&["/work/secret".to_owned()][..]));
    }

    #[test]
    fn sandbox_settings_reject_empty_path_entries() {
        let settings = Settings {
            sandbox: Some(SandboxSettings {
                enabled: Some(true),
                network: None,
                read_only: None,
                allowed_paths: Some(vec!["/work".to_owned(), "  ".to_owned()]),
                denied_paths: None,
                extra: Map::new(),
            }),
            ..Default::default()
        };
        let error = validate_settings(&settings, SettingsScope::Global, Path::new("/tmp/settings.json"))
            .expect_err("empty allowed path must fail validation");
        assert!(
            format!("{error:#}").contains("sandbox.allowedPaths"),
            "got: {error:#}"
        );
    }

    fn rule(action: PermissionRuleAction, path: &str) -> PermissionRule {
        PermissionRule {
            action,
            path: path.to_owned(),
            tools: None,
            extra: Map::new(),
        }
    }

    #[test]
    fn permission_rules_parse_and_round_trip_preserving_unknown_fields() {
        let private_path = Path::new("/")
            .join("workspace")
            .join("secrets")
            .to_string_lossy()
            .into_owned();
        let settings: Settings = serde_json::from_value(json!({
            "permissionRules": [
                {
                    "action": "deny",
                    "path": private_path.clone(),
                    "futureField": {"keep": true}
                },
                {"action": "ask", "path": "deploy", "tools": ["write", "edit"]},
                {"action": "allow", "path": "/tmp/build/*"}
            ]
        }))
        .expect("permissionRules parse");
        let rules = &settings.permission_rules;
        assert_eq!(rules.len(), 3);

        assert_eq!(rules[0].action, PermissionRuleAction::Deny);
        assert_eq!(rules[0].path, private_path);
        assert_eq!(rules[0].tools, None);
        assert_eq!(
            rules[0].extra.get("futureField").and_then(|value| value.get("keep")),
            Some(&Value::Bool(true)),
            "unknown rule-entry fields must be retained"
        );

        assert_eq!(rules[1].action, PermissionRuleAction::Ask);
        assert_eq!(
            rules[1].tools.as_deref(),
            Some(&[PermissionTool::Write, PermissionTool::Edit][..])
        );

        assert_eq!(rules[2].action, PermissionRuleAction::Allow);

        // Unknown top-level fields coexist with typed rules through a round trip.
        let settings: Settings = serde_json::from_str(
            r#"{"permissionRules":[{"action":"deny","path":"/x"}],"futureTopLevel":7}"#,
        )
        .expect("parse with extra top-level");
        let serialized = serde_json::to_string(&settings).expect("serialize");
        let reloaded: Settings = serde_json::from_str(&serialized).expect("re-parse");
        assert_eq!(reloaded.permission_rules[0].action, PermissionRuleAction::Deny);
        assert_eq!(reloaded.extra.get("futureTopLevel"), Some(&Value::Number(7.into())));
    }

    #[test]
    fn permission_rules_reject_unknown_actions_and_tools() {
        let error = serde_json::from_str::<Settings>(
            r#"{"permissionRules":[{"action":"maybe","path":"/x"}]}"#,
        )
        .expect_err("unknown action");
        assert!(error.to_string().contains("maybe"), "{error}");
        let error = serde_json::from_str::<Settings>(
            r#"{"permissionRules":[{"action":"deny","path":"/x","tools":["bash"]}]}"#,
        )
        .expect_err("bash is not rule-addressable");
        assert!(error.to_string().contains("bash"), "{error}");
    }

    #[test]
    fn validate_settings_rejects_empty_rule_path_and_empty_tools() {
        let path = PathBuf::from("/tmp/settings.json");
        let empty_path = Settings {
            permission_rules: vec![rule(PermissionRuleAction::Deny, "  ")],
            ..Settings::default()
        };
        let error = validate_settings(&empty_path, SettingsScope::Global, &path)
            .expect_err("empty path must fail");
        assert!(format!("{error:#}").contains("permissionRules[0].path"), "{error:#}");

        let empty_tools = Settings {
            permission_rules: vec![PermissionRule {
                action: PermissionRuleAction::Deny,
                path: "/x".to_owned(),
                tools: Some(Vec::new()),
                extra: Map::new(),
            }],
            ..Settings::default()
        };
        let error = validate_settings(&empty_tools, SettingsScope::Global, &path)
            .expect_err("empty tools must fail");
        assert!(format!("{error:#}").contains("permissionRules[0].tools"), "{error:#}");
    }

    #[test]
    fn permission_verdict_precedence_deny_beats_allow_and_specific_beats_general() {
        let cwd_dir = tempfile::tempdir().expect("cwd");
        let cwd = cwd_dir.path();
        // deny at equal specificity beats allow.
        let verdict = permission_verdict(
            "read",
            &json!({"path": "/a/b/c.txt"}),
            cwd,
            &[
                rule(PermissionRuleAction::Allow, "/a"),
                rule(PermissionRuleAction::Deny, "/a"),
            ],
        );
        assert!(matches!(verdict, PermissionVerdict::Deny(_)), "{verdict:?}");

        // Specific allow beats general deny.
        let verdict = permission_verdict(
            "read",
            &json!({"path": "/a/b/c.txt"}),
            cwd,
            &[
                rule(PermissionRuleAction::Deny, "/a"),
                rule(PermissionRuleAction::Allow, "/a/b"),
            ],
        );
        assert_eq!(verdict, PermissionVerdict::Allow, "more specific path wins");

        // Specific deny beats general allow.
        let verdict = permission_verdict(
            "read",
            &json!({"path": "/a/b/c.txt"}),
            cwd,
            &[
                rule(PermissionRuleAction::Allow, "/a"),
                rule(PermissionRuleAction::Deny, "/a/b"),
            ],
        );
        assert!(matches!(verdict, PermissionVerdict::Deny(_)), "{verdict:?}");

        // ask beats allow at equal specificity; deny beats ask at equal specificity.
        let verdict = permission_verdict(
            "read",
            &json!({"path": "/a/b"}),
            cwd,
            &[
                rule(PermissionRuleAction::Allow, "/a"),
                rule(PermissionRuleAction::Ask, "/a"),
            ],
        );
        assert_eq!(verdict, PermissionVerdict::Ask);
        let verdict = permission_verdict(
            "read",
            &json!({"path": "/a/b"}),
            cwd,
            &[
                rule(PermissionRuleAction::Ask, "/a"),
                rule(PermissionRuleAction::Deny, "/a"),
            ],
        );
        assert!(matches!(verdict, PermissionVerdict::Deny(_)), "{verdict:?}");

        // No matching rule falls through to the capability decision.
        let verdict = permission_verdict(
            "read",
            &json!({"path": "/other/x.txt"}),
            cwd,
            &[rule(PermissionRuleAction::Deny, "/a")],
        );
        assert_eq!(verdict, PermissionVerdict::NoMatch);
    }

    #[test]
    fn permission_verdict_relative_paths_trailing_star_and_component_boundary() {
        let cwd_dir = tempfile::tempdir().expect("cwd");
        let cwd = cwd_dir.path();
        // Relative rule and target both resolve against cwd.
        let verdict = permission_verdict(
            "read",
            &json!({"path": "src/main.rs"}),
            cwd,
            &[rule(PermissionRuleAction::Deny, "src")],
        );
        assert!(matches!(verdict, PermissionVerdict::Deny(_)), "{verdict:?}");

        // Parent-relative targets normalize lexically.
        let verdict = permission_verdict(
            "read",
            &json!({"path": "./src/../src/main.rs"}),
            cwd,
            &[rule(PermissionRuleAction::Deny, "src")],
        );
        assert!(matches!(verdict, PermissionVerdict::Deny(_)), "{verdict:?}");

        // Trailing `*` is an explicit prefix marker with the same semantics.
        let verdict = permission_verdict(
            "read",
            &json!({"path": "build/out/x.txt"}),
            cwd,
            &[rule(PermissionRuleAction::Deny, "build/*")],
        );
        assert!(matches!(verdict, PermissionVerdict::Deny(_)), "{verdict:?}");

        // Component boundary: /a/secret does not match rule /a/secre.
        let verdict = permission_verdict(
            "read",
            &json!({"path": "/a/secret.txt"}),
            cwd,
            &[rule(PermissionRuleAction::Deny, "/a/secre")],
        );
        assert_eq!(verdict, PermissionVerdict::NoMatch);
    }

    #[test]
    fn permission_verdict_tool_allowlist_filters_and_non_file_tools_never_match() {
        let cwd = Path::new("/");
        let rules = [PermissionRule {
            action: PermissionRuleAction::Deny,
            path: "/x".to_owned(),
            tools: Some(vec![PermissionTool::Write, PermissionTool::Edit]),
            extra: Map::new(),
        }];
        // read is not allowlisted: falls through.
        assert_eq!(
            permission_verdict("read", &json!({"path": "/x/a.txt"}), cwd, &rules),
            PermissionVerdict::NoMatch
        );
        // write is allowlisted: denied.
        assert!(matches!(
            permission_verdict("write", &json!({"path": "/x/a.txt"}), cwd, &rules),
            PermissionVerdict::Deny(_)
        ));
        // bash has no rule-addressable path in the MVP.
        assert_eq!(
            permission_verdict("bash", &json!({"command": "echo hi"}), cwd, &rules),
            PermissionVerdict::NoMatch
        );
    }

    #[test]
    fn permission_verdict_internal_uris_are_not_filesystem_targets() {
        let cwd = Path::new("/");
        let rules = [rule(PermissionRuleAction::Deny, "/")];
        assert_eq!(
            permission_verdict("read", &json!({"path": "skill://trusted/asset.txt"}), cwd, &rules),
            PermissionVerdict::NoMatch
        );
        assert_eq!(
            permission_verdict("read", &json!({"path": "agent://abc"}), cwd, &rules),
            PermissionVerdict::NoMatch
        );
    }

    #[test]
    fn permission_verdict_glob_multiple_targets_take_strongest() {
        let cwd = Path::new("/");
        // One denied target poisons the whole call.
        let verdict = permission_verdict(
            "glob",
            &json!({"pattern": "**/*.rs", "path": "/a;/b"}),
            cwd,
            &[
                rule(PermissionRuleAction::Allow, "/a"),
                rule(PermissionRuleAction::Deny, "/b"),
            ],
        );
        assert!(matches!(verdict, PermissionVerdict::Deny(_)), "{verdict:?}");

        // Any ask outranks a mere allow.
        let verdict = permission_verdict(
            "glob",
            &json!({"pattern": "**/*.rs", "path": "/a;/b"}),
            cwd,
            &[
                rule(PermissionRuleAction::Allow, "/a"),
                rule(PermissionRuleAction::Ask, "/b"),
            ],
        );
        assert_eq!(verdict, PermissionVerdict::Ask);

        // Missing path searches the cwd, which rules may match.
        let cwd_dir = tempfile::tempdir().expect("cwd");
        let cwd = cwd_dir.path();
        let cwd_rule = cwd.to_string_lossy();
        let verdict = permission_verdict(
            "grep",
            &json!({"pattern": "needle"}),
            cwd,
            &[rule(PermissionRuleAction::Deny, cwd_rule.as_ref())],
        );
        assert!(matches!(verdict, PermissionVerdict::Deny(_)), "{verdict:?}");
    }

    #[test]
    fn permission_rules_persist_through_manager_round_trip() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let private_path = cwd.path().join("secrets").to_string_lossy().into_owned();
        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("manager");
        manager
            .update_global(|settings| {
                settings.permission_rules = vec![
                    rule(PermissionRuleAction::Deny, &private_path),
                    PermissionRule {
                        action: PermissionRuleAction::Ask,
                        path: "deploy".to_owned(),
                        tools: Some(vec![PermissionTool::Write]),
                        extra: Map::new(),
                    },
                ];
            })
            .expect("persist permission rules");
        let reloaded = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("reload");
        assert_eq!(
            reloaded.permission_rules(),
            vec![
                rule(PermissionRuleAction::Deny, &private_path),
                PermissionRule {
                    action: PermissionRuleAction::Ask,
                    path: "deploy".to_owned(),
                    tools: Some(vec![PermissionTool::Write]),
                    extra: Map::new(),
                },
            ]
        );
    }

    #[test]
    fn permission_rules_project_layer_replaces_global_like_other_array_settings() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        fs::write(
            agent.path().join("settings.json"),
            r#"{"permissionRules":[{"action":"deny","path":"/global-secret"}]}"#,
        )
        .expect("global settings");
        fs::create_dir_all(cwd.path().join(CONFIG_DIR_NAME)).expect("project dir");
        fs::write(
            cwd.path().join(CONFIG_DIR_NAME).join("settings.json"),
            r#"{"permissionRules":[{"action":"allow","path":"/project-public"}]}"#,
        )
        .expect("project settings");

        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("manager");
        manager.load_project(true).expect("trusted project");
        let rules = manager.permission_rules();
        assert_eq!(rules.len(), 1, "project array replaces global: {rules:?}");
        assert_eq!(rules[0].action, PermissionRuleAction::Allow);
        assert_eq!(rules[0].path, "/project-public");
    }

    #[test]
    fn toml_settings_loads_round_trips_and_targets_toml_writes() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let toml = agent.path().join("settings.toml");
        fs::write(
            &toml,
            r#"
defaultProvider = "anthropic"
theme = "dark"
sessionImportSources = ["omp", "grok"]
future = { nested = 1 }

[compaction]
enabled = true
reserveTokens = 2000

[agents.reviewer]
model = "grok/think"
tools = ["read", "grep"]
"#,
        )
        .expect("toml settings");

        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("manager");
        let settings = manager.settings();
        assert_eq!(settings.default_provider.as_deref(), Some("anthropic"));
        assert_eq!(settings.theme.as_deref(), Some("dark"));
        assert_eq!(
            settings.session_import_sources.as_deref(),
            Some(&["omp".to_owned(), "grok".to_owned()][..])
        );
        assert_eq!(
            settings.compaction.as_ref().and_then(|config| config.enabled),
            Some(true)
        );
        assert_eq!(
            settings.compaction.as_ref().and_then(|config| config.reserve_tokens),
            Some(2000)
        );
        let reviewer = settings.agents.get("reviewer").expect("reviewer agent");
        assert_eq!(reviewer.model.as_deref(), Some("grok/think"));
        assert_eq!(
            reviewer.tools.as_deref(),
            Some(&["read".to_owned(), "grep".to_owned()][..])
        );
        assert_eq!(
            settings.extra.get("future").and_then(|value| value.get("nested")),
            Some(&serde_json::json!(1))
        );

        // A settings write targets the TOML file the loader reads, in TOML.
        manager
            .update_global(|settings| settings.theme = Some("light".to_owned()))
            .expect("persist toml settings");
        assert!(
            !agent.path().join("settings.json").exists(),
            "no canonical JSON file is created next to settings.toml"
        );
        let written = fs::read_to_string(&toml).expect("read toml settings");
        assert!(written.contains("theme = \"light\""), "TOML write: {written}");
        assert!(
            written.contains("defaultProvider = \"anthropic\""),
            "TOML write keeps other fields: {written}"
        );

        let reloaded = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("reload");
        assert_eq!(reloaded.settings().theme.as_deref(), Some("light"));
        assert_eq!(reloaded.settings().default_provider.as_deref(), Some("anthropic"));
    }

    #[test]
    fn json_settings_load_and_write_unchanged_without_toml_sibling() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        fs::write(
            agent.path().join("settings.json"),
            r#"{"defaultProvider":"openai","theme":"dark","extensions":["a","b"]}"#,
        )
        .expect("json settings");

        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("manager");
        assert_eq!(manager.settings().default_provider.as_deref(), Some("openai"));
        assert_eq!(manager.settings().theme.as_deref(), Some("dark"));
        assert_eq!(manager.settings().extensions, vec!["a".to_owned(), "b".to_owned()]);

        manager
            .update_global(|settings| settings.theme = Some("light".to_owned()))
            .expect("persist json settings");
        let written = fs::read_to_string(agent.path().join("settings.json")).expect("read json settings");
        let parsed: Value = serde_json::from_str(&written).expect("write stays JSON");
        assert_eq!(parsed["theme"], "light");
        assert_eq!(parsed["defaultProvider"], "openai");
    }

    #[test]
    fn settings_toml_sibling_wins_over_settings_json() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        fs::write(agent.path().join("settings.json"), r#"{"theme":"json"}"#).expect("json settings");
        fs::write(agent.path().join("settings.toml"), "theme = \"toml\"\n").expect("toml settings");
        let manager = SettingsManager::load_phase_one(cwd.path(), agent.path()).expect("manager");
        assert_eq!(manager.settings().theme.as_deref(), Some("toml"));

        // Without the TOML sibling the canonical JSON file is used.
        let json_only = tempfile::tempdir().expect("agent dir");
        fs::write(json_only.path().join("settings.json"), r#"{"theme":"json"}"#).expect("json settings");
        let manager = SettingsManager::load_phase_one(cwd.path(), json_only.path()).expect("manager");
        assert_eq!(manager.settings().theme.as_deref(), Some("json"));
    }

    #[test]
    fn settings_format_follows_extension_never_content() {
        // A .json file containing TOML syntax must fail as JSON.
        let json_dir = tempfile::tempdir().expect("dir");
        let json = json_dir.path().join("settings.json");
        fs::write(&json, "theme = \"dark\"\n").expect("write toml-shaped content");
        let error = load_settings_file(&json, SettingsScope::Global)
            .expect_err("JSON extension with TOML content must fail");
        assert!(format!("{error:#}").contains("Failed to parse settings.json"), "{error:#}");

        // A .toml file containing JSON syntax must fail as TOML.
        let toml_dir = tempfile::tempdir().expect("dir");
        let toml = toml_dir.path().join("settings.toml");
        fs::write(&toml, r#"{"theme":"dark"}"#).expect("write json-shaped content");
        let error = load_settings_file(&toml, SettingsScope::Global)
            .expect_err("TOML extension with JSON content must fail");
        assert!(format!("{error:#}").contains("Failed to parse settings.toml"), "{error:#}");

        // Extensionless files parse as JSON.
        let bare_dir = tempfile::tempdir().expect("dir");
        let bare = bare_dir.path().join("settings");
        fs::write(&bare, r#"{"theme":"dark"}"#).expect("write extensionless settings");
        let settings = load_settings_file(&bare, SettingsScope::Global).expect("extensionless parses as JSON");
        assert_eq!(settings.theme.as_deref(), Some("dark"));

        // Non-canonical names are used verbatim: a sibling .toml does not win.
        let custom_dir = tempfile::tempdir().expect("dir");
        fs::write(custom_dir.path().join("custom.json"), r#"{"theme":"json"}"#).expect("custom json");
        fs::write(custom_dir.path().join("custom.toml"), "theme = \"toml\"\n").expect("custom toml");
        let settings = load_settings_file(&custom_dir.path().join("custom.json"), SettingsScope::Global)
            .expect("non-canonical names are used verbatim");
        assert_eq!(settings.theme.as_deref(), Some("json"));
    }

    #[test]
    fn env_expansion_applies_to_toml_and_json_string_values() {
        let dir = tempfile::tempdir().expect("dir");
        let toml = dir.path().join("settings.toml");
        fs::write(
            &toml,
            r#"
defaultProvider = "$PROVIDER"
theme = "${THEME_DIR}/dark"
extensions = ["$EXT_ONE", "${EXT_TWO}"]
"#,
        )
        .expect("toml settings");
        let lookup = |name: &str| match name {
            "PROVIDER" => Some("openai".to_owned()),
            "THEME_DIR" => Some("/themes".to_owned()),
            "EXT_ONE" => Some("one".to_owned()),
            "EXT_TWO" => Some("two".to_owned()),
            _ => None,
        };
        // Loading keeps the pre-expansion document literal so writes never
        // persist expanded values...
        let settings = load_settings_file(&toml, SettingsScope::Global).expect("load");
        assert_eq!(settings.default_provider.as_deref(), Some("$PROVIDER"));
        assert_eq!(settings.theme.as_deref(), Some("${THEME_DIR}/dark"));
        assert_eq!(
            settings.extensions,
            vec!["$EXT_ONE".to_owned(), "${EXT_TWO}".to_owned()]
        );
        // ...and projecting the runtime view resolves them.
        let expanded = expand_settings_with(&settings, &lookup);
        assert_eq!(expanded.default_provider.as_deref(), Some("openai"));
        assert_eq!(expanded.theme.as_deref(), Some("/themes/dark"));
        assert_eq!(expanded.extensions, vec!["one".to_owned(), "two".to_owned()]);
    }

    #[test]
    fn env_expansion_reaches_nested_string_collections() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("settings.json");
        let project = tempfile::tempdir().expect("project dir");
        let project_root = project.path().to_string_lossy().into_owned();
        fs::write(
            &path,
            r#"{
                "defaultProvider": "x",
                "hooks": [{"event": "session_start", "command": ["$HOOK_BIN", "run", "${HOOK_ARGS}"]}],
                "sandbox": {"allowedPaths": ["$PROJECT_ROOT", "${CACHE_DIR}/pi"]},
                "mcpServers": [{"name": "local", "command": "$MCP_CMD", "args": ["-y", "${MCP_PKG}"]}]
            }"#,
        )
        .expect("json settings");
        let lookup = |name: &str| match name {
            "HOOK_BIN" => Some("/usr/local/bin/hook".to_owned()),
            "HOOK_ARGS" => Some("--verbose".to_owned()),
            "PROJECT_ROOT" => Some(project_root.clone()),
            "CACHE_DIR" => Some("/var/cache".to_owned()),
            "MCP_CMD" => Some("npx".to_owned()),
            "MCP_PKG" => Some("@modelcontextprotocol/server-filesystem".to_owned()),
            _ => None,
        };
        // Loading keeps the pre-expansion document literal...
        let settings = load_settings_file(&path, SettingsScope::Global).expect("load");
        let hook = settings.hooks.as_ref().expect("hooks").first().expect("hook");
        assert_eq!(hook.command, vec!["$HOOK_BIN".to_owned(), "run".to_owned(), "${HOOK_ARGS}".to_owned()]);
        // ...and projecting the runtime view resolves nested collections.
        let expanded = expand_settings_with(&settings, &lookup);
        let hook = expanded.hooks.as_ref().expect("hooks").first().expect("hook");
        assert_eq!(
            hook.command,
            vec!["/usr/local/bin/hook".to_owned(), "run".to_owned(), "--verbose".to_owned()]
        );
        let allowed = expanded
            .sandbox
            .as_ref()
            .and_then(|sandbox| sandbox.allowed_paths.as_ref())
            .expect("allowed paths");
        assert_eq!(allowed, &[project_root, "/var/cache/pi".to_owned()]);
        let mcp = expanded.mcp_servers.first().expect("mcp server");
        assert_eq!(mcp.command.as_deref(), Some("npx"));
        assert_eq!(
            mcp.args.as_deref(),
            Some(&["-y".to_owned(), "@modelcontextprotocol/server-filesystem".to_owned()][..])
        );
    }

    #[test]
    fn env_expansion_missing_vars_are_left_verbatim_and_reported() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            r#"{
                "defaultProvider": "$MISSING_PROVIDER_XYZ",
                "theme": "${ALSO_MISSING_XYZ}/dark",
                "extensions": ["$MISSING_EXT_XYZ", "stable"]
            }"#,
        )
        .expect("json settings");
        // Loading keeps missing references verbatim in the persisted layer...
        let settings = load_settings_file(&path, SettingsScope::Global).expect("load");
        assert_eq!(settings.default_provider.as_deref(), Some("$MISSING_PROVIDER_XYZ"));
        assert_eq!(settings.theme.as_deref(), Some("${ALSO_MISSING_XYZ}/dark"));
        assert_eq!(
            settings.extensions,
            vec!["$MISSING_EXT_XYZ".to_owned(), "stable".to_owned()]
        );
        // ...and projecting the runtime view reports them once.
        let (_, diagnostics) = capture_settings_diagnostics(|| {
            expand_settings_with(&settings, &|_name| None)
        });
        assert!(
            diagnostics.iter().any(|message| message.contains("MISSING_PROVIDER_XYZ")),
            "missing var is reported once: {diagnostics:?}"
        );
    }

    #[test]
    fn env_expansion_leaves_non_strings_and_literal_dollars_alone() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            r#"{
                "defaultProvider": "costs $5 and ${UNSET_X}",
                "autoRetry": true,
                "maxRetries": 3,
                "temperature": 0.7,
                "sessionTtlDays": 30,
                "compaction": {"reserveTokens": 1000}
            }"#,
        )
        .expect("json settings");
        let settings = load_settings_file(&path, SettingsScope::Global).expect("load");
        assert_eq!(settings.default_provider.as_deref(), Some("costs $5 and ${UNSET_X}"));
        assert_eq!(settings.auto_retry, Some(true));
        assert_eq!(settings.max_retries, Some(3));
        assert_eq!(settings.temperature, Some(0.7));
        assert_eq!(settings.session_ttl_days, Some(30));
        assert_eq!(
            settings.compaction.as_ref().and_then(|config| config.reserve_tokens),
            Some(1000)
        );
    }

    #[test]
    fn env_expanded_settings_keep_override_precedence() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        fs::write(
            agent.path().join("settings.toml"),
            "theme = \"$EXPANDED_THEME\"\n",
        )
        .expect("toml settings");
        let lookup = |name: &str| (name == "EXPANDED_THEME").then(|| "dark".to_owned());
        let manager =
            SettingsManager::load_phase_one_with(cwd.path(), agent.path(), lookup).expect("manager");
        assert_eq!(manager.settings().theme.as_deref(), Some("dark"));

        let mut overrides = Settings::default();
        overrides.theme = Some("cli-theme".to_owned());
        manager.apply_overrides(overrides);
        assert_eq!(manager.settings().theme.as_deref(), Some("cli-theme"));
    }

    #[test]
    fn env_expansion_is_runtime_only_and_never_persisted() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        fs::write(
            agent.path().join("settings.json"),
            r#"{
                "theme": "$THEME_NAME",
                "mcpServers": [{
                    "name": "local",
                    "command": "npx",
                    "env": {"MCP_TOKEN": "$MCP_TOKEN", "STATIC": "plain"}
                }]
            }"#,
        )
        .expect("agent settings");
        let lookup = |name: &str| match name {
            "THEME_NAME" => Some("nord".to_owned()),
            "MCP_TOKEN" => Some("supersecret-value".to_owned()),
            _ => None,
        };
        let manager =
            SettingsManager::load_phase_one_with(cwd.path(), agent.path(), lookup).expect("manager");

        // The runtime view (consumed by sessions) is expanded.
        let runtime = manager.settings();
        assert_eq!(runtime.theme.as_deref(), Some("nord"));
        let mcp = runtime.mcp_servers.first().expect("mcp server");
        assert_eq!(
            mcp.env.as_ref().and_then(|env| env.get("MCP_TOKEN")).map(String::as_str),
            Some("supersecret-value")
        );

        // The persistence view keeps the literal references.
        let persisted = manager.global_settings();
        assert_eq!(persisted.theme.as_deref(), Some("$THEME_NAME"));
        let mcp = persisted.mcp_servers.first().expect("mcp server");
        assert_eq!(
            mcp.env.as_ref().and_then(|env| env.get("MCP_TOKEN")).map(String::as_str),
            Some("$MCP_TOKEN")
        );

        // Applying an unrelated edit (theme) must not persist expanded
        // secrets: the file keeps the $VAR literal and no expanded token.
        manager
            .update_global(|settings| settings.theme = Some("solarized".to_owned()))
            .expect("persist theme");
        let on_disk =
            fs::read_to_string(agent.path().join("settings.json")).expect("read settings file");
        assert!(
            on_disk.contains("$MCP_TOKEN"),
            "settings file must keep the env reference literal:\n{on_disk}"
        );
        assert!(
            !on_disk.contains("supersecret-value"),
            "settings file must not contain the expanded secret:\n{on_disk}"
        );
        assert!(
            on_disk.contains("solarized"),
            "the applied edit must be persisted:\n{on_disk}"
        );

        // Settings writes are owner-only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(agent.path().join("settings.json"))
                .expect("settings metadata")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "settings.json must be written owner-only (0600), got {mode:#o}"
            );
        }
    }

    #[test]
    fn reload_keeps_literal_references_in_the_persistence_view() {
        let agent = tempfile::tempdir().expect("agent dir");
        let cwd = tempfile::tempdir().expect("cwd");
        fs::write(
            agent.path().join("settings.json"),
            r#"{"mcpServers":[{"name":"local","command":"npx","env":{"TOKEN":"$TOKEN"}}]}"#,
        )
        .expect("agent settings");
        let lookup = |name: &str| (name == "TOKEN").then(|| "secret".to_owned());
        let manager =
            SettingsManager::load_phase_one_with(cwd.path(), agent.path(), lookup).expect("manager");
        manager.reload().expect("reload");
        let persisted = manager.global_settings();
        let mcp = persisted.mcp_servers.first().expect("mcp server");
        assert_eq!(
            mcp.env.as_ref().and_then(|env| env.get("TOKEN")).map(String::as_str),
            Some("$TOKEN"),
            "reload must keep the literal reference in the persistence view"
        );
        let runtime = manager.settings();
        let mcp = runtime.mcp_servers.first().expect("mcp server");
        assert_eq!(
            mcp.env.as_ref().and_then(|env| env.get("TOKEN")).map(String::as_str),
            Some("secret"),
            "reload must keep the expanded value in the runtime view"
        );
    }
}
