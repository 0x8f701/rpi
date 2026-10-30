use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SpawnFamily {
    ForegroundShell,
    ManagedPipeProcess,
    ManagedPtyProcess,
    ProcessExtension,
    McpStdio,
    Hook,
    LanguageServer,
    DebugAdapter,
    EvalKernel,
    NotebookKernel,
    DocumentConverter,
    VideoConverter,
    Browser,
    GithubCli,
    RepositoryHelper,
    PackageHelper,
    PluginHelper,
    HandoffHelper,
    ShareHelper,
}

impl SpawnFamily {
    pub const ALL: [Self; 19] = [
        Self::ForegroundShell,
        Self::ManagedPipeProcess,
        Self::ManagedPtyProcess,
        Self::ProcessExtension,
        Self::McpStdio,
        Self::Hook,
        Self::LanguageServer,
        Self::DebugAdapter,
        Self::EvalKernel,
        Self::NotebookKernel,
        Self::DocumentConverter,
        Self::VideoConverter,
        Self::Browser,
        Self::GithubCli,
        Self::RepositoryHelper,
        Self::PackageHelper,
        Self::PluginHelper,
        Self::HandoffHelper,
        Self::ShareHelper,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ForegroundShell => "foreground-shell",
            Self::ManagedPipeProcess => "managed-pipe-process",
            Self::ManagedPtyProcess => "managed-pty-process",
            Self::ProcessExtension => "process-extension",
            Self::McpStdio => "mcp-stdio",
            Self::Hook => "hook",
            Self::LanguageServer => "language-server",
            Self::DebugAdapter => "debug-adapter",
            Self::EvalKernel => "eval-kernel",
            Self::NotebookKernel => "notebook-kernel",
            Self::DocumentConverter => "document-converter",
            Self::VideoConverter => "video-converter",
            Self::Browser => "browser",
            Self::GithubCli => "github-cli",
            Self::RepositoryHelper => "repository-helper",
            Self::PackageHelper => "package-helper",
            Self::PluginHelper => "plugin-helper",
            Self::HandoffHelper => "handoff-helper",
            Self::ShareHelper => "share-helper",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoverageRoute {
    LinuxConfined,
    MacosConfined,
    WindowsConfined,
    LinuxDanger,
    MacosDanger,
    WindowsDanger,
}

#[must_use]
pub const fn covers(route: CoverageRoute, family: SpawnFamily) -> bool {
    match route {
        CoverageRoute::LinuxConfined | CoverageRoute::LinuxDanger | CoverageRoute::MacosDanger => {
            true
        }
        CoverageRoute::MacosConfined | CoverageRoute::WindowsDanger => {
            !matches!(family, SpawnFamily::Browser)
        }
        CoverageRoute::WindowsConfined => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_spawn_families_have_one_authoritative_route() {
        assert_eq!(SpawnFamily::ALL.len(), 19);
        let mut seen = std::collections::BTreeSet::new();
        for family in SpawnFamily::ALL {
            assert!(seen.insert(family.as_str()), "{}", family.as_str());
            let covered: usize = [
                CoverageRoute::LinuxConfined,
                CoverageRoute::MacosConfined,
                CoverageRoute::WindowsConfined,
                CoverageRoute::LinuxDanger,
                CoverageRoute::MacosDanger,
                CoverageRoute::WindowsDanger,
            ]
            .into_iter()
            .filter(|route| covers(*route, family))
            .count();
            assert!(covered >= 1, "{}", family.as_str());
            assert_eq!(covers(CoverageRoute::WindowsConfined, family), false);
            if family == SpawnFamily::Browser {
                assert!(!covers(CoverageRoute::MacosConfined, family));
                assert!(!covers(CoverageRoute::WindowsDanger, family));
                assert!(covers(CoverageRoute::LinuxConfined, family));
                assert!(covers(CoverageRoute::LinuxDanger, family));
                assert!(covers(CoverageRoute::MacosDanger, family));
            } else {
                assert!(covers(CoverageRoute::MacosConfined, family));
                assert!(covers(CoverageRoute::WindowsDanger, family));
            }
        }
    }
}
