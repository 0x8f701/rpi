//! Generation-bound sandbox policy, filesystem, HTTP, credential, and spawn
//! authority for the coding application.

mod approval;
mod environment;
pub(crate) mod generation;
mod grant_registry;
pub mod http;
pub(crate) use http::{
    BuiltinHttpEndpoints, RawSessionHttpTransport, SessionHttpAuthority, SessionHttpError,
    SessionHttpRequirements, SystemDnsResolver,
};

pub(crate) use approval::{
    ApprovedSpawnRegistry, CanonicalSpawnCall, ConsumedApprovedSpawn, RawSpawnRequest,
    SpawnAuthorization, SpawnAuthorizationError, target_fd_summary,
};
pub(crate) use environment::{
    EnvironmentConstructionError, build_confined_environment, build_raw_host_environment,
};
#[cfg(test)]
pub(crate) use generation::test_durable_policy_state;
pub use generation::{
    DurablePolicyState, DurableResumeError, DurableStateValidationError, SandboxConfigError,
    SandboxGenerationError, durable_policy_is_protected_subset_upgrade,
    durable_policy_same_authority,
};
pub(crate) use generation::{
    DurableResumeStaging, SandboxGeneration, SessionPolicyInputs, build_sandbox_generation,
    resume_durable_generation,
};
pub(crate) use grant_registry::CredentialResolutionError;
pub use grant_registry::{
    CredentialAudience, CredentialConfigError, CredentialGrantDefinition,
    CredentialGrantOutputConfig, CredentialGrantOutputType, CredentialGrantRegistry,
    CredentialGrantRegistrySnapshot, CredentialMcpBindingSnapshot, CredentialScopeName,
    McpServerName, OAuthGrantRefresh, ProviderId, ResolvedCredentialGrant,
};
