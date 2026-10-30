use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fmt::{self, Debug, Formatter},
    sync::Arc,
};

use pi_sandbox::{
    CredentialGrantId, CredentialPolicy, EnvironmentVariableName, PolicyValidationError,
    SpawnFamily,
};
use serde::{Deserialize, Deserializer, Serialize, de};

use crate::{
    AuthManager, Credential, CredentialAudienceWire, CredentialGrantDefinitionSettings,
    CredentialGrantOutputConfigWire, McpServerConfig, McpTransport, OAuthGrantRefreshWire,
};

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CredentialConfigError {
    #[error("invalid lower policy value: {0}")]
    Policy(#[from] PolicyValidationError),
    #[error("invalid credential field {field}: {reason}")]
    InvalidField { field: &'static str, reason: String },
    #[error("duplicate credential grant id")]
    DuplicateGrantId,
    #[error("duplicate credential output for one audience")]
    DuplicateAudienceOutput,
    #[error("credential grant references an unknown definition")]
    UnknownGrantReference,
    #[error("credential audience does not match its binding")]
    AudienceMismatch,
    #[error("MCP stdio credentials must use an exact MCP audience")]
    InvalidMcpAudience,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum CredentialResolutionError {
    #[error("credential grant is unavailable")]
    GrantUnavailable,
    #[error("credential grant audience does not match")]
    AudienceMismatch,
    #[error("credential grant is denied by policy")]
    PolicyDenied,
    #[error("credential provider or scope lookup failed")]
    LookupFailed,
    #[error("credential refresh failed")]
    RefreshFailed,
    #[error("credential secret is empty")]
    EmptySecret,
    #[error("credential output collides with the launch environment")]
    EnvironmentCollision,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(transparent)]
pub struct McpServerName(String);

impl McpServerName {
    pub fn parse(value: String) -> Result<Self, CredentialConfigError> {
        validate_label("mcpServer", value, 128).map(Self)
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

impl<'de> Deserialize<'de> for McpServerName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(transparent)]
pub struct ProviderId(String);

impl ProviderId {
    pub fn parse(value: String) -> Result<Self, CredentialConfigError> {
        validate_label("provider", value, 128).map(Self)
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

impl<'de> Deserialize<'de> for ProviderId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(transparent)]
pub struct CredentialScopeName(String);

impl CredentialScopeName {
    pub fn parse(value: String) -> Result<Self, CredentialConfigError> {
        validate_label("scope", value, 128).map(Self)
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

impl<'de> Deserialize<'de> for CredentialScopeName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OAuthGrantRefresh {
    IfExpired,
}

impl OAuthGrantRefresh {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        "if-expired"
    }
}

impl<'de> Deserialize<'de> for OAuthGrantRefresh {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "if-expired" => Ok(Self::IfExpired),
            value => Err(de::Error::unknown_variant(value, &["if-expired"])),
        }
    }
}

impl From<OAuthGrantRefreshWire> for OAuthGrantRefresh {
    fn from(wire: OAuthGrantRefreshWire) -> Self {
        match wire {
            OAuthGrantRefreshWire::IfExpired => Self::IfExpired,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialGrantOutputType {
    ApiKey,
    OAuthAccessToken,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialGrantOutputConfig {
    credential_type: CredentialGrantOutputType,
    environment: EnvironmentVariableName,
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh: Option<OAuthGrantRefresh>,
}

impl CredentialGrantOutputConfig {
    pub fn api_key(environment: EnvironmentVariableName) -> Result<Self, CredentialConfigError> {
        Ok(Self {
            credential_type: CredentialGrantOutputType::ApiKey,
            environment,
            refresh: None,
        })
    }

    pub fn oauth_access_token(
        environment: EnvironmentVariableName,
        refresh: OAuthGrantRefresh,
    ) -> Result<Self, CredentialConfigError> {
        Ok(Self {
            credential_type: CredentialGrantOutputType::OAuthAccessToken,
            environment,
            refresh: Some(refresh),
        })
    }

    #[must_use]
    pub const fn credential_type(&self) -> CredentialGrantOutputType {
        self.credential_type
    }

    #[must_use]
    pub fn environment(&self) -> &EnvironmentVariableName {
        &self.environment
    }

    #[must_use]
    pub const fn refresh(&self) -> Option<OAuthGrantRefresh> {
        self.refresh
    }
}

impl TryFrom<CredentialGrantOutputConfigWire> for CredentialGrantOutputConfig {
    type Error = CredentialConfigError;

    fn try_from(wire: CredentialGrantOutputConfigWire) -> Result<Self, Self::Error> {
        match wire {
            CredentialGrantOutputConfigWire::ApiKey { environment } => {
                Self::api_key(EnvironmentVariableName::parse(environment)?)
            }
            CredentialGrantOutputConfigWire::OAuthAccessToken {
                environment,
                refresh,
            } => Self::oauth_access_token(
                EnvironmentVariableName::parse(environment)?,
                refresh.into(),
            ),
        }
    }
}

impl<'de> Deserialize<'de> for CredentialGrantOutputConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::try_from(CredentialGrantOutputConfigWire::deserialize(deserializer)?)
            .map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CredentialAudience {
    McpServer { name: McpServerName },
    SpawnFamily { family: SpawnFamily },
}

impl CredentialAudience {
    pub fn new_mcp_server(name: McpServerName) -> Result<Self, CredentialConfigError> {
        Ok(Self::McpServer { name })
    }

    pub fn new_spawn_family(family: SpawnFamily) -> Result<Self, CredentialConfigError> {
        if family == SpawnFamily::McpStdio {
            return Err(CredentialConfigError::InvalidMcpAudience);
        }
        Ok(Self::SpawnFamily { family })
    }

    #[must_use]
    pub fn mcp_server(&self) -> Option<&McpServerName> {
        match self {
            Self::McpServer { name } => Some(name),
            Self::SpawnFamily { .. } => None,
        }
    }

    #[must_use]
    pub const fn spawn_family(&self) -> Option<SpawnFamily> {
        match self {
            Self::McpServer { .. } => None,
            Self::SpawnFamily { family } => Some(*family),
        }
    }
}

impl TryFrom<CredentialAudienceWire> for CredentialAudience {
    type Error = CredentialConfigError;

    fn try_from(wire: CredentialAudienceWire) -> Result<Self, Self::Error> {
        match wire {
            CredentialAudienceWire::McpServer { name } => {
                Self::new_mcp_server(McpServerName::parse(name)?)
            }
            CredentialAudienceWire::SpawnFamily { family } => Self::new_spawn_family(family),
        }
    }
}

impl<'de> Deserialize<'de> for CredentialAudience {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::try_from(CredentialAudienceWire::deserialize(deserializer)?)
            .map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialGrantDefinition {
    id: CredentialGrantId,
    provider: ProviderId,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<CredentialScopeName>,
    audience: CredentialAudience,
    output: CredentialGrantOutputConfig,
}

impl CredentialGrantDefinition {
    pub fn new(
        id: CredentialGrantId,
        provider: ProviderId,
        scope: Option<CredentialScopeName>,
        audience: CredentialAudience,
        output: CredentialGrantOutputConfig,
    ) -> Result<Self, CredentialConfigError> {
        if audience.spawn_family() == Some(SpawnFamily::McpStdio) {
            return Err(CredentialConfigError::InvalidMcpAudience);
        }
        Ok(Self {
            id,
            provider,
            scope,
            audience,
            output,
        })
    }

    #[must_use]
    pub fn id(&self) -> &CredentialGrantId {
        &self.id
    }

    #[must_use]
    pub fn provider(&self) -> &ProviderId {
        &self.provider
    }

    #[must_use]
    pub fn scope(&self) -> Option<&CredentialScopeName> {
        self.scope.as_ref()
    }

    #[must_use]
    pub fn audience(&self) -> &CredentialAudience {
        &self.audience
    }

    #[must_use]
    pub fn output(&self) -> &CredentialGrantOutputConfig {
        &self.output
    }
}

impl TryFrom<CredentialGrantDefinitionSettings> for CredentialGrantDefinition {
    type Error = CredentialConfigError;

    fn try_from(wire: CredentialGrantDefinitionSettings) -> Result<Self, Self::Error> {
        Self::new(
            CredentialGrantId::parse(wire.id)?,
            ProviderId::parse(wire.provider)?,
            wire.scope.map(CredentialScopeName::parse).transpose()?,
            CredentialAudience::try_from(wire.audience)?,
            CredentialGrantOutputConfig::try_from(wire.output)?,
        )
    }
}

impl<'de> Deserialize<'de> for CredentialGrantDefinition {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::try_from(CredentialGrantDefinitionSettings::deserialize(
            deserializer,
        )?)
        .map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialMcpBindingSnapshot {
    server: McpServerName,
    grants: Box<[CredentialGrantId]>,
}

impl CredentialMcpBindingSnapshot {
    pub fn new(
        server: McpServerName,
        mut grants: Vec<CredentialGrantId>,
    ) -> Result<Self, CredentialConfigError> {
        grants.sort_unstable();
        if grants.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(CredentialConfigError::UnknownGrantReference);
        }
        Ok(Self {
            server,
            grants: grants.into_boxed_slice(),
        })
    }

    #[must_use]
    pub fn server(&self) -> &McpServerName {
        &self.server
    }

    #[must_use]
    pub fn grants(&self) -> &[CredentialGrantId] {
        &self.grants
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialGrantRegistrySnapshot {
    definitions: Box<[CredentialGrantDefinition]>,
    mcp_bindings: Box<[CredentialMcpBindingSnapshot]>,
}

impl CredentialGrantRegistrySnapshot {
    pub fn new(
        definitions: Vec<CredentialGrantDefinition>,
        mcp_bindings: Vec<CredentialMcpBindingSnapshot>,
    ) -> Result<Self, CredentialConfigError> {
        let registry = CredentialGrantRegistry::from_snapshot(Self {
            definitions: definitions.into_boxed_slice(),
            mcp_bindings: mcp_bindings.into_boxed_slice(),
        })?;
        Ok(registry.snapshot().clone())
    }

    #[must_use]
    pub fn definitions(&self) -> &[CredentialGrantDefinition] {
        &self.definitions
    }

    #[must_use]
    pub fn mcp_bindings(&self) -> &[CredentialMcpBindingSnapshot] {
        &self.mcp_bindings
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CredentialGrantRegistrySnapshotWire {
    definitions: Vec<CredentialGrantDefinitionSettings>,
    mcp_bindings: Vec<CredentialMcpBindingSnapshotWire>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CredentialMcpBindingSnapshotWire {
    server: String,
    grants: Vec<String>,
}

impl<'de> Deserialize<'de> for CredentialGrantRegistrySnapshot {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = CredentialGrantRegistrySnapshotWire::deserialize(deserializer)?;
        let definitions = wire
            .definitions
            .into_iter()
            .map(CredentialGrantDefinition::try_from)
            .collect::<Result<Vec<_>, _>>()
            .map_err(de::Error::custom)?;
        let mcp_bindings = wire
            .mcp_bindings
            .into_iter()
            .map(|binding| {
                CredentialMcpBindingSnapshot::new(
                    McpServerName::parse(binding.server)?,
                    binding
                        .grants
                        .into_iter()
                        .map(CredentialGrantId::parse)
                        .collect::<Result<Vec<_>, _>>()?,
                )
            })
            .collect::<Result<Vec<_>, CredentialConfigError>>()
            .map_err(de::Error::custom)?;
        Self::new(definitions, mcp_bindings).map_err(de::Error::custom)
    }
}

pub struct ResolvedCredentialGrant {
    environment: EnvironmentVariableName,
    secret: String,
}

impl ResolvedCredentialGrant {
    pub(crate) fn new(
        environment: EnvironmentVariableName,
        secret: String,
    ) -> Result<Self, CredentialResolutionError> {
        if secret.is_empty() {
            return Err(CredentialResolutionError::EmptySecret);
        }
        Ok(Self {
            environment,
            secret,
        })
    }

    #[must_use]
    pub fn environment(&self) -> &EnvironmentVariableName {
        &self.environment
    }

    pub(crate) fn insert_into(
        self,
        environment: &mut BTreeMap<OsString, OsString>,
    ) -> Result<(), CredentialResolutionError> {
        let output = OsString::from(self.environment.as_str());
        if contains_environment_key(environment, &output) {
            return Err(CredentialResolutionError::EnvironmentCollision);
        }
        environment.insert(output, OsString::from(self.secret));
        Ok(())
    }
}

impl Debug for ResolvedCredentialGrant {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedCredentialGrant")
            .field("environment", &self.environment)
            .field("has_secret", &!self.secret.is_empty())
            .finish()
    }
}

pub struct CredentialGrantRegistry {
    snapshot: Arc<CredentialGrantRegistrySnapshot>,
    by_id: BTreeMap<CredentialGrantId, usize>,
}

impl CredentialGrantRegistry {
    pub fn build(
        definitions: &[CredentialGrantDefinitionSettings],
        mcp_servers: &[McpServerConfig],
    ) -> Result<Self, CredentialConfigError> {
        let definitions = definitions
            .iter()
            .cloned()
            .map(CredentialGrantDefinition::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let bindings = mcp_servers
            .iter()
            .filter(|server| !server.disabled && server.transport == McpTransport::Stdio)
            .map(|server| {
                CredentialMcpBindingSnapshot::new(
                    McpServerName::parse(server.name.clone())?,
                    server
                        .credential_grants
                        .iter()
                        .cloned()
                        .map(CredentialGrantId::parse)
                        .collect::<Result<Vec<_>, _>>()?,
                )
            })
            .collect::<Result<Vec<_>, CredentialConfigError>>()?;
        Self::from_snapshot(CredentialGrantRegistrySnapshot {
            definitions: definitions.into_boxed_slice(),
            mcp_bindings: bindings.into_boxed_slice(),
        })
    }

    pub fn from_snapshot(
        snapshot: CredentialGrantRegistrySnapshot,
    ) -> Result<Self, CredentialConfigError> {
        let mut definitions = snapshot.definitions.into_vec();
        definitions.sort_by(|left, right| left.id.cmp(&right.id));
        let mut by_id = BTreeMap::new();
        let mut outputs = BTreeSet::new();
        for (index, definition) in definitions.iter().enumerate() {
            if by_id.insert(definition.id.clone(), index).is_some() {
                return Err(CredentialConfigError::DuplicateGrantId);
            }
            if !outputs.insert((
                definition.audience.clone(),
                definition.output.environment.clone(),
            )) {
                return Err(CredentialConfigError::DuplicateAudienceOutput);
            }
            if definition.audience.spawn_family() == Some(SpawnFamily::McpStdio) {
                return Err(CredentialConfigError::InvalidMcpAudience);
            }
        }

        let mut bindings = snapshot.mcp_bindings.into_vec();
        bindings.sort_by(|left, right| left.server.cmp(&right.server));
        if bindings
            .windows(2)
            .any(|pair| pair[0].server == pair[1].server)
        {
            return Err(CredentialConfigError::UnknownGrantReference);
        }
        let mut referenced_mcp_grants = BTreeSet::new();
        for binding in &bindings {
            for id in &binding.grants {
                let Some(index) = by_id.get(id).copied() else {
                    return Err(CredentialConfigError::UnknownGrantReference);
                };
                let definition = &definitions[index];
                if definition.audience.mcp_server() != Some(&binding.server) {
                    return Err(CredentialConfigError::AudienceMismatch);
                }
                if !referenced_mcp_grants.insert(id.clone()) {
                    return Err(CredentialConfigError::UnknownGrantReference);
                }
            }
        }
        for definition in &definitions {
            if definition.audience.mcp_server().is_some()
                && !referenced_mcp_grants.contains(&definition.id)
            {
                return Err(CredentialConfigError::UnknownGrantReference);
            }
        }

        Ok(Self {
            snapshot: Arc::new(CredentialGrantRegistrySnapshot {
                definitions: definitions.into_boxed_slice(),
                mcp_bindings: bindings.into_boxed_slice(),
            }),
            by_id,
        })
    }

    #[must_use]
    pub fn snapshot(&self) -> &CredentialGrantRegistrySnapshot {
        &self.snapshot
    }

    pub(crate) fn validate_policy(
        &self,
        policy: &CredentialPolicy,
    ) -> Result<(), CredentialConfigError> {
        if policy
            .allowed_grants()
            .iter()
            .all(|id| self.by_id.contains_key(id))
        {
            Ok(())
        } else {
            Err(CredentialConfigError::UnknownGrantReference)
        }
    }

    pub async fn resolve_for_spawn(
        &self,
        id: &CredentialGrantId,
        audience: &CredentialAudience,
        policy: &CredentialPolicy,
        auth: &AuthManager,
        oauth_http: pi_ai::SharedHttpExecutor,
    ) -> Result<ResolvedCredentialGrant, CredentialResolutionError> {
        let index = self
            .by_id
            .get(id)
            .copied()
            .ok_or(CredentialResolutionError::GrantUnavailable)?;
        let definition = &self.snapshot.definitions[index];
        if &definition.audience != audience {
            return Err(CredentialResolutionError::AudienceMismatch);
        }
        if !policy.allowed_grants().contains(id) {
            return Err(CredentialResolutionError::PolicyDenied);
        }
        let credential = auth
            .resolve_stored_credential_exact(
                definition.provider.as_str(),
                definition.scope.as_ref().map(CredentialScopeName::as_str),
                oauth_http,
            )
            .await
            .map_err(|failure| match failure {
                crate::CredentialGrantAuthError::Lookup => CredentialResolutionError::LookupFailed,
                crate::CredentialGrantAuthError::Refresh => {
                    CredentialResolutionError::RefreshFailed
                }
            })?
            .ok_or(CredentialResolutionError::GrantUnavailable)?;
        let secret = match (definition.output.credential_type, credential) {
            (CredentialGrantOutputType::ApiKey, Credential::ApiKey { key, env, .. }) => {
                resolve_api_key(key.as_deref(), &env)?
            }
            (CredentialGrantOutputType::OAuthAccessToken, Credential::OAuth { access, .. }) => {
                access
            }
            _ => return Err(CredentialResolutionError::GrantUnavailable),
        };
        ResolvedCredentialGrant::new(definition.output.environment.clone(), secret)
    }
}

fn validate_label(
    field: &'static str,
    value: String,
    maximum_bytes: usize,
) -> Result<String, CredentialConfigError> {
    if value.is_empty() || value.len() > maximum_bytes || value.trim() != value {
        return Err(CredentialConfigError::InvalidField {
            field,
            reason: format!("must be nonempty, trimmed, and at most {maximum_bytes} bytes"),
        });
    }
    if value.chars().any(char::is_control) {
        return Err(CredentialConfigError::InvalidField {
            field,
            reason: "must not contain control characters".to_owned(),
        });
    }
    Ok(value)
}

fn resolve_api_key(
    key: Option<&str>,
    credential_environment: &std::collections::HashMap<String, String>,
) -> Result<String, CredentialResolutionError> {
    let Some(key) = key else {
        return Err(CredentialResolutionError::EmptySecret);
    };
    let mut resolved = String::with_capacity(key.len());
    let mut remainder = key;
    while let Some(start) = remainder.find("${") {
        resolved.push_str(&remainder[..start]);
        let variable = &remainder[start + 2..];
        let Some(end) = variable.find('}') else {
            return Err(CredentialResolutionError::LookupFailed);
        };
        let name = &variable[..end];
        let value = credential_environment
            .get(name)
            .ok_or(CredentialResolutionError::LookupFailed)?;
        resolved.push_str(value);
        remainder = &variable[end + 1..];
    }
    resolved.push_str(remainder);
    if resolved.is_empty() {
        Err(CredentialResolutionError::EmptySecret)
    } else {
        Ok(resolved)
    }
}

fn contains_environment_key(
    environment: &BTreeMap<OsString, OsString>,
    candidate: &OsString,
) -> bool {
    #[cfg(windows)]
    {
        let folded = candidate.to_string_lossy().to_ascii_uppercase();
        environment
            .keys()
            .any(|key| key.to_string_lossy().to_ascii_uppercase() == folded)
    }
    #[cfg(not(windows))]
    {
        environment.contains_key(candidate)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use anyhow::anyhow;
    use futures_util::future::BoxFuture;

    use super::*;

    struct UnexpectedHttp;

    impl pi_ai::HttpExecutor for UnexpectedHttp {
        fn execute<'a>(
            &'a self,
            _request: pi_ai::HttpRequest,
        ) -> BoxFuture<'a, anyhow::Result<pi_ai::HttpResponse>> {
            Box::pin(async { Err(anyhow!("credential test must not perform HTTP")) })
        }
    }

    #[tokio::test]
    async fn resolve_for_spawn_returns_exact_nonserializable_output() {
        let directory = tempfile::tempdir().expect("credential test directory");
        let auth =
            AuthManager::new(directory.path().join("auth.json")).expect("credential auth manager");
        auth.storage()
            .modify("d3-provider", Some("d3-scope"), |_| async {
                Ok(Some(Credential::ApiKey {
                    key: Some("prefix-${TOKEN}".to_owned()),
                    env: HashMap::from([("TOKEN".to_owned(), "exact-secret".to_owned())]),
                    extra: serde_json::Map::new(),
                }))
            })
            .await
            .expect("store exact credential");

        let grant_id = CredentialGrantId::parse("d3-grant".to_owned()).expect("grant id");
        let audience = CredentialAudience::new_spawn_family(SpawnFamily::ManagedPipeProcess)
            .expect("audience");
        let definition = CredentialGrantDefinition::new(
            grant_id.clone(),
            ProviderId::parse("d3-provider".to_owned()).expect("provider"),
            Some(CredentialScopeName::parse("d3-scope".to_owned()).expect("scope")),
            audience.clone(),
            CredentialGrantOutputConfig::api_key(
                EnvironmentVariableName::parse("D3_EXACT_TOKEN".to_owned()).expect("environment"),
            )
            .expect("output"),
        )
        .expect("definition");
        let registry = CredentialGrantRegistry::from_snapshot(
            CredentialGrantRegistrySnapshot::new(vec![definition], Vec::new()).expect("snapshot"),
        )
        .expect("registry");
        let policy = CredentialPolicy::new(vec![grant_id.clone()]).expect("credential policy");
        let resolved = registry
            .resolve_for_spawn(
                &grant_id,
                &audience,
                &policy,
                &auth,
                Arc::new(UnexpectedHttp),
            )
            .await
            .expect("resolve exact credential");

        assert_eq!(resolved.environment().as_str(), "D3_EXACT_TOKEN");
        let debug = format!("{resolved:?}");
        assert!(debug.contains("has_secret: true"));
        assert!(!debug.contains("prefix-exact-secret"));
        let mut environment = BTreeMap::new();
        resolved
            .insert_into(&mut environment)
            .expect("consume credential");
        assert_eq!(
            environment.get(&OsString::from("D3_EXACT_TOKEN")),
            Some(&OsString::from("prefix-exact-secret")),
        );
        let snapshot =
            serde_json::to_string(registry.snapshot()).expect("serialize non-secret snapshot");
        assert!(snapshot.contains("D3_EXACT_TOKEN"));
        assert!(!snapshot.contains("exact-secret"));
    }

    #[test]
    fn resolution_errors_are_complete_and_redacted() {
        let errors = [
            CredentialResolutionError::GrantUnavailable,
            CredentialResolutionError::AudienceMismatch,
            CredentialResolutionError::PolicyDenied,
            CredentialResolutionError::LookupFailed,
            CredentialResolutionError::RefreshFailed,
            CredentialResolutionError::EmptySecret,
            CredentialResolutionError::EnvironmentCollision,
        ];
        for error in errors {
            let display = error.to_string();
            let debug = format!("{error:?}");
            assert!(!display.contains("token"));
            assert!(!display.contains("secret="));
            assert!(!debug.contains("secret="));
        }
    }
}
