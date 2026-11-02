//! Shared test-only fixtures for pi-cli crate tests.

use pi_sandbox::{CredentialPolicy, HostPlatform, PolicyRevision, PolicySnapshot, SandboxPolicy};

/// Minimal valid durable policy state for workflow fixtures. Mirrors
/// `pi_coding::sandbox::generation::test_durable_policy_state` through
/// public constructors only (the coding crate's helper is `pub(crate)`).
pub(crate) fn test_durable_policy_state() -> pi_coding::DurablePolicyState {
    let credentials = CredentialPolicy::new(Vec::new()).expect("empty credential policy");
    let policy = SandboxPolicy::danger_full_access(credentials).expect("danger policy");
    let snapshot = PolicySnapshot::new(
        PolicyRevision::new(1).expect("policy revision"),
        current_host_platform(),
        policy,
    )
    .expect("policy snapshot");
    let grants = pi_coding::CredentialGrantRegistrySnapshot::new(Vec::new(), Vec::new())
        .expect("empty credential registry");
    pi_coding::DurablePolicyState::new(snapshot, grants).expect("durable policy state")
}

const fn current_host_platform() -> HostPlatform {
    #[cfg(target_os = "linux")]
    {
        HostPlatform::Linux
    }
    #[cfg(target_os = "macos")]
    {
        HostPlatform::Macos
    }
    #[cfg(target_os = "windows")]
    {
        HostPlatform::Windows
    }
}
