use std::collections::{BTreeMap, HashMap};
use std::future::Future;
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

use pi_sandbox::AbsolutePath;

use crate::fs::{
    CapabilityAccess, CapabilityFs, CapabilityRootLease, CapabilityRootPurpose, FileIdentity,
    FileMode, FsError, Replacement, RootCreation,
};
use crate::oauth;

const MAX_AVAILABLE_MODEL_IDS: usize = 1_024;
const MAX_AVAILABLE_MODEL_ID_BYTES: usize = 512;
const MAX_AVAILABLE_MODEL_IDS_BYTES: usize = 64 * 1_024;
const AUTH_LOCK_TIMEOUT: Duration = Duration::from_secs(45);
const AUTH_LOCK_RETRY_DELAY: Duration = Duration::from_millis(50);
const MAX_AUTH_FILE_BYTES: u64 = 1024 * 1024;
const MAX_AUTH_LOCK_BYTES: u64 = 16 * 1024;
#[cfg(target_os = "linux")]
const MAX_PROC_IDENTITY_BYTES: u64 = 4 * 1024;
/// Environment variable that selects the active credential scope. It wins
/// over the `authScope` settings key when both are set.
pub const AUTH_SCOPE_ENV: &str = "PI_AUTH_SCOPE";
/// Reserved top-level auth.json key holding the `scope -> provider -> credential`
/// section. A provider literally named `scopes` is disambiguated on load by
/// shape: a single credential object (has a `type` field) is a legacy flat
/// entry, anything else is the scoped section.
const AUTH_SCOPES_KEY: &str = "scopes";
const MAX_SCOPE_LABEL_BYTES: usize = 128;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Credential {
    ApiKey {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        env: HashMap<String, String>,
        /// Unknown fields preserved verbatim so rewriting auth.json never
        /// drops metadata written by newer tool versions.
        #[serde(default, flatten)]
        extra: Map<String, Value>,
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
    /// Scope label the credential is stored under; `None` is the default
    /// (unscoped) credential. Never carries secret material.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// One stored credential slot: the credential itself plus its optional scope
/// label. A `None` scope is the default (unscoped) credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopedCredential {
    pub credential: Credential,
    pub scope: Option<String>,
}

impl ScopedCredential {
    #[must_use]
    pub fn unscoped(credential: Credential) -> Self {
        Self {
            credential,
            scope: None,
        }
    }

    #[must_use]
    pub fn scoped(credential: Credential, scope: impl Into<String>) -> Self {
        Self {
            credential,
            scope: Some(scope.into()),
        }
    }
}

/// Validate and normalize a scope label supplied by the user. Labels are
/// trimmed, single-line, and limited in length so they stay safe as JSON keys
/// and in interactive prompts.
pub fn validate_scope_label(label: &str) -> Result<String> {
    let label = label.trim();
    if label.is_empty() {
        bail!("credential scope must not be empty");
    }
    if label.len() > MAX_SCOPE_LABEL_BYTES {
        bail!("credential scope exceeds {MAX_SCOPE_LABEL_BYTES} bytes: {label:?}");
    }
    if label
        .chars()
        .any(|character| character.is_control() || matches!(character, '/' | '\\'))
    {
        bail!("credential scope must be a single-line label without slashes");
    }
    Ok(label.to_owned())
}

/// Resolve which stored credential entry applies to `provider` under an
/// active scope preference.
///
/// - With an active scope: the scope-matched entry wins; the unscoped entry
///   is the fallback default.
/// - Without an active scope: the unscoped entry is the default.
/// - When entries exist but none can be selected, the error names the
///   available scopes and how to activate one. Errors never include secret
///   values.
pub fn select_scoped_credential<'a>(
    provider: &str,
    entries: &'a [ScopedCredential],
    active_scope: Option<&str>,
) -> Result<Option<&'a ScopedCredential>> {
    select_scoped_credential_by(
        provider,
        entries,
        |entry| entry.scope.as_deref(),
        active_scope,
    )
}

/// Shared credential-selection core behind [`select_scoped_credential`] and
/// pi-cli's `models_config` stored-credential lookup. Resolves which stored
/// credential entry applies to `provider` under an active scope preference,
/// reading each entry's optional scope label through `scope_of` (`None` =
/// unscoped default entry).
///
/// - With an active scope: the scope-matched entry wins; the unscoped entry
///   is the fallback default.
/// - Without an active scope: the unscoped entry is the default.
/// - When entries exist but none can be selected, the error names the
///   available scopes and how to activate one. Errors never include secret
///   values.
pub fn select_scoped_credential_by<'a, T, F>(
    provider: &str,
    entries: &'a [T],
    scope_of: F,
    active_scope: Option<&str>,
) -> Result<Option<&'a T>>
where
    F: Fn(&T) -> Option<&str>,
{
    let Some(active_scope) = active_scope.filter(|scope| !scope.trim().is_empty()) else {
        if let Some(entry) = entries.iter().find(|entry| scope_of(entry).is_none()) {
            return Ok(Some(entry));
        }
        if entries.is_empty() {
            return Ok(None);
        }
        let scopes = list_scopes(entries, &scope_of);
        bail!(
            "provider {provider:?} has credentials only in scope(s) {scopes}; set {AUTH_SCOPE_ENV} or the authScope setting to select one"
        );
    };
    if let Some(entry) = entries
        .iter()
        .find(|entry| scope_of(entry) == Some(active_scope))
    {
        return Ok(Some(entry));
    }
    if let Some(entry) = entries.iter().find(|entry| scope_of(entry).is_none()) {
        return Ok(Some(entry));
    }
    if entries.is_empty() {
        return Ok(None);
    }
    let scopes = list_scopes(entries, &scope_of);
    bail!(
        "provider {provider:?} has credentials only in scope(s) {scopes}; none match active scope {active_scope:?} (set {AUTH_SCOPE_ENV} or the authScope setting)"
    );
}

fn list_scopes<T, F>(entries: &[T], scope_of: &F) -> String
where
    F: Fn(&T) -> Option<&str>,
{
    let mut scopes = entries
        .iter()
        .filter_map(|entry| scope_of(entry))
        .collect::<Vec<_>>();
    scopes.sort_unstable();
    scopes
        .into_iter()
        .map(|scope| format!("{scope:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Active credential scope preference: the `PI_AUTH_SCOPE` environment
/// variable wins, then the `authScope` settings key. Settings lookup is
/// best-effort: unreadable or invalid settings fall back to no active scope.
#[must_use]
pub fn active_auth_scope() -> Option<String> {
    resolve_scope_preference(std::env::var(AUTH_SCOPE_ENV).ok(), settings_auth_scope())
}

/// Precedence for the active credential scope: a non-empty `PI_AUTH_SCOPE`
/// environment value wins, otherwise the `authScope` settings value applies.
/// Values are trimmed; empty or whitespace-only values behave as unset.
#[must_use]
pub fn resolve_scope_preference(
    env_scope: Option<String>,
    settings_scope: Option<String>,
) -> Option<String> {
    env_scope
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            settings_scope
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        })
}

fn settings_auth_scope() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let manager =
        crate::settings::SettingsManager::load_phase_one(cwd, crate::resources::agent_dir_path())
            .ok()?;
    manager.global_settings().auth_scope
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
    ApiKey {
        name: String,
    },
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RequestAuthKind {
    #[default]
    ApiKeyOrHeaders,
    StoredOAuth,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct RequestAuth {
    pub api_key: String,
    pub headers: HashMap<String, String>,
    pub env: HashMap<String, String>,
    pub available_model_ids: Option<Vec<String>>,
    pub kind: RequestAuthKind,
}

impl RequestAuth {
    #[must_use]
    pub const fn kind(&self) -> RequestAuthKind {
        self.kind
    }

    #[must_use]
    pub const fn uses_stored_oauth(&self) -> bool {
        matches!(self.kind, RequestAuthKind::StoredOAuth)
    }
}

impl std::fmt::Debug for RequestAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RequestAuth")
            .field("has_api_key", &!self.api_key.is_empty())
            .field("header_count", &self.headers.len())
            .field("env_count", &self.env.len())
            .field(
                "available_model_count",
                &self.available_model_ids.as_ref().map(Vec::len),
            )
            .field("kind", &self.kind)
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

#[derive(Clone, Serialize, Deserialize)]
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
    parent: CapabilityRootLease,
    relative: PathBuf,
    token: String,
}

impl AuthFileLock {
    async fn acquire(storage: &AuthStorage, options: AuthLockOptions) -> Result<Self> {
        let relative = auth_lock_relative(&storage.file_name);
        let owner = AuthLockOwner::current();
        let bytes = serialize_lock_owner(&owner)?;
        let started = Instant::now();
        loop {
            match storage.parent.create_new(&relative, FileMode::Private) {
                Ok(mut file) => {
                    file.write_all(&bytes).context("writing auth lock owner")?;
                    file.finish().context("syncing auth lock owner")?;
                    return Ok(Self {
                        parent: storage.parent.clone(),
                        relative,
                        token: owner.token,
                    });
                }
                Err(error) if fs_error_kind(&error) == Some(std::io::ErrorKind::AlreadyExists) => {
                    if remove_stale_auth_lock(&storage.parent, &relative, &owner)? {
                        continue;
                    }
                }
                Err(error) => return Err(error).context("creating auth lock file"),
            }

            let elapsed = started.elapsed();
            if elapsed >= options.timeout {
                bail!(
                    "timed out acquiring auth lock for {}",
                    storage.path.display()
                );
            }
            let remaining = options.timeout - elapsed;
            tokio::time::sleep(options.retry_delay.min(remaining)).await;
            if started.elapsed() >= options.timeout {
                bail!(
                    "timed out acquiring auth lock for {}",
                    storage.path.display()
                );
            }
        }
    }
}

impl Drop for AuthFileLock {
    fn drop(&mut self) {
        let Ok(content) = self
            .parent
            .read_bounded(&self.relative, MAX_AUTH_LOCK_BYTES)
        else {
            return;
        };
        let Ok(owner) = serde_json::from_slice::<AuthLockOwner>(&content) else {
            return;
        };
        if owner.token == self.token {
            let _ = self.parent.remove_file(&self.relative);
        }
    }
}

fn serialize_lock_owner(owner: &AuthLockOwner) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(owner).context("serializing auth lock owner")?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn remove_stale_auth_lock(
    parent: &CapabilityRootLease,
    relative: &Path,
    current: &AuthLockOwner,
) -> Result<bool> {
    let owner = match read_auth_lock_owner(parent, relative)? {
        LockOwnerRead::Missing => return Ok(true),
        LockOwnerRead::Invalid => return Ok(false),
        LockOwnerRead::Owner(owner) => owner,
    };
    if !owner.is_definitely_stale(current) {
        return Ok(false);
    }

    // Claim the stale lock by creating a hard-link sibling with a unique name.
    // The hard link pins the stale inode so two concurrent recoverers cannot
    // both act on a replaced lock: only a recoverer whose claim still shares
    // the original inode proceeds to the unlink, and a fresh lock created by
    // another process survives the race.
    let claim = loop {
        let candidate = stale_claim_relative(relative);
        match parent.link_existing(relative, &candidate) {
            Ok(()) => break candidate,
            Err(error) if fs_error_kind(&error) == Some(std::io::ErrorKind::NotFound) => {
                // Lock vanished between the staleness read and the claim.
                return Ok(true);
            }
            Err(error) if fs_error_kind(&error) == Some(std::io::ErrorKind::AlreadyExists) => {
                // UUID collision on the claim name; retry with a fresh name.
                continue;
            }
            Err(FsError::IdentityChanged) => {
                // `relative` was replaced under us; the link may target the
                // new inode, so drop the stray claim before retrying
                // acquisition so the live lock is not stolen.
                let _ = parent.remove_file(&candidate);
                return Ok(true);
            }
            Err(error) => return Err(error).context("claiming stale auth lock"),
        }
    };

    let result = (|| -> Result<bool> {
        let claimed_owner = match read_auth_lock_owner(parent, &claim)? {
            LockOwnerRead::Owner(owner) => owner,
            LockOwnerRead::Missing | LockOwnerRead::Invalid => return Ok(false),
        };
        if claimed_owner.token != owner.token || !claimed_owner.is_definitely_stale(current) {
            return Ok(false);
        }
        let path_identity = optional_identity(parent, relative)?;
        let claim_identity = optional_identity(parent, &claim)?;
        let claim_id = match (path_identity, claim_identity) {
            (None, _) => return Ok(true),  // lock already gone
            (_, None) => return Ok(false), // claim lost unexpectedly
            (Some(path), Some(claim_id)) if path != claim_id => return Ok(false), // replaced
            (Some(_), Some(claim_id)) => claim_id,
        };
        // Remove the lock only while it still shares the claimed (stale)
        // inode. `remove_file_expected` re-verifies the identity atomically
        // with the unlink, so a fresh lock created by another process in the
        // remaining race window survives instead of being deleted.
        match parent.remove_file_expected(relative, claim_id) {
            Ok(()) => Ok(true),
            Err(error) if fs_error_kind(&error) == Some(std::io::ErrorKind::NotFound) => Ok(true),
            Err(FsError::IdentityChanged) => Ok(true),
            Err(error) => Err(error).context("removing stale auth lock"),
        }
    })();
    let _ = parent.remove_file(&claim);
    result
}

enum LockOwnerRead {
    Missing,
    Invalid,
    Owner(AuthLockOwner),
}

fn read_auth_lock_owner(parent: &CapabilityRootLease, relative: &Path) -> Result<LockOwnerRead> {
    let content = match parent.read_bounded(relative, MAX_AUTH_LOCK_BYTES) {
        Ok(content) => content,
        Err(error) if fs_error_kind(&error) == Some(std::io::ErrorKind::NotFound) => {
            return Ok(LockOwnerRead::Missing);
        }
        Err(FsError::ReadLimitExceeded { .. } | FsError::UnsupportedEntryType) => {
            return Ok(LockOwnerRead::Invalid);
        }
        Err(error) => return Err(error).context("reading auth lock owner"),
    };
    Ok(match serde_json::from_slice(&content) {
        Ok(owner) => LockOwnerRead::Owner(owner),
        Err(_) => LockOwnerRead::Invalid,
    })
}

fn optional_identity(
    parent: &CapabilityRootLease,
    relative: &Path,
) -> Result<Option<FileIdentity>> {
    match parent.metadata(relative) {
        Ok(metadata) => Ok(Some(metadata.identity)),
        Err(error) if fs_error_kind(&error) == Some(std::io::ErrorKind::NotFound) => Ok(None),
        Err(error) => Err(error).context("reading auth lock identity"),
    }
}

fn auth_lock_relative(auth_file_name: &std::ffi::OsStr) -> PathBuf {
    let mut file_name = auth_file_name.to_os_string();
    file_name.push(".lock");
    PathBuf::from(file_name)
}

fn stale_claim_relative(path: &Path) -> PathBuf {
    let mut file_name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("auth.json.lock"))
        .to_os_string();
    file_name.push(format!(".{}.claim", Uuid::new_v4()));
    PathBuf::from(file_name)
}

fn local_host_fingerprint() -> Option<String> {
    // Prefer the kernel hostname over shell-exported env vars: on Linux the
    // `HOSTNAME` shell variable is not exported into the process environment
    // for non-interactive contexts (systemd units, spawned children), which
    // would otherwise leave the fingerprint unset and silently disable
    // same-host stale-lock detection.
    let identity = hostname_from_system()
        .or_else(|| {
            std::env::var("HOSTNAME")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| {
            std::env::var("COMPUTERNAME")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })?;
    let trimmed = identity.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(format!("{:x}", Sha256::digest(trimmed.as_bytes())))
}

/// Reliable per-host identity source independent of shell-exported environment
/// variables. On Linux the kernel hostname is read directly from
/// `/proc/sys/kernel/hostname` (where `HOSTNAME` is not exported into
/// non-interactive environments); elsewhere `None` is returned so the env-var
/// fallbacks in [`local_host_fingerprint`] apply. The bounded
/// [`read_fixed_system_file`] helper caps the read so a malformed proc entry
/// cannot exhaust memory.
#[cfg(target_os = "linux")]
fn hostname_from_system() -> Option<String> {
    read_fixed_system_file(Path::new("/proc/sys/kernel/hostname"))
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_owned())
        .filter(|identity| !identity.is_empty())
}

#[cfg(not(target_os = "linux"))]
fn hostname_from_system() -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
fn local_boot_id() -> Option<String> {
    read_fixed_system_file(Path::new("/proc/sys/kernel/random/boot_id"))
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_owned())
        .filter(|identity| !identity.is_empty())
}

#[cfg(not(target_os = "linux"))]
fn local_boot_id() -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
fn process_start_identity(pid: u32) -> std::io::Result<Option<String>> {
    let path = PathBuf::from(format!("/proc/{pid}/stat"));
    let bytes = match read_fixed_system_file(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let stat = std::str::from_utf8(&bytes)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid proc stat"))?;
    let after_name = stat
        .rsplit_once(") ")
        .map(|(_, fields)| fields)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid proc stat"))?;
    Ok(after_name
        .split_ascii_whitespace()
        .nth(19)
        .map(str::to_owned))
}

#[cfg(target_os = "linux")]
fn read_fixed_system_file(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;

    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    // `Read::take` bounds the read so a malformed proc entry cannot exhaust
    // memory; reading one byte past the limit lets us detect oversize files
    // instead of silently truncating them.
    file.take(MAX_PROC_IDENTITY_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_PROC_IDENTITY_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "system identity file exceeds bounded read limit",
        ));
    }
    Ok(bytes)
}

#[cfg(target_os = "linux")]
fn process_owner_is_stale(owner: &AuthLockOwner) -> bool {
    match process_start_identity(owner.pid) {
        Ok(None) => true,
        Ok(Some(current_start)) => owner
            .process_start
            .as_ref()
            .is_some_and(|recorded_start| recorded_start != &current_start),
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
    parent: CapabilityRootLease,
    file_name: std::ffi::OsString,
    lock: Arc<Mutex<()>>,
    lock_options: AuthLockOptions,
}

impl AuthStorage {
    pub fn new(path: PathBuf) -> Result<Self> {
        let capabilities = CapabilityFs::new().context("creating auth filesystem capabilities")?;
        let absolute = AbsolutePath::parse(path.clone()).map_err(|_| FsError::InvalidPath)?;
        let (parent, file_name) = capabilities
            .open_exact_parent(
                CapabilityRootPurpose::OperatorAuth,
                absolute,
                CapabilityAccess::PrivateReadWrite,
                RootCreation::CreateDirectory,
            )
            .context("opening private auth storage")?;
        Ok(Self {
            path,
            parent,
            file_name,
            lock: Arc::new(Mutex::new(())),
            lock_options: AuthLockOptions::default(),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the credential stored in exactly one slot: `scope == None` is the
    /// default (unscoped) slot, `Some(label)` that scope's slot. No fallback
    /// resolution is applied.
    pub async fn read(&self, provider: &str, scope: Option<&str>) -> Result<Option<Credential>> {
        let _guard = self.lock.lock().await;
        let _file_guard = AuthFileLock::acquire(self, self.lock_options).await?;
        let store =
            load_scoped_credentials_from(&self.parent, Path::new(&self.file_name), &self.path)?;
        Ok(store
            .get(provider)
            .and_then(|entries| entries.iter().find(|entry| entry.scope.as_deref() == scope))
            .map(|entry| entry.credential.clone()))
    }

    /// Resolve the credential for `provider` under an active scope preference
    /// (scope match wins, unscoped falls back) without touching the file.
    pub async fn resolve_entry(
        &self,
        provider: &str,
        active_scope: Option<&str>,
    ) -> Result<Option<ScopedCredential>> {
        let _guard = self.lock.lock().await;
        let _file_guard = AuthFileLock::acquire(self, self.lock_options).await?;
        let store =
            load_scoped_credentials_from(&self.parent, Path::new(&self.file_name), &self.path)?;
        let entries = store.get(provider).map(Vec::as_slice).unwrap_or_default();
        Ok(select_scoped_credential(provider, entries, active_scope)?.cloned())
    }

    /// All stored slots for one provider (default plus every scope), without
    /// resolution.
    pub async fn entries(&self, provider: &str) -> Result<Vec<ScopedCredential>> {
        let _guard = self.lock.lock().await;
        let _file_guard = AuthFileLock::acquire(self, self.lock_options).await?;
        Ok(
            load_scoped_credentials_from(&self.parent, Path::new(&self.file_name), &self.path)?
                .get(provider)
                .cloned()
                .unwrap_or_default(),
        )
    }

    pub async fn list(&self) -> Result<Vec<CredentialInfo>> {
        let _guard = self.lock.lock().await;
        let _file_guard = AuthFileLock::acquire(self, self.lock_options).await?;
        let mut entries =
            load_scoped_credentials_from(&self.parent, Path::new(&self.file_name), &self.path)?
                .into_iter()
                .flat_map(|(provider_id, entries)| {
                    entries.into_iter().map(move |entry| CredentialInfo {
                        provider_id: provider_id.clone(),
                        credential_type: entry.credential.credential_type(),
                        scope: entry.scope,
                    })
                })
                .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.provider_id
                .cmp(&right.provider_id)
                .then_with(|| left.scope.cmp(&right.scope))
        });
        Ok(entries)
    }

    /// Read-modify-write one exact slot (`scope == None` is the default slot).
    pub async fn modify<F, Fut>(
        &self,
        provider: &str,
        scope: Option<&str>,
        update: F,
    ) -> Result<Option<Credential>>
    where
        F: FnOnce(Option<Credential>) -> Fut + Send,
        Fut: Future<Output = Result<Option<Credential>>> + Send,
    {
        let _guard = self.lock.lock().await;
        let _file_guard = AuthFileLock::acquire(self, self.lock_options).await?;
        let mut store =
            load_scoped_credentials_from(&self.parent, Path::new(&self.file_name), &self.path)?;
        let entries = store.entry(provider.to_owned()).or_default();
        let current = entries
            .iter()
            .find(|entry| entry.scope.as_deref() == scope)
            .map(|entry| entry.credential.clone());
        let Some(next) = update(current.clone()).await? else {
            return Ok(current);
        };
        match entries
            .iter_mut()
            .find(|entry| entry.scope.as_deref() == scope)
        {
            Some(entry) => entry.credential = next.clone(),
            None => entries.push(ScopedCredential {
                credential: next.clone(),
                scope: scope.map(str::to_owned),
            }),
        }
        write_scoped_credentials_to(&self.parent, Path::new(&self.file_name), &store)?;
        Ok(Some(next))
    }

    pub async fn delete(&self, provider: &str, scope: Option<&str>) -> Result<bool> {
        let _guard = self.lock.lock().await;
        let _file_guard = AuthFileLock::acquire(self, self.lock_options).await?;
        let mut store =
            load_scoped_credentials_from(&self.parent, Path::new(&self.file_name), &self.path)?;
        let Some(entries) = store.get_mut(provider) else {
            return Ok(false);
        };
        let Some(index) = entries
            .iter()
            .position(|entry| entry.scope.as_deref() == scope)
        else {
            return Ok(false);
        };
        entries.remove(index);
        if entries.is_empty() {
            store.remove(provider);
        }
        write_scoped_credentials_to(&self.parent, Path::new(&self.file_name), &store)?;
        Ok(true)
    }
}

pub use crate::operator_http::InitialAuthError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CredentialGrantAuthError {
    Lookup,
    Refresh,
}

#[derive(Clone)]
pub struct AuthManager {
    storage: AuthStorage,
    operator: Arc<crate::operator_http::OperatorHttpTransport>,
}

impl AuthManager {
    pub fn new(path: PathBuf) -> Result<Self> {
        Ok(Self {
            storage: AuthStorage::new(path)?,
            operator: crate::operator_http::OperatorHttpTransport::new()?,
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
        scope: Option<&str>,
        interaction: &dyn AuthInteraction,
    ) -> Result<CredentialInfo> {
        let scope = scope
            .filter(|value| !value.trim().is_empty())
            .map(validate_scope_label)
            .transpose()?;
        let provider_id = match provider.filter(|value| !value.trim().is_empty()) {
            Some(provider) => provider.trim().to_owned(),
            None => {
                select_provider(
                    interaction,
                    &self.providers(),
                    "Select provider to configure:",
                )
                .await?
            }
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
                    extra: Map::new(),
                }
            }
            AuthType::OAuth => oauth::login(&provider_id, interaction, &self.operator)
                .await
                .with_context(|| format!("{} login failed", info.name))?,
        };
        self.storage
            .modify(&provider_id, scope.as_deref(), |_| async move {
                Ok(Some(credential))
            })
            .await
            .with_context(|| format!("storing credential for provider {provider_id:?}"))?;
        Ok(CredentialInfo {
            provider_id,
            credential_type: selected_type,
            scope,
        })
    }

    pub async fn logout(
        &self,
        provider: Option<&str>,
        scope: Option<&str>,
        interaction: &dyn AuthInteraction,
    ) -> Result<CredentialInfo> {
        let configured = self
            .storage
            .list()
            .await
            .context("listing stored credentials")?;
        let (provider_id, scope) = match provider.filter(|value| !value.trim().is_empty()) {
            Some(provider) => (
                provider.trim().to_owned(),
                scope
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_owned),
            ),
            None => {
                if configured.is_empty() {
                    bail!("no stored credentials to remove")
                }
                let options = configured
                    .iter()
                    .map(|entry| AuthPromptOption {
                        id: scoped_selection_id(&entry.provider_id, entry.scope.as_deref()),
                        label: provider_info(&entry.provider_id).name,
                        description: Some(match &entry.scope {
                            Some(scope) => {
                                format!("{} (scope {scope})", entry.credential_type.label())
                            }
                            None => entry.credential_type.label().to_owned(),
                        }),
                    })
                    .collect();
                let selected = interaction
                    .prompt(AuthPrompt::Select {
                        message: "Select provider to log out:".to_owned(),
                        options,
                    })
                    .await?;
                let (provider_id, scope) = parse_scoped_selection_id(&selected);
                (provider_id, scope)
            }
        };
        let credential_type = configured
            .iter()
            .find(|entry| {
                entry.provider_id == provider_id && entry.scope == scope
            })
            .map(|entry| entry.credential_type)
            .ok_or_else(|| {
                match &scope {
                    Some(scope) => anyhow!(
                        "provider {provider_id:?} has no stored credential in scope {scope:?}"
                    ),
                    None => {
                        let stored_scopes = configured
                            .iter()
                            .filter(|entry| entry.provider_id == provider_id)
                            .filter_map(|entry| entry.scope.as_deref())
                            .collect::<Vec<_>>();
                        if stored_scopes.is_empty() {
                            anyhow!("provider {provider_id:?} has no stored credential")
                        } else {
                            anyhow!(
                                "provider {provider_id:?} has no unscoped credential; stored scope(s): {}; use --scope <label> to remove one",
                                stored_scopes
                                    .into_iter()
                                    .map(|scope| format!("{scope:?}"))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                        }
                    }
                }
            })?;
        if !self
            .storage
            .delete(&provider_id, scope.as_deref())
            .await
            .with_context(|| format!("removing credential for provider {provider_id:?}"))?
        {
            bail!("provider {provider_id:?} has no stored credential")
        }
        Ok(CredentialInfo {
            provider_id,
            credential_type,
            scope,
        })
    }

    pub async fn resolve_stored(
        &self,
        provider: &str,
        env: Option<&HashMap<String, String>>,
    ) -> Result<Option<RequestAuth>> {
        let active_scope = active_auth_scope();
        self.resolve_stored_with_scope(provider, env, active_scope.as_deref())
            .await
    }

    /// Resolve a stored credential under an explicit scope preference. This is
    /// the testable core of [`Self::resolve_stored`]; callers that want the
    /// ambient `PI_AUTH_SCOPE` / `authScope` selection use [`Self::resolve_stored`].
    pub async fn resolve_stored_with_scope(
        &self,
        provider: &str,
        env: Option<&HashMap<String, String>>,
        active_scope: Option<&str>,
    ) -> Result<Option<RequestAuth>> {
        let Some(selected) = self.storage.resolve_entry(provider, active_scope).await? else {
            return Ok(None);
        };
        let slot = selected.scope.as_deref();
        let operator = self.operator.clone();
        let provider_id = provider.to_owned();
        let credential = match selected.credential {
            Credential::OAuth { expires, .. }
                if chrono::Utc::now().timestamp_millis() >= expires =>
            {
                self.storage
                    .modify(provider, slot, move |current| {
                        let provider_id = provider_id.clone();
                        let operator = operator.clone();
                        async move {
                            let Some(Credential::OAuth { expires, .. }) = current.as_ref() else {
                                return Ok(None);
                            };
                            if chrono::Utc::now().timestamp_millis() < *expires {
                                return Ok(None);
                            }
                            let current = current.expect("OAuth credential is present");
                            let refreshed = oauth::refresh_initial_with_operator(
                                &provider_id,
                                &current,
                                &operator,
                            )
                            .await
                            .with_context(|| {
                                format!("refreshing OAuth credential for provider {provider_id:?}")
                            })?;
                            Ok(Some(refreshed))
                        }
                    })
                    .await?
            }
            credential => Some(credential),
        };
        match credential {
            Some(Credential::ApiKey {
                key,
                env: credential_env,
                ..
            }) => {
                let key = match key {
                    Some(key) => resolve_stored_value(&key, env, &credential_env)?,
                    None => String::new(),
                };
                Ok((!key.trim().is_empty()).then_some(RequestAuth {
                    api_key: key,
                    env: credential_env,
                    headers: HashMap::new(),
                    available_model_ids: None,
                    kind: RequestAuthKind::ApiKeyOrHeaders,
                }))
            }
            Some(credential @ Credential::OAuth { .. }) => {
                oauth::to_request_auth(provider, &credential).map(Some)
            }
            None => Ok(None),
        }
    }

    pub(crate) async fn resolve_stored_credential_exact(
        &self,
        provider: &str,
        scope: Option<&str>,
        oauth_http: pi_ai::SharedHttpExecutor,
    ) -> std::result::Result<Option<Credential>, CredentialGrantAuthError> {
        let selected = self
            .storage
            .read(provider, scope)
            .await
            .map_err(|_| CredentialGrantAuthError::Lookup)?;
        let Some(credential) = selected else {
            return Ok(None);
        };
        let Credential::OAuth { expires, .. } = &credential else {
            return Ok(Some(credential));
        };
        if chrono::Utc::now().timestamp_millis() < *expires {
            return Ok(Some(credential));
        }
        let provider_id = provider.to_owned();
        self.storage
            .modify(provider, scope, move |current| {
                let provider_id = provider_id.clone();
                let oauth_http = oauth_http.clone();
                async move {
                    let Some(current @ Credential::OAuth { expires, .. }) = current else {
                        return Ok(current);
                    };
                    if chrono::Utc::now().timestamp_millis() < expires {
                        return Ok(Some(current));
                    }
                    crate::oauth::refresh_with_executor(&provider_id, &current, oauth_http)
                        .await
                        .map(Some)
                }
            })
            .await
            .map_err(|_| CredentialGrantAuthError::Refresh)
    }
}

/// Encode a provider + optional scope into an interactive selection id.
/// Scope labels are validated to exclude the separator, so the encoding is
/// unambiguous.
const SCOPED_SELECTION_SEPARATOR: char = '\u{1f}';

fn scoped_selection_id(provider: &str, scope: Option<&str>) -> String {
    match scope {
        Some(scope) => format!("{provider}{SCOPED_SELECTION_SEPARATOR}{scope}"),
        None => provider.to_owned(),
    }
}

fn parse_scoped_selection_id(id: &str) -> (String, Option<String>) {
    match id.split_once(SCOPED_SELECTION_SEPARATOR) {
        Some((provider, scope)) => (provider.to_owned(), Some(scope.to_owned())),
        None => (id.to_owned(), None),
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
        "anthropic" => ("Anthropic", Some("Anthropic (Claude Pro/Max)"), None, true),
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
    let (parent, file_name) = operator_auth_parent(path)?;
    let content = read_optional_auth_bytes(&parent, Path::new(&file_name), path)?;
    let Some(content) = content else {
        return Ok(BTreeMap::new());
    };
    parse_credentials(&content, path)
}

fn parse_credentials(content: &[u8], path: &Path) -> Result<BTreeMap<String, Credential>> {
    if content.iter().all(u8::is_ascii_whitespace) {
        return Ok(BTreeMap::new());
    }
    let root: Value = serde_json::from_slice(content)
        .with_context(|| format!("parsing auth.json at {}", path.display()))?;
    let entries = root.as_object().ok_or_else(|| {
        anyhow!(
            "invalid auth.json at {}: expected an object",
            path.display()
        )
    })?;
    let mut credentials = BTreeMap::new();
    for (provider, value) in entries {
        if provider == AUTH_SCOPES_KEY && value.get("type").is_none() {
            continue;
        }
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

/// Load every stored credential slot: the flat map holds the default
/// (unscoped) entries, and the optional reserved `scopes` section holds
/// `scope label -> provider -> credential`. Old files without a `scopes`
/// section load as unscoped entries only. Invalid scope labels are rejected
/// with an actionable error.
pub fn load_scoped_credentials(path: &Path) -> Result<BTreeMap<String, Vec<ScopedCredential>>> {
    let (parent, file_name) = operator_auth_parent(path)?;
    load_scoped_credentials_from(&parent, Path::new(&file_name), path)
}

fn load_scoped_credentials_from(
    parent: &CapabilityRootLease,
    relative: &Path,
    display_path: &Path,
) -> Result<BTreeMap<String, Vec<ScopedCredential>>> {
    let content = read_optional_auth_bytes(parent, relative, display_path)?;
    let Some(content) = content else {
        return Ok(BTreeMap::new());
    };
    if content.iter().all(u8::is_ascii_whitespace) {
        return Ok(BTreeMap::new());
    }
    let root: Value = serde_json::from_slice(&content)
        .with_context(|| format!("parsing auth.json at {}", display_path.display()))?;
    let entries = root.as_object().ok_or_else(|| {
        anyhow!(
            "invalid auth.json at {}: expected an object",
            display_path.display()
        )
    })?;
    let mut store: BTreeMap<String, Vec<ScopedCredential>> = BTreeMap::new();
    for (provider, value) in entries {
        if provider == AUTH_SCOPES_KEY {
            // A single credential object under `scopes` (it carries a `type`
            // tag) is a legacy flat entry for a provider literally named
            // "scopes"; anything else is the scoped section.
            if value.get("type").is_some() {
                let credential = serde_json::from_value(value.clone()).map_err(|error| {
                    anyhow!(
                        "invalid auth.json credential for provider {provider:?} at {}: {error}",
                        display_path.display()
                    )
                })?;
                store
                    .entry(provider.clone())
                    .or_default()
                    .push(ScopedCredential::unscoped(credential));
            } else {
                parse_scopes_section(&mut store, value, display_path)?;
            }
            continue;
        }
        let credential = serde_json::from_value(value.clone()).map_err(|error| {
            anyhow!(
                "invalid auth.json credential for provider {provider:?} at {}: {error}",
                display_path.display()
            )
        })?;
        store
            .entry(provider.clone())
            .or_default()
            .push(ScopedCredential::unscoped(credential));
    }
    for entries in store.values_mut() {
        sort_credential_entries(entries);
    }
    Ok(store)
}

/// Deterministic per-provider slot order: the default (unscoped) entry first,
/// then scoped entries by label.
fn sort_credential_entries(entries: &mut [ScopedCredential]) {
    entries.sort_by(|left, right| left.scope.cmp(&right.scope));
}

fn parse_scopes_section(
    store: &mut BTreeMap<String, Vec<ScopedCredential>>,
    value: &Value,
    path: &Path,
) -> Result<()> {
    let scopes = value.as_object().ok_or_else(|| {
        anyhow!(
            "invalid auth.json {AUTH_SCOPES_KEY} section at {}: expected an object of scope labels",
            path.display()
        )
    })?;
    for (scope, providers) in scopes {
        let scope = validate_scope_label(scope)
            .with_context(|| format!("invalid auth.json scope at {}", path.display()))?;
        let providers = providers.as_object().ok_or_else(|| {
            anyhow!(
                "invalid auth.json credential for provider in scope {scope:?} at {}: expected an object",
                path.display()
            )
        })?;
        for (provider, credential_value) in providers {
            let credential = serde_json::from_value(credential_value.clone()).map_err(|error| {
                anyhow!(
                    "invalid auth.json credential for provider {provider:?} in scope {scope:?} at {}: {error}",
                    path.display()
                )
            })?;
            store
                .entry(provider.clone())
                .or_default()
                .push(ScopedCredential::scoped(credential, scope.clone()));
        }
    }
    Ok(())
}

/// Serialize a scoped credential store back to auth.json: default entries in
/// the flat map, scoped entries under the reserved `scopes` section. Old flat
/// files round-trip byte-for-byte at the JSON level (plus pretty-printing).
pub fn write_scoped_credentials_atomic(
    path: &Path,
    store: &BTreeMap<String, Vec<ScopedCredential>>,
) -> Result<()> {
    let (parent, file_name) = operator_auth_parent(path)?;
    write_scoped_credentials_to(&parent, Path::new(&file_name), store)
}

fn write_scoped_credentials_to(
    parent: &CapabilityRootLease,
    relative: &Path,
    store: &BTreeMap<String, Vec<ScopedCredential>>,
) -> Result<()> {
    let has_unscoped_scopes_provider = store
        .get(AUTH_SCOPES_KEY)
        .is_some_and(|entries| entries.iter().any(|entry| entry.scope.is_none()));
    let has_scoped_entries = store.values().flatten().any(|entry| entry.scope.is_some());
    if has_unscoped_scopes_provider && has_scoped_entries {
        bail!(
            "cannot write auth.json: a provider named {AUTH_SCOPES_KEY:?} cannot coexist with scoped credentials (the {AUTH_SCOPES_KEY} key is reserved for the scoped section)"
        );
    }
    let mut root = Map::new();
    for (provider, entries) in store {
        let mut entries = entries.clone();
        sort_credential_entries(&mut entries);
        for entry in entries {
            let value = serde_json::to_value(&entry.credential)
                .with_context(|| format!("serializing credential for provider {provider:?}"))?;
            match &entry.scope {
                None => {
                    root.insert(provider.clone(), value);
                }
                Some(scope) => {
                    let scopes = root
                        .entry(AUTH_SCOPES_KEY.to_owned())
                        .or_insert_with(|| Value::Object(Map::new()));
                    let scope_map = scopes
                        .as_object_mut()
                        .expect("scopes section is always an object");
                    let providers = scope_map
                        .entry(scope.clone())
                        .or_insert_with(|| Value::Object(Map::new()));
                    providers
                        .as_object_mut()
                        .expect("scope entry is always an object")
                        .insert(provider.clone(), value);
                }
            }
        }
    }
    write_auth_json_to(parent, relative, &Value::Object(root))
}

/// Read one stored credential without constructing an [`AuthStorage`] or
/// resolving configured API-key values.
///
/// Missing, unreadable, or invalid files are treated as having no credential.
/// The helper never includes stored values in an error because it does not
/// surface storage errors. Returns the default (unscoped) credential.
#[must_use]
pub fn read_stored_credential(provider: &str, path: impl AsRef<Path>) -> Option<Credential> {
    load_credentials(path.as_ref()).ok()?.remove(provider)
}

pub fn write_credentials_atomic(
    path: &Path,
    credentials: &BTreeMap<String, Credential>,
) -> Result<()> {
    let root = serde_json::to_value(credentials).context("serializing auth.json")?;
    write_auth_json(path, &root)
}

fn write_auth_json(path: &Path, root: &Value) -> Result<()> {
    let (parent, file_name) = operator_auth_parent(path)?;
    write_auth_json_to(&parent, Path::new(&file_name), root)
}

fn write_auth_json_to(parent: &CapabilityRootLease, relative: &Path, root: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(root).context("serializing auth.json")?;
    bytes.push(b'\n');
    replace_private_auth_bytes(parent, relative, bytes)
}

fn replace_private_auth_bytes(
    parent: &CapabilityRootLease,
    relative: &Path,
    bytes: Vec<u8>,
) -> Result<()> {
    parent
        .replace_file_allowing_live_leaf_link(Replacement {
            relative: relative.to_path_buf(),
            expected: None,
            bytes,
            mode: FileMode::Private,
        })
        .context("atomically replacing private auth file")?;
    Ok(())
}

fn read_optional_auth_bytes(
    parent: &CapabilityRootLease,
    relative: &Path,
    display_path: &Path,
) -> Result<Option<Vec<u8>>> {
    match parent.read_bounded(relative, MAX_AUTH_FILE_BYTES) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if fs_error_kind(&error) == Some(std::io::ErrorKind::NotFound) => Ok(None),
        Err(FsError::ReadLimitExceeded { .. }) => bail!(
            "auth.json at {} exceeds {MAX_AUTH_FILE_BYTES} bytes",
            display_path.display()
        ),
        Err(error)
            if crate::fs::is_link_blocked(&error)
                || matches!(error, FsError::UnsupportedEntryType) =>
        {
            match parent.open_managed_file_within_root(relative) {
                Ok(file) => match file.read_bounded(MAX_AUTH_FILE_BYTES) {
                    Ok(bytes) => Ok(Some(bytes)),
                    Err(FsError::ReadLimitExceeded { .. }) => bail!(
                        "auth.json at {} exceeds {MAX_AUTH_FILE_BYTES} bytes",
                        display_path.display()
                    ),
                    Err(error) => Err(error).with_context(|| {
                        format!("reading private auth.json at {}", display_path.display())
                    }),
                },
                Err(error) => Err(error).with_context(|| {
                    format!("reading private auth.json at {}", display_path.display())
                }),
            }
        }
        Err(error) => Err(error)
            .with_context(|| format!("reading private auth.json at {}", display_path.display())),
    }
}

pub fn read_operator_auth_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let (parent, file_name) = operator_auth_parent(path)?;
    let relative = Path::new(&file_name);
    match parent.read_bounded(relative, max_bytes) {
        Ok(bytes) => Ok(bytes),
        Err(error)
            if crate::fs::is_link_blocked(&error)
                || matches!(error, FsError::UnsupportedEntryType) =>
        {
            parent
                .open_managed_file_within_root(relative)
                .context("opening operator auth file")?
                .read_bounded(max_bytes)
                .context("reading bounded operator auth file")
        }
        Err(error) => Err(error)
            .context("opening operator auth file")
            .context("reading bounded operator auth file"),
    }
}

pub fn replace_private_operator_auth_file(path: &Path, bytes: Vec<u8>) -> Result<()> {
    let (parent, file_name) = operator_auth_parent(path)?;
    replace_private_auth_bytes(&parent, Path::new(&file_name), bytes)
}

fn operator_auth_parent(path: &Path) -> Result<(CapabilityRootLease, std::ffi::OsString)> {
    let capabilities = CapabilityFs::new().context("creating auth filesystem capabilities")?;
    let absolute = AbsolutePath::parse(path.to_path_buf()).map_err(|_| FsError::InvalidPath)?;
    capabilities
        .open_exact_parent(
            CapabilityRootPurpose::OperatorAuth,
            absolute,
            CapabilityAccess::PrivateReadWrite,
            RootCreation::CreateDirectory,
        )
        .context("opening private auth storage")
}

fn fs_error_kind(error: &FsError) -> Option<std::io::ErrorKind> {
    match error {
        FsError::System { kind, .. } => Some(*kind),
        _ => None,
    }
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
            while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '_') {
                end += 1;
            }
            (chars[index + 1..end].iter().collect::<String>(), end)
        } else {
            output.push('$');
            index += 1;
            continue;
        };
        let resolved =
            env_lookup_with_fallback(&name, request_env, credential_env).ok_or_else(|| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
            extra: Map::new(),
        }
    }
    #[cfg(target_os = "linux")]
    fn current_lock_owner() -> AuthLockOwner {
        let owner = AuthLockOwner::current();
        assert!(owner.host_fingerprint.is_some(), "test host fingerprint");
        assert!(owner.boot_id.is_some(), "Linux boot identity");
        assert!(
            owner.process_start.is_some(),
            "Linux process start identity"
        );
        owner
    }

    #[cfg(target_os = "linux")]
    fn write_lock_owner(storage: &AuthStorage, owner: &AuthLockOwner) {
        let relative = auth_lock_relative(&storage.file_name);
        let mut file = storage
            .parent
            .create_new(&relative, FileMode::Private)
            .expect("create auth lock owner file");
        file.write_all(&serialize_lock_owner(owner).expect("serialize lock owner"))
            .expect("write auth lock owner");
        file.finish().expect("finish auth lock owner file");
    }

    #[cfg(target_os = "linux")]
    fn short_test_lock_options() -> AuthLockOptions {
        AuthLockOptions {
            timeout: Duration::from_millis(25),
            retry_delay: Duration::from_millis(2),
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn live_same_boot_auth_lock_remains_exclusive() {
        let directory = tempfile::tempdir().expect("temporary auth directory");
        let path = directory.path().join("auth.json");
        let storage = AuthStorage::new(path).expect("auth storage");
        write_lock_owner(&storage, &current_lock_owner());

        let error = match AuthFileLock::acquire(&storage, short_test_lock_options()).await {
            Ok(_) => panic!("live lock owner must remain protected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("timed out acquiring auth lock"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn crashed_auth_lock_owner_is_recovered() {
        let directory = tempfile::tempdir().expect("temporary auth directory");
        let path = directory.path().join("auth.json");
        let storage = AuthStorage::new(path).expect("auth storage");
        let mut owner = current_lock_owner();
        owner.pid = u32::MAX;
        owner.process_start = Some("1".to_owned());
        write_lock_owner(&storage, &owner);

        let lock = AuthFileLock::acquire(&storage, short_test_lock_options())
            .await
            .expect("crashed owner lock recovery");
        drop(lock);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn reused_pid_auth_lock_owner_is_recovered() {
        let directory = tempfile::tempdir().expect("temporary auth directory");
        let path = directory.path().join("auth.json");
        let storage = AuthStorage::new(path).expect("auth storage");
        let mut owner = current_lock_owner();
        owner.process_start = Some(format!(
            "{}-reused",
            owner.process_start.as_deref().unwrap()
        ));
        write_lock_owner(&storage, &owner);

        let lock = AuthFileLock::acquire(&storage, short_test_lock_options())
            .await
            .expect("reused PID lock recovery");
        drop(lock);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn prior_boot_auth_lock_owner_is_recovered() {
        let directory = tempfile::tempdir().expect("temporary auth directory");
        let path = directory.path().join("auth.json");
        let storage = AuthStorage::new(path).expect("auth storage");
        let mut owner = current_lock_owner();
        owner.boot_id = Some(format!("{}-prior", owner.boot_id.as_deref().unwrap()));
        write_lock_owner(&storage, &owner);

        let lock = AuthFileLock::acquire(&storage, short_test_lock_options())
            .await
            .expect("prior boot lock recovery");
        drop(lock);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn foreign_host_auth_lock_owner_is_never_stolen() {
        // A dead PID on a different host must never be declared stale: the
        // same-host gate fails closed so a lock written elsewhere survives
        // even when its recorded PID is gone on this machine.
        let directory = tempfile::tempdir().expect("temporary auth directory");
        let path = directory.path().join("auth.json");
        let storage = AuthStorage::new(path).expect("auth storage");
        let mut owner = current_lock_owner();
        owner.host_fingerprint = Some("foreign-host-fingerprint".to_owned());
        owner.pid = u32::MAX;
        owner.process_start = Some("1".to_owned());
        write_lock_owner(&storage, &owner);
        let error = match AuthFileLock::acquire(&storage, short_test_lock_options()).await {
            Ok(_) => panic!("foreign-host lock must not be stolen"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("timed out acquiring auth lock"),
            "expected timeout, got {error}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn concurrent_stale_claim_recovery_is_race_safe() {
        // Two recoverers race on the same stale lock. The hard-link claim
        // protocol must guarantee the lock is removed at most once (no live
        // lock is unlinked), both recoverers complete without error, and no
        // stray `*.claim` files leak in the storage directory.
        let directory = tempfile::tempdir().expect("temporary auth directory");
        let path = directory.path().join("auth.json");
        let storage = AuthStorage::new(path).expect("auth storage");
        let mut owner = current_lock_owner();
        owner.pid = u32::MAX;
        owner.process_start = Some("1".to_owned());
        write_lock_owner(&storage, &owner);

        let relative = auth_lock_relative(&storage.file_name);
        let current = AuthLockOwner::current();
        let parent1 = storage.parent.clone();
        let parent2 = storage.parent.clone();
        let rel1 = relative.clone();
        let rel2 = relative.clone();
        let cur1 = current.clone();
        let cur2 = current.clone();

        let handle1 = std::thread::spawn(move || remove_stale_auth_lock(&parent1, &rel1, &cur1));
        let handle2 = std::thread::spawn(move || remove_stale_auth_lock(&parent2, &rel2, &cur2));
        let removed1 = handle1
            .join()
            .expect("recoverer 1 thread")
            .expect("recoverer 1 result");
        let removed2 = handle2
            .join()
            .expect("recoverer 2 thread")
            .expect("recoverer 2 result");

        assert!(
            removed1 || removed2,
            "at least one recoverer must report the stale lock removed"
        );
        match read_auth_lock_owner(&storage.parent, &relative).expect("read owner after race") {
            LockOwnerRead::Missing => {}
            _ => panic!("stale lock must be absent after concurrent recovery"),
        }

        let leaked_claims = fs::read_dir(directory.path())
            .expect("list storage directory")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".claim"))
            .count();
        assert_eq!(
            leaked_claims, 0,
            "no stray claim files may leak after recovery"
        );

        // A fresh acquirer must be able to take the lock now that it is free.
        let lock = AuthFileLock::acquire(&storage, short_test_lock_options())
            .await
            .expect("fresh acquire after recovery");
        drop(lock);
    }

    #[tokio::test]
    async fn concurrent_stores_preserve_different_providers() {
        let directory = tempfile::tempdir().expect("temporary auth directory");
        let path = directory.path().join("auth.json");
        let mut first = AuthStorage::new(path.clone()).expect("first auth storage");
        first.lock_options = long_test_lock_options();
        let mut second = AuthStorage::new(path.clone()).expect("second auth storage");
        second.lock_options = long_test_lock_options();
        let (first_ready, first_entered) = tokio::sync::oneshot::channel();
        let (release_first, wait_for_release) = tokio::sync::oneshot::channel();

        let first_store = tokio::spawn(async move {
            first
                .modify("provider-a", None, move |_| async move {
                    first_ready.send(()).expect("signal first updater");
                    wait_for_release.await.expect("release first updater");
                    Ok(Some(api_key("secret-a")))
                })
                .await
        });
        first_entered.await.expect("first updater entered");

        let second_store = tokio::spawn(async move {
            second
                .modify("provider-b", None, |_| async {
                    Ok(Some(api_key("secret-b")))
                })
                .await
        });
        assert!(
            !second_store.is_finished(),
            "second store must wait for the first process lock"
        );
        release_first.send(()).expect("release first updater");

        first_store
            .await
            .expect("first store task")
            .expect("first store");
        second_store
            .await
            .expect("second store task")
            .expect("second store");

        let stored = load_credentials(&path).expect("load merged credentials");
        assert_eq!(stored.len(), 2);
        assert!(stored.contains_key("provider-a"));
        assert!(stored.contains_key("provider-b"));
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
        let mut updater = AuthStorage::new(path.clone()).expect("updater auth storage");
        updater.lock_options = long_test_lock_options();
        let mut deleter = AuthStorage::new(path.clone()).expect("deleter auth storage");
        deleter.lock_options = long_test_lock_options();
        let (updater_ready, wait_for_updater) = tokio::sync::oneshot::channel();
        let (release_updater, wait_for_release) = tokio::sync::oneshot::channel();

        let store_task = tokio::spawn(async move {
            updater
                .modify("keep-me", None, move |_| async move {
                    updater_ready.send(()).expect("signal updater");
                    wait_for_release.await.expect("release updater");
                    Ok(Some(api_key("new-secret")))
                })
                .await
        });
        wait_for_updater.await.expect("updater entered");
        let delete_task = tokio::spawn(async move { deleter.delete("remove-me", None).await });
        tokio::task::yield_now().await;
        assert!(
            !delete_task.is_finished(),
            "delete must wait for the file lock"
        );
        release_updater.send(()).expect("release updater");

        store_task
            .await
            .expect("store task")
            .expect("store credential");
        assert!(
            delete_task
                .await
                .expect("delete task")
                .expect("delete credential")
        );
        let stored = load_credentials(&path).expect("load credentials after delete");
        assert!(stored.contains_key("keep-me"));
        assert!(!stored.contains_key("remove-me"));
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn auth_storage_modify_preserves_live_leaf_symlink_and_replaces_private_target() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

        let directory = tempfile::tempdir().expect("temporary auth directory");
        let target = directory.path().join("auth-target.json");
        let path = directory.path().join("auth.json");
        fs::write(
            &target,
            serde_json::to_vec(&BTreeMap::from([(
                "existing".to_owned(),
                api_key("kept-secret"),
            )]))
            .expect("serialize initial credentials"),
        )
        .expect("write initial target");
        symlink("auth-target.json", &path).expect("create auth symlink");
        let link_before = fs::symlink_metadata(&path).expect("auth link metadata");
        let link_target = fs::read_link(&path).expect("auth link target");

        let storage = AuthStorage::new(path.clone()).expect("auth storage");
        storage
            .modify("added", None, |_| async { Ok(Some(api_key("new-secret"))) })
            .await
            .expect("modify symlinked auth storage");

        let link_after = fs::symlink_metadata(&path).expect("auth link metadata after write");
        assert!(
            link_after.file_type().is_symlink(),
            "auth path remains a symlink"
        );
        assert_eq!(
            link_after.dev(),
            link_before.dev(),
            "link device remains unchanged"
        );
        assert_eq!(
            link_after.ino(),
            link_before.ino(),
            "link inode remains unchanged"
        );
        assert_eq!(
            fs::read_link(&path).expect("auth link target after write"),
            link_target
        );
        assert_eq!(
            storage
                .read("added", None)
                .await
                .expect("read through preserved auth symlink"),
            Some(api_key("new-secret"))
        );
        let target_bytes = fs::read(&target).expect("read replaced auth target bytes");
        let credentials =
            parse_credentials(&target_bytes, &target).expect("parse auth target bytes");
        assert_eq!(credentials.get("existing"), Some(&api_key("kept-secret")));
        assert_eq!(credentials.get("added"), Some(&api_key("new-secret")));
        let mode = fs::metadata(&target)
            .expect("auth target metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "resolved auth target must be mode 0600"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dangling_auth_leaf_symlink_fails_closed_without_creating_target() {
        use std::os::unix::fs::{MetadataExt, symlink};

        let directory = tempfile::tempdir().expect("temporary auth directory");
        let target = directory.path().join("missing-target.json");
        let path = directory.path().join("auth.json");
        symlink("missing-target.json", &path).expect("create dangling auth symlink");
        let link_before = fs::symlink_metadata(&path).expect("dangling link metadata");
        let error = write_credentials_atomic(
            &path,
            &BTreeMap::from([("provider".to_owned(), api_key("secret"))]),
        )
        .expect_err("dangling auth symlink must fail closed");
        assert!(
            !error.to_string().is_empty(),
            "failure must remain actionable"
        );
        let link_after = fs::symlink_metadata(&path).expect("dangling link after failure");
        assert!(
            link_after.file_type().is_symlink(),
            "dangling link must remain"
        );
        assert_eq!(link_after.dev(), link_before.dev());
        assert_eq!(link_after.ino(), link_before.ino());
        assert_eq!(
            fs::read_link(&path).expect("dangling link target"),
            Path::new("missing-target.json")
        );
        assert!(!target.exists(), "dangling target must not be created");
    }

    #[cfg(unix)]
    #[test]
    fn auth_storage_rejects_symlinked_parent_directory() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temporary auth directory");
        let actual_parent = directory.path().join("actual-parent");
        fs::create_dir(&actual_parent).expect("create actual parent");
        let linked_parent = directory.path().join("linked-parent");
        symlink(&actual_parent, &linked_parent).expect("create linked parent");
        let error = match AuthStorage::new(linked_parent.join("auth.json")) {
            Ok(_) => panic!("symlinked auth parent must remain rejected"),
            Err(error) => error,
        };
        assert!(
            !error.to_string().is_empty(),
            "failure must remain actionable"
        );
        assert!(!actual_parent.join("auth.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn auth_replacement_remains_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary auth directory");
        let path = directory.path().join("auth.json");
        write_credentials_atomic(
            &path,
            &BTreeMap::from([("provider".to_owned(), api_key("secret"))]),
        )
        .expect("write private auth file");
        let mode = fs::metadata(&path)
            .expect("auth metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "auth.json must reject group/other access");
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
                extra: Map::new(),
            },
        )]);
        write_credentials_atomic(&path, &credentials).expect("write credential file");

        match read_stored_credential("provider", &path).expect("read stored credential") {
            Credential::ApiKey { key, env, .. } => {
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
        assert!(
            load_credentials(&path)
                .expect("load exact-boundary auth file")
                .is_empty()
        );

        let oversized = vec![b' '; usize::try_from(MAX_AUTH_FILE_BYTES + 1).expect("test size")];
        fs::write(&path, oversized).expect("write oversized auth file");
        let error = load_credentials(&path).expect_err("oversized auth file must fail");
        let message = format!("{error:#}");
        assert!(message.contains("exceeds"), "{message}");
        assert!(message.contains("auth.json"), "{message}");
    }

    #[test]
    fn flat_file_backward_compat_loads_unscoped_and_preserves_unknown_fields() {
        let directory = tempfile::tempdir().expect("temporary auth directory");
        let path = directory.path().join("auth.json");
        fs::write(
            &path,
            r#"{
  "anthropic": {
    "type": "api_key",
    "key": "legacy-key",
    "futureField": { "kept": true }
  }
}
"#,
        )
        .expect("write legacy flat auth file");

        let flat = load_credentials(&path).expect("load legacy flat file");
        let Credential::ApiKey { key, extra, .. } = flat.get("anthropic").expect("provider") else {
            panic!("expected api_key credential");
        };
        assert_eq!(key.as_deref(), Some("legacy-key"));
        assert_eq!(
            extra.get("futureField").and_then(|value| value.get("kept")),
            Some(&Value::Bool(true)),
            "unknown fields must survive parsing"
        );

        let scoped = load_scoped_credentials(&path).expect("load scoped view");
        let entries = scoped.get("anthropic").expect("provider entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].scope, None, "legacy entries are unscoped");

        // Rewriting through the flat writer must preserve the unknown field.
        write_credentials_atomic(&path, &flat).expect("rewrite flat file");
        let reloaded: Value = serde_json::from_slice(&fs::read(&path).expect("reload"))
            .expect("parse rewritten file");
        assert_eq!(
            reloaded
                .get("anthropic")
                .and_then(|value| value.get("futureField"))
                .and_then(|value| value.get("kept")),
            Some(&Value::Bool(true)),
            "rewrite must preserve unknown fields"
        );
    }

    #[test]
    fn scoped_store_round_trips_file_shape_and_reserved_key_handling() {
        let directory = tempfile::tempdir().expect("temporary auth directory");
        let path = directory.path().join("auth.json");
        let mut store = BTreeMap::new();
        store.insert(
            "anthropic".to_owned(),
            vec![
                ScopedCredential::unscoped(api_key("default-key")),
                ScopedCredential::scoped(api_key("work-key"), "work"),
                ScopedCredential::scoped(api_key("personal-key"), "personal"),
            ],
        );
        write_scoped_credentials_atomic(&path, &store).expect("write scoped store");

        let file: Value =
            serde_json::from_slice(&fs::read(&path).expect("read file")).expect("parse file");
        assert_eq!(
            file.get("anthropic").and_then(|value| value.get("key")),
            Some(&Value::String("default-key".to_owned())),
            "unscoped entry stays in the flat map"
        );
        let scopes = file.get("scopes").expect("scopes section");
        assert_eq!(
            scopes
                .get("work")
                .and_then(|value| value.get("anthropic"))
                .and_then(|value| value.get("key")),
            Some(&Value::String("work-key".to_owned()))
        );
        assert_eq!(
            scopes
                .get("personal")
                .and_then(|value| value.get("anthropic"))
                .and_then(|value| value.get("key")),
            Some(&Value::String("personal-key".to_owned()))
        );

        let reloaded = load_scoped_credentials(&path).expect("reload scoped store");
        let entries = reloaded.get("anthropic").expect("provider entries");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].scope, None, "unscoped slot sorts first");
        assert_eq!(entries[1].scope.as_deref(), Some("personal"));
        assert_eq!(entries[2].scope.as_deref(), Some("work"));

        // The flat view must expose only the default entry.
        let flat = load_credentials(&path).expect("flat view");
        assert_eq!(flat.len(), 1);
        assert!(flat.contains_key("anthropic"));
    }

    #[test]
    fn scoped_store_rejects_invalid_scope_labels_actionably() {
        let directory = tempfile::tempdir().expect("temporary auth directory");
        let path = directory.path().join("auth.json");
        fs::write(
            &path,
            r#"{
  "scopes": {
    "": { "anthropic": { "type": "api_key", "key": "k" } }
  }
}
"#,
        )
        .expect("write invalid scope file");
        let error = load_scoped_credentials(&path).expect_err("empty scope must fail");
        let message = format!("{error:#}");
        assert!(message.contains("scope"), "{message}");
        assert!(message.contains("empty"), "{message}");
    }

    #[test]
    fn provider_named_scopes_is_disambiguated_from_section() {
        let directory = tempfile::tempdir().expect("temporary auth directory");
        let path = directory.path().join("auth.json");
        // Legacy flat entry for a provider literally named "scopes".
        fs::write(
            &path,
            r#"{
  "scopes": { "type": "api_key", "key": "provider-secret" }
}
"#,
        )
        .expect("write provider-named-scopes file");
        let scoped = load_scoped_credentials(&path).expect("load");
        let entries = scoped.get("scopes").expect("provider named scopes");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].scope, None);
        let Credential::ApiKey { key, .. } = &entries[0].credential else {
            panic!("expected api_key credential");
        };
        assert_eq!(key.as_deref(), Some("provider-secret"));
        let flat = load_credentials(&path).expect("flat view");
        assert!(
            flat.contains_key("scopes"),
            "flat view must keep a legacy provider literally named scopes"
        );

        // A real section plus the provider-named entry must both parse.
        fs::write(
            &path,
            r#"{
  "scopes": {
    "work": { "anthropic": { "type": "api_key", "key": "work-key" } }
  }
}
"#,
        )
        .expect("write scopes section");
        let scoped = load_scoped_credentials(&path).expect("load section");
        assert!(scoped.get("scopes").is_none());
        let entries = scoped.get("anthropic").expect("anthropic entries");
        assert_eq!(entries[0].scope.as_deref(), Some("work"));
    }

    #[test]
    fn scope_selection_prefers_match_then_unscoped_and_errors_actionably() {
        let entries = vec![
            ScopedCredential::unscoped(api_key("default-key")),
            ScopedCredential::scoped(api_key("work-key"), "work"),
        ];
        let matched = select_scoped_credential("p", &entries, Some("work"))
            .expect("match")
            .expect("entry");
        assert_eq!(matched.scope.as_deref(), Some("work"));
        let fallback = select_scoped_credential("p", &entries, Some("other"))
            .expect("fallback")
            .expect("entry");
        assert_eq!(fallback.scope, None);
        let default = select_scoped_credential("p", &entries, None)
            .expect("default")
            .expect("entry");
        assert_eq!(default.scope, None);
        assert!(
            select_scoped_credential("p", &[], None)
                .expect("empty")
                .is_none()
        );
        assert!(
            select_scoped_credential("p", &[], Some("work"))
                .expect("empty with scope")
                .is_none()
        );

        let scoped_only = vec![ScopedCredential::scoped(api_key("work-key"), "work")];
        let error = select_scoped_credential("p", &scoped_only, None)
            .expect_err("scoped-only without active scope must fail");
        let message = format!("{error:#}");
        assert!(message.contains("p"), "{message}");
        assert!(message.contains("\"work\""), "{message}");
        assert!(message.contains("PI_AUTH_SCOPE"), "{message}");
        assert!(!message.contains("work-key"), "secret leak: {message}");

        let mismatch = select_scoped_credential("p", &scoped_only, Some("personal"))
            .expect_err("unmatched scope without fallback must fail");
        let message = format!("{mismatch:#}");
        assert!(message.contains("personal"), "{message}");
        assert!(message.contains("work"), "{message}");
    }

    #[test]
    fn scope_labels_are_validated_and_normalized() {
        assert_eq!(
            validate_scope_label("  work  ").expect("trimmed label"),
            "work"
        );
        assert!(validate_scope_label("").is_err());
        assert!(validate_scope_label("   ").is_err());
        assert!(validate_scope_label("a\nb").is_err());
        assert!(validate_scope_label("a/b").is_err());
        assert!(validate_scope_label(&"x".repeat(129)).is_err());
        assert_eq!(
            validate_scope_label("work-1.personal_x").expect("typical label"),
            "work-1.personal_x"
        );
    }

    #[test]
    fn scope_preference_prefers_env_and_ignores_blank_values() {
        assert_eq!(resolve_scope_preference(None, None), None);
        assert_eq!(
            resolve_scope_preference(Some("work".to_owned()), Some("personal".to_owned())),
            Some("work".to_owned()),
            "env wins over settings"
        );
        assert_eq!(
            resolve_scope_preference(None, Some("personal".to_owned())),
            Some("personal".to_owned()),
            "settings apply without env"
        );
        assert_eq!(
            resolve_scope_preference(Some("  work  ".to_owned()), None),
            Some("work".to_owned()),
            "whitespace-only env is trimmed but still wins"
        );
        assert_eq!(
            resolve_scope_preference(Some("   ".to_owned()), Some("personal".to_owned())),
            Some("personal".to_owned()),
            "blank env behaves as unset"
        );
        assert_eq!(
            resolve_scope_preference(Some(String::new()), Some("personal".to_owned())),
            Some("personal".to_owned()),
            "empty env behaves as unset"
        );
    }

    #[tokio::test]
    async fn storage_slots_read_modify_delete_per_scope() {
        let directory = tempfile::tempdir().expect("temporary auth directory");
        let path = directory.path().join("auth.json");
        let storage = AuthStorage::new(path.clone()).expect("auth storage");
        storage
            .modify("p", Some("work"), |_| async {
                Ok(Some(api_key("work-key")))
            })
            .await
            .expect("store scoped");
        storage
            .modify("p", None, |_| async { Ok(Some(api_key("default-key"))) })
            .await
            .expect("store unscoped");

        assert_eq!(
            storage.read("p", Some("work")).await.expect("read work"),
            Some(api_key("work-key"))
        );
        assert_eq!(
            storage.read("p", None).await.expect("read default"),
            Some(api_key("default-key"))
        );
        assert_eq!(
            storage
                .read("p", Some("missing"))
                .await
                .expect("read missing scope"),
            None
        );

        assert!(
            storage
                .delete("p", Some("work"))
                .await
                .expect("delete work")
        );
        assert!(
            storage
                .read("p", Some("work"))
                .await
                .expect("work gone")
                .is_none(),
            "scoped slot must be removed"
        );
        assert!(
            storage
                .read("p", None)
                .await
                .expect("default survives")
                .is_some(),
            "unscoped slot must survive scoped delete"
        );
        assert!(
            storage
                .delete("p", Some("work"))
                .await
                .expect("idempotent delete")
                == false
        );
        assert!(storage.delete("p", None).await.expect("delete default"));
        assert!(load_credentials(&path).expect("empty file").is_empty());
    }
}
