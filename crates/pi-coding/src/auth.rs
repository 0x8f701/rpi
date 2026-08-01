use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::oauth;

const MAX_AVAILABLE_MODEL_IDS: usize = 1_024;
const MAX_AVAILABLE_MODEL_ID_BYTES: usize = 512;
const MAX_AVAILABLE_MODEL_IDS_BYTES: usize = 64 * 1_024;
const AUTH_LOCK_TIMEOUT: Duration = Duration::from_secs(45);
const AUTH_LOCK_RETRY_DELAY: Duration = Duration::from_millis(50);
const MAX_AUTH_FILE_BYTES: u64 = 1024 * 1024;
const MAX_AUTH_LOCK_BYTES: u64 = 16 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Credential {
    ApiKey {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        env: HashMap<String, String>,
    },
    #[serde(rename = "oauth", alias = "o_auth")]
    OAuth {
        refresh: String,
        access: String,
        expires: i64,
        #[serde(flatten)]
        fields: Map<String, Value>,
    },
}

impl Credential {
    #[must_use]
    pub fn credential_type(&self) -> AuthType {
        match self {
            Self::ApiKey { .. } => AuthType::ApiKey,
            Self::OAuth { .. } => AuthType::OAuth,
        }
    }

    #[must_use]
    pub fn oauth_parts(&self) -> Option<(&str, &str, i64, &Map<String, Value>)> {
        match self {
            Self::OAuth {
                refresh,
                access,
                expires,
                fields,
            } => Some((refresh, access, *expires, fields)),
            Self::ApiKey { .. } => None,
        }
    }
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Credential")
            .field("type", &self.credential_type())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthType {
    ApiKey,
    OAuth,
}

impl AuthType {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ApiKey => "API key",
            Self::OAuth => "subscription",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialInfo {
    pub provider_id: String,
    pub credential_type: AuthType,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthPrompt {
    Text {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
    },
    Secret {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
    },
    Select {
        message: String,
        options: Vec<AuthPromptOption>,
    },
    ManualCode {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthPromptOption {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthEvent {
    Info {
        message: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        links: Vec<AuthInfoLink>,
    },
    AuthUrl {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
    },
    DeviceCode {
        user_code: String,
        verification_uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interval_seconds: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_in_seconds: Option<f64>,
    },
    Progress {
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthInfoLink {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[async_trait]
pub trait AuthInteraction: Send + Sync {
    async fn prompt(&self, prompt: AuthPrompt) -> Result<String>;
    fn notify(&self, event: AuthEvent);
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthMethodInfo {
    ApiKey { name: String },
    OAuth {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        login_label: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthProviderInfo {
    pub id: String,
    pub name: String,
    pub methods: Vec<AuthMethodInfo>,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct RequestAuth {
    pub api_key: String,
    pub headers: HashMap<String, String>,
    pub env: HashMap<String, String>,
    pub available_model_ids: Option<Vec<String>>,
}

impl std::fmt::Debug for RequestAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RequestAuth")
            .field("has_api_key", &!self.api_key.is_empty())
            .field("header_count", &self.headers.len())
            .field("env_count", &self.env.len())
            .field("available_model_count", &self.available_model_ids.as_ref().map(Vec::len))
            .finish()
    }
}

pub(crate) fn available_model_ids(fields: &Map<String, Value>) -> Option<Vec<String>> {
    let values = fields.get("availableModelIds")?.as_array()?;
    if values.len() > MAX_AVAILABLE_MODEL_IDS {
        return None;
    }
    let mut total_bytes = 0usize;
    let mut ids = Vec::with_capacity(values.len());
    for value in values {
        let id = value.as_str()?;
        if id.len() > MAX_AVAILABLE_MODEL_ID_BYTES {
            return None;
        }
        total_bytes = total_bytes.checked_add(id.len())?;
        if total_bytes > MAX_AVAILABLE_MODEL_IDS_BYTES {
            return None;
        }
        ids.push(id.to_owned());
    }
    Some(ids)
}

#[derive(Clone, Copy)]
struct AuthLockOptions {
    timeout: Duration,
    retry_delay: Duration,
}

impl Default for AuthLockOptions {
    fn default() -> Self {
        Self {
            timeout: AUTH_LOCK_TIMEOUT,
            retry_delay: AUTH_LOCK_RETRY_DELAY,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthLockOwner {
    pid: u32,
    host_fingerprint: Option<String>,
    boot_id: Option<String>,
    process_start: Option<String>,
    token: String,
}

impl AuthLockOwner {
    fn current() -> Self {
        Self {
            pid: std::process::id(),
            host_fingerprint: local_host_fingerprint(),
            boot_id: local_boot_id(),
            process_start: process_start_identity(std::process::id()).ok().flatten(),
            token: Uuid::new_v4().to_string(),
        }
    }

    fn is_definitely_stale(&self, current: &Self) -> bool {
        let same_host = self
            .host_fingerprint
            .as_ref()
            .zip(current.host_fingerprint.as_ref())
            .is_some_and(|(owner, local)| owner == local);
        if !same_host {
            return false;
        }

        if self
            .boot_id
            .as_ref()
            .zip(current.boot_id.as_ref())
            .is_some_and(|(owner, local)| owner != local)
        {
            return true;
        }

        process_owner_is_stale(self)
    }
}

struct AuthFileLock {
    path: PathBuf,
    token: String,
}

impl AuthFileLock {
    async fn acquire(auth_path: &Path, options: AuthLockOptions) -> Result<Self> {
        let parent = auth_path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .with_context(|| format!("creating auth directory {}", parent.display()))?;
        set_directory_permissions(parent)?;

        let path = auth_lock_path(auth_path);
        let owner = AuthLockOwner::current();
        let started = Instant::now();
        loop {
            match create_auth_lock_file(&path, &owner) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        token: owner.token,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if remove_stale_auth_lock(&path, &owner)? {
                        continue;
                    }
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("creating auth lock file {}", path.display()));
                }
            }

            let elapsed = started.elapsed();
            if elapsed >= options.timeout {
                bail!("timed out acquiring auth lock for {}", auth_path.display());
            }
            let remaining = options.timeout - elapsed;
            tokio::time::sleep(options.retry_delay.min(remaining)).await;
            if started.elapsed() >= options.timeout {
                bail!("timed out acquiring auth lock for {}", auth_path.display());
            }
        }
    }
}

impl Drop for AuthFileLock {
    fn drop(&mut self) {
        let Ok(content) = fs::read(&self.path) else {
            return;
        };
        let Ok(owner) = serde_json::from_slice::<AuthLockOwner>(&content) else {
            return;
        };
        if owner.token == self.token {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn create_auth_lock_file(path: &Path, owner: &AuthLockOwner) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options.open(path)?;
    let result = (|| {
        serde_json::to_writer(&mut file, owner).map_err(std::io::Error::other)?;
        file.write_all(b"\n")?;
        file.sync_all()
    })();
    drop(file);
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

fn remove_stale_auth_lock(path: &Path, current: &AuthLockOwner) -> Result<bool> {
    let owner = match read_auth_lock_owner(path)? {
        LockOwnerRead::Missing => return Ok(true),
        LockOwnerRead::Invalid => return Ok(false),
        LockOwnerRead::Owner(owner) => owner,
    };
    if !owner.is_definitely_stale(current) {
        return Ok(false);
    }

    let claim = loop {
        let candidate = stale_claim_path(path);
        match fs::hard_link(path, &candidate) {
            Ok(()) => break candidate,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("claiming stale auth lock at {}", path.display()));
            }
        }
    };

    let result = (|| -> Result<bool> {
        let claimed_owner = match read_auth_lock_owner(&claim)? {
            LockOwnerRead::Owner(owner) => owner,
            LockOwnerRead::Missing | LockOwnerRead::Invalid => return Ok(false),
        };
        if claimed_owner.token != owner.token || !claimed_owner.is_definitely_stale(current) {
            return Ok(false);
        }

        let Some(path_identity) = file_identity(path)? else {
            return Ok(true);
        };
        let Some(claim_identity) = file_identity(&claim)? else {
            return Ok(false);
        };
        if path_identity != claim_identity {
            return Ok(false);
        }
        let Some(final_path_identity) = file_identity(path)? else {
            return Ok(true);
        };
        if final_path_identity != claim_identity {
            return Ok(false);
        }

        match fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(error)
                .with_context(|| format!("removing stale auth lock at {}", path.display())),
        }
    })();
    let _ = fs::remove_file(&claim);
    result
}
enum LockOwnerRead {
    Missing,
    Invalid,
    Owner(AuthLockOwner),
}

fn read_auth_lock_owner(path: &Path) -> Result<LockOwnerRead> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(LockOwnerRead::Missing),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading auth lock metadata at {}", path.display()));
        }
    };
    if !metadata.is_file() || metadata.len() > MAX_AUTH_LOCK_BYTES {
        return Ok(LockOwnerRead::Invalid);
    }

    let mut file = File::open(path)
        .with_context(|| format!("opening auth lock owner at {}", path.display()))?;
    let mut content = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    Read::by_ref(&mut file)
        .take(MAX_AUTH_LOCK_BYTES + 1)
        .read_to_end(&mut content)
        .with_context(|| format!("reading auth lock owner at {}", path.display()))?;
    if content.len() as u64 > MAX_AUTH_LOCK_BYTES {
        return Ok(LockOwnerRead::Invalid);
    }
    Ok(match serde_json::from_slice(&content) {
        Ok(owner) => LockOwnerRead::Owner(owner),
        Err(_) => LockOwnerRead::Invalid,
    })
}

fn stale_claim_path(path: &Path) -> PathBuf {
    let mut file_name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("auth.json.lock"))
        .to_os_string();
    file_name.push(format!(".{}.claim", Uuid::new_v4()));
    path.with_file_name(file_name)
}

#[cfg(unix)]
fn file_identity(path: &Path) -> Result<Option<(u64, u64)>> {
    use std::os::unix::fs::MetadataExt;

    match fs::metadata(path) {
        Ok(metadata) => Ok(Some((metadata.dev(), metadata.ino()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("reading auth lock identity at {}", path.display())),
    }
}

#[cfg(not(unix))]
fn file_identity(_path: &Path) -> Result<Option<(u64, u64)>> {
    // Stable std does not expose a portable file identity on every platform.
    // Without it, stale recovery must fail closed rather than risk unlinking a
    // replacement live lock.
    Ok(None)
}

fn auth_lock_path(auth_path: &Path) -> PathBuf {
    let mut file_name = auth_path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("auth.json"))
        .to_os_string();
    file_name.push(".lock");
    auth_path.with_file_name(file_name)
}


fn local_host_fingerprint() -> Option<String> {
    let identity = ["/etc/machine-id", "/var/lib/dbus/machine-id"]
        .into_iter()
        .find_map(read_nonempty_trimmed)
        .or_else(|| read_nonempty_trimmed("/proc/sys/kernel/hostname"))
        .or_else(|| {
            std::env::var("COMPUTERNAME")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| {
            std::env::var("HOSTNAME")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })?;
    Some(format!("{:x}", Sha256::digest(identity.trim().as_bytes())))
}

fn read_nonempty_trimmed(path: &str) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(target_os = "linux")]
fn local_boot_id() -> Option<String> {
    read_nonempty_trimmed("/proc/sys/kernel/random/boot_id")
}

#[cfg(not(target_os = "linux"))]
fn local_boot_id() -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
fn process_start_identity(pid: u32) -> std::io::Result<Option<String>> {
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let command_end = stat.rfind(')').ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid process stat")
    })?;
    let start = stat[command_end + 1..]
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid process stat")
        })?;
    Ok(Some(start.to_owned()))
}

#[cfg(not(target_os = "linux"))]
fn process_start_identity(_pid: u32) -> std::io::Result<Option<String>> {
    Ok(None)
}

#[cfg(target_os = "linux")]
fn process_owner_is_stale(owner: &AuthLockOwner) -> bool {
    match process_start_identity(owner.pid) {
        Ok(None) => true,
        Ok(Some(actual_start)) => owner
            .process_start
            .as_ref()
            .is_some_and(|recorded_start| recorded_start != &actual_start),
        Err(_) => false,
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_owner_is_stale(owner: &AuthLockOwner) -> bool {
    let Ok(pid) = i32::try_from(owner.pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    matches!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
        Err(nix::errno::Errno::ESRCH)
    )
}

#[cfg(not(unix))]
fn process_owner_is_stale(_owner: &AuthLockOwner) -> bool {
    false
}

#[derive(Clone)]
pub struct AuthStorage {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
    lock_options: AuthLockOptions,
}

impl AuthStorage {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Arc::new(Mutex::new(())),
            lock_options: AuthLockOptions::default(),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn read(&self, provider: &str) -> Result<Option<Credential>> {
        let _guard = self.lock.lock().await;
        let _file_guard = AuthFileLock::acquire(&self.path, self.lock_options).await?;
        Ok(load_credentials(&self.path)?.remove(provider))
    }

    pub async fn list(&self) -> Result<Vec<CredentialInfo>> {
        let _guard = self.lock.lock().await;
        let _file_guard = AuthFileLock::acquire(&self.path, self.lock_options).await?;
        Ok(load_credentials(&self.path)?
            .into_iter()
            .map(|(provider_id, credential)| CredentialInfo {
                provider_id,
                credential_type: credential.credential_type(),
            })
            .collect())
    }

    pub async fn modify<F, Fut>(&self, provider: &str, update: F) -> Result<Option<Credential>>
    where
        F: FnOnce(Option<Credential>) -> Fut + Send,
        Fut: Future<Output = Result<Option<Credential>>> + Send,
    {
        let _guard = self.lock.lock().await;
        let _file_guard = AuthFileLock::acquire(&self.path, self.lock_options).await?;
        let mut credentials = load_credentials(&self.path)?;
        let current = credentials.get(provider).cloned();
        let Some(next) = update(current.clone()).await? else {
            return Ok(current);
        };
        credentials.insert(provider.to_owned(), next.clone());
        write_credentials_atomic(&self.path, &credentials)?;
        Ok(Some(next))
    }

    pub async fn delete(&self, provider: &str) -> Result<bool> {
        let _guard = self.lock.lock().await;
        let _file_guard = AuthFileLock::acquire(&self.path, self.lock_options).await?;
        let mut credentials = load_credentials(&self.path)?;
        let removed = credentials.remove(provider).is_some();
        if removed {
            write_credentials_atomic(&self.path, &credentials)?;
        }
        Ok(removed)
    }
}

#[derive(Clone)]
pub struct AuthManager {
    storage: AuthStorage,
    client: reqwest::Client,
}

impl AuthManager {
    pub fn new(path: PathBuf) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("creating authentication HTTP client")?;
        Ok(Self {
            storage: AuthStorage::new(path),
            client,
        })
    }

    #[must_use]
    pub fn storage(&self) -> &AuthStorage {
        &self.storage
    }

    #[must_use]
    pub fn providers(&self) -> Vec<AuthProviderInfo> {
        let mut providers = pi_ai::get_providers()
            .into_iter()
            .map(|id| provider_info(&id))
            .collect::<Vec<_>>();
        for id in oauth::SUPPORTED_PROVIDERS {
            if !providers.iter().any(|provider| provider.id == *id) {
                providers.push(provider_info(id));
            }
        }
        providers.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
        providers
    }

    pub async fn login(
        &self,
        provider: Option<&str>,
        auth_type: Option<AuthType>,
        interaction: &dyn AuthInteraction,
    ) -> Result<CredentialInfo> {
        let provider_id = match provider.filter(|value| !value.trim().is_empty()) {
            Some(provider) => provider.trim().to_owned(),
            None => select_provider(interaction, &self.providers(), "Select provider to configure:").await?,
        };
        let info = provider_info(&provider_id);
        let selected_type = match auth_type {
            Some(auth_type) => auth_type,
            None if info.methods.len() == 1 => match info.methods[0] {
                AuthMethodInfo::ApiKey { .. } => AuthType::ApiKey,
                AuthMethodInfo::OAuth { .. } => AuthType::OAuth,
            },
            None => {
                let options = info
                    .methods
                    .iter()
                    .map(|method| match method {
                        AuthMethodInfo::ApiKey { name } => AuthPromptOption {
                            id: "api_key".to_owned(),
                            label: name.clone(),
                            description: Some("Store an API key in auth.json".to_owned()),
                        },
                        AuthMethodInfo::OAuth { name, login_label } => AuthPromptOption {
                            id: "oauth".to_owned(),
                            label: login_label.clone().unwrap_or_else(|| name.clone()),
                            description: Some("Use a provider subscription".to_owned()),
                        },
                    })
                    .collect();
                match interaction
                    .prompt(AuthPrompt::Select {
                        message: format!("Select authentication method for {}:", info.name),
                        options,
                    })
                    .await?
                    .as_str()
                {
                    "api_key" => AuthType::ApiKey,
                    "oauth" => AuthType::OAuth,
                    other => bail!("unsupported authentication method {other:?}"),
                }
            }
        };
        if !method_supported(&info, selected_type) {
            bail!(
                "provider {:?} does not support {} login",
                provider_id,
                selected_type.label()
            );
        }
        let credential = match selected_type {
            AuthType::ApiKey => {
                let key = interaction
                    .prompt(AuthPrompt::Secret {
                        message: format!("Enter {} API key", info.name),
                        placeholder: None,
                    })
                    .await?;
                if key.trim().is_empty() {
                    bail!("API key must not be empty")
                }
                Credential::ApiKey {
                    key: Some(key),
                    env: HashMap::new(),
                }
            }
            AuthType::OAuth => oauth::login(&provider_id, interaction, &self.client)
                .await
                .with_context(|| format!("{} login failed", info.name))?,
        };
        self.storage
            .modify(&provider_id, |_| async move { Ok(Some(credential)) })
            .await
            .with_context(|| format!("storing credential for provider {provider_id:?}"))?;
        Ok(CredentialInfo {
            provider_id,
            credential_type: selected_type,
        })
    }

    pub async fn logout(
        &self,
        provider: Option<&str>,
        interaction: &dyn AuthInteraction,
    ) -> Result<CredentialInfo> {
        let configured = self.storage.list().await.context("listing stored credentials")?;
        let provider_id = match provider.filter(|value| !value.trim().is_empty()) {
            Some(provider) => provider.trim().to_owned(),
            None => {
                if configured.is_empty() {
                    bail!("no stored credentials to remove")
                }
                let options = configured
                    .iter()
                    .map(|entry| AuthPromptOption {
                        id: entry.provider_id.clone(),
                        label: provider_info(&entry.provider_id).name,
                        description: Some(entry.credential_type.label().to_owned()),
                    })
                    .collect();
                interaction
                    .prompt(AuthPrompt::Select {
                        message: "Select provider to log out:".to_owned(),
                        options,
                    })
                    .await?
            }
        };
        let credential_type = configured
            .iter()
            .find(|entry| entry.provider_id == provider_id)
            .map(|entry| entry.credential_type)
            .ok_or_else(|| anyhow!("provider {provider_id:?} has no stored credential"))?;
        if !self
            .storage
            .delete(&provider_id)
            .await
            .with_context(|| format!("removing credential for provider {provider_id:?}"))?
        {
            bail!("provider {provider_id:?} has no stored credential")
        }
        Ok(CredentialInfo {
            provider_id,
            credential_type,
        })
    }

    pub async fn resolve_stored(
        &self,
        provider: &str,
        env: Option<&HashMap<String, String>>,
    ) -> Result<Option<RequestAuth>> {
        let provider_id = provider.to_owned();
        let client = self.client.clone();
        let credential = self
            .storage
            .modify(provider, move |current| {
                let provider_id = provider_id.clone();
                let client = client.clone();
                async move {
                    let Some(Credential::OAuth { expires, .. }) = current.as_ref() else {
                        return Ok(None);
                    };
                    if chrono::Utc::now().timestamp_millis() < *expires {
                        return Ok(None);
                    }
                    let current = current.expect("OAuth credential is present");
                    let refreshed = oauth::refresh(&provider_id, &current, &client)
                        .await
                        .with_context(|| format!("refreshing OAuth credential for provider {provider_id:?}"))?;
                    Ok(Some(refreshed))
                }
            })
            .await?;
        match credential {
            Some(Credential::ApiKey { key, env: credential_env }) => {
                let key = match key {
                    Some(key) => resolve_stored_value(&key, env, &credential_env)?,
                    None => String::new(),
                };
                Ok((!key.trim().is_empty()).then_some(RequestAuth {
                    api_key: key,
                    env: credential_env,
                    headers: HashMap::new(),
                    available_model_ids: None,
                }))
            }
            Some(credential @ Credential::OAuth { .. }) => {
                oauth::to_request_auth(provider, &credential).map(Some)
            }
            None => Ok(None),
        }
    }
}

async fn select_provider(
    interaction: &dyn AuthInteraction,
    providers: &[AuthProviderInfo],
    message: &str,
) -> Result<String> {
    interaction
        .prompt(AuthPrompt::Select {
            message: message.to_owned(),
            options: providers
                .iter()
                .map(|provider| AuthPromptOption {
                    id: provider.id.clone(),
                    label: provider.name.clone(),
                    description: Some(
                        provider
                            .methods
                            .iter()
                            .map(|method| match method {
                                AuthMethodInfo::ApiKey { .. } => "API key",
                                AuthMethodInfo::OAuth { .. } => "subscription",
                            })
                            .collect::<Vec<_>>()
                            .join(" or "),
                    ),
                })
                .collect(),
        })
        .await
}

fn provider_info(id: &str) -> AuthProviderInfo {
    let (name, oauth_name, oauth_label, api_key) = match id {
        "anthropic" => (
            "Anthropic",
            Some("Anthropic (Claude Pro/Max)"),
            None,
            true,
        ),
        "openai-codex" => (
            "OpenAI Codex",
            Some("OpenAI (ChatGPT Plus/Pro)"),
            Some("Sign in with ChatGPT"),
            false,
        ),
        "google-gemini-cli" => (
            "Google Gemini CLI",
            Some("Google Cloud Code Assist (Gemini CLI)"),
            Some("Sign in with Google"),
            false,
        ),
        "xai" => (
            "xAI",
            Some("xAI (Grok/X subscription)"),
            Some("Sign in with SuperGrok or X Premium"),
            true,
        ),
        "openrouter" => (
            "OpenRouter",
            Some("OpenRouter OAuth"),
            Some("Sign in with OpenRouter"),
            true,
        ),
        "kimi-coding" => (
            "Kimi For Coding",
            Some("Kimi Code (subscription)"),
            Some("Sign in with Kimi Code"),
            true,
        ),
        _ => (id, None, None, true),
    };
    let mut methods = Vec::new();
    if api_key {
        methods.push(AuthMethodInfo::ApiKey {
            name: format!("{name} API key"),
        });
    }
    if let Some(oauth_name) = oauth_name {
        methods.push(AuthMethodInfo::OAuth {
            name: oauth_name.to_owned(),
            login_label: oauth_label.map(str::to_owned),
        });
    }
    AuthProviderInfo {
        id: id.to_owned(),
        name: name.to_owned(),
        methods,
    }
}

fn method_supported(provider: &AuthProviderInfo, auth_type: AuthType) -> bool {
    provider.methods.iter().any(|method| {
        matches!(
            (method, auth_type),
            (AuthMethodInfo::ApiKey { .. }, AuthType::ApiKey)
                | (AuthMethodInfo::OAuth { .. }, AuthType::OAuth)
        )
    })
}

pub fn load_credentials(path: &Path) -> Result<BTreeMap<String, Credential>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading auth.json metadata at {}", path.display()));
        }
    };
    if !metadata.is_file() {
        bail!("auth.json path is not a file: {}", path.display());
    }
    if metadata.len() > MAX_AUTH_FILE_BYTES {
        bail!(
            "auth.json at {} exceeds {MAX_AUTH_FILE_BYTES} bytes",
            path.display()
        );
    }

    let mut file = File::open(path)
        .with_context(|| format!("opening auth.json at {}", path.display()))?;
    let mut content = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    Read::by_ref(&mut file)
        .take(MAX_AUTH_FILE_BYTES + 1)
        .read_to_end(&mut content)
        .with_context(|| format!("reading auth.json at {}", path.display()))?;
    if content.len() as u64 > MAX_AUTH_FILE_BYTES {
        bail!(
            "auth.json at {} exceeds {MAX_AUTH_FILE_BYTES} bytes",
            path.display()
        );
    }
    if content.iter().all(u8::is_ascii_whitespace) {
        return Ok(BTreeMap::new());
    }

    let root: Value = serde_json::from_slice(&content)
        .with_context(|| format!("parsing auth.json at {}", path.display()))?;
    let entries = root.as_object().ok_or_else(|| {
        anyhow!("invalid auth.json at {}: expected an object", path.display())
    })?;
    let mut credentials = BTreeMap::new();
    for (provider, value) in entries {
        let credential = serde_json::from_value(value.clone()).map_err(|error| {
            anyhow!(
                "invalid auth.json credential for provider {provider:?} at {}: {error}",
                path.display()
            )
        })?;
        credentials.insert(provider.clone(), credential);
    }
    Ok(credentials)
}

/// Read one stored credential without constructing an [`AuthStorage`] or
/// resolving configured API-key values.
///
/// Missing, unreadable, or invalid files are treated as having no credential.
/// The helper never includes stored values in an error because it does not
/// surface storage errors.
#[must_use]
pub fn read_stored_credential(provider: &str, path: impl AsRef<Path>) -> Option<Credential> {
    load_credentials(path.as_ref()).ok()?.remove(provider)
}

pub fn write_credentials_atomic(
    path: &Path,
    credentials: &BTreeMap<String, Credential>,
) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("creating auth directory {}", parent.display()))?;
    set_directory_permissions(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("auth.json");
    let temporary = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let file = open_private_file(&temporary)?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer_pretty(&mut writer, credentials)
            .context("serializing auth.json")?;
        writer.write_all(b"\n").context("writing auth.json newline")?;
        writer.flush().context("flushing auth.json temporary file")?;
        writer
            .get_ref()
            .sync_all()
            .context("syncing auth.json temporary file")?;
        fs::rename(&temporary, path).with_context(|| {
            format!(
                "atomically replacing auth.json {} from {}",
                path.display(),
                temporary.display()
            )
        })?;
        set_file_permissions(path)?;
        if let Ok(directory) = File::open(parent) {
            directory
                .sync_all()
                .with_context(|| format!("syncing auth directory {}", parent.display()))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn open_private_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("creating temporary auth file {}", path.display()))
}

fn set_directory_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("securing auth directory {}", path.display()))?;
    }
    Ok(())
}

fn set_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("securing auth file {}", path.display()))?;
    }
    Ok(())
}

fn resolve_stored_value(
    value: &str,
    request_env: Option<&HashMap<String, String>>,
    credential_env: &HashMap<String, String>,
) -> Result<String> {
    if value.starts_with('!') {
        bail!("command-valued stored API keys are not supported")
    }
    expand_credential_value(value, request_env, Some(credential_env), "auth.json")
}

/// Expand `$VAR` / `${VAR}` templates the same way `models.json` does.
///
/// - `$$` → literal `$`
/// - `$!` → literal `!`
/// - `$VAR` / `${VAR}` resolve from `request_env`, then `credential_env`, then
///   the process environment (empty values are treated as unset)
/// - Invalid braced names (e.g. `${1bad}`) are left unchanged
///
/// Errors name the missing variable and source only — never resolved secret
/// values or the original template text that might embed them.
fn expand_credential_value(
    value: &str,
    request_env: Option<&HashMap<String, String>>,
    credential_env: Option<&HashMap<String, String>>,
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
            while end < chars.len()
                && (chars[end].is_ascii_alphanumeric() || chars[end] == '_')
            {
                end += 1;
            }
            (chars[index + 1..end].iter().collect::<String>(), end)
        } else {
            output.push('$');
            index += 1;
            continue;
        };
        let resolved = env_lookup_with_fallback(&name, request_env, credential_env).ok_or_else(
            || anyhow!("environment variable {name} referenced by {source} is not set"),
        )?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_lock_options() -> AuthLockOptions {
        AuthLockOptions {
            timeout: Duration::from_millis(80),
            retry_delay: Duration::from_millis(5),
        }
    }

    fn long_test_lock_options() -> AuthLockOptions {
        AuthLockOptions {
            timeout: Duration::from_secs(2),
            retry_delay: Duration::from_millis(5),
        }
    }

    fn api_key(value: &str) -> Credential {
        Credential::ApiKey {
            key: Some(value.to_owned()),
            env: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn concurrent_stores_preserve_different_providers() {
        let directory = tempfile::tempdir().expect("temporary auth directory");
        let path = directory.path().join("auth.json");
        let mut first = AuthStorage::new(path.clone());
        first.lock_options = long_test_lock_options();
        let mut second = AuthStorage::new(path.clone());
        second.lock_options = long_test_lock_options();
        let (first_ready, first_entered) = tokio::sync::oneshot::channel();
        let (release_first, wait_for_release) = tokio::sync::oneshot::channel();

        let first_store = tokio::spawn(async move {
            first
                .modify("provider-a", move |_| async move {
                    first_ready.send(()).expect("signal first updater");
                    wait_for_release.await.expect("release first updater");
                    Ok(Some(api_key("secret-a")))
                })
                .await
        });
        first_entered.await.expect("first updater entered");
        assert!(auth_lock_path(&path).exists(), "first updater must own the lock");

        let second_store = tokio::spawn(async move {
            second
                .modify("provider-b", |_| async { Ok(Some(api_key("secret-b"))) })
                .await
        });
        while !auth_lock_path(&path).exists() {
            tokio::task::yield_now().await;
        }
        assert!(
            !second_store.is_finished(),
            "second store must wait for the first process lock"
        );
        release_first.send(()).expect("release first updater");

        first_store.await.expect("first store task").expect("first store");
        second_store.await.expect("second store task").expect("second store");

        let stored = load_credentials(&path).expect("load merged credentials");
        assert_eq!(stored.len(), 2);
        assert!(stored.contains_key("provider-a"));
        assert!(stored.contains_key("provider-b"));
        assert!(
            !auth_lock_path(&path).exists(),
            "RAII must clean up the lock file"
        );
    }

    #[tokio::test]
    async fn live_lock_owner_times_out_without_exposing_owner_data() {
        let directory = tempfile::tempdir().expect("temporary auth directory");
        let path = directory.path().join("auth.json");
        let lock_path = auth_lock_path(&path);
        let owner = AuthLockOwner::current();
        create_auth_lock_file(&lock_path, &owner).expect("create live lock");

        let error = AuthFileLock::acquire(&path, test_lock_options())
            .await
            .err()
            .expect("live owner must force a bounded timeout");
        let message = format!("{error:#}");
        assert!(message.contains("timed out acquiring auth lock"), "{message}");
        assert!(!message.contains(&owner.token), "owner token leaked: {message}");
        assert!(lock_path.exists(), "contender must not delete a live lock");
    }

    #[tokio::test]
    async fn stale_same_host_lock_is_recovered() {
        let directory = tempfile::tempdir().expect("temporary auth directory");
        let path = directory.path().join("auth.json");
        let lock_path = auth_lock_path(&path);
        let mut stale = AuthLockOwner::current();
        stale.pid = u32::MAX;
        stale.process_start = Some("not-a-live-process".to_owned());
        stale.token = "stale-owner-token".to_owned();
        assert!(stale.host_fingerprint.is_some(), "test requires a local host identity");
        create_auth_lock_file(&lock_path, &stale).expect("create stale lock");

        let lock = AuthFileLock::acquire(&path, test_lock_options())
            .await
            .expect("recover stale same-host lock");
        let replacement: AuthLockOwner = serde_json::from_slice(
            &fs::read(&lock_path).expect("read replacement lock owner"),
        )
        .expect("parse replacement lock owner");
        assert_ne!(replacement.token, stale.token);
        drop(lock);
        assert!(!lock_path.exists(), "recovered lock must clean up on drop");
    }

    #[tokio::test]
    async fn stale_previous_boot_lock_is_recovered() {
        let directory = tempfile::tempdir().expect("temporary auth directory");
        let path = directory.path().join("auth.json");
        let lock_path = auth_lock_path(&path);
        let mut stale = AuthLockOwner::current();

        assert!(stale.host_fingerprint.is_some(), "test requires a local host identity");
        assert!(stale.boot_id.is_some(), "test requires a boot identity");
        stale.boot_id = Some("previous-boot".to_owned());
        stale.token = "previous-boot-token".to_owned();
        create_auth_lock_file(&lock_path, &stale).expect("create previous-boot lock");

        let lock = AuthFileLock::acquire(&path, test_lock_options())
            .await
            .expect("recover previous-boot same-host lock");
        drop(lock);
        assert!(!lock_path.exists(), "recovered lock must clean up on drop");
    }
    #[cfg(unix)]
    #[test]
    fn stale_claim_identity_distinguishes_replacement_live_lock() {
        let directory = tempfile::tempdir().expect("temporary auth directory");
        let path = directory.path().join("auth.json");
        let lock_path = auth_lock_path(&path);
        let mut stale = AuthLockOwner::current();
        stale.pid = u32::MAX;
        stale.process_start = Some("not-a-live-process".to_owned());
        stale.token = "stale-owner-token".to_owned();
        assert!(stale.host_fingerprint.is_some(), "test requires a local host identity");
        create_auth_lock_file(&lock_path, &stale).expect("create stale lock");

        let claim = stale_claim_path(&lock_path);
        fs::hard_link(&lock_path, &claim).expect("claim stale lock inode");
        fs::remove_file(&lock_path).expect("remove stale path");
        let live = AuthLockOwner::current();
        create_auth_lock_file(&lock_path, &live).expect("install replacement live lock");

        let path_identity = file_identity(&lock_path).expect("read live identity");
        let claim_identity = file_identity(&claim).expect("read claimed identity");
        assert_ne!(path_identity, claim_identity);
        assert!(lock_path.exists(), "identity mismatch must preserve live lock");
        let LockOwnerRead::Owner(remaining) =
            read_auth_lock_owner(&lock_path).expect("read replacement owner")
        else {
            panic!("replacement owner must remain valid");
        };
        assert_eq!(remaining.token, live.token);
        fs::remove_file(&claim).expect("remove stale claim");
    }

    #[tokio::test]
    async fn concurrent_delete_preserves_other_provider_store() {
        let directory = tempfile::tempdir().expect("temporary auth directory");
        let path = directory.path().join("auth.json");
        write_credentials_atomic(
            &path,
            &BTreeMap::from([("remove-me".to_owned(), api_key("old-secret"))]),
        )
        .expect("write initial credential");
        let mut updater = AuthStorage::new(path.clone());
        updater.lock_options = long_test_lock_options();
        let mut deleter = AuthStorage::new(path.clone());
        deleter.lock_options = long_test_lock_options();
        let (updater_ready, wait_for_updater) = tokio::sync::oneshot::channel();
        let (release_updater, wait_for_release) = tokio::sync::oneshot::channel();

        let store_task = tokio::spawn(async move {
            updater
                .modify("keep-me", move |_| async move {
                    updater_ready.send(()).expect("signal updater");
                    wait_for_release.await.expect("release updater");
                    Ok(Some(api_key("new-secret")))
                })
                .await
        });
        wait_for_updater.await.expect("updater entered");
        assert!(auth_lock_path(&path).exists(), "updater must own the file lock");
        let delete_task = tokio::spawn(async move { deleter.delete("remove-me").await });
        tokio::task::yield_now().await;
        assert!(!delete_task.is_finished(), "delete must wait for the file lock");
        release_updater.send(()).expect("release updater");

        store_task.await.expect("store task").expect("store credential");
        assert!(delete_task.await.expect("delete task").expect("delete credential"));
        let stored = load_credentials(&path).expect("load credentials after delete");
        assert!(stored.contains_key("keep-me"));
        assert!(!stored.contains_key("remove-me"));
    }


    #[test]
    fn one_off_reader_returns_raw_credential_and_swallows_invalid_storage() {
        let directory = tempfile::tempdir().expect("temporary auth directory");
        let path = directory.path().join("auth.json");
        let credentials = BTreeMap::from([(
            "provider".to_owned(),
            Credential::ApiKey {
                key: Some("$TOKEN".to_owned()),
                env: HashMap::from([("TOKEN".to_owned(), "stored-secret".to_owned())]),
            },
        )]);
        write_credentials_atomic(&path, &credentials).expect("write credential file");

        match read_stored_credential("provider", &path).expect("read stored credential") {
            Credential::ApiKey { key, env } => {
                assert_eq!(key.as_deref(), Some("$TOKEN"));

                assert_eq!(env.get("TOKEN").map(String::as_str), Some("stored-secret"));
            }
            Credential::OAuth { .. } => panic!("expected API-key credential"),
        }
        assert!(read_stored_credential("missing", &path).is_none());
        fs::write(&path, "not valid json").expect("corrupt credential file");
        assert!(read_stored_credential("provider", &path).is_none());
    }
    #[test]
    fn auth_file_size_limit_accepts_exact_boundary_and_rejects_oversize() {
        let directory = tempfile::tempdir().expect("temporary auth directory");
        let path = directory.path().join("auth.json");

        let exact_padding = usize::try_from(MAX_AUTH_FILE_BYTES).expect("test size fits usize") - 2;
        let mut exact = Vec::with_capacity(exact_padding + 2);
        exact.push(b'{');
        exact.extend(std::iter::repeat_n(b' ', exact_padding));
        exact.push(b'}');
        fs::write(&path, &exact).expect("write exact-boundary auth file");
        assert!(load_credentials(&path).expect("load exact-boundary auth file").is_empty());

        let oversized = vec![b' '; usize::try_from(MAX_AUTH_FILE_BYTES + 1).expect("test size")];
        fs::write(&path, oversized).expect("write oversized auth file");
        let error = load_credentials(&path).expect_err("oversized auth file must fail");
        let message = format!("{error:#}");
        assert!(message.contains("exceeds"), "{message}");
        assert!(message.contains("auth.json"), "{message}");
    }

}
