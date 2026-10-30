use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
};

use pi_sandbox::{EnvironmentVariableName, HostPlatform, PlatformFacts, StdioMode};

use super::grant_registry::{CredentialResolutionError, ResolvedCredentialGrant};

const CREDENTIAL_PREFIXES: [&str; 16] = [
    "PI_",
    "RPI_",
    "OPENAI_",
    "ANTHROPIC_",
    "GOOGLE_",
    "GEMINI_",
    "AZURE_",
    "AWS_",
    "GITHUB_",
    "GH_",
    "GITLAB_",
    "NPM_",
    "COHERE_",
    "MISTRAL_",
    "GROQ_",
    "XAI_",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum EnvironmentConstructionError {
    #[error("requested environment variable is reserved")]
    ReservedVariable,
    #[error("requested environment collides with the fixed sandbox baseline")]
    BaselineCollision,
    #[error("requested environment value is invalid")]
    InvalidValue,
    #[error("credential environment insertion failed: {0}")]
    Credential(#[from] CredentialResolutionError),
}

pub(crate) fn build_confined_environment(
    platform_facts: &PlatformFacts,
    revision_home: &str,
    revision_tmp: &str,
    revision_scratch: &str,
    requested: BTreeMap<EnvironmentVariableName, Option<String>>,
    credentials: impl IntoIterator<Item = ResolvedCredentialGrant>,
    stdio: StdioMode,
) -> Result<BTreeMap<OsString, OsString>, EnvironmentConstructionError> {
    let mut environment = confined_baseline(
        platform_facts,
        revision_home,
        revision_tmp,
        revision_scratch,
        stdio,
    )?;
    for (name, value) in requested {
        if is_reserved_credential_name(name.as_str()) {
            return Err(EnvironmentConstructionError::ReservedVariable);
        }
        let name = OsString::from(name.into_string());
        if let Some(existing) = baseline_value(&environment, &name) {
            // The confined baseline pins a fixed set of variables (PATH, HOME,
            // locale, …). A request that exactly matches the pinned value is an
            // idempotent no-op — trusted helpers legitimately re-assert
            // `LC_ALL=C` for their own defensive guarantees. Any other value,
            // or an attempt to clear a pinned variable, is an override attempt
            // and must fail closed.
            match value {
                Some(ref value) if OsStr::new(value) == existing => continue,
                _ => return Err(EnvironmentConstructionError::BaselineCollision),
            }
        }
        if let Some(value) = value {
            if value.contains('\0') {
                return Err(EnvironmentConstructionError::InvalidValue);
            }
            environment.insert(name, OsString::from(value));
        }
    }
    insert_credentials(&mut environment, credentials)?;
    Ok(environment)
}

pub(crate) fn build_raw_host_environment(
    ambient: impl IntoIterator<Item = (OsString, OsString)>,
    changes: BTreeMap<OsString, Option<OsString>>,
    credentials: impl IntoIterator<Item = ResolvedCredentialGrant>,
) -> Result<BTreeMap<OsString, OsString>, EnvironmentConstructionError> {
    let mut environment = BTreeMap::new();
    for (name, value) in ambient {
        insert_native(&mut environment, name, value);
    }
    for (name, value) in changes {
        match value {
            Some(value) => insert_native(&mut environment, name, value),
            None => remove_native(&mut environment, &name),
        }
    }
    insert_credentials(&mut environment, credentials)?;
    Ok(environment)
}

fn confined_baseline(
    platform_facts: &PlatformFacts,
    revision_home: &str,
    revision_tmp: &str,
    revision_scratch: &str,
    stdio: StdioMode,
) -> Result<BTreeMap<OsString, OsString>, EnvironmentConstructionError> {
    let (path, home, tmpdir, scratch, user) = match platform_facts.platform() {
        HostPlatform::Linux => (
            "/usr/local/bin:/usr/bin:/bin",
            "/run/pi/home",
            "/tmp",
            "/run/pi/scratch",
            "rpi",
        ),
        HostPlatform::Macos => (
            "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin",
            revision_home,
            revision_tmp,
            revision_scratch,
            platform_facts
                .account_user_name()
                .ok_or(EnvironmentConstructionError::InvalidValue)?,
        ),
        HostPlatform::Windows => return Err(EnvironmentConstructionError::InvalidValue),
    };
    let mut environment = BTreeMap::from([
        (OsString::from("PATH"), OsString::from(path)),
        (OsString::from("HOME"), OsString::from(home)),
        (OsString::from("TMPDIR"), OsString::from(tmpdir)),
        (
            OsString::from("PI_SANDBOX_SCRATCH"),
            OsString::from(scratch),
        ),
        (OsString::from("USER"), OsString::from(user)),
        (OsString::from("LOGNAME"), OsString::from(user)),
        (OsString::from("LANG"), OsString::from("C")),
        (OsString::from("LC_ALL"), OsString::from("C")),
        (OsString::from("LC_CTYPE"), OsString::from("C")),
        (OsString::from("TZ"), OsString::from("UTC")),
    ]);
    if stdio == StdioMode::Pty {
        environment.insert(OsString::from("TERM"), OsString::from("xterm-256color"));
        environment.insert(OsString::from("COLORTERM"), OsString::from("truecolor"));
    }
    Ok(environment)
}

fn insert_credentials(
    environment: &mut BTreeMap<OsString, OsString>,
    credentials: impl IntoIterator<Item = ResolvedCredentialGrant>,
) -> Result<(), CredentialResolutionError> {
    for credential in credentials {
        credential.insert_into(environment)?;
    }
    Ok(())
}

fn is_reserved_credential_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    CREDENTIAL_PREFIXES
        .iter()
        .any(|prefix| upper.starts_with(prefix))
}

fn insert_native(environment: &mut BTreeMap<OsString, OsString>, name: OsString, value: OsString) {
    remove_native(environment, &name);
    environment.insert(name, value);
}

fn remove_native(environment: &mut BTreeMap<OsString, OsString>, name: &OsString) {
    #[cfg(windows)]
    {
        let folded = name.to_string_lossy().to_ascii_uppercase();
        environment.retain(|existing, _| existing.to_string_lossy().to_ascii_uppercase() != folded);
    }
    #[cfg(not(windows))]
    {
        environment.remove(name);
    }
}

fn baseline_value<'a>(
    environment: &'a BTreeMap<OsString, OsString>,
    name: &OsString,
) -> Option<&'a OsString> {
    #[cfg(windows)]
    {
        let folded = name.to_string_lossy().to_ascii_uppercase();
        environment.iter().find_map(|(key, value)| {
            (key.to_string_lossy().to_ascii_uppercase() == folded).then_some(value)
        })
    }
    #[cfg(not(windows))]
    {
        environment.get(name)
    }
}

#[cfg(test)]
mod tests {
    use pi_sandbox::{AbsolutePath, PlatformFactInputs, derive_platform_facts};

    use super::*;

    fn platform_facts(root: &std::path::Path) -> PlatformFacts {
        for name in ["workspace", "agent", "session"] {
            std::fs::create_dir_all(root.join(name)).expect("platform fixture directory");
        }
        derive_platform_facts(
            PlatformFactInputs::new(
                "d3-environment".to_owned(),
                vec![AbsolutePath::parse(root.join("workspace")).expect("workspace")],
                AbsolutePath::parse(root.join("agent")).expect("agent"),
                AbsolutePath::parse(root.join("session")).expect("session"),
            )
            .expect("platform inputs"),
        )
        .expect("platform facts")
    }

    #[test]
    fn confined_environment_is_fixed_and_credentials_insert_last() {
        let root = tempfile::tempdir().expect("environment root");
        let facts = platform_facts(root.path());
        if facts.platform() == HostPlatform::Windows {
            return;
        }
        let credential = ResolvedCredentialGrant::new(
            EnvironmentVariableName::parse("D3_EXACT_TOKEN".to_owned()).expect("credential name"),
            "secret-value".to_owned(),
        )
        .expect("credential");
        let environment = build_confined_environment(
            &facts,
            "/revision/home",
            "/revision/tmp",
            "/revision/scratch",
            BTreeMap::from([(
                EnvironmentVariableName::parse("ORDINARY".to_owned()).expect("requested name"),
                Some("requested".to_owned()),
            )]),
            [credential],
            StdioMode::Pty,
        )
        .expect("confined environment");

        let (path, home, tmp, scratch, user) = match facts.platform() {
            HostPlatform::Linux => (
                "/usr/local/bin:/usr/bin:/bin",
                "/run/pi/home",
                "/tmp",
                "/run/pi/scratch",
                "rpi",
            ),
            HostPlatform::Macos => (
                "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin",
                "/revision/home",
                "/revision/tmp",
                "/revision/scratch",
                facts.account_user_name().expect("account user"),
            ),
            HostPlatform::Windows => unreachable!(),
        };
        let expected = BTreeMap::from([
            (OsString::from("PATH"), OsString::from(path)),
            (OsString::from("HOME"), OsString::from(home)),
            (OsString::from("TMPDIR"), OsString::from(tmp)),
            (
                OsString::from("PI_SANDBOX_SCRATCH"),
                OsString::from(scratch),
            ),
            (OsString::from("USER"), OsString::from(user)),
            (OsString::from("LOGNAME"), OsString::from(user)),
            (OsString::from("LANG"), OsString::from("C")),
            (OsString::from("LC_ALL"), OsString::from("C")),
            (OsString::from("LC_CTYPE"), OsString::from("C")),
            (OsString::from("TZ"), OsString::from("UTC")),
            (OsString::from("TERM"), OsString::from("xterm-256color")),
            (OsString::from("COLORTERM"), OsString::from("truecolor")),
            (OsString::from("ORDINARY"), OsString::from("requested")),
            (
                OsString::from("D3_EXACT_TOKEN"),
                OsString::from("secret-value"),
            ),
        ]);
        assert_eq!(environment, expected);

        let collision = build_confined_environment(
            &facts,
            "/revision/home",
            "/revision/tmp",
            "/revision/scratch",
            BTreeMap::new(),
            [ResolvedCredentialGrant::new(
                EnvironmentVariableName::parse("PATH".to_owned()).expect("path name"),
                "must-not-overwrite".to_owned(),
            )
            .expect("credential")],
            StdioMode::Null,
        );
        assert!(matches!(
            collision,
            Err(EnvironmentConstructionError::Credential(
                CredentialResolutionError::EnvironmentCollision,
            )),
        ));
    }

    #[test]
    fn raw_environment_applies_native_case_rules_and_authorized_credentials() {
        let credential = ResolvedCredentialGrant::new(
            EnvironmentVariableName::parse("D3_EXACT_TOKEN".to_owned()).expect("name"),
            "secret-value".to_owned(),
        )
        .expect("credential");
        let environment = build_raw_host_environment(
            [(OsString::from("ordinary"), OsString::from("ambient"))],
            BTreeMap::from([(OsString::from("ordinary"), Some(OsString::from("changed")))]),
            [credential],
        )
        .expect("environment");
        assert_eq!(
            environment.get(&OsString::from("ordinary")),
            Some(&OsString::from("changed"))
        );
        assert_eq!(
            environment.get(&OsString::from("D3_EXACT_TOKEN")),
            Some(&OsString::from("secret-value"))
        );
    }
}
