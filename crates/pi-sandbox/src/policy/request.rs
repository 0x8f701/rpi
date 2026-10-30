use crate::error::PolicyValidationError;
use crate::platform::PlatformFacts;
use crate::policy::{
    AbsolutePath, CredentialPolicy, FileSystemPolicy, HostHttpPolicy, HostPlatform, NetworkPolicy,
    PolicyRevision, PolicySnapshot, ResourceLimits, SandboxMode, SandboxPolicy, WritableRoot,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionScope {
    session_id: Box<str>,
    workspace_roots: Box<[AbsolutePath]>,
    agent_dir: AbsolutePath,
    session_dir: AbsolutePath,
}

impl SessionScope {
    pub fn new(
        session_id: String,
        workspace_roots: Vec<AbsolutePath>,
        agent_dir: AbsolutePath,
        session_dir: AbsolutePath,
    ) -> Result<Self, PolicyValidationError> {
        if session_id.is_empty() || session_id.chars().any(char::is_control) {
            return Err(PolicyValidationError::InvalidField {
                field: "sessionId",
                reason: "must be nonempty and control-free",
            });
        }
        if workspace_roots.is_empty() {
            return Err(PolicyValidationError::InvalidField {
                field: "workspaceRoots",
                reason: "at least one workspace root is required",
            });
        }
        let mut unique = workspace_roots.clone();
        unique.sort();
        unique.dedup();
        if unique.len() != workspace_roots.len() {
            return Err(PolicyValidationError::DuplicateEntry {
                field: "workspaceRoots",
            });
        }
        Ok(Self {
            session_id: session_id.into_boxed_str(),
            workspace_roots: workspace_roots.into_boxed_slice(),
            agent_dir,
            session_dir,
        })
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    #[must_use]
    pub fn workspace_roots(&self) -> &[AbsolutePath] {
        &self.workspace_roots
    }

    #[must_use]
    pub fn agent_dir(&self) -> &AbsolutePath {
        &self.agent_dir
    }

    #[must_use]
    pub fn session_dir(&self) -> &AbsolutePath {
        &self.session_dir
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestedPolicy {
    mode: SandboxMode,
    network: NetworkPolicy,
    read_only_roots: Box<[AbsolutePath]>,
    writable_roots: Box<[WritableRoot]>,
    denied_paths: Box<[AbsolutePath]>,
    host_http: HostHttpPolicy,
    credentials: CredentialPolicy,
    limits: ResourceLimits,
}

impl RequestedPolicy {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mode: SandboxMode,
        network: NetworkPolicy,
        read_only_roots: Vec<AbsolutePath>,
        writable_roots: Vec<WritableRoot>,
        denied_paths: Vec<AbsolutePath>,
        host_http: HostHttpPolicy,
        credentials: CredentialPolicy,
        limits: ResourceLimits,
    ) -> Result<Self, PolicyValidationError> {
        if matches!(network, NetworkPolicy::ProxyRouted) {
            return Err(PolicyValidationError::InvalidField {
                field: "network",
                reason: "proxy-routed child networking is unavailable",
            });
        }
        match mode {
            SandboxMode::DangerFullAccess => {
                if !read_only_roots.is_empty() {
                    return Err(PolicyValidationError::ConflictingFields {
                        left: "mode",
                        right: "readOnlyRoots",
                    });
                }
                if !writable_roots.is_empty() {
                    return Err(PolicyValidationError::ConflictingFields {
                        left: "mode",
                        right: "writableRoots",
                    });
                }
                if !denied_paths.is_empty() {
                    return Err(PolicyValidationError::ConflictingFields {
                        left: "mode",
                        right: "deniedPaths",
                    });
                }
                if !host_http.is_empty() {
                    return Err(PolicyValidationError::ConflictingFields {
                        left: "mode",
                        right: "hostHttp",
                    });
                }
                if limits != ResourceLimits::confined_defaults() {
                    return Err(PolicyValidationError::ConflictingFields {
                        left: "mode",
                        right: "limits",
                    });
                }
                if !matches!(network, NetworkPolicy::FullAccess) {
                    return Err(PolicyValidationError::ConflictingFields {
                        left: "mode",
                        right: "network",
                    });
                }
            }
            SandboxMode::ReadOnly => {
                if !writable_roots.is_empty() {
                    return Err(PolicyValidationError::ConflictingFields {
                        left: "mode",
                        right: "writableRoots",
                    });
                }
            }
            SandboxMode::WorkspaceWrite => {}
        }
        let filesystem = FileSystemPolicy::new(
            read_only_roots.clone(),
            writable_roots.clone(),
            denied_paths.clone(),
        )?;
        let _ = filesystem;
        Ok(Self {
            mode,
            network,
            read_only_roots: read_only_roots.into_boxed_slice(),
            writable_roots: writable_roots.into_boxed_slice(),
            denied_paths: denied_paths.into_boxed_slice(),
            host_http,
            credentials,
            limits,
        })
    }
}

pub fn resolve_policy_snapshot(
    revision: PolicyRevision,
    platform: HostPlatform,
    scope: &SessionScope,
    requested: RequestedPolicy,
    platform_facts: &PlatformFacts,
    revision_home: AbsolutePath,
    revision_tmp: AbsolutePath,
    revision_scratch: AbsolutePath,
    private_roots: Vec<AbsolutePath>,
) -> Result<PolicySnapshot, PolicyValidationError> {
    if platform_facts.platform() != platform {
        return Err(PolicyValidationError::InvalidField {
            field: "platform",
            reason: "sealed platform facts do not match the requested platform",
        });
    }
    if matches!(requested.mode, SandboxMode::DangerFullAccess) {
        return PolicySnapshot::new(
            revision,
            platform,
            SandboxPolicy::danger_full_access(requested.credentials)?,
        );
    }
    if matches!(requested.mode, SandboxMode::WorkspaceWrite)
        && scope.workspace_roots().iter().any(|root| {
            scope.agent_dir().as_path().starts_with(root.as_path())
                && scope
                    .agent_dir()
                    .as_path()
                    .strip_prefix(root.as_path())
                    .is_ok_and(|relative| relative.as_os_str().is_empty())
        })
    {
        return Err(PolicyValidationError::InvalidField {
            field: "agentDir",
            reason: "must not be the writable workspace root itself",
        });
    }

    let agent_overlaps_generated_write =
        agent_overlaps_generated_write(scope, &revision_scratch, requested.mode);
    let mut read_only = Vec::new();
    read_only.extend(platform_facts.policy_runtime_roots().iter().cloned());
    if !agent_overlaps_generated_write {
        read_only.push(scope.agent_dir().clone());
    }
    if matches!(requested.mode, SandboxMode::ReadOnly) {
        read_only.extend(scope.workspace_roots().iter().cloned());
    }
    read_only.extend(requested.read_only_roots.iter().cloned());

    let mut writable = Vec::new();
    if matches!(requested.mode, SandboxMode::WorkspaceWrite) {
        for root in scope.workspace_roots() {
            let protected = required_workspace_protected_descendants(scope, platform_facts, root)?;
            writable.push(WritableRoot::new(root.clone(), protected)?);
        }
    }
    let sealed_scratch = if matches!(requested.mode, SandboxMode::ReadOnly) {
        Some(revision_scratch.clone())
    } else {
        if !writable.iter().any(|root| {
            revision_scratch
                .as_path()
                .starts_with(root.root().as_path())
        }) {
            writable.push(WritableRoot::new(revision_scratch.clone(), Vec::new())?);
        }
        None
    };
    for revision_projection in [&revision_home, &revision_tmp] {
        if !writable.iter().any(|root| {
            revision_projection
                .as_path()
                .starts_with(root.root().as_path())
        }) {
            read_only.push(revision_projection.clone());
        }
    }
    if matches!(requested.mode, SandboxMode::WorkspaceWrite) {
        for mandatory in platform_facts.mandatory_protected_roots() {
            if !writable
                .iter()
                .any(|existing| existing.root() == mandatory.root())
            {
                writable.push(mandatory.clone());
            }
        }
    }

    let mut denied = requested.denied_paths.to_vec();
    denied.extend(platform_facts.mandatory_denied_paths().iter().cloned());

    let filesystem = FileSystemPolicy::with_private_roots(
        read_only,
        writable,
        denied,
        sealed_scratch,
        private_roots,
    )?;
    let policy = match requested.mode {
        SandboxMode::ReadOnly => SandboxPolicy::resolved_read_only(
            filesystem,
            requested.network,
            requested.host_http,
            requested.credentials,
            requested.limits,
        )?,
        SandboxMode::WorkspaceWrite => SandboxPolicy::workspace_write(
            filesystem,
            requested.network,
            requested.host_http,
            requested.credentials,
            requested.limits,
        )?,
        SandboxMode::DangerFullAccess => unreachable!(),
    };
    PolicySnapshot::new(revision, platform, policy)
}

#[must_use]
pub fn agent_overlaps_generated_write(
    scope: &SessionScope,
    revision_scratch: &AbsolutePath,
    mode: SandboxMode,
) -> bool {
    !matches!(mode, SandboxMode::DangerFullAccess)
        && (scope.workspace_roots().iter().any(|root| {
            root.as_path().starts_with(scope.agent_dir().as_path())
                || scope.agent_dir().as_path().starts_with(root.as_path())
        }) || revision_scratch
            .as_path()
            .starts_with(scope.agent_dir().as_path())
            || scope
                .agent_dir()
                .as_path()
                .starts_with(revision_scratch.as_path()))
}

pub fn required_workspace_protected_descendants(
    scope: &SessionScope,
    facts: &PlatformFacts,
    workspace: &AbsolutePath,
) -> Result<Vec<super::RelativePath>, PolicyValidationError> {
    let mut protected = facts
        .mandatory_protected_roots()
        .iter()
        .find(|candidate| candidate.root() == workspace)
        .map(|candidate| candidate.protected_descendants().to_vec())
        .unwrap_or_default();
    if let Ok(relative_session) = scope
        .session_dir()
        .as_path()
        .strip_prefix(workspace.as_path())
        && !relative_session.as_os_str().is_empty()
    {
        protected.push(super::RelativePath::parse(relative_session.to_path_buf())?);
    }
    if let Ok(relative_agent) = scope
        .agent_dir()
        .as_path()
        .strip_prefix(workspace.as_path())
        && !relative_agent.as_os_str().is_empty()
    {
        protected.push(super::RelativePath::parse(relative_agent.to_path_buf())?);
    }
    protected.sort_unstable();
    protected.dedup();
    Ok(protected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{PlatformFactInputs, derive_platform_facts};

    #[test]
    fn workspace_write_includes_session_denial_exactly_once() {
        let root = tempfile::tempdir().expect("policy fixture root");
        let workspace = root.path().join("workspace");
        let agent = root.path().join("agent");
        let session = root.path().join("session");
        let revision = root.path().join("revision");
        for path in [&workspace, &agent, &session, &revision] {
            std::fs::create_dir(path).expect("policy fixture directory");
        }
        let workspace = AbsolutePath::parse(workspace).expect("workspace path");
        let agent = AbsolutePath::parse(agent).expect("agent path");
        let session = AbsolutePath::parse(session).expect("session path");
        let inputs = PlatformFactInputs::new(
            "policy-fixture".to_owned(),
            vec![workspace.clone()],
            agent.clone(),
            session.clone(),
        )
        .expect("platform fact inputs");
        let facts = derive_platform_facts(inputs).expect("platform facts");
        let scope = SessionScope::new(
            "policy-fixture".to_owned(),
            vec![workspace],
            agent,
            session.clone(),
        )
        .expect("session scope");
        let requested = RequestedPolicy::new(
            SandboxMode::WorkspaceWrite,
            NetworkPolicy::None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HostHttpPolicy::new(Vec::new()).expect("empty host HTTP policy"),
            CredentialPolicy::new(Vec::new()).expect("empty credential policy"),
            ResourceLimits::confined_defaults(),
        )
        .expect("requested policy");
        let revision = AbsolutePath::parse(revision).expect("revision path");
        let snapshot = resolve_policy_snapshot(
            PolicyRevision::new(1).expect("revision"),
            facts.platform(),
            &scope,
            requested,
            &facts,
            AbsolutePath::parse(revision.as_path().join("home")).expect("revision home"),
            AbsolutePath::parse(revision.as_path().join("tmp")).expect("revision tmp"),
            AbsolutePath::parse(revision.as_path().join("scratch")).expect("revision scratch"),
            Vec::new(),
        )
        .expect("workspace-write policy");
        let filesystem = snapshot
            .policy()
            .filesystem()
            .expect("confined filesystem policy");
        assert_eq!(
            filesystem
                .denied_paths()
                .iter()
                .filter(|path| *path == &session)
                .count(),
            1,
            "the sealed session authority must be denied exactly once",
        );
    }

    #[test]
    fn workspace_write_contains_agent_store_without_root_overlap() {
        let root = tempfile::tempdir().expect("policy fixture root");
        let workspace = root.path().join("workspace");
        let agent = root.path().join("agent");
        let session = agent.join("sessions/service-cwd");
        let revision = session.join("sandbox/revision-1");
        for path in [&workspace, &agent, &session, &revision] {
            std::fs::create_dir_all(path).expect("policy fixture directory");
        }
        std::fs::write(agent.join("auth.json"), "{}").expect("agent credentials");
        std::fs::write(agent.join("settings.json"), "{}").expect("agent settings");
        let workspace = AbsolutePath::parse(workspace).expect("workspace path");
        let agent = AbsolutePath::parse(agent).expect("agent path");
        let session = AbsolutePath::parse(session).expect("session path");
        let facts = derive_platform_facts(
            PlatformFactInputs::new(
                "service-cwd".to_owned(),
                vec![workspace.clone()],
                agent.clone(),
                session.clone(),
            )
            .expect("platform fact inputs"),
        )
        .expect("platform facts");
        let scope = SessionScope::new(
            "service-cwd".to_owned(),
            vec![workspace.clone()],
            agent.clone(),
            session,
        )
        .expect("session scope");
        let requested = RequestedPolicy::new(
            SandboxMode::WorkspaceWrite,
            NetworkPolicy::None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HostHttpPolicy::new(Vec::new()).expect("empty host HTTP policy"),
            CredentialPolicy::new(Vec::new()).expect("empty credential policy"),
            ResourceLimits::confined_defaults(),
        )
        .expect("requested policy");
        let revision = AbsolutePath::parse(revision).expect("revision path");
        let snapshot = resolve_policy_snapshot(
            PolicyRevision::new(1).expect("revision"),
            facts.platform(),
            &scope,
            requested,
            &facts,
            AbsolutePath::parse(revision.as_path().join("home")).expect("revision home"),
            AbsolutePath::parse(revision.as_path().join("tmp")).expect("revision tmp"),
            AbsolutePath::parse(revision.as_path().join("scratch")).expect("revision scratch"),
            Vec::new(),
        )
        .expect("workspace-write policy");
        let filesystem = snapshot
            .policy()
            .filesystem()
            .expect("confined filesystem policy");
        assert!(!filesystem.read_only_roots().contains(&agent));
        let writable = filesystem
            .writable_roots()
            .iter()
            .find(|candidate| candidate.root() == &workspace)
            .expect("workspace root");
        assert!(
            filesystem.denied_paths().contains(
                &AbsolutePath::parse(agent.as_path().join("auth.json")).expect("auth path")
            )
        );
        assert!(filesystem.denied_paths().contains(&scope.session_dir));
    }

    #[test]
    fn workspace_write_private_agent_store_overlays_the_read_only_grant() {
        let root = tempfile::tempdir().expect("policy fixture root");
        let workspace = root.path().join("workspace");
        let agent = root.path().join("agent");
        let session = root.path().join("session");
        let revision = session.join("sandbox/1");
        for path in [&workspace, &agent, &session, &revision] {
            std::fs::create_dir_all(path).expect("policy fixture directory");
        }
        let workspace = AbsolutePath::parse(workspace).expect("workspace path");
        let agent = AbsolutePath::parse(agent).expect("agent path");
        let session = AbsolutePath::parse(session).expect("session path");
        let facts = derive_platform_facts(
            PlatformFactInputs::new(
                "private-agent".to_owned(),
                vec![workspace.clone()],
                agent.clone(),
                session.clone(),
            )
            .expect("platform fact inputs"),
        )
        .expect("platform facts");
        let scope = SessionScope::new(
            "private-agent".to_owned(),
            vec![workspace.clone()],
            agent.clone(),
            session,
        )
        .expect("session scope");
        let requested = RequestedPolicy::new(
            SandboxMode::WorkspaceWrite,
            NetworkPolicy::None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HostHttpPolicy::new(Vec::new()).expect("empty host HTTP policy"),
            CredentialPolicy::new(Vec::new()).expect("empty credential policy"),
            ResourceLimits::confined_defaults(),
        )
        .expect("requested policy");
        let revision = AbsolutePath::parse(revision).expect("revision path");
        let snapshot = resolve_policy_snapshot(
            PolicyRevision::new(1).expect("revision"),
            facts.platform(),
            &scope,
            requested,
            &facts,
            AbsolutePath::parse(revision.as_path().join("home")).expect("revision home"),
            AbsolutePath::parse(revision.as_path().join("tmp")).expect("revision tmp"),
            AbsolutePath::parse(revision.as_path().join("scratch")).expect("revision scratch"),
            vec![agent.clone()],
        )
        .expect("private agent overlay must resolve");
        let filesystem = snapshot
            .policy()
            .filesystem()
            .expect("confined filesystem policy");
        assert!(filesystem.private_roots().contains(&agent));
        assert!(!filesystem.read_only_roots().contains(&agent));
        assert!(
            filesystem
                .writable_roots()
                .iter()
                .any(|candidate| candidate.root() == &workspace)
        );
    }


    #[test]
    fn workspace_write_configured_session_inside_workspace_stays_denied() {
        let root = tempfile::tempdir().expect("policy fixture root");
        let workspace = root.path().join("workspace");
        let agent = root.path().join("agent");
        let session = workspace.join(".sessions");
        let sibling = session.join("other.jsonl");
        let revision = session.join("sandbox/1");
        for path in [&workspace, &agent, &session, &revision] {
            std::fs::create_dir_all(path).expect("policy fixture directory");
        }
        std::fs::write(&sibling, "sibling").expect("sibling session");
        let workspace = AbsolutePath::parse(workspace).expect("workspace");
        let agent = AbsolutePath::parse(agent).expect("agent");
        let session = AbsolutePath::parse(session).expect("session");
        let facts = derive_platform_facts(
            PlatformFactInputs::new(
                "configured-session".to_owned(),
                vec![workspace.clone()],
                agent,
                session.clone(),
            )
            .expect("inputs"),
        )
        .expect("facts");
        let scope = SessionScope::new(
            "configured-session".to_owned(),
            vec![workspace.clone()],
            AbsolutePath::parse(root.path().join("agent")).expect("agent"),
            session.clone(),
        )
        .expect("scope");
        let requested = RequestedPolicy::new(
            SandboxMode::WorkspaceWrite,
            NetworkPolicy::None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HostHttpPolicy::new(Vec::new()).expect("http"),
            CredentialPolicy::new(Vec::new()).expect("credentials"),
            ResourceLimits::confined_defaults(),
        )
        .expect("requested");
        let snapshot = resolve_policy_snapshot(
            PolicyRevision::new(1).expect("revision"),
            facts.platform(),
            &scope,
            requested,
            &facts,
            AbsolutePath::parse(revision.join("home")).expect("home"),
            AbsolutePath::parse(revision.join("tmp")).expect("tmp"),
            AbsolutePath::parse(revision.join("scratch")).expect("scratch"),
            Vec::new(),
        )
        .expect("workspace-write policy");
        let filesystem = snapshot.policy().filesystem().expect("filesystem");
        let writable = filesystem
            .writable_roots()
            .iter()
            .find(|root| root.root() == &workspace)
            .expect("workspace authority");
        assert!(filesystem.denied_paths().contains(&session));
        assert!(
            writable
                .protected_descendants()
                .iter()
                .any(|path| { path.as_path() == std::path::Path::new(".sessions") })
        );
        assert!(filesystem.denied_paths().iter().any(|denied| {
            AbsolutePath::parse(sibling.clone())
                .expect("sibling")
                .as_path()
                .starts_with(denied.as_path())
        }));
    }

    #[test]
    fn workspace_write_protects_agent_store_inside_workspace() {
        let root = tempfile::tempdir().expect("policy fixture root");
        let workspace = root.path().join("workspace");
        let agent = workspace.join(".pi/agent");
        let session = agent.join("sessions/current");
        let revision = session.join("sandbox/1");
        for path in [&workspace, &agent, &session, &revision] {
            std::fs::create_dir_all(path).expect("policy fixture directory");
        }
        let workspace = AbsolutePath::parse(workspace).expect("workspace path");
        let agent = AbsolutePath::parse(agent).expect("agent path");
        let session = AbsolutePath::parse(session).expect("session path");
        let facts = derive_platform_facts(
            PlatformFactInputs::new(
                "agent-in-workspace".to_owned(),
                vec![workspace.clone()],
                agent.clone(),
                session.clone(),
            )
            .expect("platform inputs"),
        )
        .expect("platform facts");
        let scope = SessionScope::new(
            "agent-in-workspace".to_owned(),
            vec![workspace.clone()],
            agent.clone(),
            session,
        )
        .expect("scope");
        let requested = RequestedPolicy::new(
            SandboxMode::WorkspaceWrite,
            NetworkPolicy::None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HostHttpPolicy::new(Vec::new()).expect("http"),
            CredentialPolicy::new(Vec::new()).expect("credentials"),
            ResourceLimits::confined_defaults(),
        )
        .expect("requested policy");
        let snapshot = resolve_policy_snapshot(
            PolicyRevision::new(1).expect("revision"),
            facts.platform(),
            &scope,
            requested,
            &facts,
            AbsolutePath::parse(revision.join("home")).expect("home"),
            AbsolutePath::parse(revision.join("tmp")).expect("tmp"),
            AbsolutePath::parse(revision.join("scratch")).expect("scratch"),
            Vec::new(),
        )
        .expect("agent store inside writable workspace is protected");
        let filesystem = snapshot.policy().filesystem().expect("filesystem");
        let writable = filesystem
            .writable_roots()
            .iter()
            .find(|root| root.root() == &workspace)
            .expect("workspace authority");
        assert!(
            writable
                .protected_descendants()
                .iter()
                .any(|path| { path.as_path() == std::path::Path::new(".pi/agent") })
        );
    }

    #[test]
    fn read_only_resolution_adds_only_sealed_scratch_write_authority() {
        let root = tempfile::tempdir().expect("policy fixture root");
        let workspace = root.path().join("workspace");
        let agent = root.path().join("agent");
        let session = root.path().join("session");
        let revision = root.path().join("revision");
        for path in [&workspace, &agent, &session, &revision] {
            std::fs::create_dir(path).expect("policy fixture directory");
        }
        let workspace = AbsolutePath::parse(workspace).expect("workspace path");
        let agent = AbsolutePath::parse(agent).expect("agent path");
        let session = AbsolutePath::parse(session).expect("session path");
        let revision_home = AbsolutePath::parse(revision.join("home")).expect("revision home");
        let revision_tmp = AbsolutePath::parse(revision.join("tmp")).expect("revision tmp");
        let revision_scratch =
            AbsolutePath::parse(revision.join("scratch")).expect("revision scratch");
        let facts = derive_platform_facts(
            PlatformFactInputs::new(
                "read-only-policy".to_owned(),
                vec![workspace.clone()],
                agent.clone(),
                session.clone(),
            )
            .expect("platform fact inputs"),
        )
        .expect("platform facts");
        let scope = SessionScope::new(
            "read-only-policy".to_owned(),
            vec![workspace.clone()],
            agent,
            session,
        )
        .expect("session scope");
        let requested = RequestedPolicy::new(
            SandboxMode::ReadOnly,
            NetworkPolicy::None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HostHttpPolicy::new(Vec::new()).expect("host HTTP policy"),
            CredentialPolicy::new(Vec::new()).expect("credential policy"),
            ResourceLimits::confined_defaults(),
        )
        .expect("requested policy");

        let snapshot = resolve_policy_snapshot(
            PolicyRevision::new(1).expect("revision"),
            facts.platform(),
            &scope,
            requested,
            &facts,
            revision_home.clone(),
            revision_tmp.clone(),
            revision_scratch.clone(),
            Vec::new(),
        )
        .expect("read-only resolved policy");
        let filesystem = snapshot.policy().filesystem().expect("filesystem policy");

        assert_eq!(snapshot.policy().mode(), SandboxMode::ReadOnly);
        assert!(filesystem.read_only_roots().contains(&workspace));
        assert!(filesystem.read_only_roots().contains(&revision_home));
        assert!(filesystem.read_only_roots().contains(&revision_tmp));
        assert!(filesystem.writable_roots().is_empty());
        assert_eq!(filesystem.sealed_scratch_root(), Some(&revision_scratch));
    }

    #[test]
    fn read_only_resolution_with_session_under_agent_reconciles_agent_root() {
        let root = tempfile::tempdir().expect("policy fixture root");
        let workspace = root.path().join("workspace");
        let agent = root.path().join("agent");
        let session = agent.join("sessions/service");
        let revision = session.join("sandbox/1");
        for path in [&workspace, &agent, &session, &revision] {
            std::fs::create_dir_all(path).expect("policy fixture directory");
        }
        let workspace = AbsolutePath::parse(workspace).expect("workspace");
        let agent = AbsolutePath::parse(agent).expect("agent");
        let session = AbsolutePath::parse(session).expect("session");
        let revision_scratch = AbsolutePath::parse(revision.join("scratch")).expect("scratch");
        let facts = derive_platform_facts(
            PlatformFactInputs::new(
                "read-only-standard".to_owned(),
                vec![workspace.clone()],
                agent.clone(),
                session.clone(),
            )
            .expect("inputs"),
        )
        .expect("facts");
        let scope = SessionScope::new(
            "read-only-standard".to_owned(),
            vec![workspace.clone()],
            agent.clone(),
            session.clone(),
        )
        .expect("scope");
        let requested = RequestedPolicy::new(
            SandboxMode::ReadOnly,
            NetworkPolicy::None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HostHttpPolicy::new(Vec::new()).expect("http"),
            CredentialPolicy::new(Vec::new()).expect("credentials"),
            ResourceLimits::confined_defaults(),
        )
        .expect("requested");
        let snapshot = resolve_policy_snapshot(
            PolicyRevision::new(1).expect("revision"),
            facts.platform(),
            &scope,
            requested,
            &facts,
            AbsolutePath::parse(revision.join("home")).expect("home"),
            AbsolutePath::parse(revision.join("tmp")).expect("tmp"),
            revision_scratch.clone(),
            Vec::new(),
        )
        .expect("read-only policy");
        let filesystem = snapshot.policy().filesystem().expect("filesystem");
        assert!(!filesystem.read_only_roots().contains(&agent));
        assert!(filesystem.read_only_roots().contains(&workspace));
        assert_eq!(filesystem.sealed_scratch_root(), Some(&revision_scratch));
        assert!(filesystem.denied_paths().contains(&session));
    }
}
