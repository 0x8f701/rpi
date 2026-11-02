use std::collections::BTreeSet;
use std::fmt;
use std::net::IpAddr;
use std::num::{NonZeroU16, NonZeroU64};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::PolicyValidationError;

pub(crate) mod request;
pub(crate) mod wire;

pub use request::{
    RequestedPolicy, SessionScope, agent_overlaps_generated_write,
    required_workspace_protected_descendants, resolve_policy_snapshot,
};

const ADDRESS_SPACE_DEFAULT: u64 = 8 * 1024 * 1024 * 1024;
const CPU_SECONDS_DEFAULT: u64 = 900;
const FILE_SIZE_DEFAULT: u64 = 1024 * 1024 * 1024;
const OPEN_FILES_DEFAULT: u64 = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostPlatform {
    Linux,
    Macos,
    Windows,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
pub enum NetworkPolicy {
    None,
    ProxyRouted,
    FullAccess,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct AbsolutePath(PathBuf);

impl AbsolutePath {
    pub fn parse(path: PathBuf) -> Result<Self, PolicyValidationError> {
        validate_path(path, true, "path").map(Self)
    }

    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    #[must_use]
    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct RelativePath(PathBuf);

impl RelativePath {
    pub fn parse(path: PathBuf) -> Result<Self, PolicyValidationError> {
        validate_path(path, false, "path").map(Self)
    }

    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    #[must_use]
    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(transparent)]
pub struct CredentialGrantId(String);

impl CredentialGrantId {
    pub fn parse(value: String) -> Result<Self, PolicyValidationError> {
        if !is_grant_id(&value) {
            return Err(PolicyValidationError::InvalidField {
                field: "credentialGrantId",
                reason: "must match [a-z][a-z0-9_.:-]{0,95}",
            });
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl<'de> Deserialize<'de> for CredentialGrantId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(transparent)]
pub struct EnvironmentVariableName(String);

impl EnvironmentVariableName {
    pub fn parse(value: String) -> Result<Self, PolicyValidationError> {
        if !is_environment_name(&value) {
            return Err(PolicyValidationError::InvalidField {
                field: "environment",
                reason: "must match [A-Za-z_][A-Za-z0-9_]{0,127}",
            });
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl<'de> Deserialize<'de> for EnvironmentVariableName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct DnsName(String);

impl DnsName {
    pub fn parse(value: String) -> Result<Self, PolicyValidationError> {
        Ok(Self(normalize_dns_name(&value)?))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl<'de> Deserialize<'de> for DnsName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WritableRoot {
    root: AbsolutePath,
    protected_descendants: Box<[RelativePath]>,
}

impl WritableRoot {
    pub fn new(
        root: AbsolutePath,
        protected: Vec<RelativePath>,
    ) -> Result<Self, PolicyValidationError> {
        let protected = unique_sorted(
            protected,
            |path| path.as_path().to_path_buf(),
            "protectedDescendants",
        )?;
        Ok(Self {
            root,
            protected_descendants: protected.into_boxed_slice(),
        })
    }

    #[must_use]
    pub fn root(&self) -> &AbsolutePath {
        &self.root
    }

    #[must_use]
    pub fn protected_descendants(&self) -> &[RelativePath] {
        &self.protected_descendants
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FileSystemPolicy {
    read_only_roots: Box<[AbsolutePath]>,
    writable_roots: Box<[WritableRoot]>,
    denied_paths: Box<[AbsolutePath]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sealed_scratch_root: Option<AbsolutePath>,
    #[serde(rename = "privateRoots", skip_serializing_if = "private_roots_are_empty")]
    private_roots: Box<[AbsolutePath]>,
}

impl FileSystemPolicy {
    pub fn new(
        read_only: Vec<AbsolutePath>,
        writable: Vec<WritableRoot>,
        denied: Vec<AbsolutePath>,
    ) -> Result<Self, PolicyValidationError> {
        Self::new_with_private_roots(read_only, writable, denied, None, Vec::new())
    }
    pub fn resolved(
        read_only: Vec<AbsolutePath>,
        writable: Vec<WritableRoot>,
        denied: Vec<AbsolutePath>,
        sealed_scratch_root: AbsolutePath,
    ) -> Result<Self, PolicyValidationError> {
        Self::new_with_private_roots(
            read_only,
            writable,
            denied,
            Some(sealed_scratch_root),
            Vec::new(),
        )
    }
    pub fn with_private_roots(
        read_only: Vec<AbsolutePath>,
        writable: Vec<WritableRoot>,
        denied: Vec<AbsolutePath>,
        sealed_scratch_root: Option<AbsolutePath>,
        private_roots: Vec<AbsolutePath>,
    ) -> Result<Self, PolicyValidationError> {
        Self::new_with_private_roots(
            read_only,
            writable,
            denied,
            sealed_scratch_root,
            private_roots,
        )
    }

    fn new_with_private_roots(
        read_only: Vec<AbsolutePath>,
        writable: Vec<WritableRoot>,
        denied: Vec<AbsolutePath>,
        sealed_scratch_root: Option<AbsolutePath>,
        private_roots: Vec<AbsolutePath>,
    ) -> Result<Self, PolicyValidationError> {
        let denied = unique_sorted(denied, |path| path.as_path().to_path_buf(), "deniedPaths")?;
        let private_roots = collapse_private_roots(private_roots)?;
        let writable = collapse_equal_writable_roots(writable)?;
        reject_overlapping_writable_roots(&writable)?;
        let read_only = unique_sorted(
            read_only,
            |path| path.as_path().to_path_buf(),
            "readOnlyRoots",
        )?;
        let read_only = strip_private_covered_read_only(read_only, &private_roots);
        reject_cross_category_overlaps(&read_only, &writable, sealed_scratch_root.as_ref())?;
        reject_private_root_overlaps(&private_roots, &writable)?;
        Ok(Self {
            read_only_roots: read_only.into_boxed_slice(),
            writable_roots: writable.into_boxed_slice(),
            denied_paths: denied.into_boxed_slice(),
            sealed_scratch_root,
            private_roots: private_roots.into_boxed_slice(),
        })
    }

    #[must_use]
    pub fn read_only_roots(&self) -> &[AbsolutePath] {
        &self.read_only_roots
    }

    #[must_use]
    pub fn writable_roots(&self) -> &[WritableRoot] {
        &self.writable_roots
    }

    #[must_use]
    pub fn denied_paths(&self) -> &[AbsolutePath] {
        &self.denied_paths
    }

    #[must_use]
    pub fn sealed_scratch_root(&self) -> Option<&AbsolutePath> {
        self.sealed_scratch_root.as_ref()
    }

    #[must_use]
    pub fn private_roots(&self) -> &[AbsolutePath] {
        &self.private_roots
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostHttpClass {
    ProviderApi,
    ImageGeneration,
    WebSearch,
    GithubApi,
    MemoryApi,
    PluginMarketplace,
    PluginArchive,
    NpmRegistry,
    NpmTarball,
    AuthOauth,
    LiveStt,
    LlamaRouter,
    LlamaHuggingFace,
    LlamaGguf,
    RealtimeApi,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostHttpScheme {
    Https,
    Http,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IpScope {
    Global,
    Private,
    Loopback,
    LinkLocal,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HostHttpHost {
    Dns { name: DnsName },
    Ip { address: IpAddr },
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub struct HostHttpDestinationKey {
    class: HostHttpClass,
    scheme: HostHttpScheme,
    host: HostHttpHost,
    port: NonZeroU16,
    allowed_ip_scopes: Box<[IpScope]>,
}

impl HostHttpDestinationKey {
    fn from_destination(destination: &HostHttpDestination) -> Self {
        Self {
            class: destination.class,
            scheme: destination.scheme,
            host: destination.host.clone(),
            port: destination.port,
            allowed_ip_scopes: destination.allowed_ip_scopes.clone(),
        }
    }

    #[must_use]
    pub const fn class(&self) -> HostHttpClass {
        self.class
    }

    #[must_use]
    pub const fn scheme(&self) -> HostHttpScheme {
        self.scheme
    }

    #[must_use]
    pub fn host(&self) -> &HostHttpHost {
        &self.host
    }

    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port.get()
    }

    #[must_use]
    pub fn allowed_ip_scopes(&self) -> &[IpScope] {
        &self.allowed_ip_scopes
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HostHttpDestination {
    class: HostHttpClass,
    scheme: HostHttpScheme,
    host: HostHttpHost,
    port: NonZeroU16,
    allowed_ip_scopes: Box<[IpScope]>,
}

impl HostHttpDestination {
    pub fn new(
        class: HostHttpClass,
        scheme: HostHttpScheme,
        host: HostHttpHost,
        port: u16,
        scopes: Vec<IpScope>,
    ) -> Result<Self, PolicyValidationError> {
        let port = NonZeroU16::new(port).ok_or(PolicyValidationError::InvalidField {
            field: "port",
            reason: "must be nonzero",
        })?;
        if scopes.is_empty() {
            return Err(PolicyValidationError::InvalidField {
                field: "allowedIpScopes",
                reason: "at least one IP scope is required",
            });
        }
        let scopes = unique_sorted(scopes, |scope| format!("{scope:?}"), "allowedIpScopes")?;
        Ok(Self {
            class,
            scheme,
            host,
            port,
            allowed_ip_scopes: scopes.into_boxed_slice(),
        })
    }

    #[must_use]
    pub const fn class(&self) -> HostHttpClass {
        self.class
    }

    #[must_use]
    pub const fn scheme(&self) -> HostHttpScheme {
        self.scheme
    }

    #[must_use]
    pub fn host(&self) -> &HostHttpHost {
        &self.host
    }

    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port.get()
    }

    #[must_use]
    pub fn allowed_ip_scopes(&self) -> &[IpScope] {
        &self.allowed_ip_scopes
    }

    #[must_use]
    pub fn canonical_key(&self) -> HostHttpDestinationKey {
        HostHttpDestinationKey::from_destination(self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HostHttpPolicy {
    destinations: Box<[HostHttpDestination]>,
}

impl HostHttpPolicy {
    pub fn new(destinations: Vec<HostHttpDestination>) -> Result<Self, PolicyValidationError> {
        let mut keys = BTreeSet::new();
        for destination in &destinations {
            if !keys.insert(canonical_destination_key(destination)) {
                return Err(PolicyValidationError::DuplicateEntry {
                    field: "hostHttp.destinations",
                });
            }
        }
        Ok(Self {
            destinations: destinations.into_boxed_slice(),
        })
    }

    #[must_use]
    pub fn destinations(&self) -> &[HostHttpDestination] {
        &self.destinations
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.destinations.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CredentialPolicy {
    allowed_grants: Box<[CredentialGrantId]>,
}

impl CredentialPolicy {
    pub fn new(ids: Vec<CredentialGrantId>) -> Result<Self, PolicyValidationError> {
        let ids = unique_sorted(ids, |id| id.as_str().to_owned(), "allowedGrants")?;
        Ok(Self {
            allowed_grants: ids.into_boxed_slice(),
        })
    }

    #[must_use]
    pub fn allowed_grants(&self) -> &[CredentialGrantId] {
        &self.allowed_grants
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.allowed_grants.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ResourceLimits {
    address_space_bytes: NonZeroU64,
    cpu_seconds: NonZeroU64,
    file_size_bytes: NonZeroU64,
    open_files: NonZeroU64,
}

impl ResourceLimits {
    pub fn new(
        address: u64,
        cpu: u64,
        file: u64,
        files: u64,
    ) -> Result<Self, PolicyValidationError> {
        Ok(Self {
            address_space_bytes: limit("addressSpaceBytes", address, ADDRESS_SPACE_DEFAULT)?,
            cpu_seconds: limit("cpuSeconds", cpu, CPU_SECONDS_DEFAULT)?,
            file_size_bytes: limit("fileSizeBytes", file, FILE_SIZE_DEFAULT)?,
            open_files: limit("openFiles", files, OPEN_FILES_DEFAULT)?,
        })
    }

    #[must_use]
    pub const fn confined_defaults() -> Self {
        Self {
            address_space_bytes: NonZeroU64::new(ADDRESS_SPACE_DEFAULT).expect("nonzero default"),
            cpu_seconds: NonZeroU64::new(CPU_SECONDS_DEFAULT).expect("nonzero default"),
            file_size_bytes: NonZeroU64::new(FILE_SIZE_DEFAULT).expect("nonzero default"),
            open_files: NonZeroU64::new(OPEN_FILES_DEFAULT).expect("nonzero default"),
        }
    }

    #[must_use]
    pub const fn address_space_bytes(self) -> u64 {
        self.address_space_bytes.get()
    }

    #[must_use]
    pub const fn cpu_seconds(self) -> u64 {
        self.cpu_seconds.get()
    }

    #[must_use]
    pub const fn file_size_bytes(self) -> u64 {
        self.file_size_bytes.get()
    }

    #[must_use]
    pub const fn open_files(self) -> u64 {
        self.open_files.get()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
pub enum SandboxPolicy {
    ReadOnly {
        filesystem: FileSystemPolicy,
        network: NetworkPolicy,
        #[serde(rename = "hostHttp")]
        host_http: HostHttpPolicy,
        credentials: CredentialPolicy,
        limits: ResourceLimits,
    },
    WorkspaceWrite {
        filesystem: FileSystemPolicy,
        network: NetworkPolicy,
        #[serde(rename = "hostHttp")]
        host_http: HostHttpPolicy,
        credentials: CredentialPolicy,
        limits: ResourceLimits,
    },
    DangerFullAccess {
        credentials: CredentialPolicy,
    },
}

impl SandboxPolicy {
    pub fn read_only(
        fs: FileSystemPolicy,
        network: NetworkPolicy,
        http: HostHttpPolicy,
        credentials: CredentialPolicy,
        limits: ResourceLimits,
    ) -> Result<Self, PolicyValidationError> {
        reject_proxy_routed(network)?;
        if !fs.writable_roots().is_empty() {
            return Err(PolicyValidationError::ConflictingFields {
                left: "mode",
                right: "writableRoots",
            });
        }
        if fs.sealed_scratch_root().is_some() {
            return Err(PolicyValidationError::ConflictingFields {
                left: "mode",
                right: "sealedScratchRoot",
            });
        }
        Ok(Self::ReadOnly {
            filesystem: fs,
            network,
            host_http: http,
            credentials,
            limits,
        })
    }

    pub(crate) fn resolved_read_only(
        fs: FileSystemPolicy,
        network: NetworkPolicy,
        http: HostHttpPolicy,
        credentials: CredentialPolicy,
        limits: ResourceLimits,
    ) -> Result<Self, PolicyValidationError> {
        reject_proxy_routed(network)?;
        if !fs.writable_roots().is_empty() || fs.sealed_scratch_root().is_none() {
            return Err(PolicyValidationError::InvalidField {
                field: "sealedScratchRoot",
                reason: "resolved read-only policy requires sealed scratch and no writable roots",
            });
        }
        Ok(Self::ReadOnly {
            filesystem: fs,
            network,
            host_http: http,
            credentials,
            limits,
        })
    }

    pub fn workspace_write(
        fs: FileSystemPolicy,
        network: NetworkPolicy,
        http: HostHttpPolicy,
        credentials: CredentialPolicy,
        limits: ResourceLimits,
    ) -> Result<Self, PolicyValidationError> {
        reject_proxy_routed(network)?;
        Ok(Self::WorkspaceWrite {
            filesystem: fs,
            network,
            host_http: http,
            credentials,
            limits,
        })
    }

    pub fn danger_full_access(
        credentials: CredentialPolicy,
    ) -> Result<Self, PolicyValidationError> {
        Ok(Self::DangerFullAccess { credentials })
    }

    #[must_use]
    pub const fn mode(&self) -> SandboxMode {
        match self {
            Self::ReadOnly { .. } => SandboxMode::ReadOnly,
            Self::WorkspaceWrite { .. } => SandboxMode::WorkspaceWrite,
            Self::DangerFullAccess { .. } => SandboxMode::DangerFullAccess,
        }
    }

    #[must_use]
    pub fn filesystem(&self) -> Option<&FileSystemPolicy> {
        match self {
            Self::ReadOnly { filesystem, .. } | Self::WorkspaceWrite { filesystem, .. } => {
                Some(filesystem)
            }
            Self::DangerFullAccess { .. } => None,
        }
    }

    #[must_use]
    pub fn network(&self) -> Option<NetworkPolicy> {
        match self {
            Self::ReadOnly { network, .. } | Self::WorkspaceWrite { network, .. } => Some(*network),
            Self::DangerFullAccess { .. } => None,
        }
    }

    #[must_use]
    pub fn host_http(&self) -> Option<&HostHttpPolicy> {
        match self {
            Self::ReadOnly { host_http, .. } | Self::WorkspaceWrite { host_http, .. } => {
                Some(host_http)
            }
            Self::DangerFullAccess { .. } => None,
        }
    }

    #[must_use]
    pub fn credentials(&self) -> &CredentialPolicy {
        match self {
            Self::ReadOnly { credentials, .. }
            | Self::WorkspaceWrite { credentials, .. }
            | Self::DangerFullAccess { credentials } => credentials,
        }
    }

    #[must_use]
    pub fn limits(&self) -> Option<ResourceLimits> {
        match self {
            Self::ReadOnly { limits, .. } | Self::WorkspaceWrite { limits, .. } => Some(*limits),
            Self::DangerFullAccess { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct PolicyRevision(u64);

impl PolicyRevision {
    pub fn new(value: u64) -> Result<Self, PolicyValidationError> {
        if value == 0 {
            return Err(PolicyValidationError::InvalidField {
                field: "revision",
                reason: "must be nonzero",
            });
        }
        Ok(Self(value))
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct PolicyDigest([u8; 32]);

impl Serialize for PolicyDigest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl PolicyDigest {
    pub fn parse_hex(value: &str) -> Result<Self, PolicyValidationError> {
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(PolicyValidationError::InvalidField {
                field: "digest",
                reason: "must be 64 lowercase hexadecimal characters",
            });
        }
        let mut bytes = [0_u8; 32];
        for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
            let text = std::str::from_utf8(chunk).expect("hex pair");
            bytes[index] =
                u8::from_str_radix(text, 16).map_err(|_| PolicyValidationError::InvalidField {
                    field: "digest",
                    reason: "must be 64 lowercase hexadecimal characters",
                })?;
        }
        if value.chars().any(|ch| ch.is_ascii_uppercase()) {
            return Err(PolicyValidationError::InvalidField {
                field: "digest",
                reason: "must be 64 lowercase hexadecimal characters",
            });
        }
        Ok(Self(bytes))
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    #[must_use]
    pub fn to_hex(self) -> String {
        hex_encode(&self.0)
    }
}

impl fmt::Debug for PolicyDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("PolicyDigest")
            .field(&self.to_hex())
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PolicySnapshot {
    revision: PolicyRevision,
    created_on: HostPlatform,
    policy: SandboxPolicy,
    digest: PolicyDigest,
}

impl PolicySnapshot {
    pub fn new(
        revision: PolicyRevision,
        created_on: HostPlatform,
        policy: SandboxPolicy,
    ) -> Result<Self, PolicyValidationError> {
        let digest = compute_digest(revision, created_on, &policy)?;
        Ok(Self {
            revision,
            created_on,
            policy,
            digest,
        })
    }

    #[must_use]
    pub const fn revision(&self) -> PolicyRevision {
        self.revision
    }

    #[must_use]
    pub const fn created_on(&self) -> HostPlatform {
        self.created_on
    }

    #[must_use]
    pub fn policy(&self) -> &SandboxPolicy {
        &self.policy
    }

    #[must_use]
    pub const fn digest(&self) -> PolicyDigest {
        self.digest
    }

    pub fn validate_digest(&self) -> Result<(), PolicyValidationError> {
        let expected = compute_digest(self.revision, self.created_on, &self.policy)?;
        if expected == self.digest {
            Ok(())
        } else {
            Err(PolicyValidationError::DigestMismatch)
        }
    }
}

impl<'de> Deserialize<'de> for PolicySnapshot {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = wire::PolicySnapshotWire::deserialize(deserializer)?;
        let expected = PolicyDigest::parse_hex(&wire.digest).map_err(de::Error::custom)?;
        let revision = PolicyRevision::new(wire.revision).map_err(de::Error::custom)?;
        let policy = SandboxPolicy::try_from(wire.policy).map_err(de::Error::custom)?;
        let snapshot = PolicySnapshot::new(revision, wire.created_on.into(), policy)
            .map_err(de::Error::custom)?;
        if snapshot.digest() != expected {
            return Err(de::Error::custom("policy snapshot digest mismatch"));
        }
        Ok(snapshot)
    }
}

pub type SharedPolicySnapshot = Arc<PolicySnapshot>;

fn validate_path(
    path: PathBuf,
    absolute: bool,
    field: &'static str,
) -> Result<PathBuf, PolicyValidationError> {
    let text = path.to_str().ok_or(PolicyValidationError::InvalidField {
        field,
        reason: "must be valid UTF-8",
    })?;
    if text.contains('\0') {
        return Err(PolicyValidationError::InvalidField {
            field,
            reason: "must not contain NUL",
        });
    }
    if absolute != path.is_absolute() {
        return Err(PolicyValidationError::InvalidField {
            field,
            reason: if absolute {
                "must be absolute"
            } else {
                "must be relative"
            },
        });
    }
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(PolicyValidationError::InvalidField {
            field,
            reason: "must not contain . or .. components",
        });
    }
    if !absolute && path.as_os_str().is_empty() {
        return Err(PolicyValidationError::InvalidField {
            field,
            reason: "must be relative",
        });
    }
    Ok(path)
}

fn is_grant_id(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some('a'..='z'))
        && value.len() <= 96
        && chars.all(|ch| matches!(ch, 'a'..='z' | '0'..='9' | '_' | '.' | ':' | '-'))
}

fn is_environment_name(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some('A'..='Z' | 'a'..='z' | '_'))
        && value.len() <= 128
        && chars.all(|ch| matches!(ch, 'A'..='Z' | 'a'..='z' | '0'..='9' | '_'))
}

fn normalize_dns_name(value: &str) -> Result<String, PolicyValidationError> {
    if value.is_empty() || value.ends_with('.') {
        return Err(invalid_dns());
    }
    if value.eq_ignore_ascii_case("localhost") {
        return Err(invalid_dns());
    }
    if value.parse::<IpAddr>().is_ok() {
        return Err(invalid_dns());
    }
    if !value.is_ascii() {
        return Err(invalid_dns());
    }
    let normalized = value.to_ascii_lowercase();
    if normalized.len() > 253 {
        return Err(invalid_dns());
    }
    let labels: Vec<&str> = normalized.split('.').collect();
    if labels.is_empty() {
        return Err(invalid_dns());
    }
    for label in &labels {
        if label.is_empty() || label.len() > 63 || label.starts_with('-') || label.ends_with('-') {
            return Err(invalid_dns());
        }
        if !label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(invalid_dns());
        }
    }
    Ok(normalized)
}

fn invalid_dns() -> PolicyValidationError {
    PolicyValidationError::InvalidField {
        field: "dnsName",
        reason: "must be a lowercase ASCII A-label",
    }
}

fn unique_sorted<T, K: Ord>(
    mut values: Vec<T>,
    key: impl Fn(&T) -> K,
    field: &'static str,
) -> Result<Vec<T>, PolicyValidationError> {
    let mut seen = BTreeSet::new();
    for value in &values {
        if !seen.insert(key(value)) {
            return Err(PolicyValidationError::DuplicateEntry { field });
        }
    }
    values.sort_by_key(key);
    Ok(values)
}

fn collapse_equal_writable_roots(
    mut writable: Vec<WritableRoot>,
) -> Result<Vec<WritableRoot>, PolicyValidationError> {
    writable.sort_by(|left, right| left.root().cmp(right.root()));
    let mut collapsed = Vec::<WritableRoot>::new();
    for root in writable {
        if collapsed
            .last()
            .is_some_and(|existing| existing.root() == root.root())
        {
            let Some(existing) = collapsed.pop() else {
                collapsed.push(root);
                continue;
            };
            let mut protected = existing.protected_descendants().to_vec();
            protected.extend(root.protected_descendants().iter().cloned());
            protected.sort();
            protected.dedup();
            collapsed.push(WritableRoot::new(root.root().clone(), protected)?);
            continue;
        }
        collapsed.push(root);
    }
    Ok(collapsed)
}

fn reject_overlapping_writable_roots(
    writable: &[WritableRoot],
) -> Result<(), PolicyValidationError> {
    for (index, left) in writable.iter().enumerate() {
        for right in &writable[index + 1..] {
            if paths_overlap(left.root().as_path(), right.root().as_path()) {
                return Err(PolicyValidationError::ConflictingFields {
                    left: "writableRoots",
                    right: "writableRoots",
                });
            }
        }
    }
    Ok(())
}

fn reject_cross_category_overlaps(
    read_only: &[AbsolutePath],
    writable: &[WritableRoot],
    sealed_scratch_root: Option<&AbsolutePath>,
) -> Result<(), PolicyValidationError> {
    for read_only_root in read_only {
        for writable_root in writable {
            if paths_overlap(read_only_root.as_path(), writable_root.root().as_path()) {
                return Err(PolicyValidationError::ConflictingFields {
                    left: "readOnlyRoots",
                    right: "writableRoots",
                });
            }
        }
    }
    if let Some(scratch) = sealed_scratch_root {
        for read_only_root in read_only {
            if paths_overlap(read_only_root.as_path(), scratch.as_path()) {
                return Err(PolicyValidationError::ConflictingFields {
                    left: "sealedScratchRoot",
                    right: "readOnlyRoots",
                });
            }
        }
        for writable_root in writable {
            if paths_overlap(writable_root.root().as_path(), scratch.as_path()) {
                return Err(PolicyValidationError::ConflictingFields {
                    left: "sealedScratchRoot",
                    right: "writableRoots",
                });
            }
        }
    }
    Ok(())
}

fn private_roots_are_empty(roots: &Box<[AbsolutePath]>) -> bool {
    roots.is_empty()
}

fn collapse_private_roots(
    roots: Vec<AbsolutePath>,
) -> Result<Vec<AbsolutePath>, PolicyValidationError> {
    let roots = unique_sorted(roots, |path| path.as_path().to_path_buf(), "privateRoots")?;
    let mut kept = Vec::new();
    for root in roots {
        if kept
            .iter()
            .any(|existing: &AbsolutePath| root.as_path().starts_with(existing.as_path()))
        {
            continue;
        }
        kept.push(root);
    }
    Ok(kept)
}

fn strip_private_covered_read_only(
    read_only: Vec<AbsolutePath>,
    private_roots: &[AbsolutePath],
) -> Vec<AbsolutePath> {
    read_only
        .into_iter()
        .filter(|read_only_root| {
            !private_roots.iter().any(|private| {
                read_only_root.as_path() == private.as_path()
                    || read_only_root.as_path().starts_with(private.as_path())
            })
        })
        .collect()
}

fn reject_private_root_overlaps(
    private_roots: &[AbsolutePath],
    writable: &[WritableRoot],
) -> Result<(), PolicyValidationError> {
    for private in private_roots {
        for writable_root in writable {
            if writable_root.root().as_path() == private.as_path()
                || writable_root.root().as_path().starts_with(private.as_path())
            {
                return Err(PolicyValidationError::ConflictingFields {
                    left: "privateRoots",
                    right: "writableRoots",
                });
            }
        }
    }
    Ok(())
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn reject_proxy_routed(network: NetworkPolicy) -> Result<(), PolicyValidationError> {
    if matches!(network, NetworkPolicy::ProxyRouted) {
        Err(PolicyValidationError::InvalidField {
            field: "network",
            reason: "proxy-routed child networking is unavailable",
        })
    } else {
        Ok(())
    }
}

fn limit(
    field: &'static str,
    value: u64,
    maximum: u64,
) -> Result<NonZeroU64, PolicyValidationError> {
    if value == 0 || value > maximum {
        return Err(PolicyValidationError::ResourceLimitOutOfRange { field, maximum });
    }
    Ok(NonZeroU64::new(value).expect("checked nonzero"))
}

fn canonical_destination_key(destination: &HostHttpDestination) -> String {
    serde_json::to_string(&destination.canonical_key())
        .expect("canonical destination key is serializable")
}

fn compute_digest(
    revision: PolicyRevision,
    created_on: HostPlatform,
    policy: &SandboxPolicy,
) -> Result<PolicyDigest, PolicyValidationError> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct CanonicalSnapshot<'a> {
        revision: PolicyRevision,
        created_on: HostPlatform,
        policy: &'a SandboxPolicy,
    }

    let bytes = serde_json::to_vec(&CanonicalSnapshot {
        revision,
        created_on,
        policy,
    })
    .map_err(|_| PolicyValidationError::InvalidField {
        field: "policy",
        reason: "canonical snapshot serialization failed",
    })?;
    let digest = Sha256::digest(bytes);
    let mut bytes = [0_u8; 32];
    bytes.copy_from_slice(&digest);
    Ok(PolicyDigest(bytes))
}

fn hex_encode(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dest(name: &str) -> HostHttpDestination {
        HostHttpDestination::new(
            HostHttpClass::GithubApi,
            HostHttpScheme::Https,
            HostHttpHost::Dns {
                name: DnsName::parse(name.to_owned()).expect("dns"),
            },
            443,
            vec![IpScope::Global],
        )
        .expect("destination")
    }

    fn confined_fs() -> FileSystemPolicy {
        FileSystemPolicy::new(
            vec![AbsolutePath::parse("/usr".into()).expect("usr")],
            vec![
                WritableRoot::new(
                    AbsolutePath::parse("/workspace".into()).expect("workspace"),
                    vec![RelativePath::parse(".git".into()).expect("git")],
                )
                .expect("writable"),
            ],
            vec![AbsolutePath::parse("/workspace/secret".into()).expect("denied")],
        )
        .expect("fs")
    }

    #[test]
    fn dns_name_deserialize_revalidates() {
        assert!(serde_json::from_str::<DnsName>("\"API.GitHub.com\"").is_ok());
        assert!(serde_json::from_str::<DnsName>("\"localhost\"").is_err());
        assert!(serde_json::from_str::<DnsName>("\"127.0.0.1\"").is_err());
        assert!(serde_json::from_str::<DnsName>("\"example.com.\"").is_err());
        assert_eq!(
            DnsName::parse("API.GitHub.com".to_owned())
                .expect("parse")
                .as_str(),
            "api.github.com"
        );
    }

    #[test]
    fn snapshot_deserialize_revalidates_and_checks_digest() {
        let snapshot = PolicySnapshot::new(
            PolicyRevision::new(1).expect("revision"),
            HostPlatform::Linux,
            SandboxPolicy::workspace_write(
                confined_fs(),
                NetworkPolicy::None,
                HostHttpPolicy::new(vec![dest("api.github.com")]).expect("http"),
                CredentialPolicy::new(vec![
                    CredentialGrantId::parse("github.api".to_owned()).expect("id"),
                ])
                .expect("credentials"),
                ResourceLimits::confined_defaults(),
            )
            .expect("policy"),
        )
        .expect("snapshot");
        let json = serde_json::to_string(&snapshot).expect("serialize");
        let decoded: PolicySnapshot = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(decoded, snapshot);

        let mut tampered: serde_json::Value = serde_json::from_str(&json).expect("value");
        tampered["digest"] = serde_json::Value::String("00".repeat(32));
        assert!(serde_json::from_value::<PolicySnapshot>(tampered).is_err());

        let mut mutated: serde_json::Value = serde_json::from_str(&json).expect("value");
        mutated["policy"]["network"] = serde_json::json!({ "mode": "full-access" });
        assert!(serde_json::from_value::<PolicySnapshot>(mutated).is_err());
    }

    #[test]
    fn duplicate_normalized_policy_entries_are_rejected() {
        let root = AbsolutePath::parse("/workspace".into()).expect("root");
        assert!(matches!(
            FileSystemPolicy::new(vec![root.clone(), root.clone()], Vec::new(), Vec::new()),
            Err(PolicyValidationError::DuplicateEntry {
                field: "readOnlyRoots"
            })
        ));
        assert!(matches!(
            HostHttpPolicy::new(vec![dest("API.GitHub.com"), dest("api.github.com")]),
            Err(PolicyValidationError::DuplicateEntry { .. })
        ));
        assert!(matches!(
            CredentialPolicy::new(vec![
                CredentialGrantId::parse("github.api".to_owned()).expect("a"),
                CredentialGrantId::parse("github.api".to_owned()).expect("b"),
            ]),
            Err(PolicyValidationError::DuplicateEntry {
                field: "allowedGrants"
            })
        ));
        assert!(matches!(
            ResourceLimits::new(0, 1, 1, 1),
            Err(PolicyValidationError::ResourceLimitOutOfRange {
                field: "addressSpaceBytes",
                ..
            })
        ));
        assert!(matches!(
            ResourceLimits::new(ADDRESS_SPACE_DEFAULT + 1, 1, 1, 1),
            Err(PolicyValidationError::ResourceLimitOutOfRange {
                field: "addressSpaceBytes",
                ..
            })
        ));
    }

    fn abs(value: &str) -> AbsolutePath {
        AbsolutePath::parse(value.into()).expect("absolute path")
    }

    fn writable(value: &str) -> WritableRoot {
        WritableRoot::new(abs(value), Vec::new()).expect("writable root")
    }

    #[test]
    fn equal_writable_roots_union_protected_descendants() {
        let root = abs("/workspace");
        let git = RelativePath::parse(".git".into()).expect("git");
        let agents = RelativePath::parse(".agents".into()).expect("agents");
        let policy = FileSystemPolicy::new(
            vec![abs("/usr")],
            vec![
                WritableRoot::new(root.clone(), vec![git.clone()]).expect("first"),
                WritableRoot::new(root.clone(), vec![agents.clone(), git.clone()]).expect("second"),
            ],
            Vec::new(),
        )
        .expect("equal writable roots union");
        assert_eq!(policy.writable_roots().len(), 1);
        let protected = policy.writable_roots()[0].protected_descendants();
        assert!(protected.contains(&git), "{protected:?}");
        assert!(protected.contains(&agents), "{protected:?}");
        assert_eq!(protected.len(), 2, "{protected:?}");
    }

    #[test]
    fn read_only_writable_root_overlaps_are_rejected() {
        // writable child inside read-only parent
        assert!(matches!(
            FileSystemPolicy::new(vec![abs("/app")], vec![writable("/app/data")], Vec::new(),),
            Err(PolicyValidationError::ConflictingFields {
                left: "readOnlyRoots",
                right: "writableRoots"
            })
        ));
        // read-only child inside writable parent
        assert!(matches!(
            FileSystemPolicy::new(
                vec![abs("/workspace/sub")],
                vec![writable("/workspace")],
                Vec::new(),
            ),
            Err(PolicyValidationError::ConflictingFields {
                left: "readOnlyRoots",
                right: "writableRoots"
            })
        ));
    }

    #[test]
    fn sealed_scratch_overlaps_are_rejected() {
        // sealed scratch overlapping a read-only root
        assert!(matches!(
            FileSystemPolicy::resolved(vec![abs("/rev/home")], Vec::new(), Vec::new(), abs("/rev"),),
            Err(PolicyValidationError::ConflictingFields {
                left: "sealedScratchRoot",
                right: "readOnlyRoots"
            })
        ));
        // sealed scratch overlapping a writable root
        assert!(matches!(
            FileSystemPolicy::resolved(
                Vec::new(),
                vec![writable("/workspace")],
                Vec::new(),
                abs("/workspace/sub"),
            ),
            Err(PolicyValidationError::ConflictingFields {
                left: "sealedScratchRoot",
                right: "writableRoots"
            })
        ));
        // read-only root inside sealed scratch
        assert!(matches!(
            FileSystemPolicy::resolved(
                vec![abs("/rev/scratch/nested")],
                Vec::new(),
                Vec::new(),
                abs("/rev/scratch"),
            ),
            Err(PolicyValidationError::ConflictingFields {
                left: "sealedScratchRoot",
                right: "readOnlyRoots"
            })
        ));
    }

    #[test]
    fn private_roots_overlay_matching_and_nested_read_only_roots() {
        let fs = FileSystemPolicy::with_private_roots(
            vec![abs("/usr"), abs("/store"), abs("/store/sessions")],
            vec![writable("/workspace")],
            Vec::new(),
            None,
            vec![abs("/store")],
        )
        .expect("private overlay may cover a matching or nested read-only grant");
        assert!(fs.private_roots().contains(&abs("/store")));
        assert!(!fs.read_only_roots().contains(&abs("/store")));
        assert!(!fs.read_only_roots().contains(&abs("/store/sessions")));
        assert!(fs.read_only_roots().contains(&abs("/usr")));
        assert!(
            fs.writable_roots()
                .iter()
                .any(|root| root.root() == &abs("/workspace"))
        );
    }

    #[test]
    fn private_child_under_read_only_parent_is_an_overlay_not_a_conflict() {
        let fs = FileSystemPolicy::with_private_roots(
            vec![abs("/home")],
            Vec::new(),
            Vec::new(),
            Some(abs("/rev/scratch")),
            vec![abs("/home/.rpi")],
        )
        .expect("private child may overlay a read-only parent");
        assert!(fs.read_only_roots().contains(&abs("/home")));
        assert!(fs.private_roots().contains(&abs("/home/.rpi")));
    }

    #[test]
    fn private_roots_still_reject_writable_overlap() {
        assert!(matches!(
            FileSystemPolicy::with_private_roots(
                vec![abs("/usr")],
                vec![writable("/store")],
                Vec::new(),
                None,
                vec![abs("/store")],
            ),
            Err(PolicyValidationError::ConflictingFields {
                left: "privateRoots",
                right: "writableRoots"
            })
        ));
        assert!(matches!(
            FileSystemPolicy::with_private_roots(
                vec![abs("/usr")],
                vec![writable("/store/sessions")],
                Vec::new(),
                None,
                vec![abs("/store")],
            ),
            Err(PolicyValidationError::ConflictingFields {
                left: "privateRoots",
                right: "writableRoots"
            })
        ));
    }


    #[test]
    fn resolved_policy_with_disjoint_roots_is_representable() {
        let fs = FileSystemPolicy::resolved(
            vec![abs("/usr"), abs("/rev/home"), abs("/rev/tmp")],
            vec![writable("/workspace")],
            Vec::new(),
            abs("/rev/scratch"),
        )
        .expect("disjoint resolved policy is representable");
        assert_eq!(fs.sealed_scratch_root(), Some(&abs("/rev/scratch")));
        assert!(fs.read_only_roots().contains(&abs("/rev/home")));
        assert!(fs.read_only_roots().contains(&abs("/rev/tmp")));
        assert!(
            fs.writable_roots()
                .iter()
                .any(|root| root.root() == &abs("/workspace"))
        );
    }

    #[test]
    fn resolved_read_only_policy_with_sealed_scratch_is_representable() {
        let fs = FileSystemPolicy::resolved(
            vec![
                abs("/usr"),
                abs("/rev/home"),
                abs("/rev/tmp"),
                abs("/workspace"),
            ],
            Vec::new(),
            Vec::new(),
            abs("/rev/scratch"),
        )
        .expect("disjoint resolved read-only filesystem is representable");
        let policy = SandboxPolicy::resolved_read_only(
            fs,
            NetworkPolicy::None,
            HostHttpPolicy::new(Vec::new()).expect("empty host HTTP policy"),
            CredentialPolicy::new(Vec::new()).expect("empty credential policy"),
            ResourceLimits::confined_defaults(),
        )
        .expect("resolved read-only policy is representable without authority widening");
        assert!(matches!(policy, SandboxPolicy::ReadOnly { .. }));
        let filesystem = policy
            .filesystem()
            .expect("read-only policy has a filesystem");
        assert!(filesystem.writable_roots().is_empty());
        assert!(filesystem.sealed_scratch_root().is_some());
    }
}
