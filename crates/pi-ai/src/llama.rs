//! llama.cpp router and Hugging Face GGUF protocol clients.
//!
//! Inference deliberately reuses the existing `openai-completions` provider:
//! this module only discovers router models and drives router management APIs.

use std::{collections::BTreeMap, path::PathBuf, sync::OnceLock, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use reqwest::{Client, RequestBuilder, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{API_OPENAI_COMPLETIONS, Model, ModelCost};

pub const LLAMA_PROVIDER_ID: &str = "llama.cpp";
pub const DEFAULT_LLAMA_SERVER_URL: &str = "http://127.0.0.1:8080";
pub const DEFAULT_HUGGING_FACE_URL: &str = "https://huggingface.co";

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlamaRouterSettings {
    pub base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

impl std::fmt::Debug for LlamaRouterSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LlamaRouterSettings")
            .field("base_url", &self.base_url)
            .field("api_key_configured", &self.api_key.is_some())
            .finish()
    }
}

impl LlamaRouterSettings {
    pub fn validated(base_url: &str, api_key: Option<String>) -> Result<Self> {
        Ok(Self {
            base_url: normalize_llama_server_url(base_url)?,
            api_key: api_key.filter(|key| !key.trim().is_empty()),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlamaModelStatusValue {
    Unloaded,
    Loading,
    Loaded,
    Downloading,
    Sleeping,
    Other(String),
}

impl Serialize for LlamaModelStatusValue {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for LlamaModelStatusValue {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "unloaded" => Self::Unloaded,
            "loading" => Self::Loading,
            "loaded" => Self::Loaded,
            "downloading" => Self::Downloading,
            "sleeping" => Self::Sleeping,
            _ => Self::Other(value),
        })
    }
}

impl LlamaModelStatusValue {
    #[must_use]
    pub fn is_loaded(&self) -> bool {
        matches!(self, Self::Loaded | Self::Sleeping)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Unloaded => "unloaded",
            Self::Loading => "loading",
            Self::Loaded => "loaded",
            Self::Downloading => "downloading",
            Self::Sleeping => "sleeping",
            Self::Other(value) => value,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlamaModelStatus {
    pub value: LlamaModelStatusValue,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub failed: bool,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub progress: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LlamaArchitecture {
    #[serde(default)]
    pub input_modalities: Vec<String>,
    #[serde(default)]
    pub output_modalities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LlamaModelMeta {
    #[serde(default)]
    pub n_ctx: Option<i64>,
    #[serde(default)]
    pub n_ctx_train: Option<i64>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub ftype: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlamaRouterModel {
    pub id: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub status: LlamaModelStatus,
    #[serde(default)]
    pub architecture: Option<LlamaArchitecture>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub meta: Option<LlamaModelMeta>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct LlamaRouterModelsResponse {
    data: Vec<LlamaRouterModel>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct OpenAiModelsResponse {
    data: Vec<LlamaLiveModel>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct LlamaLiveModel {
    id: String,
    #[serde(default)]
    architecture: Option<LlamaArchitecture>,
    #[serde(default)]
    meta: Option<LlamaModelMeta>,
}

#[derive(Clone, Debug)]
pub struct LlamaRouterClient {
    settings: LlamaRouterSettings,
    client: Client,
}

impl LlamaRouterClient {
    pub fn new(settings: LlamaRouterSettings) -> Result<Self> {
        let settings = LlamaRouterSettings::validated(&settings.base_url, settings.api_key)?;
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .context("building llama.cpp HTTP client")?;
        Ok(Self { settings, client })
    }

    #[must_use]
    pub fn settings(&self) -> &LlamaRouterSettings {
        &self.settings
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.settings.base_url, path)
    }

    fn request(&self, builder: RequestBuilder) -> RequestBuilder {
        match self.settings.api_key.as_deref() {
            Some(key) if !key.trim().is_empty() => builder.bearer_auth(key),
            _ => builder,
        }
    }

    async fn response_json(&self, request: RequestBuilder, operation: &str) -> Result<Value> {
        let response = self
            .request(request)
            .send()
            .await
            .with_context(|| format!("{operation} at {}", self.settings.base_url))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .with_context(|| format!("reading llama.cpp response while {operation}"))?;
        let payload = serde_json::from_slice::<Value>(&bytes).ok();
        if !status.is_success() {
            let message = payload
                .as_ref()
                .and_then(|value| value.get("error"))
                .and_then(|error| error.get("message").or(Some(error)))
                .and_then(Value::as_str)
                .filter(|message| !message.is_empty())
                .map_or_else(
                    || format!("llama.cpp returned HTTP {status}"),
                    ToOwned::to_owned,
                );
            bail!("{operation}: {message}");
        }
        payload.ok_or_else(|| anyhow!("{operation}: llama.cpp returned invalid JSON"))
    }
    async fn response_empty_ok(&self, request: RequestBuilder, operation: &str) -> Result<()> {
        let response = self
            .request(request)
            .send()
            .await
            .with_context(|| format!("{operation} at {}", self.settings.base_url))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let payload = response.json::<Value>().await.ok();
        let message = payload
            .as_ref()
            .and_then(|value| value.get("error"))
            .and_then(|error| error.get("message").or(Some(error)))
            .and_then(Value::as_str)
            .filter(|message| !message.is_empty())
            .map_or_else(
                || format!("llama.cpp returned HTTP {status}"),
                ToOwned::to_owned,
            );
        bail!("{operation}: {message}")
    }

    /// Discover models exposed for inference by the router's OpenAI-compatible
    /// `/v1/models` endpoint. An empty response remains empty; no placeholder
    /// model is ever manufactured.
    pub async fn discover_models(&self) -> Result<Vec<Model>> {
        let operation = "discovering live llama.cpp models";
        let payload = self
            .response_json(self.client.get(self.endpoint("/v1/models")), operation)
            .await?;
        let response: OpenAiModelsResponse = serde_json::from_value(payload)
            .with_context(|| format!("{operation}: response must contain a data array"))?;
        let inference_url = llama_inference_url(&self.settings.base_url)?;
        let mut models = Vec::with_capacity(response.data.len());
        for live in response.data {
            if live.id.trim().is_empty() {
                bail!("{operation}: response contained an empty model id");
            }
            models.push(live_model_to_pi_model(live, &inference_url));
        }
        Ok(models)
    }

    /// Return the router management catalog from `/models`. Unlike inference
    /// discovery, this requires router status objects and therefore rejects a
    /// plain llama-server that is not running in router mode.
    pub async fn list_router_models(&self, reload: bool) -> Result<Vec<LlamaRouterModel>> {
        let suffix = if reload {
            "/models?reload=1"
        } else {
            "/models"
        };
        let operation = "listing llama.cpp router models";
        let payload = self
            .response_json(self.client.get(self.endpoint(suffix)), operation)
            .await?;
        let response: LlamaRouterModelsResponse = serde_json::from_value(payload)
            .with_context(|| format!("{operation}: server is not running in router mode"))?;
        if response.data.iter().any(|model| model.id.trim().is_empty()) {
            bail!("{operation}: response contained an empty model id");
        }
        Ok(response.data)
    }

    pub async fn load(&self, model: &str) -> Result<()> {
        let operation = format!("loading llama.cpp model {model:?}");
        self.response_empty_ok(
            self.client
                .post(self.endpoint("/models/load"))
                .json(&serde_json::json!({ "model": model })),
            &operation,
        )
        .await?;
        Ok(())
    }

    pub async fn unload(&self, model: &str) -> Result<()> {
        let operation = format!("unloading llama.cpp model {model:?}");
        self.response_empty_ok(
            self.client
                .post(self.endpoint("/models/unload"))
                .json(&serde_json::json!({ "model": model })),
            &operation,
        )
        .await?;
        Ok(())
    }
}

fn live_model_to_pi_model(live: LlamaLiveModel, inference_url: &str) -> Model {
    let context_window = live
        .meta
        .as_ref()
        .and_then(|meta| meta.n_ctx.or(meta.n_ctx_train))
        .filter(|window| *window > 0)
        .unwrap_or(128_000);
    let has_images = live.architecture.as_ref().is_some_and(|architecture| {
        architecture
            .input_modalities
            .iter()
            .any(|value| value == "image")
    });
    Model {
        name: live.id.clone(),
        id: live.id,
        api: API_OPENAI_COMPLETIONS.to_owned(),
        provider: LLAMA_PROVIDER_ID.to_owned(),
        base_url: inference_url.to_owned(),
        reasoning: false,
        input: if has_images {
            vec!["text".to_owned(), "image".to_owned()]
        } else {
            vec!["text".to_owned()]
        },
        cost: ModelCost::default(),
        context_window,
        max_tokens: context_window,
        compat: Some(serde_json::json!({
            "supportsStore": false,
            "supportsDeveloperRole": false,
            "supportsReasoningEffort": false,
            "supportsUsageInStreaming": false,
            "supportsStrictMode": false,
            "maxTokensField": "max_tokens"
        })),
        ..Model::default()
    }
}

pub fn normalize_llama_server_url(value: &str) -> Result<String> {
    let mut url = Url::parse(value.trim()).context("invalid llama.cpp server URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("llama.cpp server URL must use http or https");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("llama.cpp server URL must not contain embedded credentials");
    }
    url.set_query(None);
    url.set_fragment(None);
    let mut path = url.path().trim_end_matches('/').to_owned();
    if path.ends_with("/v1") {
        path.truncate(path.len() - 3);
    }
    url.set_path(if path.is_empty() { "/" } else { &path });
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

pub fn llama_inference_url(server_url: &str) -> Result<String> {
    Ok(format!("{}/v1", normalize_llama_server_url(server_url)?))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HuggingFaceModel {
    pub id: String,
    pub downloads: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HuggingFaceGgufFile {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub quantization: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HuggingFaceQuantization {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    pub files: Vec<HuggingFaceGgufFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HuggingFaceModelDetails {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gated: Option<String>,
    pub quantizations: Vec<HuggingFaceQuantization>,
}

#[derive(Clone)]
pub struct HuggingFaceClient {
    token: Option<String>,
    base_url: String,
    client: Client,
}

impl std::fmt::Debug for HuggingFaceClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HuggingFaceClient")
            .field("base_url", &self.base_url)
            .field("token_configured", &self.token.is_some())
            .finish()
    }
}

impl HuggingFaceClient {
    pub fn new(token: Option<String>, base_url: Option<&str>) -> Result<Self> {
        let raw_base = base_url.unwrap_or(DEFAULT_HUGGING_FACE_URL).trim();
        let parsed = Url::parse(raw_base).context("invalid Hugging Face base URL")?;
        if !matches!(parsed.scheme(), "http" | "https") {
            bail!("Hugging Face base URL must use http or https");
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .context("building Hugging Face HTTP client")?;
        Ok(Self {
            token: token.filter(|token| !token.trim().is_empty()),
            base_url: raw_base.trim_end_matches('/').to_owned(),
            client,
        })
    }

    fn request(&self, builder: RequestBuilder) -> RequestBuilder {
        match self.token.as_deref() {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }

    async fn get_json(&self, request: RequestBuilder, operation: &str) -> Result<Value> {
        let response = self
            .request(request)
            .send()
            .await
            .with_context(|| format!("{operation} from Hugging Face"))?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let payload = response.json::<Value>().await.ok();
        if !status.is_success() {
            if status.as_u16() == 429 {
                bail!(
                    "Hugging Face rate limit reached{}",
                    retry_after.map_or_else(String::new, |delay| format!("; retry in {delay}s"))
                );
            }
            let message = payload
                .as_ref()
                .and_then(|value| value.get("error"))
                .and_then(Value::as_str)
                .filter(|message| !message.is_empty())
                .map_or_else(
                    || format!("Hugging Face returned HTTP {status}"),
                    ToOwned::to_owned,
                );
            bail!("{operation}: {message}");
        }
        payload.ok_or_else(|| anyhow!("{operation}: Hugging Face returned invalid JSON"))
    }

    pub async fn search(&self, query: &str) -> Result<Vec<HuggingFaceModel>> {
        let payload = self
            .get_json(
                self.client
                    .get(format!("{}/api/models", self.base_url))
                    .query(&[
                        ("search", query),
                        ("filter", "gguf"),
                        ("sort", "downloads"),
                        ("direction", "-1"),
                        ("limit", "20"),
                    ]),
                "searching GGUF models",
            )
            .await?;
        let values = payload.as_array().ok_or_else(|| {
            anyhow!("searching GGUF models: Hugging Face returned invalid results")
        })?;
        Ok(values
            .iter()
            .filter_map(|value| {
                Some(HuggingFaceModel {
                    id: value.get("id")?.as_str()?.to_owned(),
                    downloads: value
                        .get("downloads")
                        .and_then(Value::as_u64)
                        .unwrap_or_default(),
                })
            })
            .collect())
    }

    pub async fn details(&self, repository: &str) -> Result<HuggingFaceModelDetails> {
        let encoded = encode_repository(repository)?;
        let payload = self
            .get_json(
                self.client
                    .get(format!("{}/api/models/{encoded}?blobs=true", self.base_url)),
                &format!("loading GGUF metadata for {repository:?}"),
            )
            .await?;
        let object = payload.as_object().ok_or_else(|| {
            anyhow!(
                "loading GGUF metadata for {repository:?}: Hugging Face returned invalid details"
            )
        })?;
        let mut grouped: BTreeMap<String, Vec<HuggingFaceGgufFile>> = BTreeMap::new();
        if let Some(siblings) = object.get("siblings").and_then(Value::as_array) {
            for sibling in siblings {
                let Some(name) = sibling.get("rfilename").and_then(Value::as_str) else {
                    continue;
                };
                if !name.to_ascii_lowercase().ends_with(".gguf")
                    || name
                        .rsplit('/')
                        .next()
                        .is_some_and(|base| base.to_ascii_lowercase().starts_with("mmproj"))
                {
                    continue;
                }
                let Some(quantization) = quantization_from_filename(name) else {
                    continue;
                };
                let size = sibling
                    .get("size")
                    .and_then(Value::as_u64)
                    .or_else(|| sibling.get("lfs")?.get("size")?.as_u64());
                let sha256 = sibling
                    .get("lfs")
                    .and_then(|lfs| lfs.get("sha256").or_else(|| lfs.get("oid")))
                    .and_then(Value::as_str)
                    .and_then(normalize_sha256);
                grouped
                    .entry(quantization.clone())
                    .or_default()
                    .push(HuggingFaceGgufFile {
                        name: name.to_owned(),
                        size,
                        sha256,
                        quantization,
                    });
            }
        }
        let mut quantizations = grouped
            .into_iter()
            .map(|(name, mut files)| {
                files.sort_by(|left, right| left.name.cmp(&right.name));
                let size = files.iter().try_fold(0_u64, |total, file| {
                    file.size.and_then(|size| total.checked_add(size))
                });
                HuggingFaceQuantization { name, size, files }
            })
            .collect::<Vec<_>>();
        quantizations.sort_by(|left, right| {
            let left_rank = usize::from(left.name != "Q4_K_M");
            let right_rank = usize::from(right.name != "Q4_K_M");
            left_rank
                .cmp(&right_rank)
                .then_with(|| {
                    left.size
                        .unwrap_or(u64::MAX)
                        .cmp(&right.size.unwrap_or(u64::MAX))
                })
                .then_with(|| left.name.cmp(&right.name))
        });
        let gated = match object.get("gated") {
            Some(Value::String(value)) if matches!(value.as_str(), "auto" | "manual") => {
                Some(value.clone())
            }
            Some(Value::Bool(true)) => Some("auto".to_owned()),
            _ => None,
        };
        Ok(HuggingFaceModelDetails {
            id: object
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(repository)
                .to_owned(),
            gated,
            quantizations,
        })
    }

    pub fn resolve_url(&self, repository: &str, file_name: &str) -> Result<Url> {
        let encoded_repository = encode_repository(repository)?;
        let encoded_file = encode_path(file_name)?;
        Url::parse(&format!(
            "{}/{encoded_repository}/resolve/main/{encoded_file}",
            self.base_url
        ))
        .context("building Hugging Face download URL")
    }

    #[must_use]
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }
}

pub async fn find_hugging_face_token() -> Option<String> {
    if let Some(token) = std::env::var("HF_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return Some(token.trim().to_owned());
    }
    let mut paths = Vec::new();
    if let Some(path) = std::env::var_os("HF_TOKEN_PATH") {
        paths.push(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("HF_HOME") {
        paths.push(PathBuf::from(path).join("token"));
    }
    if let Some(path) = std::env::var_os("XDG_CACHE_HOME") {
        paths.push(PathBuf::from(path).join("huggingface/token"));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    if let Some(home) = home {
        paths.push(PathBuf::from(home).join(".cache/huggingface/token"));
    }
    paths.sort();
    paths.dedup();
    for path in paths {
        if let Ok(token) = tokio::fs::read_to_string(path).await {
            let token = token.trim();
            if !token.is_empty() {
                return Some(token.to_owned());
            }
        }
    }
    None
}

fn quantization_from_filename(file_name: &str) -> Option<String> {
    static QUANTIZATION: OnceLock<Regex> = OnceLock::new();
    static SHARD: OnceLock<Regex> = OnceLock::new();
    let quantization = QUANTIZATION.get_or_init(|| {
        Regex::new(
            r"(?i)(?:^|[-_.])((?:UD-)?(?:IQ\d(?:_[A-Z0-9]+)+|Q\d(?:_[A-Z0-9]+)+|BF16|F16|F32|MXFP\d(?:_[A-Z0-9]+)*))$",
        )
        .expect("static quantization regex")
    });
    let shard = SHARD.get_or_init(|| Regex::new(r"-\d{5}-of-\d{5}$").expect("static shard regex"));
    let base = file_name.rsplit('/').next()?;
    let stem = base
        .strip_suffix(".gguf")
        .or_else(|| base.strip_suffix(".GGUF"))?;
    let stem = shard.replace(stem, "");
    quantization
        .captures(&stem)
        .and_then(|captures| captures.get(1))
        .map(|capture| capture.as_str().to_ascii_uppercase())
}

fn normalize_sha256(value: &str) -> Option<String> {
    let value = value.strip_prefix("sha256:").unwrap_or(value);
    (value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
}

fn encode_repository(repository: &str) -> Result<String> {
    let segments = repository.split('/').collect::<Vec<_>>();
    if segments.len() != 2 || segments.iter().any(|segment| segment.is_empty()) {
        bail!("Hugging Face repository must be OWNER/NAME");
    }
    Ok(segments
        .into_iter()
        .map(percent_encode_segment)
        .collect::<Vec<_>>()
        .join("/"))
}

fn encode_path(path: &str) -> Result<String> {
    if path.starts_with('/') || path.split('/').any(|part| matches!(part, "" | "." | "..")) {
        bail!("GGUF file path must be a relative path without traversal");
    }
    Ok(path
        .split('/')
        .map(percent_encode_segment)
        .collect::<Vec<_>>()
        .join("/"))
}

fn percent_encode_segment(segment: &str) -> String {
    let mut encoded = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}
