use crate::{
    AbsolutePath, HostPlatform, PolicyValidationError, SandboxError, SandboxIoOperation,
    WritableRoot,
};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

pub struct PlatformFactInputs {
    session_id: Box<str>,
    workspace_roots: Box<[AbsolutePath]>,
    agent_dir: AbsolutePath,
    session_dir: AbsolutePath,
}

impl PlatformFactInputs {
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
                reason: "must contain at least one root",
            });
        }
        let mut unique = workspace_roots.iter().collect::<Vec<_>>();
        unique.sort_unstable();
        if unique.windows(2).any(|pair| pair[0] == pair[1]) {
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

    pub(crate) fn workspace_roots(&self) -> &[AbsolutePath] {
        &self.workspace_roots
    }

    pub(crate) fn agent_dir(&self) -> &AbsolutePath {
        &self.agent_dir
    }

    pub(crate) fn session_dir(&self) -> &AbsolutePath {
        &self.session_dir
    }
}

#[derive(Clone)]
pub struct PlatformFacts {
    pub(crate) inner: PlatformFactsInner,
    mandatory_denied_paths: Box<[AbsolutePath]>,
    mandatory_protected_roots: Box<[WritableRoot]>,
}

#[derive(Clone)]
pub(crate) enum PlatformFactsInner {
    #[cfg(target_os = "linux")]
    Linux(LinuxPlatformFacts),
    #[cfg(target_os = "macos")]
    Macos(MacosPlatformFacts),
    #[cfg(target_os = "windows")]
    Windows,
}

#[cfg(unix)]
#[derive(Clone)]
pub(crate) struct UnixAccountFacts {
    effective_uid: u32,
    effective_gid: u32,
    home: AbsolutePath,
    user_name: Box<str>,
}

#[cfg(unix)]
impl UnixAccountFacts {
    pub(crate) fn new(
        effective_uid: u32,
        effective_gid: u32,
        home: AbsolutePath,
        user_name: String,
    ) -> Result<Self, PolicyValidationError> {
        if user_name.is_empty() || user_name.chars().any(char::is_control) {
            return Err(PolicyValidationError::InvalidField {
                field: "account.userName",
                reason: "must be nonempty and control-free",
            });
        }
        Ok(Self {
            effective_uid,
            effective_gid,
            home,
            user_name: user_name.into_boxed_str(),
        })
    }

    pub(crate) const fn effective_uid(&self) -> u32 {
        self.effective_uid
    }

    pub(crate) const fn effective_gid(&self) -> u32 {
        self.effective_gid
    }

    pub(crate) fn home(&self) -> &AbsolutePath {
        &self.home
    }

    pub(crate) fn user_name(&self) -> &str {
        &self.user_name
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LinuxArchitecture {
    X86_64,
    Aarch64,
}

#[cfg(target_os = "linux")]
#[derive(Clone)]
pub(crate) struct LinuxPlatformFacts {
    architecture: LinuxArchitecture,
    account: UnixAccountFacts,
    runtime_roots: Box<[AbsolutePath]>,
    policy_runtime_roots: Box<[AbsolutePath]>,
    device_paths: Box<[AbsolutePath]>,
}

#[cfg(target_os = "linux")]
impl LinuxPlatformFacts {
    pub(crate) fn new(
        architecture: LinuxArchitecture,
        account: UnixAccountFacts,
        runtime_roots: Vec<AbsolutePath>,
        policy_runtime_roots: Vec<AbsolutePath>,
        device_paths: Vec<AbsolutePath>,
    ) -> Self {
        Self {
            architecture,
            account,
            runtime_roots: runtime_roots.into_boxed_slice(),
            policy_runtime_roots: policy_runtime_roots.into_boxed_slice(),
            device_paths: device_paths.into_boxed_slice(),
        }
    }

    pub(crate) const fn architecture(&self) -> LinuxArchitecture {
        self.architecture
    }

    pub(crate) fn account(&self) -> &UnixAccountFacts {
        &self.account
    }

    pub(crate) fn runtime_roots(&self) -> &[AbsolutePath] {
        &self.runtime_roots
    }

    pub(crate) fn policy_runtime_roots(&self) -> &[AbsolutePath] {
        &self.policy_runtime_roots
    }

    pub(crate) fn device_paths(&self) -> &[AbsolutePath] {
        &self.device_paths
    }
}

#[cfg(target_os = "macos")]
#[derive(Clone)]
pub(crate) struct MacosPlatformFacts {
    account: UnixAccountFacts,
    runtime_roots: Box<[AbsolutePath]>,
    darwin_temp_root: AbsolutePath,
}

#[cfg(target_os = "macos")]
impl MacosPlatformFacts {
    pub(crate) fn new(
        account: UnixAccountFacts,
        runtime_roots: Vec<AbsolutePath>,
        darwin_temp_root: AbsolutePath,
    ) -> Self {
        Self {
            account,
            runtime_roots: runtime_roots.into_boxed_slice(),
            darwin_temp_root,
        }
    }

    pub(crate) fn account(&self) -> &UnixAccountFacts {
        &self.account
    }

    pub(crate) fn runtime_roots(&self) -> &[AbsolutePath] {
        &self.runtime_roots
    }

    pub(crate) fn darwin_temp_root(&self) -> &AbsolutePath {
        &self.darwin_temp_root
    }
}

impl PlatformFacts {
    pub fn platform(&self) -> HostPlatform {
        match &self.inner {
            #[cfg(target_os = "linux")]
            PlatformFactsInner::Linux(_) => HostPlatform::Linux,
            #[cfg(target_os = "macos")]
            PlatformFactsInner::Macos(_) => HostPlatform::Macos,
            #[cfg(target_os = "windows")]
            PlatformFactsInner::Windows => HostPlatform::Windows,
        }
    }

    pub fn account_home(&self) -> Option<&AbsolutePath> {
        match &self.inner {
            #[cfg(target_os = "linux")]
            PlatformFactsInner::Linux(facts) => Some(facts.account().home()),
            #[cfg(target_os = "macos")]
            PlatformFactsInner::Macos(facts) => Some(facts.account().home()),
            #[cfg(target_os = "windows")]
            PlatformFactsInner::Windows => None,
        }
    }

    pub fn account_user_name(&self) -> Option<&str> {
        match &self.inner {
            #[cfg(target_os = "linux")]
            PlatformFactsInner::Linux(facts) => Some(facts.account().user_name()),
            #[cfg(target_os = "macos")]
            PlatformFactsInner::Macos(facts) => Some(facts.account().user_name()),
            #[cfg(target_os = "windows")]
            PlatformFactsInner::Windows => None,
        }
    }

    pub fn runtime_roots(&self) -> &[AbsolutePath] {
        match &self.inner {
            #[cfg(target_os = "linux")]
            PlatformFactsInner::Linux(facts) => facts.runtime_roots(),
            #[cfg(target_os = "macos")]
            PlatformFactsInner::Macos(facts) => facts.runtime_roots(),
            #[cfg(target_os = "windows")]
            PlatformFactsInner::Windows => &[],
        }
    }

    /// Runtime roots as they enter the policy visibility model. Merged-usr
    /// aliases (`/bin` -> `/usr/bin`) are canonicalized because policy roots
    /// are opened symlink-free and fail closed on a symlink leaf; the raw
    /// aliases in `runtime_roots` remain the mount destinations the sandbox
    /// child sees.
    pub fn policy_runtime_roots(&self) -> &[AbsolutePath] {
        match &self.inner {
            #[cfg(target_os = "linux")]
            PlatformFactsInner::Linux(facts) => facts.policy_runtime_roots(),
            #[cfg(target_os = "macos")]
            PlatformFactsInner::Macos(facts) => facts.runtime_roots(),
            #[cfg(target_os = "windows")]
            PlatformFactsInner::Windows => &[],
        }
    }

    pub fn mandatory_denied_paths(&self) -> &[AbsolutePath] {
        &self.mandatory_denied_paths
    }

    pub fn mandatory_protected_roots(&self) -> &[WritableRoot] {
        &self.mandatory_protected_roots
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn linux(&self) -> Option<&LinuxPlatformFacts> {
        match &self.inner {
            PlatformFactsInner::Linux(facts) => Some(facts),
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn macos(&self) -> Option<&MacosPlatformFacts> {
        match &self.inner {
            PlatformFactsInner::Macos(facts) => Some(facts),
        }
    }
}

pub fn derive_platform_facts(inputs: PlatformFactInputs) -> Result<PlatformFacts, SandboxError> {
    #[cfg(target_os = "linux")]
    {
        linux::derive_linux_platform_facts(inputs)
    }
    #[cfg(target_os = "macos")]
    {
        macos::derive_macos_platform_facts(inputs)
    }
    #[cfg(target_os = "windows")]
    {
        let mandatory_denied_paths = vec![inputs.session_dir().clone()].into_boxed_slice();
        Ok(PlatformFacts {
            inner: PlatformFactsInner::Windows,
            mandatory_denied_paths,
            mandatory_protected_roots: Box::new([]),
        })
    }
}

#[cfg(unix)]
pub(crate) fn derive_mandatory_paths(
    inputs: &PlatformFactInputs,
    home: &AbsolutePath,
) -> Result<(Box<[AbsolutePath]>, Box<[WritableRoot]>), SandboxError> {
    use crate::RelativePath;

    let mut denied = vec![inputs.session_dir().clone()];
    for name in [
        "auth.json",
        "oauth.json",
        "credentials",
        "settings.json",
        "settings.toml",
    ] {
        let path = inputs.agent_dir().as_path().join(name);
        if path.exists() {
            denied.push(AbsolutePath::parse(path)?);
        }
    }
    for name in ["memory", "prompts", "skills", "themes"] {
        let path = inputs.agent_dir().as_path().join(name);
        if path.exists() {
            denied.push(AbsolutePath::parse(path)?);
        }
    }
    let sessions = inputs.agent_dir().as_path().join("sessions");
    if let Ok(entries) = std::fs::read_dir(&sessions) {
        for entry in entries {
            let path = entry
                .map_err(|error| SandboxError::from_io(SandboxIoOperation::OpenPolicyPath, &error))?
                .path();
            if path != inputs.session_dir().as_path() && path.is_dir() {
                denied.push(AbsolutePath::parse(path)?);
            }
        }
    }
    for relative in [".ssh", ".gnupg", ".aws", ".kube", ".config/gcloud"] {
        let path = home.as_path().join(relative);
        if path.exists() {
            denied.push(AbsolutePath::parse(path)?);
        }
    }
    denied.sort_unstable();
    denied.dedup();

    let mut protected = Vec::new();
    for root in inputs.workspace_roots() {
        let descendants = [".git", ".rpi", ".agents"]
            .into_iter()
            .map(|name| RelativePath::parse(name.into()))
            .collect::<Result<Vec<_>, _>>()?;
        protected.push(WritableRoot::new(root.clone(), descendants)?);
    }
    Ok((denied.into_boxed_slice(), protected.into_boxed_slice()))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::RelativePath;

    #[test]
    fn missing_metadata_dirs_are_required_protected_descendants() {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace = AbsolutePath::parse(root.path().join("workspace")).expect("workspace");
        std::fs::create_dir_all(workspace.as_path()).expect("workspace");
        let agent_dir = AbsolutePath::parse(root.path().join("agent")).expect("agent");
        let session_dir = AbsolutePath::parse(root.path().join("session")).expect("session");
        std::fs::create_dir_all(agent_dir.as_path()).expect("agent");
        std::fs::create_dir_all(session_dir.as_path()).expect("session");
        let inputs = PlatformFactInputs::new(
            "missing-agents-protected".to_owned(),
            vec![workspace.clone()],
            agent_dir,
            session_dir,
        )
        .expect("inputs");
        let home = AbsolutePath::parse(root.path().join("home")).expect("home");
        let (_denied, protected) = derive_mandatory_paths(&inputs, &home).expect("paths");
        let workspace_root = protected
            .iter()
            .find(|candidate| candidate.root() == &workspace)
            .expect("workspace protected");
        for name in [".git", ".rpi", ".agents"] {
            assert!(
                workspace_root
                    .protected_descendants()
                    .iter()
                    .any(|path| path == &RelativePath::parse(name.into()).expect("name")),
                "absent {name} must stay a required protected descendant"
            );
        }

        std::fs::create_dir_all(workspace.as_path().join(".agents")).expect("agents");
        let (_denied, protected) =
            derive_mandatory_paths(&inputs, &home).expect("paths after create");
        let workspace_root = protected
            .iter()
            .find(|candidate| candidate.root() == &workspace)
            .expect("workspace protected after create");
        assert!(
            workspace_root
                .protected_descendants()
                .iter()
                .any(|path| path == &RelativePath::parse(".agents".into()).expect("agents")),
            "existing .agents must stay a protected descendant"
        );
    }
}
