use crate::{
    API_PI_MESSAGES, Model, ModelCost, RadiusCatalogStore, ThinkingLevelMap, now_millis,
    replace_dynamic_models,
};
use super::common;
use anyhow::{Result, anyhow};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
};

pub const DEFAULT_RADIUS_GATEWAY: &str = "https://radius.pi.dev";
const MAX_RADIUS_CONFIG_BODY_BYTES: usize = 1024 * 1024;
static RADIUS_PERSIST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RadiusCatalogSnapshot {
    #[serde(default)]
    pub models: Vec<Model>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<i64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadiusGatewayConfig {
    pub base_url: String,
    pub models: Vec<RadiusGatewayModel>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadiusGatewayModel {
    pub id: String,
    pub name: String,
    pub reasoning: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<ThinkingLevelMap>,
    pub input: Vec<String>,
    pub cost: ModelCost,
    pub context_window: i64,
    pub max_tokens: i64,
}
#[derive(Clone, Default)]
pub struct RadiusRefreshOptions {
    pub headers: HashMap<String, String>,
    pub timeout_ms: Option<u64>,
    pub max_retries: usize,
    pub max_retry_delay_ms: Option<u64>,
    pub abort_signal: Option<tokio_util::sync::CancellationToken>,
}

impl std::fmt::Debug for RadiusRefreshOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut header_names = self.headers.keys().map(String::as_str).collect::<Vec<_>>();
        header_names.sort_unstable_by_key(|name| name.to_ascii_lowercase());
        formatter
            .debug_struct("RadiusRefreshOptions")
            .field("header_names", &header_names)
            .field("header_count", &self.headers.len())
            .field("timeout_ms", &self.timeout_ms)
            .field("max_retries", &self.max_retries)
            .field("max_retry_delay_ms", &self.max_retry_delay_ms)
            .field("abort_signal", &self.abort_signal)
            .finish()
    }
}
#[derive(Debug, Clone)]
pub struct RadiusCatalog {
    provider_id: String,
    gateway: String,
    store: Option<RadiusCatalogStore>,
    state: Arc<Mutex<State>>,
}
#[derive(Debug, Clone, Default)]
struct State {
    snapshot: RadiusCatalogSnapshot,
}
fn refresh_headers(
    source: &HashMap<String, String>,
    api_key: &str,
) -> Result<reqwest::header::HeaderMap> {
    let mut headers = reqwest::header::HeaderMap::new();
    common::insert_header(&mut headers, "accept", "application/json")?;
    common::insert_header(&mut headers, "authorization", &format!("Bearer {api_key}"))?;
    common::insert_header_map(&mut headers, source)?;
    Ok(headers)
}

async fn read_config_body(
    response: reqwest::Response,
    abort_signal: Option<&tokio_util::sync::CancellationToken>,
) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RADIUS_CONFIG_BODY_BYTES as u64)
    {
        return Err(anyhow!("Radius config response exceeds {MAX_RADIUS_CONFIG_BODY_BYTES} bytes"));
    }
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();
    loop {
        let next = match abort_signal {
            Some(token) => tokio::select! {
                biased;
                _ = token.cancelled() => return Err(anyhow!("Request was aborted")),
                chunk = chunks.next() => chunk,
            },
            None => chunks.next().await,
        };
        let Some(chunk) = next else { break };
        let chunk = chunk?;
        if body.len().saturating_add(chunk.len()) > MAX_RADIUS_CONFIG_BODY_BYTES {
            return Err(anyhow!("Radius config response exceeds {MAX_RADIUS_CONFIG_BODY_BYTES} bytes"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

impl RadiusCatalog {
    pub fn new(provider_id: impl Into<String>, gateway: impl AsRef<str>) -> Result<Self> {
        let provider_id = provider_id.into();
        if provider_id.trim().is_empty() {
            return Err(anyhow!("Radius provider id must not be empty"));
        }
        Ok(Self {
            provider_id,
            gateway: normalize_radius_gateway_url(gateway.as_ref())?,
            store: None,
            state: Arc::new(Mutex::new(State::default())),
        })
    }
    pub fn with_store(
        provider_id: impl Into<String>,
        gateway: impl AsRef<str>,
        store_path: impl Into<std::path::PathBuf>,
    ) -> Result<Self> {
        let mut catalog = Self::new(provider_id, gateway)?;
        catalog.store = Some(RadiusCatalogStore::new(store_path));
        Ok(catalog)
    }
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }
    pub fn gateway(&self) -> &str {
        &self.gateway
    }
    #[must_use]
    pub fn store_path(&self) -> Option<&std::path::Path> {
        self.store.as_ref().map(RadiusCatalogStore::path)
    }
    pub fn restore_stored_snapshot(&self) -> Result<Option<RadiusCatalogSnapshot>> {
        let Some(store) = &self.store else {
            return Ok(None);
        };
        let Some(snapshot) = store.read(&self.provider_id)? else {
            return Ok(None);
        };
        self.restore_snapshot(snapshot).map(Some)
    }
    pub fn snapshot(&self) -> RadiusCatalogSnapshot {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot
            .clone()
    }
    pub fn restore_snapshot(
        &self,
        snapshot: RadiusCatalogSnapshot,
    ) -> Result<RadiusCatalogSnapshot> {
        let models = validate_snapshot_models(&self.provider_id, snapshot.models)?;
        self.apply(RadiusCatalogSnapshot {
            models,
            checked_at: snapshot.checked_at,
        })
    }
    pub async fn refresh(
        &self,
        api_key: &str,
        options: RadiusRefreshOptions,
    ) -> Result<RadiusCatalogSnapshot> {
        if api_key.trim().is_empty() {
            return Err(anyhow!(
                "No API key provided for provider \"{}\"",
                self.provider_id
            ));
        }
        if options
            .abort_signal
            .as_ref()
            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
        {
            return Err(anyhow!("Request was aborted"));
        }
        let stream = crate::StreamOptions {
            api_key: Some(api_key.into()),
            headers: options.headers.clone(),
            timeout_ms: options.timeout_ms,
            max_retries: options.max_retries,
            max_retry_delay_ms: options.max_retry_delay_ms,
            abort_signal: options.abort_signal.clone(),
            ..Default::default()
        };
        let client = common::client(&stream)?;
        let url = format!("{}/v1/config", self.gateway);
        let headers = refresh_headers(&options.headers, api_key)?;
        let response = common::send_with_retry(&stream, || {
            client.get(&url).headers(headers.clone())
        })
        .await
        .map_err(|e| anyhow!(redact(&e.to_string(), api_key, &options.headers)))?;
        if !response.status().is_success() {
            let message = common::error_body("Radius config", response, &stream).await?;
            return Err(anyhow!(redact(&message, api_key, &options.headers)));
        }
        let bytes = read_config_body(response, options.abort_signal.as_ref()).await?;
        let config: RadiusGatewayConfig = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow!("Invalid Radius config from {}", self.gateway))?;
        let snapshot = RadiusCatalogSnapshot {
            models: models_from_config(&self.provider_id, config)?,
            checked_at: Some(now_millis()),
        };
        self.apply_and_persist(snapshot)
    }
    fn apply(&self, snapshot: RadiusCatalogSnapshot) -> Result<RadiusCatalogSnapshot> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        replace_dynamic_models(&format!("radius:{}", self.provider_id), snapshot.models.clone())?;
        state.snapshot = snapshot.clone();
        Ok(snapshot)
    }
    fn apply_and_persist(&self, snapshot: RadiusCatalogSnapshot) -> Result<RadiusCatalogSnapshot> {
        validate_radius_snapshot(&self.provider_id, &snapshot)?;
        let Some(store) = &self.store else {
            return self.apply(snapshot);
        };
        let _transaction = RADIUS_PERSIST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = self.snapshot();
        self.apply(snapshot.clone())?;
        if let Err(write_error) = store.write(&self.provider_id, &snapshot) {
            return match self.apply(previous) {
                Ok(_) => Err(write_error),
                Err(rollback_error) => Err(anyhow!(
                    "persisting Radius snapshot failed: {write_error:#}; rolling back in-memory catalog failed: {rollback_error:#}"
                )),
            };
        }
        Ok(snapshot)
    }
}
pub fn normalize_radius_gateway_url(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(anyhow!("Radius gateway URL must not be empty"));
    }
    let url = if value
        .get(..7)
        .is_some_and(|v| v.eq_ignore_ascii_case("http://"))
        || value
            .get(..8)
            .is_some_and(|v| v.eq_ignore_ascii_case("https://"))
    {
        value.into()
    } else {
        format!("https://{value}")
    };
    Ok(url.trim_end_matches('/').into())
}
pub fn models_from_config(provider: &str, config: RadiusGatewayConfig) -> Result<Vec<Model>> {
    if config.base_url.trim().is_empty() || config.models.is_empty() {
        return Err(anyhow!(
            "Invalid Radius config: baseUrl and models are required"
        ));
    }
    let mut seen = std::collections::HashSet::new();
    config
        .models
        .into_iter()
        .map(|m| {
            if m.id.trim().is_empty()
                || m.name.trim().is_empty()
                || m.context_window <= 0
                || m.max_tokens <= 0
                || m.max_tokens > m.context_window
                || m.input.is_empty()
                || m.input
                    .iter()
                    .any(|v| !matches!(v.as_str(), "text" | "image"))
            {
                return Err(anyhow!("Invalid Radius model"));
            }
            if !seen.insert(m.id.clone()) {
                return Err(anyhow!("Duplicate Radius model id {}", m.id));
            }
            for price in [
                m.cost.input,
                m.cost.output,
                m.cost.cache_read,
                m.cost.cache_write,
            ] {
                if !price.is_finite() || price < 0.0 {
                    return Err(anyhow!("Invalid Radius model cost"));
                }
            }
            Ok(Model {
                id: m.id,
                name: m.name,
                api: API_PI_MESSAGES.into(),
                provider: provider.into(),
                base_url: config.base_url.clone(),
                reasoning: m.reasoning,
                thinking_level_map: m.thinking_level_map,
                input: m.input,
                cost: m.cost,
                context_window: m.context_window,
                max_tokens: m.max_tokens,
                headers: None,
                compat: None,
            })
        })
        .collect()
}
pub(crate) fn validate_radius_snapshot(
    provider: &str,
    snapshot: &RadiusCatalogSnapshot,
) -> Result<()> {
    if snapshot.checked_at.is_some_and(|checked_at| checked_at < 0) {
        return Err(anyhow!("Radius snapshot checkedAt must not be negative"));
    }
    validate_snapshot_models(provider, snapshot.models.clone()).map(|_| ())
}
fn validate_snapshot_models(provider: &str, models: Vec<Model>) -> Result<Vec<Model>> {
    if models.is_empty() {
        return Err(anyhow!("Radius snapshot must contain models"));
    }
    let mut seen = std::collections::HashSet::new();
    for m in &models {
        let valid_input = !m.input.is_empty()
            && m.input
                .iter()
                .all(|value| matches!(value.as_str(), "text" | "image"));
        let valid_cost = [
            m.cost.input,
            m.cost.output,
            m.cost.cache_read,
            m.cost.cache_write,
        ]
        .into_iter()
        .all(|price| price.is_finite() && price >= 0.0);
        let valid_tiers = m.cost.tiers.iter().all(|tier| {
            tier.input_tokens_above >= 0
                && [tier.input, tier.output, tier.cache_read, tier.cache_write]
                    .into_iter()
                    .all(|price| price.is_finite() && price >= 0.0)
        });
        if m.provider != provider
            || m.api != API_PI_MESSAGES
            || m.id.trim().is_empty()
            || m.name.trim().is_empty()
            || m.base_url.trim().is_empty()
            || m.context_window <= 0
            || m.max_tokens <= 0
            || m.max_tokens > m.context_window
            || m.headers.is_some()
            || m.compat.is_some()
            || !valid_input
            || !valid_cost
            || !valid_tiers
            || !seen.insert(m.id.clone())
        {
            return Err(anyhow!("Invalid Radius snapshot model {}", m.id));
        }
    }
    Ok(models)
}
fn redact(message: &str, secret: &str, headers: &HashMap<String, String>) -> String {
    let mut secrets = vec![secret.trim().to_owned()];
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("authorization")
            || name.eq_ignore_ascii_case("x-api-key")
            || name.eq_ignore_ascii_case("cf-aig-authorization")
        {
            let value = value.trim();
            if !value.is_empty() {
                secrets.push(value.to_owned());
                if let Some(token) = value.strip_prefix("Bearer ").map(str::trim).filter(|token| !token.is_empty()) {
                    secrets.push(token.to_owned());
                }
            }
        }
    }
    secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
    secrets.dedup();
    secrets.into_iter().fold(message.to_owned(), |sanitized, secret| {
        if secret.is_empty() {
            sanitized
        } else {
            let sanitized = sanitized.replace(&format!("Bearer {secret}"), "Bearer [REDACTED]");
            sanitized.replace(&secret, "[REDACTED]")
        }
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn config(id: &str) -> RadiusGatewayConfig {
        RadiusGatewayConfig {
            base_url: "https://stream.test/v1".into(),
            models: vec![RadiusGatewayModel {
                id: id.into(),
                name: id.into(),
                reasoning: true,
                thinking_level_map: None,
                input: vec!["text".into()],
                cost: ModelCost::default(),
                context_window: 32000,
                max_tokens: 4096,
            }],
        }
    }
    #[test]
    fn valid_config_maps_pi_messages() {
        let models = models_from_config("radius-fixture", config("one")).unwrap();
        assert_eq!(models[0].api, API_PI_MESSAGES);
        assert_eq!(models[0].provider, "radius-fixture")
    }
    #[test]
    fn constructing_catalog_adds_no_models() {
        let provider = "radius-empty-fixture";
        let _ = RadiusCatalog::new(provider, DEFAULT_RADIUS_GATEWAY).unwrap();
        assert!(crate::get_models(provider).is_empty())
    }
    #[test]
    fn invalid_restore_retains_stale_snapshot() {
        let provider = "radius-stale-fixture";
        let catalog = RadiusCatalog::new(provider, DEFAULT_RADIUS_GATEWAY).unwrap();
        let snapshot = RadiusCatalogSnapshot {
            models: models_from_config(provider, config("old")).unwrap(),
            checked_at: Some(7),
        };
        catalog.restore_snapshot(snapshot.clone()).unwrap();
        let invalid = RadiusCatalogSnapshot {
            models: vec![Model {
                provider: "foreign".into(),
                ..Model::default()
            }],
            checked_at: None,
        };
        assert!(catalog.restore_snapshot(invalid).is_err());
        assert_eq!(catalog.snapshot(), snapshot);
        assert!(crate::get_model(provider, "old").is_some())
    }
}
