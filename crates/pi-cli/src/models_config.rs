//! Upstream-compatible custom `models.json` and `auth.json` request configuration.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, RwLock};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::Value;

use pi_ai::{
    Model, ModelCost, ThinkingLevelMap, builtin_models, get_env_api_key, load_builtin_models,
    replace_registered_models,
};

const ENV_AGENT_DIR: &str = "PI_CODING_AGENT_DIR";
const MAX_CONFIG_FILE_BYTES: usize = 8 * 1024 * 1024;
type ModelKey = (String, String);

#[derive(Clone, Default)]
struct ProviderAuthConfig {
    api_key: Option<String>,
    headers: HashMap<String, String>,
    model_headers: HashMap<String, HashMap<String, String>>,
    auth_header: bool,
}

#[derive(Clone)]
enum StoredCredential {
    ApiKey {
        key: String,
        env: HashMap<String, String>,
    },
    OAuth,
}

#[derive(Default)]
struct ConfigState {
    providers: HashMap<String, ProviderAuthConfig>,
    stored_credentials: HashMap<String, StoredCredential>,
    runtime_keys: HashMap<String, String>,
    owned_models: Vec<ModelKey>,
    auth_path: Option<PathBuf>,
}

static STATE: LazyLock<RwLock<ConfigState>> = LazyLock::new(|| RwLock::new(ConfigState::default()));

thread_local! {
    static AGENT_DIR_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

#[derive(Clone, PartialEq, Eq)]
pub struct ModelRequestAuth {
    pub api_key: String,
    pub headers: HashMap<String, String>,
    pub env: HashMap<String, String>,
    pub stored_oauth: bool,
    pub available_model_ids: Option<Vec<String>>,
}

impl std::fmt::Debug for ModelRequestAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelRequestAuth")
            .field("has_api_key", &!self.api_key.is_empty())
            .field("header_count", &self.headers.len())
            .field("env_count", &self.env.len())
            .field("stored_oauth", &self.stored_oauth)
            .field(
                "available_model_count",
                &self.available_model_ids.as_ref().map(Vec::len),
            )
            .finish()
    }
}
impl From<ModelRequestAuth> for pi_coding::RequestAuth {
    fn from(auth: ModelRequestAuth) -> Self {
        Self {
            api_key: auth.api_key,
            headers: auth.headers,
            env: auth.env,
            available_model_ids: auth.available_model_ids,
        }
    }
}

#[must_use]
pub fn session_auth_resolver(explicit_key: Option<String>) -> pi_coding::SessionAuthResolver {
    std::sync::Arc::new(move |model| {
        let explicit_key = explicit_key.clone();
        Box::pin(async move {
            resolve_available_model_request_auth_async(&model, explicit_key.as_deref(), None)
                .await
                .map(Into::into)
        })
    })
}

#[must_use]
pub fn model_is_available_for_request_auth(model: &Model, auth: &ModelRequestAuth) -> bool {
    pi_ai::model_is_available_for_credential(model, auth.available_model_ids.as_deref())
}

pub fn ensure_model_available_for_request_auth(
    model: &Model,
    auth: &ModelRequestAuth,
) -> Result<()> {
    if model_is_available_for_request_auth(model, auth) {
        Ok(())
    } else {
        bail!(
            "Model {}/{} is not available for the resolved credential",
            model.provider,
            model.id
        )
    }
}

pub async fn resolve_available_model_request_auth_async(
    model: &Model,
    explicit_key: Option<&str>,
    env: Option<&HashMap<String, String>>,
) -> Result<ModelRequestAuth> {
    let auth = resolve_model_request_auth_async(model, explicit_key, env).await?;
    ensure_model_available_for_request_auth(model, &auth)?;
    Ok(auth)
}

pub async fn filter_models_for_resolved_auth_async(
    models: Vec<Model>,
    env: Option<&HashMap<String, String>>,
) -> Vec<Model> {
    let Some(probe) = models.iter().find(|model| model.provider == "github-copilot") else {
        return models;
    };
    let filter = if has_configured_auth(probe) {
        resolve_model_request_auth_async(probe, None, env)
            .await
            .ok()
            .map(|auth| auth.available_model_ids)
    } else {
        None
    };
    models
        .into_iter()
        .filter(|model| {
            if model.provider != "github-copilot" {
                return true;
            }
            match &filter {
                Some(Some(ids)) => pi_ai::model_is_available_for_credential(model, Some(ids)),
                Some(None) => true,
                None => false,
            }
        })
        .collect()
}

/// Resolve the configured `models.json` path. Without an explicit config
/// directory or a home directory, implicit custom configuration is disabled;
/// the current working directory is never trusted as a fallback.
#[must_use]
pub fn models_json_path() -> Option<PathBuf> {
    agent_config_path("models.json")
}

/// Resolve the configured `auth.json` path using the same agent directory as
/// `models.json`.
#[must_use]
pub fn auth_json_path() -> Option<PathBuf> {
    agent_config_path("auth.json")
}

fn agent_config_path(file_name: &str) -> Option<PathBuf> {
    let override_dir = AGENT_DIR_OVERRIDE.with(|override_dir| override_dir.borrow().clone());
    let env_dir = std::env::var_os(ENV_AGENT_DIR)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    resolve_agent_config_path(override_dir, env_dir, home, file_name)
}

#[must_use]
pub fn resolve_models_json_path(
    override_dir: Option<PathBuf>,
    env_dir: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    resolve_agent_config_path(override_dir, env_dir, home, "models.json")
}

#[must_use]
pub fn resolve_auth_json_path(
    override_dir: Option<PathBuf>,
    env_dir: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    resolve_agent_config_path(override_dir, env_dir, home, "auth.json")
}

fn resolve_agent_config_path(
    override_dir: Option<PathBuf>,
    env_dir: Option<PathBuf>,
    home: Option<PathBuf>,
    file_name: &str,
) -> Option<PathBuf> {
    override_dir
        .or(env_dir)
        .or_else(|| home.map(|directory| directory.join(".pi").join("agent")))
        .map(|directory| directory.join(file_name))
}
#[must_use]
pub fn radius_catalog_store_path() -> Option<PathBuf> {
    agent_config_path("models-store.json")
}

pub async fn load_radius_catalog(
    allow_network: bool,
) -> Result<Option<pi_ai::providers::RadiusCatalogSnapshot>> {
    let Some(store_path) = radius_catalog_store_path() else {
        return Ok(None);
    };
    load_radius_catalog_from(&store_path, allow_network).await
}

pub async fn load_radius_catalog_from(
    store_path: &Path,
    allow_network: bool,
) -> Result<Option<pi_ai::providers::RadiusCatalogSnapshot>> {
    let catalog = pi_ai::providers::RadiusCatalog::with_store(
        "radius",
        pi_ai::providers::DEFAULT_RADIUS_GATEWAY,
        store_path,
    )?;
    let restored = catalog.restore_stored_snapshot_quarantining_invalid()?;
    if !allow_network {
        return Ok(restored);
    }
    let Some(api_key) = resolve_provider_api_key_async("radius", None).await? else {
        return Ok(restored);
    };
    match catalog
        .refresh(&api_key, pi_ai::providers::RadiusRefreshOptions::default())
        .await
    {
        Ok(snapshot) => Ok(Some(snapshot)),
        Err(_) if restored.is_some() => Ok(restored),
        Err(error) => Err(error),
    }
}

/// Reload `models.json` and `auth.json` from the configured agent directory as
/// one snapshot. Missing files are treated as empty configuration.
pub fn load_custom_models() -> Result<()> {
    match models_json_path() {
        Some(path) => load_custom_models_from(&path),
        None => apply_snapshot(HashMap::new(), HashMap::new(), Vec::new(), None),
    }?;
    if pi_ai::get_models(pi_ai::LLAMA_PROVIDER_ID).is_empty() {
        pi_coding::LlamaManager::default().load_cached_catalog()?;
    }
    Ok(())
}

/// Reload a `models.json` path and the sibling `auth.json` as one snapshot.
pub fn load_custom_models_from(path: &Path) -> Result<()> {
    let auth_path = path.with_file_name("auth.json");
    let stored_credentials = load_stored_credentials(&auth_path)?;
    let content = read_bounded_config(path, "models.json")?.unwrap_or_default();
    let config = if content.is_empty() {
        ModelsConfig::default()
    } else {
        serde_json::from_str(&strip_json_comments(&content))
            .with_context(|| format!("Failed to parse models.json\nFile: {}", path.display()))?
    };
    let (providers, models) = build_snapshot(&config, path)?;
    apply_snapshot(providers, stored_credentials, models, Some(auth_path))
}

pub fn set_runtime_api_key(provider: &str, key: &str) {
    let mut state = state_write();
    if key.trim().is_empty() {
        state.runtime_keys.remove(provider);
    } else {
        state
            .runtime_keys
            .insert(provider.to_owned(), key.to_owned());
    }
}

#[must_use]
pub fn runtime_api_key(provider: &str) -> Option<String> {
    state_read().runtime_keys.get(provider).cloned()
}
pub fn clear_runtime_api_key(provider: &str) {
    state_write().runtime_keys.remove(provider);
}
pub async fn resolve_provider_api_key_async(
    provider: &str,
    env: Option<&HashMap<String, String>>,
) -> Result<Option<String>> {
    let (runtime_key, stored, config, auth_path) = {
        let state = state_read();
        (
            state.runtime_keys.get(provider).cloned(),
            state.stored_credentials.get(provider).cloned(),
            state.providers.get(provider).cloned(),
            state.auth_path.clone(),
        )
    };
    if let Some(key) = runtime_key.filter(|key| !key.trim().is_empty()) {
        return Ok(Some(key));
    }
    if let Some(stored) = stored {
        match stored {
            StoredCredential::ApiKey { key, env: stored_env } => {
                if let Some(key) = resolve_stored_api_key(&key, &stored_env, provider, env)? {
                    return Ok(Some(key));
                }
            }
            StoredCredential::OAuth => {
                let path = auth_path.ok_or_else(|| anyhow!("auth.json path is unavailable"))?;
                return Ok(pi_coding::AuthManager::new(path)?
                    .resolve_stored(provider, env)
                    .await?
                    .map(|auth| auth.api_key)
                    .filter(|key| !key.trim().is_empty()));
            }
        }
    }
    if let Some(key) = resolve_configured_api_key(config.as_ref(), provider, env)? {
        return Ok(Some(key));
    }
    Ok(get_env_api_key(provider, env))
}

#[must_use]
pub fn has_configured_auth(model: &Model) -> bool {
    let state = state_read();
    if model.api == pi_ai::API_FAUX {
        return true;
    }
    if model.provider == pi_ai::LLAMA_PROVIDER_ID {
        return pi_coding::ProviderAuthAdapter::is_configured(
            &pi_coding::LlamaAuthAdapter::default(),
        )
        .unwrap_or(false);
    }
    let config = state.providers.get(&model.provider);
    let has_key = state
        .runtime_keys
        .get(&model.provider)
        .is_some_and(|key| !key.trim().is_empty())
        || state.stored_credentials.get(&model.provider).is_some_and(
            |credential| match credential {
                StoredCredential::ApiKey { key, .. } => !key.trim().is_empty(),
                StoredCredential::OAuth => true,
            },
        )
        || config
            .and_then(|provider| provider.api_key.as_deref())
            .is_some_and(|key| !key.trim().is_empty())
        || get_env_api_key(&model.provider, None).is_some();
    if has_key || config.is_some_and(|provider| provider.auth_header) {
        return true;
    }
    let mut headers = model.headers.clone().unwrap_or_default();
    if let Some(provider) = config {
        merge_headers_case_insensitive(&mut headers, &provider.headers);
        if let Some(model_headers) = provider.model_headers.get(&model.id) {
            merge_headers_case_insensitive(&mut headers, model_headers);
        }
    }
    has_recognized_auth_header(&model.api, &headers)
}
/// Resolve request-scoped custom authentication. Stored credentials and
/// header templates are expanded here, not at catalog load, so request-time
/// environment values are honored.
pub fn resolve_model_request_auth(
    model: &Model,
    explicit_key: Option<&str>,
    env: Option<&HashMap<String, String>>,
) -> Result<ModelRequestAuth> {
    if model.provider == pi_ai::LLAMA_PROVIDER_ID {
        let adapter = pi_coding::LlamaAuthAdapter::default();
        let api_key = explicit_key
            .filter(|key| !key.trim().is_empty())
            .map(ToOwned::to_owned)
            .or(pi_coding::ProviderAuthAdapter::resolve_api_key(&adapter)?)
            .ok_or_else(|| anyhow!("llama.cpp router is not configured"))?;
        return Ok(ModelRequestAuth {
            api_key,
            headers: HashMap::new(),
            env: HashMap::new(),
            stored_oauth: false,
            available_model_ids: None,
        });
    }
    let state = state_read();
    let config = state.providers.get(&model.provider);
    let mut credential_env = None;
    let key = if let Some(key) = explicit_key.filter(|key| !key.trim().is_empty()) {
        Some(key.to_owned())
    } else if let Some(key) = state.runtime_keys.get(&model.provider) {
        Some(key.clone())
    } else if let Some(credential) = state.stored_credentials.get(&model.provider) {
        match credential {
            StoredCredential::ApiKey { key, env: stored_env } => {
                let resolved = resolve_stored_api_key(key, stored_env, &model.provider, env)?;
                if resolved.is_some() {
                    credential_env = Some(stored_env);
                    resolved
                } else {
                    resolve_configured_api_key(config, &model.provider, env)?
                }
            }
            StoredCredential::OAuth => {
                bail!("OAuth credential for provider {:?} requires asynchronous request auth resolution", model.provider)
            }
        }
    } else {
        resolve_configured_api_key(config, &model.provider, env)?
    }
    .or_else(|| get_env_api_key(&model.provider, env));

    let mut raw_headers = model.headers.clone().unwrap_or_default();
    if let Some(provider_headers) = config.map(|provider| &provider.headers) {
        merge_headers_case_insensitive(&mut raw_headers, provider_headers);
    }
    if let Some(model_headers) = config.and_then(|provider| provider.model_headers.get(&model.id)) {
        merge_headers_case_insensitive(&mut raw_headers, model_headers);
    }
    let mut headers = resolve_headers_with_fallback(&raw_headers, env, credential_env)
        .with_context(|| {
            format!(
                "resolving request headers for model {}/{}",
                model.provider, model.id
            )
        })?;
    let auth_header = config.is_some_and(|provider| provider.auth_header);
    if auth_header {
        let resolved = key
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "no API key found for provider {:?} (authHeader requires an API key)",
                    model.provider
                )
            })?;
        insert_header_case_insensitive(&mut headers, "authorization", format!("Bearer {resolved}"));
    }
    if key.as_ref().is_none_or(|value| value.trim().is_empty())
        && !has_recognized_auth_header(&model.api, &headers)
    {
        bail!(
            "no API key found for provider {:?} (set the appropriate *_API_KEY env var or add authentication in auth.json or models.json)",
            model.provider
        );
    }
    Ok(ModelRequestAuth {
        api_key: key.unwrap_or_default(),
        headers,
        env: credential_env.cloned().unwrap_or_default(),
        stored_oauth: false,
        available_model_ids: None,
    })
}

pub async fn resolve_model_request_auth_async(
    model: &Model,
    explicit_key: Option<&str>,
    env: Option<&HashMap<String, String>>,
) -> Result<ModelRequestAuth> {
    let stored_oauth = {
        let state = state_read();
        explicit_key.filter(|key| !key.trim().is_empty()).is_none()
            && state.runtime_keys.get(&model.provider).is_none()
            && matches!(
                state.stored_credentials.get(&model.provider),
                Some(StoredCredential::OAuth)
            )
    };
    if !stored_oauth {
        return resolve_model_request_auth(model, explicit_key, env);
    }
    let path = state_read()
        .auth_path
        .clone()
        .ok_or_else(|| anyhow!("auth.json path is unavailable"))?;
    let manager = pi_coding::AuthManager::new(path)?;
    let stored = manager
        .resolve_stored(&model.provider, env)
        .await?
        .ok_or_else(|| {
            anyhow!(
                "stored OAuth credential for provider {:?} could not be resolved",
                model.provider
            )
        })?;
    let state = state_read();
    let config = state.providers.get(&model.provider);
    let mut raw_headers = model.headers.clone().unwrap_or_default();
    if let Some(provider_headers) = config.map(|provider| &provider.headers) {
        merge_headers_case_insensitive(&mut raw_headers, provider_headers);
    }
    if let Some(model_headers) = config.and_then(|provider| provider.model_headers.get(&model.id)) {
        merge_headers_case_insensitive(&mut raw_headers, model_headers);
    }
    let mut headers = resolve_headers_with_fallback(&raw_headers, env, Some(&stored.env))?;
    merge_headers_case_insensitive(&mut headers, &stored.headers);
    if config.is_some_and(|provider| provider.auth_header) {
        let api_key = stored.api_key.trim();
        if api_key.is_empty() {
            bail!(
                "no API key found for provider {:?} (authHeader requires an API key)",
                model.provider
            );
        }
        insert_header_case_insensitive(&mut headers, "authorization", format!("Bearer {api_key}"));
    }
    Ok(ModelRequestAuth {
        api_key: stored.api_key,
        headers,
        env: stored.env,
        stored_oauth: true,
        available_model_ids: stored.available_model_ids,
    })
}

fn resolve_stored_api_key(
    key: &str,
    credential_env: &HashMap<String, String>,
    provider: &str,
    env: Option<&HashMap<String, String>>,
) -> Result<Option<String>> {
    if key.starts_with('!') {
        bail!("command-valued stored API key is not supported for provider {provider:?}")
    }
    resolve_config_value_optional_with_fallback(key, env, Some(credential_env), "auth.json")
        .with_context(|| format!("resolving stored API key for provider {provider:?}"))
}

fn resolve_configured_api_key(
    config: Option<&ProviderAuthConfig>,
    provider: &str,
    env: Option<&HashMap<String, String>>,
) -> Result<Option<String>> {
    match config.and_then(|provider| provider.api_key.as_deref()) {
        Some(value) if value.starts_with('!') => {
            bail!("command-valued apiKey is not supported for provider {provider:?}")
        }
        Some(value) => resolve_config_value_optional(value, env),
        None => Ok(None),
    }
}

fn state_read() -> std::sync::RwLockReadGuard<'static, ConfigState> {
    STATE
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn state_write() -> std::sync::RwLockWriteGuard<'static, ConfigState> {
    STATE
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelsConfig {
    #[serde(default)]
    providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderConfig {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    auth_header: Option<bool>,
    #[serde(default)]
    compat: Option<Value>,
    #[serde(default)]
    models: Option<Vec<ModelDefinition>>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelDefinition {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    api: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    reasoning: Option<bool>,
    #[serde(default)]
    thinking_level_map: Option<ThinkingLevelMap>,
    #[serde(default)]
    input: Option<Vec<String>>,
    #[serde(default)]
    cost: Option<ModelCost>,
    #[serde(default)]
    context_window: Option<i64>,
    #[serde(default)]
    max_tokens: Option<i64>,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    compat: Option<Value>,
}

fn build_snapshot(
    config: &ModelsConfig,
    path: &Path,
) -> Result<(HashMap<String, ProviderAuthConfig>, Vec<Model>)> {
    load_builtin_models();
    let mut auth_configs = HashMap::new();
    let mut models = Vec::new();
    for (provider_id, provider) in &config.providers {
        if provider.models.is_none()
            && provider.base_url.is_none()
            && provider.headers.is_empty()
            && provider.compat.is_none()
            && provider.api_key.is_none()
            && provider.auth_header.is_none()
        {
            bail!(
                "Provider {provider_id}: must specify \"baseUrl\", \"headers\", \"compat\", \"apiKey\", \"authHeader\", or \"models\".\nFile: {}",
                path.display()
            );
        }
        let _ = &provider.name;
        let mut provider_auth = ProviderAuthConfig {
            api_key: provider.api_key.clone(),
            headers: provider.headers.clone(),
            auth_header: provider.auth_header.unwrap_or(false),
            ..ProviderAuthConfig::default()
        };
        let mut provider_models = builtin_models(provider_id);
        for model in &mut provider_models {
            if let Some(base_url) = &provider.base_url {
                model.base_url.clone_from(base_url);
            }
            model.compat = merge_compat(model.compat.as_ref(), provider.compat.as_ref());
        }
        for definition in provider.models.as_deref().unwrap_or_default() {
            let existing_index = provider_models
                .iter()
                .position(|model| model.id == definition.id);
            let defaults = existing_index
                .and_then(|index| provider_models.get(index))
                .or_else(|| provider_models.first());
            let model = build_model(provider_id, definition, provider, defaults)
                .with_context(|| format!("File: {}", path.display()))?;
            if !definition.headers.is_empty() {
                provider_auth
                    .model_headers
                    .insert(definition.id.clone(), definition.headers.clone());
            }
            match existing_index {
                Some(index) => provider_models[index] = model,
                None => provider_models.push(model),
            }
        }
        models.extend(provider_models);
        auth_configs.insert(provider_id.clone(), provider_auth);
    }
    Ok((auth_configs, models))
}

fn apply_snapshot(
    providers: HashMap<String, ProviderAuthConfig>,
    stored_credentials: HashMap<String, StoredCredential>,
    models: Vec<Model>,
    auth_path: Option<PathBuf>,
) -> Result<()> {
    let mut state = state_write();
    let new_owned = models
        .iter()
        .map(|model| (model.provider.clone(), model.id.clone()))
        .collect::<Vec<_>>();
    replace_registered_models(&state.owned_models, models);
    state.providers = providers;
    state.stored_credentials = stored_credentials;
    state.owned_models = new_owned;
    state.auth_path = auth_path;
    Ok(())
}

fn build_model(
    provider_id: &str,
    definition: &ModelDefinition,
    provider: &ProviderConfig,
    defaults: Option<&Model>,
) -> Result<Model> {
    let api = definition
        .api
        .clone()
        .or_else(|| provider.api.clone())
        .or_else(|| defaults.map(|model| model.api.clone()))
        .ok_or_else(|| {
            anyhow!(
                "Provider {provider_id}, model {}: no \"api\" specified. Set at provider or model level.",
                definition.id
            )
        })?;
    let base_url = definition
        .base_url
        .clone()
        .or_else(|| provider.base_url.clone())
        .or_else(|| defaults.map(|model| model.base_url.clone()))
        .ok_or_else(|| {
            anyhow!("Provider {provider_id}: \"baseUrl\" is required when defining custom models.")
        })?;
    if definition.context_window.is_some_and(|value| value <= 0) {
        bail!(
            "Provider {provider_id}, model {}: invalid contextWindow",
            definition.id
        );
    }
    if definition.max_tokens.is_some_and(|value| value <= 0) {
        bail!(
            "Provider {provider_id}, model {}: invalid maxTokens",
            definition.id
        );
    }
    let mut model = Model::default();
    model.id = definition.id.clone();
    model.name = definition
        .name
        .clone()
        .unwrap_or_else(|| definition.id.clone());
    model.api = api;
    model.provider = provider_id.to_owned();
    model.base_url = base_url;
    model.reasoning = definition.reasoning.unwrap_or(false);
    model.thinking_level_map = definition.thinking_level_map.clone();
    model.input = definition
        .input
        .clone()
        .unwrap_or_else(|| vec!["text".to_owned()]);
    model.cost = definition.cost.clone().unwrap_or_default();
    model.context_window = definition.context_window.unwrap_or(128_000);
    model.max_tokens = definition.max_tokens.unwrap_or(16_384);
    model.compat = merge_compat(provider.compat.as_ref(), definition.compat.as_ref());
    Ok(model)
}

fn merge_compat(base: Option<&Value>, override_value: Option<&Value>) -> Option<Value> {
    match (base, override_value) {
        (None, None) => None,
        (Some(base), None) => Some(base.clone()),
        (None, Some(override_value)) => Some(override_value.clone()),
        (Some(Value::Object(base)), Some(Value::Object(override_map))) => {
            let mut merged = base.clone();
            for (key, value) in override_map {
                if matches!(
                    key.as_str(),
                    "openRouterRouting" | "vercelGatewayRouting" | "chatTemplateKwargs"
                ) {
                    let mut nested = merged
                        .get(key)
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    if let Value::Object(override_nested) = value {
                        nested.extend(override_nested.clone());
                        merged.insert(key.clone(), Value::Object(nested));
                    } else {
                        merged.insert(key.clone(), value.clone());
                    }
                } else {
                    merged.insert(key.clone(), value.clone());
                }
            }
            Some(Value::Object(merged))
        }
        (_, Some(override_value)) => Some(override_value.clone()),
    }
}

fn resolve_headers_with_fallback(
    raw_headers: &HashMap<String, String>,
    env: Option<&HashMap<String, String>>,
    fallback_env: Option<&HashMap<String, String>>,
) -> Result<HashMap<String, String>> {
    let mut headers = HashMap::new();
    for (name, value) in raw_headers {
        if value.starts_with('!') {
            bail!("command-valued header {name:?} is not supported")
        }
        let resolved = resolve_config_value_with_fallback(value, env, fallback_env, "models.json")?;
        if !resolved.trim().is_empty() {
            insert_header_case_insensitive(&mut headers, name, resolved);
        }
    }
    Ok(headers)
}

fn resolve_config_value_optional(
    value: &str,
    env: Option<&HashMap<String, String>>,
) -> Result<Option<String>> {
    resolve_config_value_optional_with_fallback(value, env, None, "models.json")
}

fn resolve_config_value_optional_with_fallback(
    value: &str,
    env: Option<&HashMap<String, String>>,
    fallback_env: Option<&HashMap<String, String>>,
    source: &str,
) -> Result<Option<String>> {
    let resolved = resolve_config_value_with_fallback(value, env, fallback_env, source)?;
    Ok((!resolved.trim().is_empty()).then_some(resolved))
}

fn resolve_config_value(value: &str, env: Option<&HashMap<String, String>>) -> Result<String> {
    resolve_config_value_with_fallback(value, env, None, "models.json")
}

fn resolve_config_value_with_fallback(
    value: &str,
    env: Option<&HashMap<String, String>>,
    fallback_env: Option<&HashMap<String, String>>,
    source: &str,
) -> Result<String> {
    if !value.contains('$') {
        return Ok(value.to_owned());
    }
    let chars = value.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(value.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] != '$' {
            output.push(chars[index]);
            index += 1;
            continue;
        }
        if index + 1 >= chars.len() {
            output.push('$');
            index += 1;
            continue;
        }
        let next = chars[index + 1];
        if next == '$' || next == '!' {
            output.push(next);
            index += 2;
            continue;
        }
        let (name, next_index) = if next == '{' {
            let closing = chars[index + 2..]
                .iter()
                .position(|character| *character == '}')
                .map(|offset| index + 2 + offset);
            let Some(closing) = closing else {
                output.push('$');
                index += 1;
                continue;
            };
            let name = chars[index + 2..closing].iter().collect::<String>();
            if !is_env_name(&name) {
                output.extend(chars[index..=closing].iter());
                index = closing + 1;
                continue;
            }
            (name, closing + 1)
        } else if next.is_ascii_alphabetic() || next == '_' {
            let mut end = index + 1;
            while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '_') {
                end += 1;
            }
            (chars[index + 1..end].iter().collect::<String>(), end)
        } else {
            output.push('$');
            index += 1;
            continue;
        };
        let resolved = env_lookup_with_fallback(&name, env, fallback_env).ok_or_else(|| {
            anyhow!("environment variable {name} referenced by {source} is not set")
        })?;
        output.push_str(&resolved);
        index = next_index;
    }
    Ok(output)
}

fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(character) if character.is_ascii_alphabetic() || character == '_')
        && chars.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn env_lookup_with_fallback(
    name: &str,
    env: Option<&HashMap<String, String>>,
    fallback_env: Option<&HashMap<String, String>>,
) -> Option<String> {
    env.and_then(|values| values.get(name))
        .filter(|value| !value.is_empty())
        .or_else(|| {
            fallback_env
                .and_then(|values| values.get(name))
                .filter(|value| !value.is_empty())
        })
        .cloned()
        .or_else(|| std::env::var(name).ok().filter(|value| !value.is_empty()))
}

fn insert_header_case_insensitive(
    headers: &mut HashMap<String, String>,
    name: impl AsRef<str>,
    value: String,
) {
    if let Some(existing) = headers
        .keys()
        .find(|existing| existing.eq_ignore_ascii_case(name.as_ref()))
        .cloned()
    {
        headers.remove(&existing);
    }
    headers.insert(name.as_ref().to_owned(), value);
}

fn merge_headers_case_insensitive(
    destination: &mut HashMap<String, String>,
    source: &HashMap<String, String>,
) {
    for (name, value) in source {
        insert_header_case_insensitive(destination, name, value.clone());
    }
}

fn has_recognized_auth_header(api: &str, headers: &HashMap<String, String>) -> bool {
    let recognized: &[&str] = match api {
        pi_ai::API_ANTHROPIC_MESSAGES => &["authorization", "x-api-key", "cf-aig-authorization"],
        pi_ai::API_GOOGLE_GENERATIVE_AI => &["x-goog-api-key"],
        pi_ai::API_FAUX => return true,
        _ => &["authorization", "cf-aig-authorization"],
    };
    recognized.iter().any(|expected| {
        headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case(expected) && !value.trim().is_empty())
    })
}

fn read_bounded_config(path: &Path, name: &str) -> Result<Option<String>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => bail!("Failed to load {name}: {error}\nFile: {}", path.display()),
    };
    let metadata_len = file
        .metadata()
        .with_context(|| format!("Failed to inspect {name}\nFile: {}", path.display()))?
        .len();
    if metadata_len > MAX_CONFIG_FILE_BYTES as u64 {
        bail!(
            "Failed to load {name}: file exceeds {MAX_CONFIG_FILE_BYTES} bytes\nFile: {}",
            path.display()
        );
    }
    let mut bytes = Vec::with_capacity((metadata_len as usize).min(MAX_CONFIG_FILE_BYTES));
    std::io::Read::by_ref(&mut file)
        .take(MAX_CONFIG_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("Failed to load {name}\nFile: {}", path.display()))?;
    if bytes.len() > MAX_CONFIG_FILE_BYTES {
        bail!(
            "Failed to load {name}: file exceeds {MAX_CONFIG_FILE_BYTES} bytes\nFile: {}",
            path.display()
        );
    }
    String::from_utf8(bytes)
        .map(Some)
        .with_context(|| format!("Failed to load {name}: file is not UTF-8\nFile: {}", path.display()))
}

fn load_stored_credentials(path: &Path) -> Result<HashMap<String, StoredCredential>> {
    let Some(content) = read_bounded_config(path, "auth.json")? else {
        return Ok(HashMap::new());
    };
    if content.trim().is_empty() {
        return Ok(HashMap::new());
    }
    let value: Value = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse auth.json\nFile: {}", path.display()))?;
    let entries = value.as_object().ok_or_else(|| {
        anyhow!(
            "Invalid auth.json: expected an object\nFile: {}",
            path.display()
        )
    })?;
    let mut credentials = HashMap::with_capacity(entries.len());
    for (provider, value) in entries {
        let credential = value.as_object().ok_or_else(|| {
            invalid_stored_credential(provider, path, "credential must be an object")
        })?;
        match credential.get("type").and_then(Value::as_str) {
            Some("oauth") => {
                for field in ["access", "refresh"] {
                    if credential.get(field).and_then(Value::as_str).is_none() {
                        return Err(invalid_stored_credential(
                            provider,
                            path,
                            &format!("field {field:?} must be a string"),
                        ));
                    }
                }
                if credential.get("expires").and_then(Value::as_i64).is_none() {
                    return Err(invalid_stored_credential(
                        provider,
                        path,
                        "field \"expires\" must be an integer",
                    ));
                }
                credentials.insert(provider.clone(), StoredCredential::OAuth);
            }
            Some("api_key") => {
                let key = credential
                    .get("key")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        invalid_stored_credential(provider, path, "field \"key\" must be a string")
                    })?;
                let env = match credential.get("env") {
                    None => HashMap::new(),
                    Some(Value::Object(values)) => {
                        let mut env = HashMap::with_capacity(values.len());
                        for (name, value) in values {
                            let value = value.as_str().ok_or_else(|| {
                                invalid_stored_credential(
                                    provider,
                                    path,
                                    "all field \"env\" values must be strings",
                                )
                            })?;
                            env.insert(name.clone(), value.to_owned());
                        }
                        env
                    }
                    Some(_) => {
                        return Err(invalid_stored_credential(
                            provider,
                            path,
                            "field \"env\" must be an object",
                        ));
                    }
                };
                credentials.insert(
                    provider.clone(),
                    StoredCredential::ApiKey {
                        key: key.to_owned(),
                        env,
                    },
                );
            }
            Some(_) => {
                return Err(invalid_stored_credential(
                    provider,
                    path,
                    "credential type is not supported",
                ));
            }
            None => {
                return Err(invalid_stored_credential(
                    provider,
                    path,
                    "field \"type\" must be \"api_key\" or \"oauth\"",
                ));
            }
        }
    }
    Ok(credentials)
}

fn invalid_stored_credential(provider: &str, path: &Path, reason: &str) -> anyhow::Error {
    anyhow!(
        "Invalid auth.json credential for provider {provider:?}: {reason}\nFile: {}",
        path.display()
    )
}

fn strip_json_comments(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let current = bytes[index];
        if in_string {
            output.push(current);
            if escaped {
                escaped = false;
            } else if current == b'\\' {
                escaped = true;
            } else if current == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if current == b'"' {
            in_string = true;
            output.push(current);
            index += 1;
            continue;
        }
        if current == b'/' && bytes.get(index + 1) == Some(&b'/') {
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if current == b'/' && bytes.get(index + 1) == Some(&b'*') {
            index += 2;
            while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                index += 1;
            }
            index = (index + 2).min(bytes.len());
            continue;
        }
        output.push(current);
        index += 1;
    }
    String::from_utf8(output).unwrap_or_else(|_| input.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_values_support_templates_and_escapes() {
        let env = HashMap::from([("TOKEN".to_owned(), "test-value".to_owned())]);
        assert_eq!(
            resolve_config_value("$TOKEN/${TOKEN}", Some(&env)).unwrap(),
            "test-value/test-value"
        );
        assert_eq!(
            resolve_config_value("$$/$!/$", Some(&env)).unwrap(),
            "$/!/$"
        );
        assert_eq!(
            resolve_config_value("${1bad}", Some(&env)).unwrap(),
            "${1bad}"
        );
    }

    #[test]
    fn nested_compat_values_merge() {
        let base = serde_json::json!({"openRouterRouting":{"a":1,"b":2}});
        let override_value = serde_json::json!({"openRouterRouting":{"b":3,"c":4}});
        assert_eq!(
            merge_compat(Some(&base), Some(&override_value)),
            Some(serde_json::json!({"openRouterRouting":{"a":1,"b":3,"c":4}}))
        );
    }
}
