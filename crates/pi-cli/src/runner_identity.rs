//! Runner identity — the codename authority persisted at
//! `<agent-dir>/runner.json` (design D1/D2).
//!
//! One authority per runner: the display codename lives in exactly one
//! file, written only along the `--listen` plane. Every other carrier (mDNS
//! TXT, discovery events, `get_state` overlay) mirrors this file or the CLI
//! flag; nothing addresses storage by codename, so corruption downgrades to
//! a regenerated default instead of bailing startup. The payload is a
//! display name only — no token, origin, or path is ever persisted here.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::team::write_file_atomic_private;

/// On-disk identity schema version.
pub const RUNNER_IDENTITY_VERSION: u32 = 1;

/// The D2 codename grammar, anchored. Enforced by the strict validator
/// (no regex dependency) and by the clap value parser on `--listen-codename`.
pub const CODENAME_REGEX: &str = "^[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?$";

/// Maximum codename length implied by [`CODENAME_REGEX`].
const CODENAME_MAX_LEN: usize = 48;

/// The single identity file name under the agent dir.
const RUNNER_IDENTITY_FILE: &str = "runner.json";

/// Persisted runner identity; the file's entire content. 0600 on disk, but
/// the content itself is public display data only — never a secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerIdentity {
    /// Schema version, always [`RUNNER_IDENTITY_VERSION`].
    pub version: u32,
    /// Display codename, validated against the D2 grammar.
    pub codename: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// The D2 codename grammar.
#[must_use]
pub fn codename_regex() -> &'static str {
    CODENAME_REGEX
}

/// Strict D2 grammar: 1–48 lowercase alphanumeric/hyphen bytes, with an alphanumeric first and last byte.
#[must_use]
pub fn is_valid_codename(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.last() != Some(&b'-')
        && bytes.len() <= CODENAME_MAX_LEN
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| match byte {
                b'a'..=b'z' | b'0'..=b'9' => true,
                b'-' => index > 0,
                _ => false,
            })
}

/// D2 base: lowercase, then the same charset rule as `mdns_instance_label`
/// (every non-ASCII-alphanumeric character becomes `-`), with consecutive
/// `-` collapsed and edge `-` trimmed. Empty when the host sanitizes away —
/// the caller falls back to [`fingerprint`].
#[must_use]
pub fn sanitize_base(host: &str) -> String {
    let lowered = host.trim().to_lowercase();
    let mut out = String::with_capacity(lowered.len());
    for character in lowered.chars() {
        if matches!(character, 'a'..='z' | '0'..='9') {
            out.push(character);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_owned()
}

/// D2 empty-host fingerprint: last 3 bytes of SHA-256 over
/// `hostname + '\0' + canonical agent-dir path`. Profile roots stay
/// distinguishable even when the hostname sanitizes away; only an opaque
/// digest — never the path itself — reaches the codename.
#[must_use]
pub fn fingerprint(host: &str, agent_dir: &Path) -> [u8; 3] {
    let canonical = agent_dir
        .canonicalize()
        .unwrap_or_else(|_| agent_dir.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(host.as_bytes());
    hasher.update([0]);
    hasher.update(canonical.as_os_str().as_encoded_bytes());
    let digest = hasher.finalize();
    digest[29..].try_into().expect("digest tail is exactly 3 bytes")
}

fn fingerprint_base(host: &str, agent_dir: &Path) -> String {
    let hex: String = fingerprint(host, agent_dir)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("rpi-{hex}")
}

/// D2 default codename for a resolved bind port:
/// `{base}-{resolved_port}` where the base is truncated to
/// `48 - 1 - decimal_port_len` characters and edge `-` re-trimmed, so the
/// composed name always satisfies the grammar. An empty base — or one that
/// truncates empty — falls back to the `rpi-<fp6>` fingerprint form. The
/// post-bind port disambiguates co-hosted listeners by construction; the
/// fingerprint distinguishes profile roots when the hostname sanitizes away.
#[must_use]
pub fn default_codename(host: &str, agent_dir: &Path, resolved_port: u16) -> String {
    let fallback = fingerprint_base(host, agent_dir);
    compose_default(sanitize_base(host), fallback, resolved_port)
}

fn compose_default(base: String, fallback: String, resolved_port: u16) -> String {
    let port_text = resolved_port.to_string();
    let cap = CODENAME_MAX_LEN - 1 - port_text.len();
    let truncated: String = base.chars().take(cap).collect();
    let base = truncated.trim_end_matches('-');
    if is_valid_codename(base) {
        format!("{base}-{port_text}")
    } else {
        // `rpi-<fp6>` is 10 characters, far under the smallest cap (42).
        format!("{fallback}-{resolved_port}")
    }
}

/// Kernel hostname first — `HOSTNAME` is not exported to non-interactive
/// children on Linux, so the shell variable alone would leave co-hosted
/// listeners hostname-less — then environment fallbacks; empty routes the
/// default through the fingerprint base.
fn local_hostname() -> String {
    #[cfg(target_os = "linux")]
    let from_system = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    #[cfg(not(target_os = "linux"))]
    let from_system: Option<String> = None;
    from_system
        .or_else(|| {
            std::env::var("HOSTNAME")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| {
            std::env::var("COMPUTERNAME")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_default()
}

fn identity_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join(RUNNER_IDENTITY_FILE)
}

enum StoredOutcome {
    Valid(RunnerIdentity),
    Missing,
    Corrupt(String),
}

fn load_stored(path: &Path) -> StoredOutcome {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return StoredOutcome::Missing,
        Err(error) => return StoredOutcome::Corrupt(format!("unreadable: {error}")),
    };
    let parsed: Result<RunnerIdentity, _> = serde_json::from_slice(&bytes);
    match parsed {
        Ok(identity)
            if identity.version == RUNNER_IDENTITY_VERSION
                && is_valid_codename(&identity.codename) =>
        {
            StoredOutcome::Valid(identity)
        }
        Ok(identity) if identity.version != RUNNER_IDENTITY_VERSION => {
            StoredOutcome::Corrupt(format!("unsupported version {}", identity.version))
        }
        Ok(_) => StoredOutcome::Corrupt("stored codename violates the grammar".to_owned()),
        Err(error) => StoredOutcome::Corrupt(format!("unparseable: {error}")),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn persist(path: &Path, identity: &RunnerIdentity) -> io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(identity)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    bytes.push(b'\n');
    write_file_atomic_private(path, &bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))
}

fn default_identity(agent_dir: &Path, resolved_port: u16) -> RunnerIdentity {
    let now = now_ms();
    RunnerIdentity {
        version: RUNNER_IDENTITY_VERSION,
        codename: default_codename(&local_hostname(), agent_dir, resolved_port),
        created_at_ms: now,
        updated_at_ms: now,
    }
}

/// Resolve the runner identity for this listen start. Runs only inside
/// `modes::listen::start`, after the local bind has resolved
/// `resolved_port` — never on TUI, one-shot, `/web`-only, or `--export`
/// paths.
///
/// - CLI override: rewrites the stored codename (the only rename path —
///   there is no rename RPC), preserving `createdAtMs` when a valid file
///   exists and bumping `updatedAtMs`.
/// - Stored file: reused verbatim when readable at the current version with
///   a grammar-valid codename; port churn never regenerates it.
/// - Missing: builds the D2 default from `resolved_port` and writes it.
/// - Corruption (parse failure, version mismatch, unreadable, or a stored
///   codename that fails the grammar): the old file is renamed to
///   `runner.json.corrupt-<unix_ms>` for forensics, a warning is printed,
///   and the default is regenerated. Startup never hard-bails on identity.
pub fn resolve(
    agent_dir: &Path,
    cli_override: Option<&str>,
    resolved_port: u16,
) -> io::Result<RunnerIdentity> {
    let path = identity_path(agent_dir);
    if let Some(codename) = cli_override {
        if !is_valid_codename(codename) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid codename {codename:?}: does not match {CODENAME_REGEX}"),
            ));
        }
        let created_at_ms = match load_stored(&path) {
            StoredOutcome::Valid(stored) => stored.created_at_ms,
            _ => now_ms(),
        };
        let identity = RunnerIdentity {
            version: RUNNER_IDENTITY_VERSION,
            codename: codename.to_owned(),
            created_at_ms,
            updated_at_ms: now_ms().max(created_at_ms.saturating_add(1)),
        };
        persist(&path, &identity)?;
        return Ok(identity);
    }

    match load_stored(&path) {
        StoredOutcome::Valid(identity) => Ok(identity),
        StoredOutcome::Missing => {
            let identity = default_identity(agent_dir, resolved_port);
            persist(&path, &identity)?;
            Ok(identity)
        }
        StoredOutcome::Corrupt(reason) => {
            let corrupt_path = path.with_file_name(format!(
                "{RUNNER_IDENTITY_FILE}.corrupt-{}",
                now_ms()
            ));
            eprintln!(
                "warning: runner identity {} unusable ({reason}); preserving it as {} and regenerating",
                path.display(),
                corrupt_path.display()
            );
            let _ = fs::rename(&path, &corrupt_path);
            let identity = default_identity(agent_dir, resolved_port);
            persist(&path, &identity)?;
            Ok(identity)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn codename_grammar_accept_reject_table() {
        assert_eq!(codename_regex(), "^[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?$");
        let max_len = "a".repeat(CODENAME_MAX_LEN);
        for valid in ["a", "0", "run-1", "x-y-z", "a--b", max_len.as_str()] {
            assert!(is_valid_codename(valid), "expected valid: {valid:?}");
        }
        let too_long = "a".repeat(CODENAME_MAX_LEN + 1);
        for invalid in [
            "",
            "A",
            "-abc",
            "abc-",
            "a b",
            "run_1",
            "café",
            "ＡＢ",
            "-",
            too_long.as_str(),
        ] {
            assert!(!is_valid_codename(invalid), "expected invalid: {invalid:?}");
        }
    }

    #[test]
    fn cohosted_listeners_distinct_defaults_by_port() {
        let agent_dir = TempDir::new().unwrap();
        let first = default_codename("buildbox", agent_dir.path(), 4711);
        let second = default_codename("buildbox", agent_dir.path(), 4712);
        assert_eq!(first, "buildbox-4711");
        assert_eq!(second, "buildbox-4712");
        assert!(is_valid_codename(&first) && is_valid_codename(&second));
    }

    #[test]
    fn long_host_truncates_to_grammar_valid_default() {
        let agent_dir = TempDir::new().unwrap();
        let host = "h".repeat(60);
        let codename = default_codename(&host, agent_dir.path(), 4711);
        assert!(is_valid_codename(&codename), "got {codename:?}");
        let base = codename.strip_suffix("-4711").unwrap();
        // 48 - 1 - 4 = 43-char base, no trailing '-', valid first char.
        assert_eq!(base.len(), 43);
        assert!(!base.ends_with('-'));
        assert!(base.starts_with('h'));

        let two_digit = default_codename(&host, agent_dir.path(), 80);
        assert_eq!(two_digit.strip_suffix("-80").unwrap().len(), 45);
        assert!(is_valid_codename(&two_digit));

        // A truncation that would leave only '-' falls back to the
        // fingerprint base; the first character is always [a-z0-9].
        let dashed = compose_default("-".repeat(60), "rpi-abcdef".to_owned(), 4711);
        assert_eq!(dashed, "rpi-abcdef-4711");
    }

    #[test]
    fn empty_host_distinguishes_agent_dirs_by_fingerprint() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        let left = default_codename("", first.path(), 80);
        let right = default_codename("", second.path(), 80);
        assert_ne!(first.path(), second.path());
        assert!(left.starts_with("rpi-") && left.ends_with("-80"));
        let fp = left
            .strip_suffix("-80")
            .unwrap()
            .strip_prefix("rpi-")
            .unwrap();
        assert_eq!(fp.len(), 6);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(left, right);

        // Stable per agent dir: the same inputs reproduce the fingerprint.
        assert_eq!(left, default_codename("", first.path(), 80));
        // A real hostname takes the sanitize path, never the fingerprint.
        assert_eq!(default_codename("box", first.path(), 80), "box-80");
    }

    #[test]
    fn resolve_persists_reuses_and_keeps_codename_across_port_churn() {
        let agent_dir = TempDir::new().unwrap();
        let path = identity_path(agent_dir.path());

        let first = resolve(agent_dir.path(), None, 4711).unwrap();
        assert!(is_valid_codename(&first.codename));
        assert_eq!(first.version, RUNNER_IDENTITY_VERSION);
        assert!(first.created_at_ms > 0);
        assert!(first.updated_at_ms >= first.created_at_ms);
        assert_eq!(read_identity(&path), first);

        // Stored codename survives restarts; a different resolved port must
        // not regenerate it.
        let churned = resolve(agent_dir.path(), None, 8080).unwrap();
        assert_eq!(churned, first);

        // The persisted payload is a display name only: exactly the four
        // schema fields, no path, no origin, no token — and no local
        // absolute literals leak into the file.
        let raw = fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let keys: Vec<&str> = value.as_object().unwrap().keys().map(String::as_str).collect();
        assert!(keys.contains(&"version") && keys.contains(&"codename"));
        assert!(keys.contains(&"createdAtMs") && keys.contains(&"updatedAtMs"));
        assert_eq!(keys.len(), 4);
        assert!(!raw.contains(agent_dir.path().to_str().unwrap()));
        assert!(!raw.contains('/') && !raw.to_lowercase().contains("token"));
    }

    #[test]
    fn cli_override_rewrites_and_bumps_updated_at() {
        let agent_dir = TempDir::new().unwrap();
        let path = identity_path(agent_dir.path());
        let created_at = resolve(agent_dir.path(), None, 4711).unwrap().created_at_ms;
        std::fs::write(
            &path,
            format!(
                r#"{{"version":1,"codename":"old-name","createdAtMs":{created_at},"updatedAtMs":{created_at}}}"#
            ),
        )
        .unwrap();

        let renamed = resolve(agent_dir.path(), Some("renamed-runner"), 4711).unwrap();
        assert_eq!(renamed.codename, "renamed-runner");
        assert_eq!(renamed.created_at_ms, created_at);
        assert!(renamed.updated_at_ms > renamed.created_at_ms, "bumped updatedAt");

        // The rewrite persists: the next flag-less boot keeps the new name.
        assert_eq!(read_identity(&path), renamed);

        // Overrides obey the same grammar as the clap value parser.
        assert!(resolve(agent_dir.path(), Some("Bad_Name"), 80).is_err());
    }

    #[test]
    fn corrupt_file_renamed_aside_then_regenerated() {
        let agent_dir = TempDir::new().unwrap();
        let path = identity_path(agent_dir.path());
        let garbage: &[u8] = b"{ this is not json";
        fs::write(&path, garbage).unwrap();

        let identity = resolve(agent_dir.path(), None, 4711).unwrap();
        assert!(is_valid_codename(&identity.codename));
        assert_eq!(read_identity(&path), identity);

        let preserved: Vec<_> = fs::read_dir(agent_dir.path())
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("runner.json.corrupt-")
            })
            .collect();
        assert_eq!(preserved.len(), 1, "exactly one corrupt sibling kept");
        assert_eq!(fs::read(preserved[0].path()).unwrap(), garbage);

        // And the regenerated file is the sole runner.json again.
        assert_eq!(read_identity(&path), identity);
    }

    #[test]
    fn version_mismatch_regenerates_instead_of_bailing() {
        let agent_dir = TempDir::new().unwrap();
        let path = identity_path(agent_dir.path());
        fs::write(
            &path,
            br#"{"version":99,"codename":"old-name","createdAtMs":1,"updatedAtMs":1}"#,
        )
        .unwrap();

        let identity = resolve(agent_dir.path(), None, 80).unwrap();
        assert_eq!(identity.version, RUNNER_IDENTITY_VERSION);
        assert_ne!(identity.codename, "old-name");
        assert!(is_valid_codename(&identity.codename));

        let siblings: Vec<String> = fs::read_dir(agent_dir.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("runner.json.corrupt-"))
            .collect();
        assert_eq!(siblings.len(), 1, "old file preserved for forensics");
        assert!(fs::read_to_string(path).is_ok());
    }

    fn read_identity(path: &Path) -> RunnerIdentity {
        let bytes = fs::read(path).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
}