use std::path::PathBuf;

use pi_sandbox::{AbsolutePath, SandboxError, SandboxIoKind, SandboxIoOperation};

use crate::application::generation::ApplicationGeneration;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TrustedToolProbe {
    Browser { explicit: Option<AbsolutePath> },
    Bash,
    Git,
    GithubCli,
    PdfToText,
    Pandoc,
    LibreOffice,
    Jupyter,
    Ffmpeg,
    Python,
    LanguageServer { executable: Box<str> },
    DebugAdapter { executable: Box<str> },
}

pub(crate) fn run_trusted_tool_probe(
    generation: &ApplicationGeneration,
    probe: TrustedToolProbe,
) -> Result<AbsolutePath, SandboxError> {
    let sandbox = generation.sandbox();
    let mut first_non_missing = None;
    for candidate in candidates(probe) {
        match probe_candidate(sandbox, &candidate) {
            Ok(path) => return Ok(path),
            Err(error) if is_missing_candidate(&error) => {}
            Err(error) => {
                first_non_missing.get_or_insert(error);
            }
        }
    }
    Err(first_non_missing.unwrap_or(SandboxError::System {
        operation: SandboxIoOperation::OpenExecutable,
        kind: SandboxIoKind::NotFound,
        raw_os_error: None,
    }))
}

fn probe_candidate(
    sandbox: &crate::sandbox::SandboxGeneration,
    candidate: &AbsolutePath,
) -> Result<AbsolutePath, SandboxError> {
    #[cfg(unix)]
    {
        let opened = sandbox
            .capabilities()
            .open_managed_read_file(
                crate::fs::CapabilityRootPurpose::ToolExecutable,
                candidate.clone(),
            )
            .map_err(|error| executable_probe_error(&error))?;
        let resolved = AbsolutePath::parse(opened.resolved_path().to_path_buf()).map_err(|_| {
            SandboxError::System {
                operation: SandboxIoOperation::OpenExecutable,
                kind: SandboxIoKind::InvalidInput,
                raw_os_error: None,
            }
        })?;
        let executable = opened
            .into_file()
            .duplicate_for_spawn_executable()
            .map_err(|error| executable_probe_error(&error))?;
        if executable.canonical_executable().0 != &resolved {
            return Err(SandboxError::System {
                operation: SandboxIoOperation::OpenExecutable,
                kind: SandboxIoKind::Other,
                raw_os_error: None,
            });
        }
        executable
            .into_retained_executable(sandbox.capabilities())
            .map_err(|error| executable_probe_error(&error))?;
        Ok(resolved)
    }
    #[cfg(windows)]
    {
        let _ = (sandbox, candidate);
        Err(SandboxError::System {
            operation: SandboxIoOperation::OpenExecutable,
            kind: SandboxIoKind::NotFound,
            raw_os_error: None,
        })
    }
}

fn executable_probe_error(error: &crate::fs::FsError) -> SandboxError {
    let kind = match error {
        crate::fs::FsError::System { kind, .. } => SandboxIoKind::from(*kind),
        crate::fs::FsError::InvalidPath | crate::fs::FsError::UnsupportedEntryType => {
            SandboxIoKind::InvalidInput
        }
        crate::fs::FsError::OutsideCapability
        | crate::fs::FsError::Denied
        | crate::fs::FsError::ReadOnly => SandboxIoKind::PermissionDenied,
        crate::fs::FsError::IdentityChanged
        | crate::fs::FsError::ReadLimitExceeded { .. }
        | crate::fs::FsError::ReplacementConflict => SandboxIoKind::Other,
    };
    SandboxError::System {
        operation: SandboxIoOperation::OpenExecutable,
        kind,
        raw_os_error: None,
    }
}

fn is_missing_candidate(error: &SandboxError) -> bool {
    matches!(
        error,
        SandboxError::System {
            kind: SandboxIoKind::NotFound,
            ..
        }
    )
}

fn candidates(probe: TrustedToolProbe) -> Vec<AbsolutePath> {
    let raw = match probe {
        TrustedToolProbe::Bash => fixed(&[
            "/usr/bin/bash",
            "/bin/bash",
            "/usr/local/bin/bash",
            "/opt/homebrew/bin/bash",
        ]),
        TrustedToolProbe::Browser { explicit } => {
            let mut candidates = Vec::new();
            if let Some(explicit) = explicit {
                candidates.push(explicit);
            }
            candidates.extend(parse_candidates(browser_candidates()));
            return candidates;
        }
        TrustedToolProbe::Git => fixed(&[
            "/usr/bin/git",
            "/bin/git",
            "/usr/local/bin/git",
            "/opt/homebrew/bin/git",
        ]),
        TrustedToolProbe::GithubCli => {
            fixed(&["/usr/bin/gh", "/usr/local/bin/gh", "/opt/homebrew/bin/gh"])
        }
        TrustedToolProbe::PdfToText => fixed(&[
            "/usr/bin/pdftotext",
            "/usr/local/bin/pdftotext",
            "/opt/homebrew/bin/pdftotext",
        ]),
        TrustedToolProbe::Pandoc => fixed(&[
            "/usr/bin/pandoc",
            "/usr/local/bin/pandoc",
            "/opt/homebrew/bin/pandoc",
        ]),
        TrustedToolProbe::LibreOffice => fixed(&[
            "/usr/bin/libreoffice",
            "/usr/local/bin/libreoffice",
            "/Applications/LibreOffice.app/Contents/MacOS/soffice",
        ]),
        TrustedToolProbe::Jupyter => fixed(&[
            "/usr/bin/jupyter",
            "/usr/local/bin/jupyter",
            "/opt/homebrew/bin/jupyter",
        ]),
        TrustedToolProbe::Ffmpeg => fixed(&[
            "/usr/bin/ffmpeg",
            "/usr/local/bin/ffmpeg",
            "/opt/homebrew/bin/ffmpeg",
        ]),
        TrustedToolProbe::Python => fixed(&[
            "/usr/bin/python3",
            "/usr/local/bin/python3",
            "/opt/homebrew/bin/python3",
            "/usr/bin/python",
        ]),
        #[cfg(test)]
        TrustedToolProbe::LanguageServer { executable }
            if executable.as_ref() == "pi-lsp-sandbox-denied" =>
        {
            vec![PathBuf::from("/proc/self/fd/0")]
        }
        TrustedToolProbe::LanguageServer { executable }
        | TrustedToolProbe::DebugAdapter { executable } => named_candidates(&executable),
    };
    parse_candidates(raw)
}

fn browser_candidates() -> Vec<PathBuf> {
    fixed(&[
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/snap/bin/chromium",
        "/opt/google/chrome/chrome",
        "/usr/lib/chromium/chromium",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ])
}

fn named_candidates(executable: &str) -> Vec<PathBuf> {
    if executable.is_empty()
        || executable.contains('/')
        || executable.contains('\\')
        || executable.bytes().any(|byte| byte == 0)
    {
        return Vec::new();
    }
    let directories = [
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/opt/homebrew/bin",
        "/opt/local/bin",
    ];
    directories
        .into_iter()
        .map(|directory| PathBuf::from(directory).join(executable))
        .collect()
}

fn fixed(paths: &[&str]) -> Vec<PathBuf> {
    paths.iter().map(PathBuf::from).collect()
}

fn parse_candidates(paths: Vec<PathBuf>) -> Vec<AbsolutePath> {
    paths
        .into_iter()
        .filter_map(|path| AbsolutePath::parse(path).ok())
        .collect()
}
