//! Persistent llama.cpp router configuration, catalog refresh, and GGUF installs.

use std::{
    io::{Read as _, Seek as _, SeekFrom, Write as _},
    path::{Component, Path, PathBuf},
    sync::{LazyLock, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use pi_ai::{
    HttpMethod, HuggingFaceClient, HuggingFaceGgufFile, HuggingFaceModelDetails, LLAMA_PROVIDER_ID,
    LlamaRouterClient, LlamaRouterModel, LlamaRouterSettings, Model, SharedHttpExecutor,
    StreamOptions, replace_registered_models, request_from_url,
};
use pi_sandbox::AbsolutePath;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::fs::{
    CapabilityAccess, CapabilityFs, CapabilityRootLease, CapabilityRootPurpose, FileMode, FsError,
    Replacement, RootCreation,
};

const LLAMA_DIR: &str = "llama";
const SETTINGS_FILE: &str = "router.json";
const CATALOG_FILE: &str = "catalog.json";
const INSTALLED_FILE: &str = "installed.json";
const MODELS_DIR: &str = "models";
const MAX_STORED_JSON_BYTES: u64 = 8 * 1024 * 1024;

static CATALOG_MODELS: LazyLock<Mutex<Vec<(String, String)>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlamaCatalogSnapshot {
    pub checked_at: u64,
    pub models: Vec<Model>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledGgufFile {
    pub source: String,
    pub relative_path: PathBuf,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledGgufModel {
    pub repository: String,
    pub quantization: String,
    pub installed_at: u64,
    pub files: Vec<InstalledGgufFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledGgufCatalog {
    pub models: Vec<InstalledGgufModel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GgufDownloadProgress {
    pub file: String,
    pub downloaded: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    pub resumed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CatalogRefresh {
    pub models: Vec<Model>,
    pub source: CatalogSource,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogSource {
    Live,
    Cache,
}

#[derive(Clone)]
pub struct LlamaManager {
    root: PathBuf,
    store: CapabilityRootLease,
}

impl std::fmt::Debug for LlamaManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LlamaManager")
            .field("root", &self.root)
            .finish()
    }
}

impl LlamaManager {
    pub fn new(root: PathBuf) -> Result<Self> {
        let capabilities = CapabilityFs::new().context("creating llama filesystem capabilities")?;
        let absolute = AbsolutePath::parse(root.clone()).map_err(|_| FsError::InvalidPath)?;
        let store = capabilities
            .open_root(
                CapabilityRootPurpose::AgentStore,
                absolute,
                CapabilityAccess::PrivateReadWrite,
                RootCreation::CreateDirectory,
            )
            .context("opening private llama data store")?;
        Ok(Self { root, store })
    }

    pub fn global() -> Result<Self> {
        Self::new(llama_data_dir())
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn settings_path(&self) -> PathBuf {
        self.root.join(SETTINGS_FILE)
    }

    #[must_use]
    pub fn catalog_path(&self) -> PathBuf {
        self.root.join(CATALOG_FILE)
    }

    #[must_use]
    pub fn installed_path(&self) -> PathBuf {
        self.root.join(INSTALLED_FILE)
    }

    #[must_use]
    pub fn models_dir(&self) -> PathBuf {
        self.root.join(MODELS_DIR)
    }

    pub fn settings(&self) -> Result<Option<LlamaRouterSettings>> {
        read_optional_json(
            &self.store,
            Path::new(SETTINGS_FILE),
            "llama.cpp router settings",
        )
    }
    /// Effective settings prefer persisted configuration, then the standard
    /// `LLAMA_BASE_URL` / optional `LLAMA_API_KEY` environment adapter.
    pub fn effective_settings(&self) -> Result<Option<LlamaRouterSettings>> {
        if let Some(settings) = read_optional_private_json(
            &self.store,
            Path::new(SETTINGS_FILE),
            "llama.cpp router settings",
        )? {
            return Ok(Some(settings));
        }
        let Some(base_url) = std::env::var("LLAMA_BASE_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
        else {
            return Ok(None);
        };
        Ok(Some(LlamaRouterSettings::validated(
            &base_url,
            std::env::var("LLAMA_API_KEY").ok(),
        )?))
    }

    pub fn configured_client(&self, executor: SharedHttpExecutor) -> Result<LlamaRouterClient> {
        let settings = self.effective_settings()?.ok_or_else(|| {
            anyhow!(
                "llama.cpp router is not configured; run `rpi llama configure {}` or set LLAMA_BASE_URL",
                pi_ai::DEFAULT_LLAMA_SERVER_URL
            )
        })?;
        LlamaRouterClient::new(settings, executor)
    }

    /// Validate connectivity and the live `/v1/models` response before making
    /// the new configuration visible. Both settings and the observed catalog
    /// are then committed atomically per file.
    pub async fn configure(
        &self,
        settings: LlamaRouterSettings,
        executor: SharedHttpExecutor,
    ) -> Result<Vec<Model>> {
        let settings = LlamaRouterSettings::validated(&settings.base_url, settings.api_key)?;
        let client = LlamaRouterClient::new(settings.clone(), executor)?;
        let models = client
            .discover_models()
            .await
            .context("validating llama.cpp router configuration")?;
        let snapshot = LlamaCatalogSnapshot {
            checked_at: unix_millis(),
            models: models.clone(),
        };
        write_json_atomic(
            &self.store,
            Path::new(CATALOG_FILE),
            &snapshot,
            FileMode::Private,
        )
        .context("persisting validated llama.cpp catalog")?;
        write_json_atomic(
            &self.store,
            Path::new(SETTINGS_FILE),
            &settings,
            FileMode::Private,
        )
        .context("persisting validated llama.cpp settings")?;
        apply_catalog(models.clone());
        Ok(models)
    }

    pub fn cached_catalog(&self) -> Result<Option<LlamaCatalogSnapshot>> {
        read_optional_json(
            &self.store,
            Path::new(CATALOG_FILE),
            "llama.cpp model catalog",
        )
    }
    /// Refresh from the live router. If the router is unavailable, apply and
    /// return the last successfully observed snapshot rather than synthesizing
    /// a model. A missing cache preserves the live error.
    pub async fn refresh_catalog(&self, executor: SharedHttpExecutor) -> Result<CatalogRefresh> {
        let client = self.configured_client(executor)?;
        match client.discover_models().await {
            Ok(models) => {
                let snapshot = LlamaCatalogSnapshot {
                    checked_at: unix_millis(),
                    models: models.clone(),
                };
                write_json_atomic(
                    &self.store,
                    Path::new(CATALOG_FILE),
                    &snapshot,
                    FileMode::Private,
                )
                .context("persisting refreshed llama.cpp model catalog")?;
                apply_catalog(models.clone());
                Ok(CatalogRefresh {
                    models,
                    source: CatalogSource::Live,
                    warning: None,
                })
            }
            Err(error) => {
                let Some(snapshot) = self.cached_catalog()? else {
                    return Err(error.context("refreshing llama.cpp model catalog"));
                };
                apply_catalog(snapshot.models.clone());
                Ok(CatalogRefresh {
                    models: snapshot.models,
                    source: CatalogSource::Cache,
                    warning: Some(format!("{error:#}")),
                })
            }
        }
    }

    /// Load the persisted catalog without touching the network.
    pub fn load_cached_catalog(&self) -> Result<Vec<Model>> {
        let models = self
            .cached_catalog()?
            .map(|snapshot| snapshot.models)
            .unwrap_or_default();
        apply_catalog(models.clone());
        Ok(models)
    }

    pub async fn router_models(
        &self,
        reload: bool,
        executor: SharedHttpExecutor,
    ) -> Result<Vec<LlamaRouterModel>> {
        self.configured_client(executor)?
            .list_router_models(reload)
            .await
            .context("reading llama.cpp router management catalog")
    }

    pub async fn load_model(
        &self,
        model: &str,
        executor: SharedHttpExecutor,
    ) -> Result<Vec<Model>> {
        validate_model_argument(model)?;
        let client = self.configured_client(executor)?;
        client
            .load(model)
            .await
            .with_context(|| format!("requesting llama.cpp router to load {model:?}"))?;
        let models = client
            .discover_models()
            .await
            .with_context(|| format!("refreshing live catalog after loading {model:?}"))?;
        let snapshot = LlamaCatalogSnapshot {
            checked_at: unix_millis(),
            models: models.clone(),
        };
        write_json_atomic(
            &self.store,
            Path::new(CATALOG_FILE),
            &snapshot,
            FileMode::Private,
        )
        .context("persisting llama.cpp catalog after load")?;
        apply_catalog(models.clone());
        Ok(models)
    }

    pub async fn unload_model(
        &self,
        model: &str,
        executor: SharedHttpExecutor,
    ) -> Result<Vec<Model>> {
        validate_model_argument(model)?;
        let client = self.configured_client(executor)?;
        client
            .unload(model)
            .await
            .with_context(|| format!("requesting llama.cpp router to unload {model:?}"))?;
        let models = client
            .discover_models()
            .await
            .with_context(|| format!("refreshing live catalog after unloading {model:?}"))?;
        let snapshot = LlamaCatalogSnapshot {
            checked_at: unix_millis(),
            models: models.clone(),
        };
        write_json_atomic(
            &self.store,
            Path::new(CATALOG_FILE),
            &snapshot,
            FileMode::Private,
        )
        .context("persisting llama.cpp catalog after unload")?;
        apply_catalog(models.clone());
        Ok(models)
    }

    pub fn installed(&self) -> Result<InstalledGgufCatalog> {
        Ok(read_optional_json(
            &self.store,
            Path::new(INSTALLED_FILE),
            "installed GGUF catalog",
        )?
        .unwrap_or_default())
    }

    pub async fn install_from_hugging_face<F>(
        &self,
        client: &HuggingFaceClient,
        details: &HuggingFaceModelDetails,
        quantization: Option<&str>,
        executor: SharedHttpExecutor,
        cancel: &CancellationToken,
        mut progress: F,
    ) -> Result<InstalledGgufModel>
    where
        F: FnMut(GgufDownloadProgress) + Send,
    {
        validate_repository(&details.id)?;
        let selected = match quantization {
            Some(name) => details
                .quantizations
                .iter()
                .find(|candidate| candidate.name.eq_ignore_ascii_case(name))
                .ok_or_else(|| {
                    anyhow!(
                        "quantization {name:?} is not available for {:?}",
                        details.id
                    )
                })?,
            None => details.quantizations.first().ok_or_else(|| {
                anyhow!(
                    "Hugging Face model {:?} has no downloadable GGUF files",
                    details.id
                )
            })?,
        };
        if selected.files.is_empty() {
            bail!(
                "Hugging Face model {:?} quantization {:?} has no downloadable files",
                details.id,
                selected.name
            );
        }
        let destination_relative = self.model_install_relative(&details.id, &selected.name)?;
        self.store
            .create_dir_all(&destination_relative, FileMode::Private)
            .context("creating GGUF install directory")?;
        let mut installed_files = Vec::with_capacity(selected.files.len());
        for file in &selected.files {
            if cancel.is_cancelled() {
                bail!("GGUF download cancelled");
            }
            let file_relative = safe_relative_path(&file.name)?;
            let target_relative = destination_relative.join(&file_relative);
            if let Some(parent) = target_relative.parent() {
                self.store
                    .create_dir_all(parent, FileMode::Private)
                    .context("creating GGUF directory")?;
            }
            let source = client.resolve_url(&details.id, &file.name)?;
            download_one(
                &self.store,
                client,
                file,
                source.as_str(),
                &target_relative,
                executor.clone(),
                cancel,
                &mut progress,
            )
            .await?;
            let metadata = self
                .store
                .metadata(&target_relative)
                .context("reading downloaded GGUF metadata")?;
            installed_files.push(InstalledGgufFile {
                source: source.to_string(),
                relative_path: target_relative,
                size: metadata.len,
                sha256: file.sha256.clone(),
            });
        }
        let installed = InstalledGgufModel {
            repository: details.id.clone(),
            quantization: selected.name.clone(),
            installed_at: unix_millis(),
            files: installed_files,
        };
        let mut catalog = self.installed()?;
        catalog.models.retain(|model| {
            !(model.repository == installed.repository
                && model.quantization == installed.quantization)
        });
        catalog.models.push(installed.clone());
        catalog.models.sort_by(|left, right| {
            left.repository
                .cmp(&right.repository)
                .then_with(|| left.quantization.cmp(&right.quantization))
        });
        write_json_atomic(
            &self.store,
            Path::new(INSTALLED_FILE),
            &catalog,
            FileMode::Private,
        )
        .context("persisting installed GGUF catalog")?;
        Ok(installed)
    }

    fn model_install_relative(&self, repository: &str, quantization: &str) -> Result<PathBuf> {
        validate_repository(repository)?;
        validate_component(quantization, "quantization")?;
        let mut target = PathBuf::from(MODELS_DIR);
        for component in repository.split('/') {
            target.push(component);
        }
        target.push(quantization);
        Ok(target)
    }
}

/// Small provider-auth adapter used by CLI/model selection without coupling
/// local providers to OAuth implementation details.
pub trait ProviderAuthAdapter {
    fn provider_id(&self) -> &'static str;
    fn is_configured(&self) -> Result<bool>;
    fn resolve_api_key(&self) -> Result<Option<String>>;
}

#[derive(Debug, Clone)]
pub struct LlamaAuthAdapter {
    manager: LlamaManager,
}

impl LlamaAuthAdapter {
    #[must_use]
    pub fn new(manager: LlamaManager) -> Self {
        Self { manager }
    }

    pub fn global() -> Result<Self> {
        Ok(Self::new(LlamaManager::global()?))
    }
}

impl ProviderAuthAdapter for LlamaAuthAdapter {
    fn provider_id(&self) -> &'static str {
        LLAMA_PROVIDER_ID
    }

    fn is_configured(&self) -> Result<bool> {
        Ok(self.manager.effective_settings()?.is_some())
    }

    fn resolve_api_key(&self) -> Result<Option<String>> {
        Ok(self
            .manager
            .effective_settings()?
            .map(|settings| settings.api_key.unwrap_or_else(|| "local".to_owned())))
    }
}

#[must_use]
pub fn llama_data_dir() -> PathBuf {
    crate::resources::agent_dir_path().join(LLAMA_DIR)
}

fn apply_catalog(models: Vec<Model>) {
    let mut owned = CATALOG_MODELS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    replace_registered_models(&owned, models.clone());
    *owned = models
        .iter()
        .map(|model| (model.provider.clone(), model.id.clone()))
        .collect();
}

async fn download_one<F>(
    store: &CapabilityRootLease,
    client: &HuggingFaceClient,
    metadata: &HuggingFaceGgufFile,
    source: &str,
    target: &Path,
    executor: SharedHttpExecutor,
    cancel: &CancellationToken,
    progress: &mut F,
) -> Result<()>
where
    F: FnMut(GgufDownloadProgress) + Send,
{
    if let Ok(existing) = store.metadata(target) {
        if metadata.size.is_none_or(|size| size == existing.len)
            && checksum_matches(store, target, metadata.sha256.as_deref())?
        {
            progress(GgufDownloadProgress {
                file: metadata.name.clone(),
                downloaded: existing.len,
                total: metadata.size,
                resumed: true,
            });
            return Ok(());
        }
    }
    let part = part_path(target)?;
    let mut offset = match store.metadata(&part) {
        Ok(entry) => entry.len,
        Err(error) if fs_error_kind(&error) == Some(std::io::ErrorKind::NotFound) => 0,
        Err(error) => return Err(error).context("reading partial GGUF metadata"),
    };
    if metadata.size.is_some_and(|size| offset > size) {
        store
            .remove_file(&part)
            .context("discarding oversized partial GGUF")?;
        offset = 0;
    }
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::ACCEPT_ENCODING,
        http::HeaderValue::from_static("identity"),
    );
    if let Some(token) = client.token() {
        let mut value = http::HeaderValue::from_str(&format!("Bearer {token}"))
            .context("invalid Hugging Face token")?;
        value.set_sensitive(true);
        headers.insert(http::header::AUTHORIZATION, value);
    }
    if offset > 0 {
        headers.insert(
            http::header::RANGE,
            http::HeaderValue::from_str(&format!("bytes={offset}-"))
                .context("invalid GGUF resume range")?,
        );
    }
    let options = StreamOptions {
        timeout_ms: Some(120_000),
        abort_signal: Some(cancel.clone()),
        ..StreamOptions::default()
    };
    let request = request_from_url(
        HttpMethod::Get,
        source,
        &headers,
        Vec::new(),
        &options,
        pi_ai::MAX_HTTP_RESPONSE_BODY_BYTES,
    )?;
    let response = executor
        .execute(request)
        .await
        .with_context(|| format!("downloading GGUF from {source}"))?;
    let status = response.status();
    let append = offset > 0 && status == http::StatusCode::PARTIAL_CONTENT;
    if append {
        let expected_prefix = format!("bytes {offset}-");
        let content_range = response
            .headers()
            .get(http::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !content_range.starts_with(&expected_prefix) {
            bail!(
                "downloading GGUF {}: server returned mismatched Content-Range {content_range:?} for resume offset {offset}",
                metadata.name
            );
        }
    }
    let content_length = response
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if !status.is_success() {
        let bytes = pi_ai::collect_response_body(response)
            .await
            .unwrap_or_default();
        let body = String::from_utf8_lossy(&bytes);
        bail!(
            "downloading GGUF {}: HTTP {status}: {}",
            metadata.name,
            body.trim()
        );
    }
    if offset > 0 && !append {
        offset = 0;
    }
    let total = metadata
        .size
        .or_else(|| content_length.and_then(|length| length.checked_add(offset)));
    let mut file = if append {
        store
            .open_read_write_file(&part)
            .context("opening partial GGUF")?
            .try_clone_std()
            .context("cloning partial GGUF handle")?
    } else {
        match store.remove_file(&part) {
            Ok(()) => {}
            Err(error) if fs_error_kind(&error) == Some(std::io::ErrorKind::NotFound) => {}
            Err(error) => return Err(error).context("replacing partial GGUF"),
        }
        store
            .create_new(&part, FileMode::Private)
            .context("creating partial GGUF")?
            .finish()
            .context("syncing empty partial GGUF")?
            .try_clone_std()
            .context("cloning partial GGUF handle")?
    };
    if append {
        file.seek(SeekFrom::End(0))
            .context("seeking partial GGUF")?;
    }
    let mut downloaded = offset;
    progress(GgufDownloadProgress {
        file: metadata.name.clone(),
        downloaded,
        total,
        resumed: offset > 0,
    });
    let mut stream = response.into_body();
    loop {
        let next = tokio::select! {
            () = cancel.cancelled() => {
                file.flush().ok();
                file.sync_all().ok();
                bail!("GGUF download cancelled");
            }
            next = stream.next() => next,
        };
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.with_context(|| format!("streaming GGUF {}", metadata.name))?;
        file.write_all(&chunk).context("writing partial GGUF")?;
        downloaded = downloaded
            .checked_add(u64::try_from(chunk.len()).context("GGUF chunk length overflow")?)
            .ok_or_else(|| anyhow!("GGUF download byte count overflow"))?;
        progress(GgufDownloadProgress {
            file: metadata.name.clone(),
            downloaded,
            total,
            resumed: offset > 0,
        });
    }
    file.flush().context("flushing partial GGUF")?;
    file.sync_all().context("syncing partial GGUF")?;
    drop(file);
    if let Some(expected) = metadata.size
        && downloaded != expected
    {
        bail!(
            "downloaded GGUF {} has size {downloaded}, expected {expected}; partial file retained for resume",
            metadata.name
        );
    }
    if !checksum_matches(store, &part, metadata.sha256.as_deref())? {
        bail!(
            "downloaded GGUF {} failed SHA-256 verification; partial file retained",
            metadata.name
        );
    }
    store
        .rename(&part, target)
        .context("atomically installing GGUF")?;
    Ok(())
}

fn checksum_matches(
    store: &CapabilityRootLease,
    path: &Path,
    expected: Option<&str>,
) -> Result<bool> {
    let Some(expected) = expected else {
        return Ok(true);
    };
    let file = store.open_file(path).context("opening GGUF for checksum")?;
    let mut file = file
        .try_clone_std()
        .context("cloning GGUF checksum handle")?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .context("reading GGUF for checksum")?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()).eq_ignore_ascii_case(expected))
}

fn part_path(target: &Path) -> Result<PathBuf> {
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("GGUF target must have a UTF-8 file name"))?;
    Ok(target.with_file_name(format!("{name}.part")))
}

fn validate_model_argument(model: &str) -> Result<()> {
    let model = model.trim();
    if model.is_empty() || model.contains('\0') || model.contains(['\r', '\n']) {
        bail!("llama.cpp model id must be non-empty and single-line");
    }
    Ok(())
}

fn validate_repository(repository: &str) -> Result<()> {
    let parts = repository.split('/').collect::<Vec<_>>();
    if parts.len() != 2 {
        bail!("Hugging Face repository must be OWNER/NAME");
    }
    for component in parts {
        validate_component(component, "repository component")?;
    }
    Ok(())
}

fn validate_component(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || matches!(value, "." | "..")
        || value.contains(['/', '\\', '\0', '\r', '\n'])
    {
        bail!("invalid {label} {value:?}");
    }
    Ok(())
}

fn safe_relative_path(value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if path.is_absolute() {
        bail!("GGUF file path must be relative: {value:?}");
    }
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => safe.push(value),
            _ => bail!("GGUF file path contains traversal: {value:?}"),
        }
    }
    if safe.as_os_str().is_empty() {
        bail!("GGUF file path is empty");
    }
    Ok(safe)
}

fn read_optional_json<T: DeserializeOwned>(
    store: &CapabilityRootLease,
    relative: &Path,
    label: &str,
) -> Result<Option<T>> {
    read_optional_json_with_privacy(store, relative, label, false)
}

fn read_optional_private_json<T: DeserializeOwned>(
    store: &CapabilityRootLease,
    relative: &Path,
    label: &str,
) -> Result<Option<T>> {
    read_optional_json_with_privacy(store, relative, label, true)
}

fn read_optional_json_with_privacy<T: DeserializeOwned>(
    store: &CapabilityRootLease,
    relative: &Path,
    label: &str,
    owner_only: bool,
) -> Result<Option<T>> {
    let file = match store.open_file(relative) {
        Ok(file) => file,
        Err(error) if fs_error_kind(&error) == Some(std::io::ErrorKind::NotFound) => {
            return Ok(None);
        }
        Err(error) => return Err(error).with_context(|| format!("reading {label}")),
    };
    if owner_only {
        validate_owner_only(&file, label)?;
    }
    let bytes = match file.read_bounded(MAX_STORED_JSON_BYTES) {
        Ok(bytes) => bytes,
        Err(FsError::ReadLimitExceeded { .. }) => bail!(
            "{label} at {} is too large: exceeds the {MAX_STORED_JSON_BYTES}-byte limit",
            file.display_path().display()
        ),
        Err(error) => return Err(error).with_context(|| format!("reading {label}")),
    };
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {label} from {}", file.display_path().display()))
        .map(Some)
}

#[cfg(unix)]
fn validate_owner_only(file: &crate::fs::CapabilityFile, label: &str) -> Result<()> {
    let path = file.display_path();
    let metadata = file
        .metadata()
        .with_context(|| format!("reading {label} permissions at {}", path.display()))?;
    if metadata.mode & 0o077 != 0 {
        bail!(
            "llama.cpp settings file {} must not be accessible by group or other users",
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_owner_only(_file: &crate::fs::CapabilityFile, _label: &str) -> Result<()> {
    Ok(())
}

fn write_json_atomic<T: Serialize>(
    store: &CapabilityRootLease,
    relative: &Path,
    value: &T,
    mode: FileMode,
) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).context("serializing llama state")?;
    bytes.push(b'\n');
    let expected = match store.metadata(relative) {
        Ok(metadata) => Some(metadata.identity),
        Err(error) if fs_error_kind(&error) == Some(std::io::ErrorKind::NotFound) => None,
        Err(error) => return Err(error).context("reading stored llama identity"),
    };
    store
        .replace_file(Replacement {
            relative: relative.to_path_buf(),
            expected,
            bytes,
            mode,
        })
        .context("atomically replacing llama state")?;
    Ok(())
}

fn fs_error_kind(error: &FsError) -> Option<std::io::ErrorKind> {
    match error {
        FsError::System { kind, .. } => Some(*kind),
        _ => None,
    }
}
fn unix_millis() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;

    use super::*;
    use futures_util::FutureExt;
    use tempfile::TempDir;

    struct RecordingExecutor {
        requests: Mutex<Vec<pi_ai::HttpRequestParts>>,
        responses: Mutex<VecDeque<(http::StatusCode, http::HeaderMap, Vec<u8>)>>,
    }

    impl RecordingExecutor {
        fn new(responses: Vec<(http::StatusCode, http::HeaderMap, Vec<u8>)>) -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into()),
            })
        }
    }

    impl pi_ai::HttpExecutor for RecordingExecutor {
        fn execute<'a>(
            &'a self,
            request: pi_ai::HttpRequest,
        ) -> futures_util::future::BoxFuture<'a, Result<pi_ai::HttpResponse>> {
            async move {
                self.requests
                    .lock()
                    .expect("request lock")
                    .push(request.into_parts());
                let (status, headers, body) = self
                    .responses
                    .lock()
                    .expect("response lock")
                    .pop_front()
                    .expect("queued response");
                Ok(pi_ai::HttpResponse::new(
                    status,
                    headers,
                    Box::pin(futures_util::stream::iter([Ok(body)])),
                ))
            }
            .boxed()
        }
    }

    #[test]
    fn persisted_json_accepts_exact_size_limit() {
        let directory = TempDir::new().expect("temporary directory");
        let manager = LlamaManager::new(directory.path().to_path_buf()).expect("llama manager");
        let payload_len = usize::try_from(MAX_STORED_JSON_BYTES).expect("limit fits usize") - 2;
        let payload = format!("\"{}\"", "a".repeat(payload_len));
        assert_eq!(u64::try_from(payload.len()).unwrap(), MAX_STORED_JSON_BYTES);
        manager
            .store
            .replace_file(Replacement {
                relative: PathBuf::from(CATALOG_FILE),
                expected: None,
                bytes: payload.into_bytes(),
                mode: FileMode::Private,
            })
            .expect("write exact-boundary JSON");

        let value = read_optional_json::<String>(
            &manager.store,
            Path::new(CATALOG_FILE),
            "llama.cpp model catalog",
        )
        .expect("read exact-boundary JSON")
        .expect("stored value");
        assert_eq!(value.len(), payload_len);
    }

    #[test]
    fn persisted_json_rejects_one_byte_over_limit_with_context() {
        let directory = TempDir::new().expect("temporary directory");
        let manager = LlamaManager::new(directory.path().to_path_buf()).expect("llama manager");
        let oversized =
            vec![b' '; usize::try_from(MAX_STORED_JSON_BYTES + 1).expect("limit fits usize")];
        manager
            .store
            .replace_file(Replacement {
                relative: PathBuf::from(INSTALLED_FILE),
                expected: None,
                bytes: oversized,
                mode: FileMode::Private,
            })
            .expect("write oversized JSON");

        let error = read_optional_json::<serde_json::Value>(
            &manager.store,
            Path::new(INSTALLED_FILE),
            "installed GGUF catalog",
        )
        .expect_err("oversized JSON must fail");
        let message = format!("{error:#}");
        assert!(message.contains("installed GGUF catalog"));
        assert!(message.contains(&MAX_STORED_JSON_BYTES.to_string()));
    }

    #[tokio::test]
    async fn gguf_resume_uses_injected_executor_and_preserves_integrity() {
        let directory = TempDir::new().expect("temporary directory");
        let manager = LlamaManager::new(directory.path().to_path_buf()).expect("llama manager");
        manager
            .store
            .create_dir_all(Path::new("models"), FileMode::Private)
            .expect("models directory");
        manager
            .store
            .replace_file(Replacement {
                relative: PathBuf::from("models/model.gguf.part"),
                expected: None,
                bytes: b"abc".to_vec(),
                mode: FileMode::Private,
            })
            .expect("partial file");
        let mut response_headers = http::HeaderMap::new();
        response_headers.insert(
            http::header::CONTENT_RANGE,
            http::HeaderValue::from_static("bytes 3-5/6"),
        );
        response_headers.insert(
            http::header::CONTENT_LENGTH,
            http::HeaderValue::from_static("3"),
        );
        let executor = RecordingExecutor::new(vec![(
            http::StatusCode::PARTIAL_CONTENT,
            response_headers,
            b"def".to_vec(),
        )]);
        let client = HuggingFaceClient::new(
            Some("hf-secret".to_owned()),
            Some("https://huggingface.example"),
            executor.clone(),
        )
        .expect("client");
        let metadata = HuggingFaceGgufFile {
            name: "model.gguf".to_owned(),
            size: Some(6),
            sha256: Some(format!("{:x}", Sha256::digest(b"abcdef"))),
            quantization: "Q4_K_M".to_owned(),
        };
        let mut progress = Vec::new();

        download_one(
            &manager.store,
            &client,
            &metadata,
            "https://huggingface.example/owner/model/resolve/main/model.gguf",
            Path::new("models/model.gguf"),
            executor.clone(),
            &CancellationToken::new(),
            &mut |update| progress.push(update),
        )
        .await
        .expect("download");

        let installed = manager
            .store
            .read_bounded(Path::new("models/model.gguf"), 6)
            .expect("installed file");
        assert_eq!(installed, b"abcdef");
        assert!(progress.iter().all(|update| update.resumed));
        let request = executor.requests.lock().expect("request lock").remove(0);
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(request.timeout_ms, 120_000);
        assert_eq!(
            request.max_response_bytes.get(),
            pi_ai::MAX_HTTP_RESPONSE_BODY_BYTES
        );
        assert_eq!(
            request
                .headers
                .iter()
                .find(|(name, _)| name == http::header::RANGE)
                .expect("range header")
                .1
                .to_str()
                .expect("range"),
            "bytes=3-"
        );
        let authorization = request
            .headers
            .iter()
            .find(|(name, _)| name == http::header::AUTHORIZATION)
            .expect("authorization header");
        assert!(authorization.1.is_sensitive());
    }
}
