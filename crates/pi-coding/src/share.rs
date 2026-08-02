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

use anyhow::{Context, Result, bail};
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
}