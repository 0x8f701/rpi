#[cfg(target_os = "macos")]
use std::path::PathBuf;
use std::process::Stdio;
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

pub use crate::image_pipeline::MAX_IMAGE_BYTES;
const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;
const MAX_TYPE_LIST_BYTES: usize = 64 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const IMAGE_MIME_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];

#[cfg(target_os = "macos")]
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardImage {
    pub bytes: Vec<u8>,
    pub mime_type: String,
}

impl ClipboardImage {
    #[must_use]
    pub fn into_content_block(self) -> pi_ai::ContentBlock {
        pi_ai::ContentBlock::Image {
            data: base64::engine::general_purpose::STANDARD.encode(self.bytes),
            mime_type: self.mime_type,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipboardContent {
    Image(ClipboardImage),
    Text(String),
}

#[derive(Debug)]
struct CommandOutput {
    status: Option<i32>,
    stdout: Vec<u8>,
}

/// Read an image first and fall back to UTF-8 text. Platform command failures
/// are returned to the UI instead of being allowed to unwind terminal state.
pub async fn read() -> Result<Option<ClipboardContent>> {
    let mut failures = Vec::new();
    match read_image_platform().await {
        Ok(Some(image)) => return Ok(Some(ClipboardContent::Image(image))),
        Ok(None) => {}
        Err(error) => failures.push(error.to_string()),
    }
    match read_text_platform().await {
        Ok(Some(text)) => Ok(Some(ClipboardContent::Text(text))),
        Ok(None) if failures.is_empty() => Ok(None),
        Ok(None) => Err(anyhow!(failures.join("; "))),
        Err(error) => {
            failures.push(error.to_string());
            Err(anyhow!(failures.join("; ")))
        }
    }
}

pub async fn write_text(text: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        return write_with_fallback(
            &[("pbcopy", &[] as &[&str])],
            text.as_bytes(),
            MAX_TEXT_BYTES,
        )
        .await;
    }
    #[cfg(target_os = "windows")]
    {
        const SCRIPT: &str = "$text=[Console]::In.ReadToEnd(); Set-Clipboard -Value $text";
        return write_powershell(SCRIPT, text.as_bytes(), MAX_TEXT_BYTES).await;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut attempts: Vec<(&str, Vec<&str>)> = Vec::new();
        if is_wayland() {
            attempts.push((
                "wl-copy",
                vec!["--type", "text/plain;charset=utf-8", "--paste-once"],
            ));
        }
        if std::env::var_os("DISPLAY").is_some() {
            attempts.push(("xclip", vec!["-selection", "clipboard"]));
            attempts.push(("xsel", vec!["--clipboard", "--input"]));
        }
        if attempts.is_empty() {
            attempts.extend([
                (
                    "wl-copy",
                    vec!["--type", "text/plain;charset=utf-8", "--paste-once"],
                ),
                ("xclip", vec!["-selection", "clipboard"]),
                ("xsel", vec!["--clipboard", "--input"]),
            ]);
        }
        return write_owned_fallback(&attempts, text.as_bytes(), MAX_TEXT_BYTES).await;
    }
    #[allow(unreachable_code)]
    bail!("clipboard writing is not supported on this platform")
}

/// Write an image for callers that need to expose generated images to the
/// system clipboard. The TUI currently uses image reads and text writes.
pub async fn write_image(image: &ClipboardImage) -> Result<()> {
    let processed =
        crate::image_pipeline::process_image(image.bytes.clone(), Some(&image.mime_type))?;
    let processed_image = ClipboardImage {
        bytes: processed.bytes,
        mime_type: processed.mime_type,
    };
    let image = &processed_image;
    #[cfg(target_os = "windows")]
    {
        const SCRIPT: &str = "Add-Type -AssemblyName System.Windows.Forms; Add-Type -AssemblyName System.Drawing; $b64=[Console]::In.ReadToEnd(); $bytes=[Convert]::FromBase64String($b64); $stream=[IO.MemoryStream]::new($bytes); $img=[Drawing.Image]::FromStream($stream); [Windows.Forms.Clipboard]::SetImage($img)";
        let encoded = base64::engine::general_purpose::STANDARD.encode(&image.bytes);
        return write_powershell(SCRIPT, encoded.as_bytes(), encoded.len()).await;
    }
    #[cfg(target_os = "macos")]
    {
        if image.mime_type != "image/png" {
            bail!("macOS clipboard image write requires PNG data");
        }
        let path = write_temporary_png(&image.bytes)?;
        let path_value = path.to_string_lossy().into_owned();
        const SCRIPT: &str = "set the clipboard to (read (POSIX file (system attribute \"PI_CLIPBOARD_IMAGE\")) as «class PNGf»)";
        let result = run_command(
            "osascript",
            &["-e", SCRIPT],
            None,
            1,
            &[("PI_CLIPBOARD_IMAGE", path_value.as_str())],
        )
        .await;
        let _ = tokio::fs::remove_file(&path).await;
        let output = result.context("failed to run the macOS pasteboard helper")?;
        if output.status == Some(0) {
            return Ok(());
        }
        bail!("macOS pasteboard helper rejected the PNG image")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut attempts: Vec<(&str, Vec<&str>)> = Vec::new();
        if is_wayland() {
            attempts.push((
                "wl-copy",
                vec!["--type", image.mime_type.as_str(), "--paste-once"],
            ));
        }
        if std::env::var_os("DISPLAY").is_some() {
            attempts.push((
                "xclip",
                vec!["-selection", "clipboard", "-t", image.mime_type.as_str()],
            ));
        }
        if attempts.is_empty() {
            attempts.extend([
                (
                    "wl-copy",
                    vec!["--type", image.mime_type.as_str(), "--paste-once"],
                ),
                (
                    "xclip",
                    vec!["-selection", "clipboard", "-t", image.mime_type.as_str()],
                ),
            ]);
        }
        return write_owned_fallback(&attempts, &image.bytes, MAX_IMAGE_BYTES).await;
    }
    #[allow(unreachable_code)]
    bail!("clipboard image writing is not supported on this platform")
}

async fn read_image_platform() -> Result<Option<ClipboardImage>> {
    #[cfg(target_os = "macos")]
    {
        let output = run_command("pngpaste", &["-"], None, MAX_IMAGE_BYTES + 1, &[])
            .await
            .context("install `pngpaste` to paste clipboard images on macOS")?;
        return image_from_command(output, "image/png");
    }
    #[cfg(target_os = "windows")]
    {
        const SCRIPT: &str = "Add-Type -AssemblyName System.Windows.Forms; Add-Type -AssemblyName System.Drawing; $img=[Windows.Forms.Clipboard]::GetImage(); if ($null -eq $img) { exit 3 }; $stream=[IO.MemoryStream]::new(); $img.Save($stream,[Drawing.Imaging.ImageFormat]::Png); $bytes=$stream.ToArray(); [Console]::OpenStandardOutput().Write($bytes,0,$bytes.Length)";
        let output = run_powershell(SCRIPT, None, MAX_IMAGE_BYTES + 1).await?;
        return image_from_command(output, "image/png");
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut failures = Vec::new();
        if is_wayland() {
            match read_wayland_image().await {
                Ok(Some(image)) => return Ok(Some(image)),
                Ok(None) => {}
                Err(error) => failures.push(error.to_string()),
            }
        }
        if std::env::var_os("DISPLAY").is_some() || !is_wayland() {
            match read_x11_image().await {
                Ok(Some(image)) => return Ok(Some(image)),
                Ok(None) => {}
                Err(error) => failures.push(error.to_string()),
            }
        }
        if failures.is_empty() {
            return Ok(None);
        }
        return Err(anyhow!(failures.join("; ")));
    }
    #[allow(unreachable_code)]
    Ok(None)
}

async fn read_text_platform() -> Result<Option<String>> {
    #[cfg(target_os = "macos")]
    {
        let output = run_command("pbpaste", &[], None, MAX_TEXT_BYTES + 1, &[])
            .await
            .context("`pbpaste` could not read the macOS clipboard")?;
        return text_from_command(output);
    }
    #[cfg(target_os = "windows")]
    {
        const SCRIPT: &str = "$text=Get-Clipboard -Raw -TextFormatType Text; if($null -eq $text){exit 3}; [Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); [Console]::Write($text)";
        return text_from_command(run_powershell(SCRIPT, None, MAX_TEXT_BYTES + 1).await?);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut commands: Vec<(&str, Vec<&str>)> = Vec::new();
        if is_wayland() {
            commands.push((
                "wl-paste",
                vec!["--no-newline", "--type", "text/plain;charset=utf-8"],
            ));
            commands.push(("wl-paste", vec!["--no-newline", "--type", "text"]));
        }
        if std::env::var_os("DISPLAY").is_some() || !is_wayland() {
            commands.push((
                "xclip",
                vec!["-selection", "clipboard", "-t", "UTF8_STRING", "-o"],
            ));
            commands.push(("xsel", vec!["--clipboard", "--output"]));
        }
        let mut failures = Vec::new();
        for (program, args) in commands {
            match run_command(program, &args, None, MAX_TEXT_BYTES + 1, &[]).await {
                Ok(output) if output.status == Some(0) => return text_from_bytes(output.stdout),
                Ok(_) => failures.push(format!("{program} did not provide clipboard text")),
                Err(error) => failures.push(format!("{program}: {error}")),
            }
        }
        if failures.is_empty() {
            bail!("no Linux clipboard display is available (WAYLAND_DISPLAY or DISPLAY)");
        }
        bail!("could not read clipboard text; {}", failures.join("; "))
    }
    #[allow(unreachable_code)]
    Ok(None)
}

#[cfg(all(unix, not(target_os = "macos")))]
async fn read_wayland_image() -> Result<Option<ClipboardImage>> {
    let types = run_command(
        "wl-paste",
        &["--list-types"],
        None,
        MAX_TYPE_LIST_BYTES,
        &[],
    )
    .await
    .context("`wl-paste` is required for Wayland clipboard access")?;
    if types.status != Some(0) {
        return Ok(None);
    }
    let types =
        String::from_utf8(types.stdout).context("wl-paste returned an invalid MIME list")?;
    let Some(mime_type) = preferred_mime(types.lines()) else {
        return Ok(None);
    };
    let output = run_command(
        "wl-paste",
        &["--no-newline", "--type", mime_type],
        None,
        MAX_IMAGE_BYTES + 1,
        &[],
    )
    .await?;
    image_from_command(output, mime_type)
}

#[cfg(all(unix, not(target_os = "macos")))]
async fn read_x11_image() -> Result<Option<ClipboardImage>> {
    let targets = run_command(
        "xclip",
        &["-selection", "clipboard", "-t", "TARGETS", "-o"],
        None,
        MAX_TYPE_LIST_BYTES,
        &[],
    )
    .await
    .context("install `xclip` for X11 clipboard image access")?;
    if targets.status != Some(0) {
        return Ok(None);
    }
    let targets =
        String::from_utf8(targets.stdout).context("xclip returned an invalid target list")?;
    let Some(mime_type) = preferred_mime(targets.lines()) else {
        return Ok(None);
    };
    let output = run_command(
        "xclip",
        &["-selection", "clipboard", "-t", mime_type, "-o"],
        None,
        MAX_IMAGE_BYTES + 1,
        &[],
    )
    .await?;
    image_from_command(output, mime_type)
}

fn image_from_command(
    output: CommandOutput,
    advertised_mime: &str,
) -> Result<Option<ClipboardImage>> {
    if output.status != Some(0) || output.stdout.is_empty() {
        return Ok(None);
    }
    let image = crate::image_pipeline::process_image(output.stdout, Some(advertised_mime))?;
    Ok(Some(ClipboardImage {
        bytes: image.bytes,
        mime_type: image.mime_type,
    }))
}

fn text_from_command(output: CommandOutput) -> Result<Option<String>> {
    if output.status != Some(0) {
        return Ok(None);
    }
    text_from_bytes(output.stdout)
}

fn text_from_bytes(bytes: Vec<u8>) -> Result<Option<String>> {
    if bytes.len() > MAX_TEXT_BYTES {
        bail!(
            "clipboard text exceeds the {} MiB limit",
            MAX_TEXT_BYTES / 1024 / 1024
        );
    }
    if bytes.is_empty() {
        return Ok(None);
    }
    let text = String::from_utf8(bytes).context("clipboard data is binary, not UTF-8 text")?;
    if text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        bail!("clipboard data contains binary control bytes, not text");
    }
    Ok(Some(text))
}

pub fn validate_image(bytes: &[u8], advertised_mime: &str) -> Result<()> {
    crate::image_pipeline::validate_image(bytes, advertised_mime)
}

#[must_use]
pub fn image_mime_from_magic(bytes: &[u8]) -> Option<&'static str> {
    crate::image_pipeline::supported_mime(bytes)
}

fn preferred_mime<'a>(types: impl Iterator<Item = &'a str>) -> Option<&'static str> {
    let available = types
        .map(base_mime)
        .filter(|mime| !mime.is_empty())
        .collect::<Vec<_>>();
    IMAGE_MIME_TYPES
        .iter()
        .copied()
        .find(|preferred| available.iter().any(|mime| mime == preferred))
}

fn base_mime(mime_type: &str) -> &str {
    mime_type.split(';').next().unwrap_or(mime_type).trim()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var_os("XDG_SESSION_TYPE").is_some_and(|value| value == "wayland")
}

#[cfg(target_os = "windows")]
async fn run_powershell(
    script: &str,
    input: Option<&[u8]>,
    max_output: usize,
) -> Result<CommandOutput> {
    let args = ["-NoProfile", "-NonInteractive", "-STA", "-Command", script];
    match run_command("powershell.exe", &args, input, max_output, &[]).await {
        Ok(output) => Ok(output),
        Err(first) => run_command("pwsh", &args, input, max_output, &[])
            .await
            .with_context(|| format!("PowerShell clipboard access failed ({first})")),
    }
}

#[cfg(target_os = "windows")]
async fn write_powershell(script: &str, input: &[u8], input_limit: usize) -> Result<()> {
    if input.len() > input_limit {
        bail!("clipboard content is too large");
    }
    let output = run_powershell(script, Some(input), 1).await?;
    if output.status == Some(0) {
        Ok(())
    } else {
        bail!("PowerShell could not write the clipboard")
    }
}

#[cfg(target_os = "macos")]
fn write_temporary_png(bytes: &[u8]) -> Result<PathBuf> {
    for _ in 0..10 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pi-clipboard-{}-{sequence}.png",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write as _;
                file.write_all(bytes)?;
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).context("could not create a temporary clipboard image");
            }
        }
    }
    bail!("could not allocate a temporary clipboard image")
}

#[cfg(target_os = "macos")]
async fn write_with_fallback(
    commands: &[(&str, &[&str])],
    input: &[u8],
    limit: usize,
) -> Result<()> {
    let owned = commands
        .iter()
        .map(|(program, args)| (*program, args.to_vec()))
        .collect::<Vec<_>>();
    write_owned_fallback(&owned, input, limit).await
}

async fn write_owned_fallback(
    commands: &[(&str, Vec<&str>)],
    input: &[u8],
    limit: usize,
) -> Result<()> {
    if input.len() > limit {
        bail!("clipboard content is too large");
    }
    let mut failures = Vec::new();
    for (program, args) in commands {
        match run_command(program, args, Some(input), 1, &[]).await {
            Ok(output) if output.status == Some(0) => return Ok(()),
            Ok(_) => failures.push(format!("{program} rejected the clipboard content")),
            Err(error) => failures.push(format!("{program}: {error}")),
        }
    }
    bail!("could not write clipboard; {}", failures.join("; "))
}

async fn run_command(
    program: &str,
    args: &[&str],
    input: Option<&[u8]>,
    max_output: usize,
    env: &[(&str, &str)],
) -> Result<CommandOutput> {
    let future = async {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("could not start `{program}`"))?;
        if let Some(input) = input {
            let mut stdin = child
                .stdin
                .take()
                .context("clipboard command stdin was unavailable")?;
            stdin
                .write_all(input)
                .await
                .with_context(|| format!("could not send data to `{program}`"))?;
            drop(stdin);
        }
        let stdout = child
            .stdout
            .take()
            .context("clipboard command stdout was unavailable")?;
        let mut bytes = Vec::new();
        stdout
            .take(
                u64::try_from(max_output)
                    .unwrap_or(u64::MAX)
                    .saturating_add(1),
            )
            .read_to_end(&mut bytes)
            .await
            .with_context(|| format!("could not read `{program}` output"))?;
        if bytes.len() > max_output {
            let _ = child.kill().await;
            let _ = child.wait().await;
            bail!("`{program}` output exceeded the clipboard size limit");
        }
        let status = child
            .wait()
            .await
            .with_context(|| format!("could not wait for `{program}`"))?;
        Ok(CommandOutput {
            status: status.code(),
            stdout: bytes,
        })
    };
    tokio::time::timeout(COMMAND_TIMEOUT, future)
        .await
        .with_context(|| format!("`{program}` timed out"))?
}
