//! Persistent llama.cpp router configuration, catalog refresh, and GGUF installs.

use std::{
    fs::{File, OpenOptions},
    io::{Read as _, Write as _},
    path::{Component, Path, PathBuf},
    sync::{LazyLock, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use pi_ai::{
    HuggingFaceClient, HuggingFaceGgufFile, HuggingFaceModelDetails, LlamaRouterClient,
    LlamaRouterModel, LlamaRouterSettings, Model, LLAMA_PROVIDER_ID, replace_registered_models,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::resources::agent_dir;

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

#[derive(Debug, Clone)]
pub struct LlamaManager {
    root: PathBuf,
}

impl Default for LlamaManager {
    fn default() -> Self {
        Self::new(llama_data_dir())
    }
}

impl LlamaManager {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
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
        read_optional_json(&self.settings_path(), "llama.cpp router settings")
    }
    /// Effective settings prefer persisted configuration, then the standard
    /// `LLAMA_BASE_URL` / optional `LLAMA_API_KEY` environment adapter.
    pub fn effective_settings(&self) -> Result<Option<LlamaRouterSettings>> {
        if let Some(settings) = self.settings()? {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let path = self.settings_path();
                let metadata = std::fs::metadata(&path)
                    .with_context(|| format!("reading llama.cpp settings permissions {}", path.display()))?;
                if metadata.permissions().mode() & 0o077 != 0 {
                    bail!("llama.cpp settings file {} must not be accessible by group or other users", path.display());
                }
            }
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

    pub fn configured_client(&self) -> Result<LlamaRouterClient> {
        let settings = self.effective_settings()?.ok_or_else(|| {
            anyhow!(
                "llama.cpp router is not configured; run `rpi llama configure {}` or set LLAMA_BASE_URL",
                pi_ai::DEFAULT_LLAMA_SERVER_URL
            )
        })?;
        LlamaRouterClient::new(settings)
    }

    /// Validate connectivity and the live `/v1/models` response before making
    /// the new configuration visible. Both settings and the observed catalog
    /// are then committed atomically per file.
    pub async fn configure(&self, settings: LlamaRouterSettings) -> Result<Vec<Model>> {
        let settings = LlamaRouterSettings::validated(&settings.base_url, settings.api_key)?;
        let client = LlamaRouterClient::new(settings.clone())?;
        let models = client
            .discover_models()
            .await
            .context("validating llama.cpp router configuration")?;
        let snapshot = LlamaCatalogSnapshot {
            checked_at: unix_millis(),
            models: models.clone(),
        };
        write_json_atomic(&self.catalog_path(), &snapshot)
            .context("persisting validated llama.cpp catalog")?;
        write_json_atomic_secret(&self.settings_path(), &settings)
            .context("persisting validated llama.cpp settings")?;
        apply_catalog(models.clone());
        Ok(models)
    }

    pub fn cached_catalog(&self) -> Result<Option<LlamaCatalogSnapshot>> {
        read_optional_json(&self.catalog_path(), "llama.cpp model catalog")
    }

    /// Refresh from the live router. If the router is unavailable, apply and
    /// return the last successfully observed snapshot rather than synthesizing
    /// a model. A missing cache preserves the live error.
    pub async fn refresh_catalog(&self) -> Result<CatalogRefresh> {
        let client = self.configured_client()?;
        match client.discover_models().await {
            Ok(models) => {
                let snapshot = LlamaCatalogSnapshot {
                    checked_at: unix_millis(),
                    models: models.clone(),
                };
                write_json_atomic(&self.catalog_path(), &snapshot)
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

    pub async fn router_models(&self, reload: bool) -> Result<Vec<LlamaRouterModel>> {
        self.configured_client()?
            .list_router_models(reload)
            .await
            .context("reading llama.cpp router management catalog")
    }

    pub async fn load_model(&self, model: &str) -> Result<Vec<Model>> {
        validate_model_argument(model)?;
        let client = self.configured_client()?;
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
        write_json_atomic(&self.catalog_path(), &snapshot)
            .context("persisting llama.cpp catalog after load")?;
        apply_catalog(models.clone());
        Ok(models)
    }

    pub async fn unload_model(&self, model: &str) -> Result<Vec<Model>> {
        validate_model_argument(model)?;
        let client = self.configured_client()?;
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
        write_json_atomic(&self.catalog_path(), &snapshot)
            .context("persisting llama.cpp catalog after unload")?;
        apply_catalog(models.clone());
        Ok(models)
    }

    pub fn installed(&self) -> Result<InstalledGgufCatalog> {
        Ok(read_optional_json(&self.installed_path(), "installed GGUF catalog")?
            .unwrap_or_default())
    }

    pub async fn install_from_hugging_face<F>(
        &self,
        client: &HuggingFaceClient,
        details: &HuggingFaceModelDetails,
        quantization: Option<&str>,
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
            None => details
                .quantizations
                .first()
                .ok_or_else(|| anyhow!("Hugging Face model {:?} has no downloadable GGUF files", details.id))?,
        };
        if selected.files.is_empty() {
            bail!(
                "Hugging Face model {:?} quantization {:?} has no downloadable files",
                details.id,
                selected.name
            );
        }
        let destination = self.model_install_dir(&details.id, &selected.name)?;
        tokio::fs::create_dir_all(&destination)
            .await
            .with_context(|| format!("creating GGUF install directory {}", destination.display()))?;
        let mut installed_files = Vec::with_capacity(selected.files.len());
        for file in &selected.files {
            if cancel.is_cancelled() {
                bail!("GGUF download cancelled");
            }
            let relative = safe_relative_path(&file.name)?;
            let target = destination.join(&relative);
            ensure_within(&destination, &target)?;
            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .with_context(|| format!("creating GGUF directory {}", parent.display()))?;
            }
            let source = client.resolve_url(&details.id, &file.name)?;
            download_one(
                client,
                file,
                source.as_str(),
                &target,
                cancel,
                &mut progress,
            )
            .await?;
            let metadata = tokio::fs::metadata(&target)
                .await
                .with_context(|| format!("reading downloaded GGUF metadata {}", target.display()))?;
            let relative_path = target
                .strip_prefix(&self.root)
                .map_err(|_| anyhow!("download escaped llama data directory"))?
                .to_path_buf();
            installed_files.push(InstalledGgufFile {
                source: source.to_string(),
                relative_path,
                size: metadata.len(),
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
        write_json_atomic(&self.installed_path(), &catalog)
            .context("persisting installed GGUF catalog")?;
        Ok(installed)
    }

    fn model_install_dir(&self, repository: &str, quantization: &str) -> Result<PathBuf> {
        validate_repository(repository)?;
        validate_component(quantization, "quantization")?;
        let mut target = self.models_dir();
        for component in repository.split('/') {
            target.push(component);
        }
        target.push(quantization);
        ensure_within(&self.models_dir(), &target)?;
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

#[derive(Debug, Clone, Default)]
pub struct LlamaAuthAdapter {
    manager: LlamaManager,
}

impl LlamaAuthAdapter {
    #[must_use]
    pub fn new(manager: LlamaManager) -> Self {
        Self { manager }
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
    std::env::var_os("PI_CODING_AGENT_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(agent_dir()))
        .join(LLAMA_DIR)
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
    client: &HuggingFaceClient,
    metadata: &HuggingFaceGgufFile,
    source: &str,
    target: &Path,
    cancel: &CancellationToken,
    progress: &mut F,
) -> Result<()>
where
    F: FnMut(GgufDownloadProgress) + Send,
{
    if let Ok(existing) = tokio::fs::metadata(target).await {
        if metadata.size.is_none_or(|size| size == existing.len())
            && checksum_matches(target, metadata.sha256.as_deref()).await?
        {
            progress(GgufDownloadProgress {
                file: metadata.name.clone(),
                downloaded: existing.len(),
                total: metadata.size,
                resumed: true,
            });
            return Ok(());
        }
    }
    let part = part_path(target)?;
    let mut offset = tokio::fs::metadata(&part).await.map_or(0, |entry| entry.len());
    if metadata.size.is_some_and(|size| offset > size) {
        tokio::fs::remove_file(&part)
            .await
            .with_context(|| format!("discarding oversized partial GGUF {}", part.display()))?;
        offset = 0;
    }
    let http = reqwest::Client::builder()
        .build()
        .context("building GGUF download client")?;
    let mut request = http
        .get(source)
        .header(reqwest::header::ACCEPT_ENCODING, "identity");
    if let Some(token) = client.token() {
        request = request.bearer_auth(token);
    }
    if offset > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
    }
    let response = tokio::select! {
        () = cancel.cancelled() => bail!("GGUF download cancelled"),
        response = request.send() => response.with_context(|| format!("downloading GGUF from {source}"))?,
    };
    let status = response.status();
    let append = offset > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT;
    if append {
        let expected_prefix = format!("bytes {offset}-");
        let content_range = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !content_range.starts_with(&expected_prefix) {
            bail!(
                "downloading GGUF {}: server returned mismatched Content-Range {content_range:?} for resume offset {offset}",
                metadata.name
            );
        }
    }
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!(
            "downloading GGUF {}: HTTP {status}: {}",
            metadata.name,
            body.trim()
        );
    }
    if offset > 0 && !append {
        offset = 0;
    }
    let total = metadata.size.or_else(|| {
        response
            .content_length()
            .and_then(|length| length.checked_add(offset))
    });
    let mut options = tokio::fs::OpenOptions::new();
    options.create(true).write(true);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }
    let mut file = options
        .open(&part)
        .await
        .with_context(|| format!("opening partial GGUF {}", part.display()))?;
    if append {
        file.seek(std::io::SeekFrom::End(0))
            .await
            .with_context(|| format!("seeking partial GGUF {}", part.display()))?;
    }
    let mut downloaded = offset;
    progress(GgufDownloadProgress {
        file: metadata.name.clone(),
        downloaded,
        total,
        resumed: offset > 0,
    });
    let mut stream = response.bytes_stream();
    loop {
        let next = tokio::select! {
            () = cancel.cancelled() => {
                file.flush().await.ok();
                file.sync_all().await.ok();
                bail!("GGUF download cancelled");
            }
            next = stream.next() => next,
        };
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.with_context(|| format!("streaming GGUF {}", metadata.name))?;
        file.write_all(&chunk)
            .await
            .with_context(|| format!("writing partial GGUF {}", part.display()))?;
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
    file.flush()
        .await
        .with_context(|| format!("flushing partial GGUF {}", part.display()))?;
    file.sync_all()
        .await
        .with_context(|| format!("syncing partial GGUF {}", part.display()))?;
    drop(file);
    if let Some(expected) = metadata.size
        && downloaded != expected
    {
        bail!(
            "downloaded GGUF {} has size {downloaded}, expected {expected}; partial file retained for resume",
            metadata.name
        );
    }
    if !checksum_matches(&part, metadata.sha256.as_deref()).await? {
        bail!(
            "downloaded GGUF {} failed SHA-256 verification; partial file retained at {}",
            metadata.name,
            part.display()
        );
    }
    tokio::fs::rename(&part, target)
        .await
        .with_context(|| format!("atomically installing GGUF {}", target.display()))?;
    sync_parent(target).context("syncing installed GGUF directory")?;
    Ok(())
}

async fn checksum_matches(path: &Path, expected: Option<&str>) -> Result<bool> {
    let Some(expected) = expected else {
        return Ok(true);
    };
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("opening GGUF for checksum {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .with_context(|| format!("reading GGUF for checksum {}", path.display()))?;
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

fn ensure_within(root: &Path, target: &Path) -> Result<()> {
    if !target.starts_with(root) {
        bail!(
            "path {} escapes llama data directory {}",
            target.display(),
            root.display()
        );
    }
    Ok(())
}

fn read_optional_json<T: DeserializeOwned>(path: &Path, label: &str) -> Result<Option<T>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("opening {label} at {}", path.display()));
        }
    };
    let declared_size = file
        .metadata()
        .with_context(|| format!("reading {label} metadata at {}", path.display()))?
        .len();
    if declared_size > MAX_STORED_JSON_BYTES {
        bail!(
            "{label} at {} is too large: {declared_size} bytes exceeds the {MAX_STORED_JSON_BYTES}-byte limit",
            path.display()
        );
    }
    let capacity = usize::try_from(declared_size)
        .context("stored llama JSON size does not fit in memory address space")?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(MAX_STORED_JSON_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading bounded {label} from {}", path.display()))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_STORED_JSON_BYTES {
        bail!(
            "{label} at {} grew beyond the {MAX_STORED_JSON_BYTES}-byte limit while being read",
            path.display()
        );
    }
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {label} from {}", path.display()))
        .map(Some)
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_json_atomic_with_mode(path, value, false)
}

fn write_json_atomic_secret<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_json_atomic_with_mode(path, value, true)
}

fn write_json_atomic_with_mode<T: Serialize>(path: &Path, value: &T, secret: bool) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("stored path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating llama data directory {}", parent.display()))?;
    let bytes = serde_json::to_vec_pretty(value).context("serializing llama state")?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow!("stored llama path is not UTF-8: {}", path.display()))?;
    let temp = parent.join(format!(".{name}.tmp-{}-{}", std::process::id(), unix_millis()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if secret { 0o600 } else { 0o644 });
    }
    let mut file = options
        .open(&temp)
        .with_context(|| format!("creating temporary llama state {}", temp.display()))?;
    let result = (|| -> Result<()> {
        file.write_all(&bytes)
            .with_context(|| format!("writing temporary llama state {}", temp.display()))?;
        file.write_all(b"\n")
            .with_context(|| format!("terminating temporary llama state {}", temp.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing temporary llama state {}", temp.display()))?;
        std::fs::rename(&temp, path).with_context(|| {
            format!(
                "atomically replacing llama state {} with {}",
                path.display(),
                temp.display()
            )
        })?;
        sync_parent(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn sync_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    let directory = std::fs::File::open(parent)
        .with_context(|| format!("opening directory for sync {}", parent.display()))?;
    directory
        .sync_all()
        .with_context(|| format!("syncing directory {}", parent.display()))
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
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn persisted_json_accepts_exact_size_limit() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("catalog.json");
        let payload_len = usize::try_from(MAX_STORED_JSON_BYTES).expect("limit fits usize") - 2;
        let payload = format!("\"{}\"", "a".repeat(payload_len));
        assert_eq!(u64::try_from(payload.len()).unwrap(), MAX_STORED_JSON_BYTES);
        std::fs::write(&path, payload).expect("write exact-boundary JSON");

        let value = read_optional_json::<String>(&path, "llama.cpp model catalog")
            .expect("read exact-boundary JSON")
            .expect("stored value");
        assert_eq!(value.len(), payload_len);
    }

    #[test]
    fn persisted_json_rejects_one_byte_over_limit_with_context() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("installed.json");
        let oversized = vec![b' '; usize::try_from(MAX_STORED_JSON_BYTES + 1).expect("limit fits usize")];
        std::fs::write(&path, oversized).expect("write oversized JSON");

        let error = read_optional_json::<serde_json::Value>(&path, "installed GGUF catalog")
            .expect_err("oversized JSON must fail");
        let message = format!("{error:#}");
        assert!(message.contains("installed GGUF catalog"));
        assert!(message.contains(path.to_string_lossy().as_ref()));
        assert!(message.contains(&(MAX_STORED_JSON_BYTES + 1).to_string()));
        assert!(message.contains(&MAX_STORED_JSON_BYTES.to_string()));
    }
}
