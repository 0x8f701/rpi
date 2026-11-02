use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::sync::Arc;

use pi_sandbox::{
    AbsolutePath, CanonicalTargetFdMapping, CredentialGrantId, EnvironmentVariableName,
    HostHttpClass, SandboxMode, SpawnFamily, StdioMode, TerminalSize,
};

use crate::AuthManager;
use crate::application::generation::{ApplicationGeneration, GenerationProvenance};
use crate::sandbox::{
    CredentialAudience, CredentialResolutionError, EnvironmentConstructionError, RawSpawnRequest,
    SpawnAuthorization, SpawnAuthorizationError, build_confined_environment,
    build_raw_host_environment,
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum SpawnError {
    #[error("spawn authorization failed: {0}")]
    Authorization(#[from] SpawnAuthorizationError),
    #[error("spawn credential resolution failed: {0}")]
    Credential(#[from] CredentialResolutionError),
    #[error("spawn environment construction failed: {0}")]
    Environment(#[from] EnvironmentConstructionError),
    #[error("spawn credential configuration failed: {0}")]
    CredentialConfig(#[from] crate::sandbox::CredentialConfigError),
    #[error("sandbox launch failed: {0}")]
    Sandbox(#[from] pi_sandbox::SandboxError),
    #[error("spawn request generation does not match the captured application generation")]
    GenerationMismatch,
    #[error("spawn credential resolution requires the session authentication owner")]
    MissingAuthManager,
    #[error("spawn path is not representable as a child environment value")]
    InvalidGenerationPath,
    #[error("spawn mount source is invalid: {0}")]
    MountSource(#[from] crate::fs::FsError),
}

enum SpawnAuthority {
    Published(Arc<ApplicationGeneration>),
    Candidate {
        sandbox: Arc<crate::sandbox::SandboxGeneration>,
        _provenance: Arc<GenerationProvenance>,
    },
}

pub(crate) struct AuthorizedSpawn {
    authority: SpawnAuthority,
    family: SpawnFamily,
    audience: CredentialAudience,
    argv: Vec<OsString>,
    cwd: AbsolutePath,
    environment: BTreeMap<OsString, OsString>,
    stdio: StdioMode,
    terminal_size: Option<TerminalSize>,
    timeout_ms: Option<u64>,
    canonical_target_fd_mappings: Vec<CanonicalTargetFdMapping>,
    #[cfg(unix)]
    owned_target_fd_mappings: Vec<pi_sandbox::TargetFdMapping>,
    #[cfg(unix)]
    mount_sources: Vec<pi_sandbox::MountSource>,
    #[cfg(unix)]
    retained_executable: Option<pi_sandbox::RetainedExecutable>,
    #[cfg(unix)]
    retained_working_directory: Option<pi_sandbox::RetainedWorkingDirectory>,
}

impl AuthorizedSpawn {
    pub(crate) fn sandbox(&self) -> &Arc<crate::sandbox::SandboxGeneration> {
        match &self.authority {
            SpawnAuthority::Published(generation) => generation.sandbox(),
            SpawnAuthority::Candidate { sandbox, .. } => sandbox,
        }
    }

    pub(crate) const fn family(&self) -> SpawnFamily {
        self.family
    }

    pub(crate) fn audience(&self) -> &CredentialAudience {
        &self.audience
    }

    pub(crate) fn argv(&self) -> &[OsString] {
        &self.argv
    }

    pub(crate) fn cwd(&self) -> &AbsolutePath {
        &self.cwd
    }

    pub(crate) fn environment(&self) -> &BTreeMap<OsString, OsString> {
        &self.environment
    }

    pub(crate) const fn stdio(&self) -> StdioMode {
        self.stdio
    }

    pub(crate) const fn terminal_size(&self) -> Option<TerminalSize> {
        self.terminal_size
    }

    pub(crate) const fn timeout_ms(&self) -> Option<u64> {
        self.timeout_ms
    }

    pub(crate) fn canonical_target_fd_mappings(&self) -> &[CanonicalTargetFdMapping] {
        &self.canonical_target_fd_mappings
    }

    #[cfg(unix)]
    pub(crate) fn into_prepared_request(
        self,
        stdio: pi_sandbox::SpawnStdio,
    ) -> Result<
        (
            Arc<crate::sandbox::SandboxGeneration>,
            pi_sandbox::PreparedSpawnRequest,
        ),
        pi_sandbox::SandboxError,
    > {
        if stdio.mode() != self.stdio {
            return Err(pi_sandbox::SandboxError::InvalidSpawn {
                failure: pi_sandbox::SpawnValidationError::FamilyStdioMismatch,
            });
        }
        let request = pi_sandbox::PreparedSpawnRequest::new(
            self.family,
            self.argv,
            self.cwd,
            self.environment,
            stdio,
            self.owned_target_fd_mappings,
            self.mount_sources,
            self.retained_executable,
            self.retained_working_directory,
        )?;
        if request.canonical_target_fd_mappings() != self.canonical_target_fd_mappings {
            return Err(pi_sandbox::SandboxError::InvalidSpawn {
                failure: pi_sandbox::SpawnValidationError::FamilyTargetMappingMismatch,
            });
        }
        Ok((self.authority.into_sandbox(), request))
    }
}

impl SpawnAuthority {
    fn into_sandbox(self) -> Arc<crate::sandbox::SandboxGeneration> {
        match self {
            Self::Published(generation) => generation.sandbox().clone(),
            Self::Candidate { sandbox, .. } => sandbox,
        }
    }
}

pub(crate) async fn authorize_spawn(
    generation: Arc<ApplicationGeneration>,
    raw: RawSpawnRequest,
) -> Result<AuthorizedSpawn, SpawnError> {
    authorize_spawn_with_auth(generation, None, raw).await
}

pub(crate) async fn authorize_spawn_with_auth(
    generation: Arc<ApplicationGeneration>,
    auth: Option<&AuthManager>,
    raw: RawSpawnRequest,
) -> Result<AuthorizedSpawn, SpawnError> {
    let canonical = raw.canonical_call()?;
    authorize_generation(&generation, &raw, &canonical)?;
    authorize_for_sandbox(
        SpawnAuthority::Published(generation.clone()),
        generation.sandbox(),
        auth,
        raw,
        canonical,
    )
    .await
}

pub(crate) async fn authorize_candidate_spawn(
    sandbox: Arc<crate::sandbox::SandboxGeneration>,
    provenance: Arc<GenerationProvenance>,
    auth: Option<&AuthManager>,
    raw: RawSpawnRequest,
) -> Result<AuthorizedSpawn, SpawnError> {
    if !Arc::ptr_eq(sandbox.provenance(), &provenance) {
        return Err(SpawnError::GenerationMismatch);
    }
    let canonical = raw.canonical_call()?;
    match &raw.authorization {
        SpawnAuthorization::Captured(captured) if Arc::ptr_eq(captured, &sandbox) => {}
        SpawnAuthorization::Captured(_) | SpawnAuthorization::Approved(_) => {
            return Err(SpawnError::GenerationMismatch);
        }
    }
    authorize_for_sandbox(
        SpawnAuthority::Candidate {
            sandbox: sandbox.clone(),
            _provenance: provenance,
        },
        &sandbox,
        auth,
        raw,
        canonical,
    )
    .await
}

async fn authorize_for_sandbox(
    authority: SpawnAuthority,
    sandbox: &Arc<crate::sandbox::SandboxGeneration>,
    auth: Option<&AuthManager>,
    raw: RawSpawnRequest,
    canonical: crate::sandbox::CanonicalSpawnCall,
) -> Result<AuthorizedSpawn, SpawnError> {
    #[cfg(unix)]
    let mount_sources = raw
        .mount_sources
        .into_iter()
        .map(|source| source.into_mount_source(sandbox.capabilities()))
        .collect::<Result<Vec<_>, _>>()?;
    #[cfg(unix)]
    let retained_executable = raw
        .retained_executable
        .map(|source| source.into_retained_executable(sandbox.capabilities()))
        .transpose()?;
    #[cfg(unix)]
    let retained_working_directory = raw
        .retained_working_directory
        .map(|source| source.into_retained_working_directory(sandbox.capabilities()))
        .transpose()?;
    let credentials =
        resolve_credentials(sandbox, auth, &raw.audience, &canonical.requested_grants).await?;
    let environment = child_environment(
        sandbox,
        canonical.requested_env.clone(),
        raw.requested_env.clone(),
        credentials,
        raw.stdio,
    )?;
    Ok(AuthorizedSpawn {
        authority,
        family: raw.family,
        audience: raw.audience,
        argv: raw.argv,
        cwd: canonical.cwd,
        environment,
        stdio: raw.stdio,
        terminal_size: raw.terminal_size,
        timeout_ms: raw.timeout_ms,
        canonical_target_fd_mappings: canonical.target_fd_mappings,
        #[cfg(unix)]
        owned_target_fd_mappings: raw.owned_target_fd_mappings,
        #[cfg(unix)]
        mount_sources,
        #[cfg(unix)]
        retained_executable,
        #[cfg(unix)]
        retained_working_directory,
    })
}

fn authorize_generation(
    generation: &Arc<ApplicationGeneration>,
    raw: &RawSpawnRequest,
    canonical: &crate::sandbox::CanonicalSpawnCall,
) -> Result<(), SpawnError> {
    let sandbox = match &raw.authorization {
        SpawnAuthorization::Captured(sandbox) => sandbox.clone(),
        SpawnAuthorization::Approved(token) => {
            generation
                .security()
                .approvals()
                .consume(*token, raw.family, &raw.audience, canonical, now_ms())?
                .generation
        }
    };
    if !Arc::ptr_eq(&sandbox, generation.sandbox()) {
        return Err(SpawnError::GenerationMismatch);
    }
    Ok(())
}

async fn resolve_credentials(
    sandbox: &crate::sandbox::SandboxGeneration,
    auth: Option<&AuthManager>,
    audience: &CredentialAudience,
    requested: &BTreeSet<CredentialGrantId>,
) -> Result<Vec<crate::sandbox::ResolvedCredentialGrant>, SpawnError> {
    if requested.is_empty() {
        return Ok(Vec::new());
    }
    let auth = auth.ok_or(SpawnError::MissingAuthManager)?;
    let policy = sandbox.policy_snapshot().policy().credentials();
    let oauth_http = sandbox.http().executor(HostHttpClass::AuthOauth);
    let mut resolved = Vec::with_capacity(requested.len());
    for id in requested {
        resolved.push(
            sandbox
                .credential_grants()
                .resolve_for_spawn(id, audience, policy, auth, oauth_http.clone())
                .await?,
        );
    }
    Ok(resolved)
}

fn child_environment(
    sandbox: &crate::sandbox::SandboxGeneration,
    confined_requested: BTreeMap<EnvironmentVariableName, Option<String>>,
    raw_requested: BTreeMap<OsString, Option<OsString>>,
    credentials: Vec<crate::sandbox::ResolvedCredentialGrant>,
    stdio: StdioMode,
) -> Result<BTreeMap<OsString, OsString>, SpawnError> {
    if sandbox.policy_snapshot().policy().mode() == SandboxMode::DangerFullAccess {
        return Ok(build_raw_host_environment(
            std::env::vars_os(),
            raw_requested,
            credentials,
        )?);
    }
    Ok(build_confined_environment(
        sandbox.environment_facts(),
        generation_path(sandbox.revision_home())?,
        generation_path(sandbox.revision_tmp())?,
        generation_path(sandbox.revision_scratch())?,
        confined_requested,
        credentials,
        stdio,
    )?)
}

fn generation_path(path: &AbsolutePath) -> Result<&str, SpawnError> {
    path.as_path()
        .to_str()
        .ok_or(SpawnError::InvalidGenerationPath)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}
