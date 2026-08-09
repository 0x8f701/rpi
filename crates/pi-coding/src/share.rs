//! GitHub Gist sharing via the `gh` CLI.
//!
//! Creates a **secret** gist through `gh gist create` (gh's default visibility)
//! using argv spawning (never shell interpolation). Credentials are handled
//! entirely by the `gh` CLI — this module never reads, logs, or passes tokens.
//! The viewer URL is derived from the `PI_SHARE_VIEWER_URL` environment
//! variable (supporting a `{url}` placeholder) or the documented default.
//!
//! Fails with an actionable error when `gh` is not installed or not
//! authenticated.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use tokio::process::Command;

use crate::export::{self, ExportOptions};

/// Environment variable for the custom viewer URL template.
pub const VIEWER_URL_ENV: &str = "PI_SHARE_VIEWER_URL";

/// Default viewer URL (the gist itself is viewable on GitHub).
pub const DEFAULT_VIEWER_URL: &str = "https://gist.github.com";

/// Result of a successful share operation.
#[derive(Clone, Debug)]
pub struct ShareResult {
    /// The raw gist URL returned by `gh gist create`.
    pub gist_url: String,
    /// The viewer URL derived from `PI_SHARE_VIEWER_URL` or the default.
    pub viewer_url: String,
    /// The local path of the exported HTML (temp file or explicit output).
    pub html_path: PathBuf,
}

/// Check that `gh` is installed and authenticated.
///
/// Returns an actionable error explaining what to install or run if not.
async fn check_gh_available() -> Result<()> {
    let version = Command::new("gh")
        .arg("--version")
        .output()
        .await
        .context("gh CLI not found; install it from https://cli.github.com/")?;
    if !version.status.success() {
        bail!("gh CLI is installed but `gh --version` failed; reinstall from https://cli.github.com/");
    }
    let auth = Command::new("gh")
        .args(["auth", "status"])
        .output()
        .await
        .context("failed to run `gh auth status`")?;
    if !auth.status.success() {
        bail!("gh is not authenticated; run `gh auth login` to sign in");
    }
    Ok(())
}

/// Derive the viewer URL from `PI_SHARE_VIEWER_URL` or the documented
/// default.
///
/// If `PI_SHARE_VIEWER_URL` contains `{url}`, the gist URL is substituted.
/// Otherwise the environment value is used verbatim. When unset, the gist
/// URL itself is returned (gist.github.com renders the content).
#[must_use]
pub fn derive_viewer_url(gist_url: &str) -> String {
    let template = std::env::var(VIEWER_URL_ENV).ok();
    derive_viewer_url_with(gist_url, template.as_deref())
}

/// Pure derivation from a template value — the testable seam that avoids
/// touching the process environment (which is `unsafe` in Rust 2024).
#[must_use]
fn derive_viewer_url_with(gist_url: &str, template: Option<&str>) -> String {
    match template {
        Some(template) if !template.is_empty() => {
            if template.contains("{url}") {
                template.replace("{url}", gist_url)
            } else {
                template.to_owned()
            }
        }
        _ => gist_url.to_owned(),
    }
}

/// Parse the gist URL from `gh gist create` stdout.
///
/// `gh gist create` prints the gist URL on its own line (optionally preceded
/// by a `- Creating gist...` progress line on a TTY). We extract the first
/// URL matching the gist pattern.
fn parse_gist_url(stdout: &str) -> Result<String> {
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            if trimmed.contains("/gist.") || trimmed.contains("gist.github.com") {
                return Ok(trimmed.to_owned());
            }
        }
    }
    // Fallback: any HTTP URL in the output.
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
            return Ok(trimmed.to_owned());
        }
    }
    bail!(
        "could not parse gist URL from gh output; got: {}",
        stdout.trim()
    )
}

/// Build argv for `gh gist create` (excluding the program name).
///
/// Visibility flags are intentionally omitted so gh keeps its default
/// (secret) gist. Pure helper for unit tests and the spawn path.
#[must_use]
fn gist_create_args(description: &str, path: &Path) -> Vec<String> {
    vec![
        "gist".into(),
        "create".into(),
        "--desc".into(),
        description.into(),
        path.as_os_str().to_string_lossy().into_owned(),
    ]
}

/// Create a secret GitHub gist from a file and return the gist URL.
///
/// Spawns `gh gist create --desc <description> <path>` via argv — never
/// through a shell. Visibility is left to gh's default (secret). The path
/// must already exist.
async fn create_gist(path: &Path, description: &str) -> Result<String> {
    let args = gist_create_args(description, path);
    let output = Command::new("gh")
        .args(&args)
        .output()
        .await
        .context("failed to spawn `gh gist create`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let hint = stderr.trim();
        bail!(
            "`gh gist create` failed{}",
            if hint.is_empty() {
                String::new()
            } else {
                format!(": {hint}")
            }
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_gist_url(&stdout)
}

/// Share a session file by exporting it to HTML and uploading to a secret
/// gist. Returns the gist and viewer URLs.
///
/// No model, auth, or network access is needed for the export step; only the
/// `gh` CLI call requires network/auth.
pub async fn share_session_file(
    session_path: &Path,
    options: &ExportOptions,
) -> Result<ShareResult> {
    check_gh_available().await?;
    let html = export::export_session_html(session_path, None, options)?;
    let url = create_gist(&html, "rpi session export").await?;
    let viewer = derive_viewer_url(&url);
    Ok(ShareResult {
        gist_url: url,
        viewer_url: viewer,
        html_path: html,
    })
}

/// Share a live [`crate::Session`] by exporting it and uploading to a
/// secret gist.
pub async fn share_session(
    session: &crate::Session,
    options: &ExportOptions,
) -> Result<ShareResult> {
    check_gh_available().await?;
    let html = export::export_live_session(session, None, options)?;
    let url = create_gist(&html, "rpi session export").await?;
    let viewer = derive_viewer_url(&url);
    Ok(ShareResult {
        gist_url: url,
        viewer_url: viewer,
        html_path: html,
    })
}

/// Share arbitrary HTML content by writing it to a temp file and uploading
/// to a secret gist. The caller is responsible for cleaning up if needed;
/// the temp file is in the system temp directory.
pub async fn share_html_content(
    html: &str,
    description: &str,
) -> Result<ShareResult> {
    check_gh_available().await?;
    let temp_dir = std::env::temp_dir();
    let temp_path = temp_dir.join(format!("pi-export-{}.html", std::process::id()));
    fs::write(&temp_path, html)
        .with_context(|| format!("writing temporary HTML {}", temp_path.display()))?;
    let url = create_gist(&temp_path, description).await?;
    let viewer = derive_viewer_url(&url);
    Ok(ShareResult {
        gist_url: url,
        viewer_url: viewer,
        html_path: temp_path,
    })
}

/// Result of an encrypted share of a session.
#[derive(Clone, Debug)]
pub struct EncryptedShareResult {
    /// Path of the written ciphertext file (`<name>.jsonl.enc`).
    pub ciphertext_path: PathBuf,
    /// Human-readable note describing the scheme and decrypt steps. Never
    /// contains the passphrase.
    pub note: String,
    /// Secret-gist URL of the ciphertext when the optional upload succeeded.
    pub gist_url: Option<String>,
}

/// Encrypt the current branch of a live [`crate::Session`] (as JSONL) with
/// `passphrase` and write `<name>.jsonl.enc`.
///
/// The plaintext JSONL is serialized directly into memory and fed to the
/// encryptor — no plaintext staging file is ever created, so no plaintext
/// copy exists on disk next to the ciphertext. The passphrase is never
/// stored or logged. The default output path is
/// `<session-file-stem>.jsonl.enc` in the session working directory (or the
/// explicit `output` when given).
pub fn encrypt_session_share_to_file(
    session: &crate::Session,
    passphrase: &str,
    output: Option<&Path>,
) -> Result<EncryptedShareResult> {
    let (_, session_path) = session
        .recorder_info()
        .ok_or_else(|| anyhow!("session recording is unavailable"))?;
    // Serialize the branch in memory: exporting to a file (even a temp one)
    // would stage plaintext on disk and could collide with the `.jsonl`
    // source file.
    let plaintext = export::export_session_jsonl_bytes(&session_path)?;
    let ciphertext = crate::encrypt::encrypt(passphrase, &plaintext)?;
    let out = output.map_or_else(
        || {
            let stem = session_path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "session".to_owned());
            session.cwd().join(format!("{stem}.jsonl.enc"))
        },
        Path::to_path_buf,
    );
    if let Some(dir) = out.parent()
        && !dir.as_os_str().is_empty()
        && !dir.exists()
    {
        fs::create_dir_all(dir)
            .with_context(|| format!("creating share directory {}", dir.display()))?;
    }
    fs::write(&out, ciphertext)
        .with_context(|| format!("writing encrypted share {}", out.display()))?;
    Ok(EncryptedShareResult {
        ciphertext_path: out,
        note: encrypted_share_note(),
        gist_url: None,
    })
}

/// Decrypt an encrypted session share file (`.jsonl.enc`) with `passphrase`.
///
/// Mirrors the layout documented in [`crate::encrypt`]: the first 16 bytes
/// are the PBKDF2 salt, the next 12 bytes the nonce, and the remainder the
/// AES-256-GCM ciphertext+tag.
pub fn decrypt_encrypted_session_file(passphrase: &str, path: &Path) -> Result<Vec<u8>> {
    let data = fs::read(path)
        .with_context(|| format!("reading encrypted share {}", path.display()))?;
    crate::encrypt::decrypt(passphrase, &data)
}

/// Encrypt the current session to a local `.jsonl.enc` file and, when the
/// `gh` CLI is available and authenticated, also upload the ciphertext to a
/// secret gist. A missing/unauthenticated `gh` is non-fatal: the local file
/// is the primary deliverable and `gist_url` stays `None`.
pub async fn share_session_encrypted(
    session: &crate::Session,
    passphrase: &str,
    output: Option<&Path>,
) -> Result<EncryptedShareResult> {
    let mut result = encrypt_session_share_to_file(session, passphrase, output)?;
    match check_gh_available().await {
        Ok(()) => match create_gist(
            &result.ciphertext_path,
            "rpi encrypted session export (AES-256-GCM)",
        )
        .await
        {
            Ok(url) => {
                result.gist_url = Some(derive_viewer_url(&url));
            }
            Err(error) => {
                result.note.push_str(&format!(
                    "\nGist upload skipped: {error:#}"
                ));
            }
        },
        Err(error) => {
            result.note.push_str(&format!("\nGist upload skipped: {error:#}"));
        }
    }
    Ok(result)
}

/// Scheme description attached to every encrypted share note.
#[must_use]
fn encrypted_share_note() -> String {
    format!(
        "AES-256-GCM encrypted session share. File layout: 16-byte random PBKDF2 \
         salt + 12-byte random nonce + AES-256-GCM ciphertext; key = \
         PBKDF2-HMAC-SHA256(passphrase, salt, 210000 iterations). Decrypt by \
         re-deriving the key from the salt and authenticating the tag with the \
         same passphrase; the passphrase is never stored or logged."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_viewer_url_with_none_template_returns_gist_url() {
        let url = derive_viewer_url_with("https://gist.github.com/user/abc123", None);
        assert_eq!(url, "https://gist.github.com/user/abc123");
    }

    #[test]
    fn derive_viewer_url_with_empty_template_returns_gist_url() {
        let url = derive_viewer_url_with("https://gist.github.com/user/abc123", Some(""));
        assert_eq!(url, "https://gist.github.com/user/abc123");
    }

    #[test]
    fn derive_viewer_url_with_placeholder_substitutes() {
        let url = derive_viewer_url_with(
            "https://gist.github.com/user/abc123",
            Some("https://pi.dev/share?url={url}"),
        );
        assert_eq!(
            url,
            "https://pi.dev/share?url=https://gist.github.com/user/abc123"
        );
    }

    #[test]
    fn derive_viewer_url_verbatim_when_no_placeholder() {
        let url = derive_viewer_url_with(
            "https://gist.github.com/user/abc123",
            Some("https://pi.dev/share"),
        );
        assert_eq!(url, "https://pi.dev/share");
    }

    #[test]
    fn parse_gist_url_extracts_from_stdout() {
        let url = parse_gist_url("https://gist.github.com/cj/abc123def456\n").unwrap();
        assert_eq!(url, "https://gist.github.com/cj/abc123def456");
    }

    #[test]
    fn parse_gist_url_skips_progress_line() {
        let url = parse_gist_url("- Creating gist...\nhttps://gist.github.com/cj/abc123\n").unwrap();
        assert_eq!(url, "https://gist.github.com/cj/abc123");
    }

    #[test]
    fn parse_gist_url_fails_on_empty() {
        assert!(parse_gist_url("").is_err());
        assert!(parse_gist_url("no url here").is_err());
    }

    #[test]
    fn gist_create_args_omits_visibility_flags() {
        let args = gist_create_args("rpi session export", Path::new("/tmp/export.html"));
        assert_eq!(
            args,
            vec![
                "gist",
                "create",
                "--desc",
                "rpi session export",
                "/tmp/export.html",
            ]
        );
        assert!(!args.iter().any(|a| a == "--private" || a == "--public"));
    }

    fn recorded_session(cwd: &Path, sessions: &Path, id: &str, text: &str) -> crate::Session {
        use pi_ai::Message;
        let recorder = crate::start_session_in(
            cwd,
            None,
            Some("off"),
            Some(sessions),
            Some(id),
            None,
        )
        .expect("start recorder");
        recorder
            .record_message(&Message::user_text(text, 0))
            .expect("record message");
        recorder.persist_now().expect("persist session");
        let session = crate::Session::new(crate::SessionOptions {
            model: pi_ai::Model::default(),
            cwd: cwd.to_path_buf(),
            system_prompt: String::new(),
            thinking_level: pi_agent::ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
        })
        .expect("session");
        session.set_session_dir(sessions.to_path_buf());
        session.record(recorder).expect("attach recorder");
        session
    }

    #[test]
    fn encrypted_share_round_trips_and_hides_plaintext() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let session = recorded_session(cwd.path(), sessions.path(), "enc-rt", "top secret line");

        let result = encrypt_session_share_to_file(&session, "hunter2", None)
            .expect("encrypt share");
        let ciphertext_path = result.ciphertext_path.clone();
        assert_eq!(
            ciphertext_path.extension().and_then(|e| e.to_str()),
            Some("enc")
        );
        assert!(ciphertext_path.exists(), "ciphertext file must exist");
        assert!(
            !result.note.contains("hunter2"),
            "share note must never contain the passphrase"
        );

        // Round trip through the decrypt helper.
        let plaintext =
            decrypt_encrypted_session_file("hunter2", &ciphertext_path).expect("decrypt");
        assert!(String::from_utf8_lossy(&plaintext).contains("top secret line"));

        // No plaintext bytes inside the .enc file.
        let cipher = fs::read(&ciphertext_path).expect("read ciphertext");
        assert!(
            !cipher
                .windows(b"top secret line".len())
                .any(|window| window == b"top secret line"),
            ".enc file must not contain the plaintext"
        );

        // Wrong passphrase fails.
        let error = decrypt_encrypted_session_file("wrong", &ciphertext_path)
            .expect_err("wrong passphrase must fail");
        assert!(error.to_string().contains("decryption failed"));

        // Default output lands in the session cwd with the session stem.
        assert_eq!(
            ciphertext_path,
            cwd.path().join("enc-rt.jsonl.enc"),
            "default path is <session-stem>.jsonl.enc in cwd"
        );
    }

    #[test]
    fn encrypted_share_stages_no_plaintext_temp_file() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let session = recorded_session(cwd.path(), sessions.path(), "enc-nostage", "sensitive");

        // The plaintext JSONL must be serialized in memory: no `pi-share-*`
        // staging file may appear in the system temp dir during encryption.
        let temp_dir = std::env::temp_dir();
        let staging_entries = || {
            fs::read_dir(&temp_dir)
                .expect("read temp dir")
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("pi-share-"))
                .count()
        };
        let before = staging_entries();
        let result =
            encrypt_session_share_to_file(&session, "hunter2", None).expect("encrypt share");
        let after = staging_entries();
        assert_eq!(
            after, before,
            "encrypted share must not stage a plaintext temp file"
        );

        // The .enc still round-trips.
        let plaintext =
            decrypt_encrypted_session_file("hunter2", &result.ciphertext_path).expect("decrypt");
        assert!(String::from_utf8_lossy(&plaintext).contains("sensitive"));
    }

    #[test]
    fn encrypted_share_writes_to_explicit_output_path() {
        let cwd = tempfile::tempdir().expect("cwd");
        let sessions = tempfile::tempdir().expect("sessions");
        let session = recorded_session(cwd.path(), sessions.path(), "enc-out", "payload");

        let out = tempfile::tempdir()
            .expect("out dir")
            .path()
            .join("share.enc");
        let result = encrypt_session_share_to_file(&session, "pass", Some(&out))
            .expect("encrypt share");
        assert_eq!(result.ciphertext_path, out);
        let plaintext = decrypt_encrypted_session_file("pass", &out).expect("decrypt");
        assert!(String::from_utf8_lossy(&plaintext).contains("payload"));
    }
}