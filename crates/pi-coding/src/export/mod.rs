//! Standalone and live-session HTML/JSONL export.
//!
//! HTML export produces a single self-contained `.html` file with inline CSS
//! and JS — no external asset dependencies. All user/model/tool content is
//! HTML-escaped at render time so no script injection is possible. The
//! rendered transcript is the full chronological record from the session
//! file (every entry in file order, including compaction markers), or the
//! in-memory message list for a live session that has no persisted file.
//!
//! JSONL export writes the current branch (root → leaf) as a valid Pi v3
//! session file suitable for `--resume`.
//!
//! Stored writes are atomic (temp-file-in-same-dir + rename) and errors are
//! contextualized with the target path.

use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use pi_ai::{ContentBlock, Message, ToolCall};
use serde_json::Value;

use crate::session_store::{
    SessionEntry, SessionTree, load_session_tree, load_session_messages,
};

const STYLES: &str = include_str!("styles.css");
const THEME_JS: &str = include_str!("theme.js");

/// Default viewer URL documented for `PI_SHARE_VIEWER_URL`.
pub const DEFAULT_VIEWER_URL: &str = "https://gist.github.com";

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// Colour theme for the exported HTML.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Theme {
    /// Dark background, light text (default).
    #[default]
    Dark,
    /// Light background, dark text.
    Light,
}

/// Optional overrides for the semantic colour variables. Each value must be a
/// 3/4/6/8-digit hex colour (`#rrggbb` etc.) — validated before injection so
/// arbitrary CSS cannot escape the style context.
#[derive(Clone, Debug, Default)]
pub struct CustomColors {
    pub bg: Option<String>,
    pub fg: Option<String>,
    pub muted: Option<String>,
    pub user: Option<String>,
    pub assistant: Option<String>,
    pub tool: Option<String>,
    pub system: Option<String>,
    pub error: Option<String>,
}

/// Export behaviour.
#[derive(Clone, Debug, Default)]
pub struct ExportOptions {
    pub theme: Theme,
    pub custom_colors: CustomColors,
    pub title: Option<String>,
}

/// Lightweight metadata shown in the exported header.
#[derive(Clone, Debug, Default)]
pub struct ExportMetadata {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub timestamp: Option<String>,
    pub model: Option<String>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Export a session file to a self-contained HTML file.
///
/// Reads the full chronological transcript (every entry in file order,
/// including compaction markers). No model, auth, or network access is
/// required.
pub fn export_session_html(
    session_path: &Path,
    output: Option<&Path>,
    options: &ExportOptions,
) -> Result<PathBuf> {
    let tree = load_session_tree(session_path)
        .with_context(|| format!("loading session {}", session_path.display()))?;
    let metadata = metadata_from_tree(&tree);
    let html = render_tree_html(&tree, &metadata, options);
    let out = resolve_output(session_path, output, "html")?;
    atomic_write(&out, &html)
        .with_context(|| format!("writing HTML export {}", out.display()))?;
    Ok(out)
}

/// Export the current branch (root → leaf) of a session to JSONL.
pub fn export_session_jsonl(
    session_path: &Path,
    output: Option<&Path>,
) -> Result<PathBuf> {
    let tree = load_session_tree(session_path)
        .with_context(|| format!("loading session {}", session_path.display()))?;
    let branch_ids: HashSet<String> = tree
        .branch(None)
        .iter()
        .map(|entry| entry.id.clone())
        .collect();
    let jsonl = filter_branch_jsonl(session_path, &branch_ids)?;
    let out = resolve_output(session_path, output, "jsonl")?;
    atomic_write(&out, &jsonl)
        .with_context(|| format!("writing JSONL export {}", out.display()))?;
    Ok(out)
}

/// Export in-memory messages to a self-contained HTML file (live session
/// without a persisted file).
pub fn export_messages_html(
    messages: &[Message],
    metadata: &ExportMetadata,
    output: Option<&Path>,
    options: &ExportOptions,
) -> Result<PathBuf> {
    let html = render_messages_html(messages, metadata, options);
    let fallback = Path::new("session.html");
    let out = resolve_output(output.unwrap_or(fallback), output, "html")?;
    atomic_write(&out, &html)
        .with_context(|| format!("writing HTML export {}", out.display()))?;
    Ok(out)
}

/// Export a live session to HTML, preferring the persisted session file
/// (which preserves compaction markers) and falling back to in-memory
/// history when no file is available or the file is unreadable (e.g. a
/// partial line written mid-turn during a live session).
pub fn export_live_session(
    session: &crate::Session,
    output: Option<&Path>,
    options: &ExportOptions,
) -> Result<PathBuf> {
    if let Some((_id, path)) = session.recorder_info() {
        if path.exists() {
            if let Ok(result) = export_session_html(&path, output, options) {
                return Ok(result);
            }
            // Fall through to in-memory fallback when the file is
            // unreadable (partial write during a live turn).
        }
    }
    let metadata = ExportMetadata {
        session_id: session.recorder_info().map(|(id, _)| id),
        cwd: Some(session.cwd().display().to_string()),
        model: session.model().map(|m| format!("{}/{}", m.provider, m.id)),
        ..ExportMetadata::default()
    };
    export_messages_html(&session.history(), &metadata, output, options)
}

/// Load messages from a session file (convenience for callers that only
/// need the resolved message list).
#[must_use]
pub fn load_messages(session_path: &Path) -> Vec<Message> {
    load_session_messages(session_path).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// HTML rendering
// ---------------------------------------------------------------------------

fn metadata_from_tree(tree: &SessionTree) -> ExportMetadata {
    ExportMetadata {
        session_id: Some(tree.header.id.clone()),
        cwd: Some(tree.header.cwd.display().to_string()),
        timestamp: Some(tree.header.timestamp.clone()),
        model: None,
    }
}

fn render_tree_html(tree: &SessionTree, metadata: &ExportMetadata, options: &ExportOptions) -> String {
    let entries: Vec<&SessionEntry> = tree.entries.iter().collect();
    let body = render_entries(&entries);
    build_html(metadata, &body, options)
}

fn render_messages_html(messages: &[Message], metadata: &ExportMetadata, options: &ExportOptions) -> String {
    let body = render_message_list(messages);
    build_html(metadata, &body, options)
}

fn build_html(metadata: &ExportMetadata, body: &str, options: &ExportOptions) -> String {
    let title = escape_text(
        options
            .title
            .as_deref()
            .unwrap_or("Pi session export"),
    );
    let theme_attr = match options.theme {
        Theme::Light => "data-theme=\"light\"",
        Theme::Dark => "data-theme=\"dark\"",
    };
    let custom_vars = render_custom_colors(&options.custom_colors);
    let meta_html = render_meta(metadata);
    let meta_short = escape_text(metadata.session_id.as_deref().unwrap_or("session"));
    format!(
        "<!DOCTYPE html>\n\
         <html lang=\"en\" {theme_attr}>\n\
         <head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{title}</title>\n\
         <style>\n{styles}\n{custom_vars}\n</style>\n\
         </head>\n\
         <body>\n\
         <div class=\"bar\">\n  <span class=\"bar__title\">{title}</span>\n  <span class=\"bar__meta\">{meta_short}</span>\n  <button class=\"bar__btn\" id=\"theme-toggle\">Dark</button>\n</div>\n\
         <div class=\"meta\">\n{meta_html}\n</div>\n\
         <div class=\"transcript\">\n{body}\n</div>\n\
         <footer>Generated by pi &middot; <a href=\"{viewer}\">{viewer}</a></footer>\n\
         <script>\n{js}\n</script>\n\
         </body>\n</html>\n",
        title = title,
        theme_attr = theme_attr,
        styles = STYLES,
        custom_vars = custom_vars,
        meta_short = meta_short,
        meta_html = meta_html,
        body = body,
        viewer = DEFAULT_VIEWER_URL,
        js = THEME_JS,
    )
}

fn render_meta(metadata: &ExportMetadata) -> String {
    let mut items = Vec::new();
    if let Some(id) = &metadata.session_id {
        items.push(meta_item("Session", id));
    }
    if let Some(cwd) = &metadata.cwd {
        items.push(meta_item("Directory", cwd));
    }
    if let Some(ts) = &metadata.timestamp {
        items.push(meta_item("Started", &format_timestamp(ts)));
    }
    if let Some(model) = &metadata.model {
        items.push(meta_item("Model", model));
    }
    items.join("\n")
}

fn meta_item(label: &str, value: &str) -> String {
    format!(
        "  <div class=\"meta__item\"><span class=\"meta__label\">{}</span><span class=\"meta__value\">{}</span></div>",
        escape_text(label),
        escape_text(value)
    )
}

fn render_entries(entries: &[&SessionEntry]) -> String {
    if entries.is_empty() {
        return "<div class=\"empty\">No transcript entries.</div>".to_owned();
    }
    entries
        .iter()
        .map(|entry| render_entry(entry))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_message_list(messages: &[Message]) -> String {
    if messages.is_empty() {
        return "<div class=\"empty\">No messages.</div>".to_owned();
    }
    messages
        .iter()
        .map(render_message)
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_entry(entry: &SessionEntry) -> String {
    match entry.entry_type.as_str() {
        "message" => entry
            .message
            .as_ref()
            .map(|message| render_message(message))
            .unwrap_or_default(),
        "model_change" => render_system(
            "Model changed",
            &format!(
                "{} / {}",
                entry.provider.as_deref().unwrap_or("?"),
                entry.model_id.as_deref().unwrap_or("?")
            ),
            &entry.timestamp,
        ),
        "thinking_level_change" => render_system(
            "Thinking level",
            entry
                .thinking_level
                .as_deref()
                .unwrap_or("(unset)"),
            &entry.timestamp,
        ),
        "custom_message" => render_custom_entry(entry),
        "compaction" => render_compaction(entry),
        _ => String::new(),
    }
}

fn render_message(message: &Message) -> String {
    match message {
        Message::User(msg) => render_user(msg),
        Message::Assistant(msg) => render_assistant(msg),
        Message::ToolResult(msg) => render_tool_result(msg),
        Message::BashExecution(msg) => render_bash_execution(msg),
        Message::Custom(msg) => render_custom_message(msg),
        Message::BranchSummary(msg) => render_summary_message("Branch summary", &msg.summary, msg.timestamp),
        Message::CompactionSummary(msg) => render_summary_message("Compaction summary", &msg.summary, msg.timestamp),
    }
}

fn render_user(message: &pi_ai::UserMessage) -> String {
    let body = render_content(&message.content);
    entry_html("user", "User", None, None, &body, false)
}

fn render_assistant(message: &pi_ai::AssistantMessage) -> String {
    let mut body = String::new();
    let mut has_visible = false;
    for block in &message.content {
        match block {
            ContentBlock::Text { text, .. } => {
                if !text.is_empty() {
                    body.push_str(&format!("<p>{}</p>", escape_text(text)));
                    has_visible = true;
                }
            }
            ContentBlock::Thinking { thinking, redacted, .. } => {
                if thinking.is_empty() && !redacted {
                    continue;
                }
                let label = if *redacted { "Redacted thinking" } else { "Thinking" };
                let content = if *redacted {
                    "(redacted)".to_owned()
                } else {
                    escape_text(thinking)
                };
                body.push_str(&format!(
                    "<details><summary>{}</summary><div>{}</div></details>",
                    escape_text(label),
                    content
                ));
                has_visible = true;
            }
            ContentBlock::ToolCall(call) => {
                body.push_str(&render_tool_call(call));
                has_visible = true;
            }
            ContentBlock::Image { data, mime_type } => {
                if let Some(img) = render_image(data, mime_type) {
                    body.push_str(&img);
                    has_visible = true;
                }
            }
        }
    }
    if let Some(err) = &message.error_message {
        if !err.is_empty() {
            body.push_str(&format!(
                "<div class=\"entry__error\">Error: {}</div>",
                escape_text(err)
            ));
            has_visible = true;
        }
    }
    if !has_visible {
        body = "<p>(no visible content)</p>".to_owned();
    }
    let model_info = format!("{}/{}", message.provider, message.model);
    entry_html(
        "assistant",
        "Assistant",
        Some(&model_info),
        Some(&format_timestamp_millis(message.timestamp)),
        &body,
        false,
    )
}

fn render_tool_result(message: &pi_ai::ToolResultMessage) -> String {
    let mut body = render_content(&message.content);
    if message.is_error && body.is_empty() {
        body = "<p>(error)</p>".to_owned();
    }
    let role = format!("Tool: {}", message.tool_name);
    entry_html(
        "tool",
        &role,
        None,
        Some(&format_timestamp_millis(message.timestamp)),
        &body,
        message.is_error,
    )
}

fn render_bash_execution(message: &pi_ai::BashExecutionMessage) -> String {
    let mut body = format!("<pre>$ {}</pre>", escape_text(&message.command));
    if !message.output.is_empty() {
        body.push_str(&format!("\n<pre>{}</pre>", escape_text(&message.output)));
    }
    if message.cancelled {
        body.push_str("\n<div class=\"entry__error\">(cancelled)</div>");
    } else if let Some(code) = message.exit_code.filter(|code| *code != 0) {
        body.push_str(&format!("\n<div class=\"entry__error\">(exit {})</div>", code));
    }
    if message.truncated {
        let notice = message.full_output_path.as_deref().map_or_else(
            || "Output truncated.".to_owned(),
            |path| format!("Output truncated. Full output: {path}"),
        );
        body.push_str(&format!("\n<div>{}</div>", escape_text(&notice)));
    }
    let is_error = message.cancelled || message.exit_code.is_some_and(|code| code != 0);
    entry_html(
        "tool",
        "Bash",
        None,
        Some(&format_timestamp_millis(message.timestamp)),
        &body,
        is_error,
    )
}

fn render_custom_message(message: &pi_ai::CustomMessage) -> String {
    if !message.display {
        return String::new();
    }
    let body = render_content(&message.content.to_blocks());
    entry_html(
        "system",
        &message.custom_type,
        None,
        Some(&format_timestamp_millis(message.timestamp)),
        &body,
        false,
    )
}

fn render_custom_entry(entry: &SessionEntry) -> String {
    if entry.display != Some(true) {
        return String::new();
    }
    let body = entry
        .content
        .as_ref()
        .map_or_else(|| "<p>(no content)</p>".to_owned(), |content| render_content(&content.to_blocks()));
    entry_html(
        "system",
        entry.custom_type.as_deref().unwrap_or("Custom"),
        None,
        Some(&format_timestamp(&entry.timestamp)),
        &body,
        false,
    )
}

fn render_summary_message(label: &str, summary: &str, timestamp: i64) -> String {
    entry_html(
        "system",
        label,
        None,
        Some(&format_timestamp_millis(timestamp)),
        &format!("<pre>{}</pre>", escape_text(summary)),
        false,
    )
}

fn render_tool_call(call: &ToolCall) -> String {
    let args_pretty = if call.arguments.is_null() {
        String::new()
    } else {
        serde_json::to_string_pretty(&call.arguments)
            .unwrap_or_else(|_| call.arguments.to_string())
    };
    format!(
        "<div class=\"tool-call\"><span class=\"tool-call__name\">{}</span><span class=\"tool-call__args\">{}</span></div>",
        escape_text(&call.name),
        escape_text(&args_pretty)
    )
}

fn render_image(data: &str, mime_type: &str) -> Option<String> {
    if !is_safe_mime(mime_type) {
        return None;
    }
    Some(format!(
        "<img src=\"data:{};base64,{}\" alt=\"image\">",
        escape_text(mime_type),
        data
    ))
}

fn render_content(content: &[ContentBlock]) -> String {
    let mut parts = Vec::new();
    for block in content {
        match block {
            ContentBlock::Text { text, .. } => {
                if !text.is_empty() {
                    parts.push(format!("<p>{}</p>", escape_text(text)));
                }
            }
            ContentBlock::Image { data, mime_type } => {
                if let Some(img) = render_image(data, mime_type) {
                    parts.push(img);
                }
            }
            ContentBlock::ToolCall(call) => parts.push(render_tool_call(call)),
            ContentBlock::Thinking { .. } => {} // not shown in tool results
        }
    }
    if parts.is_empty() {
        "<p>(no content)</p>".to_owned()
    } else {
        parts.join("\n")
    }
}

fn render_system(label: &str, value: &str, timestamp: &str) -> String {
    let body = format!("<p>{}: {}</p>", escape_text(label), escape_text(value));
    entry_html("system", label, None, Some(&format_timestamp(timestamp)), &body, false)
}

fn render_compaction(entry: &SessionEntry) -> String {
    let summary = entry.summary.as_deref().unwrap_or("(no summary)");
    let body = if let Some(tokens) = entry.tokens_before {
        format!(
            "<p>History compacted ({} tokens before).</p>\n<pre>{}</pre>",
            tokens,
            escape_text(summary)
        )
    } else {
        format!("<p>History compacted.</p>\n<pre>{}</pre>", escape_text(summary))
    };
    let head = format!(
        "<div class=\"entry__head\">\n  <span class=\"entry__role entry__role--system\">Compaction</span>\n  <time class=\"entry__time\">{}</time>\n</div>",
        escape_text(&format_timestamp(&entry.timestamp))
    );
    format!("<article class=\"entry entry--compaction\">\n{head}\n<div class=\"entry__body\">\n{body}\n</div>\n</article>")
}

fn entry_html(
    role_class: &str,
    role_label: &str,
    model: Option<&str>,
    time: Option<&str>,
    body: &str,
    is_error: bool,
) -> String {
    let error_class = if is_error { " entry--error" } else { "" };
    let model_html = model
        .map(|m| format!(" <span class=\"entry__model\">{}</span>", escape_text(m)))
        .unwrap_or_default();
    let time_html = time
        .map(|t| format!(" <time class=\"entry__time\">{}</time>", escape_text(t)))
        .unwrap_or_default();
    format!(
        "<article class=\"entry entry--{role_class}{error_class}\">\n\
         <div class=\"entry__head\">\n  <span class=\"entry__role entry__role--{role_class}\">{role_label}</span>{model}{time}\n</div>\n\
         <div class=\"entry__body\">\n{body}\n</div>\n\
         </article>",
        role_class = escape_text(role_class),
        error_class = error_class,
        role_label = escape_text(role_label),
        model = model_html,
        time = time_html,
        body = body,
    )
}

// ---------------------------------------------------------------------------
// Escaping and validation
// ---------------------------------------------------------------------------

/// Escape text for safe insertion into HTML text content or attribute values.
fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Validate a MIME type for safe embedding in a `data:` URI.
fn is_safe_mime(mime: &str) -> bool {
    if mime.is_empty() || mime.len() > 64 {
        return false;
    }
    mime.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '.' || c == '+' || c == '-')
        && mime.contains('/')
}

/// Validate a hex colour string (`#rgb`, `#rrggbb`, `#rrggbbaa`, etc.).
fn is_hex_color(value: &str) -> bool {
    let s = value.strip_prefix('#').unwrap_or(value);
    let len = s.len();
    matches!(len, 3 | 4 | 6 | 8) && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Render custom colour overrides as `:root` CSS variable assignments.
/// Returns an empty string when no overrides are present.
fn render_custom_colors(colors: &CustomColors) -> String {
    let pairs: Vec<(&str, &str)> = [
        colors.bg.as_deref().map(|v| ("--bg", v)),
        colors.fg.as_deref().map(|v| ("--fg", v)),
        colors.muted.as_deref().map(|v| ("--muted", v)),
        colors.user.as_deref().map(|v| ("--user", v)),
        colors.assistant.as_deref().map(|v| ("--assistant", v)),
        colors.tool.as_deref().map(|v| ("--tool", v)),
        colors.system.as_deref().map(|v| ("--system", v)),
        colors.error.as_deref().map(|v| ("--error", v)),
    ]
    .into_iter()
    .flatten()
    .filter(|(_, v)| is_hex_color(v))
    .collect();
    if pairs.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = pairs
        .into_iter()
        .map(|(name, value)| {
            let hex = value.strip_prefix('#').unwrap_or(value);
            format!("  {name}: #{hex};")
        })
        .collect();
    format!(":root{{\n{}\n}}", lines.join("\n"))
}

// ---------------------------------------------------------------------------
// JSONL branch export
// ---------------------------------------------------------------------------

fn filter_branch_jsonl(path: &Path, branch_ids: &HashSet<String>) -> Result<String> {
    let file = fs::File::open(path).with_context(|| format!("opening session {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line.with_context(|| format!("reading session {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let is_header = value
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| t == "session");
        let in_branch = value
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| branch_ids.contains(id));
        if is_header || in_branch {
            out.push(line);
        }
    }
    Ok(out.join("\n") + "\n")
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve the output path. When `output` is `None`, derive it from the
/// session path by swapping the extension.
fn resolve_output(session_path: &Path, output: Option<&Path>, ext: &str) -> Result<PathBuf> {
    if let Some(out) = output {
        return Ok(out.to_path_buf());
    }
    let stem = session_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "session".to_owned());
    let dir = session_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    Ok(dir.join(format!("{stem}.{ext}")))
}

/// Write `content` to `path` atomically via a temp file in the same directory
/// followed by a rename.
fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    if !dir.as_os_str().is_empty() && !dir.exists() {
        fs::create_dir_all(dir)
            .with_context(|| format!("creating output directory {}", dir.display()))?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "export".to_owned());
    let temp_path = dir.join(format!(".{name}.tmp.{}", std::process::id()));
    if let Err(error) = fs::write(&temp_path, content) {
        let _ = fs::remove_file(&temp_path);
        return Err(error)
            .with_context(|| format!("writing temporary file {}", temp_path.display()));
    }
    if let Err(error) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error).with_context(|| format!("renaming to {}", path.display()));
    }
    Ok(())
}

fn format_timestamp(iso: &str) -> String {
    DateTime::parse_from_rfc3339(iso)
        .map(|dt| {
            dt.with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Secs, true)
        })
        .unwrap_or_else(|_| iso.to_owned())
}

fn format_timestamp_millis(millis: i64) -> String {
    if millis <= 0 {
        return String::new();
    }
    DateTime::from_timestamp_millis(millis)
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_text_replaces_dangerous_chars() {
        assert_eq!(escape_text("a<b>&c\"'d"), "a&lt;b&gt;&amp;c&quot;&#39;d");
    }

    #[test]
    fn escape_text_passes_safe_content() {
        assert_eq!(escape_text("hello world"), "hello world");
    }

    #[test]
    fn is_hex_color_validates() {
        assert!(is_hex_color("#fff"));
        assert!(is_hex_color("#ff0000"));
        assert!(is_hex_color("#ff0000ff"));
        assert!(!is_hex_color("red"));
        assert!(!is_hex_color("#gggggg"));
        assert!(!is_hex_color("javascript:alert(1)"));
    }

    #[test]
    fn is_safe_mime_validates() {
        assert!(is_safe_mime("image/png"));
        assert!(is_safe_mime("image/jpeg"));
        assert!(!is_safe_mime(""));
        assert!(!is_safe_mime("text/html\"><script>"));
        assert!(!is_safe_mime("image"));
    }

    #[test]
    fn render_custom_colors_only_injects_valid_hex() {
        let colors = CustomColors {
            bg: Some("#112233".to_owned()),
            user: Some("not-a-color".to_owned()),
            ..CustomColors::default()
        };
        let css = render_custom_colors(&colors);
        assert!(css.contains("--bg: #112233;"));
        assert!(!css.contains("--user"));
    }

    #[test]
    fn html_contains_no_script_injection() {
        let messages = vec![Message::user_text(
            "<script>alert('xss')</script><img onerror=alert(1) src=x>",
            0,
        )];
        let metadata = ExportMetadata::default();
        let html = render_messages_html(&messages, &metadata, &ExportOptions::default());
        // The dangerous markup must be escaped — no raw <script> or <img> tags
        // from user content survive into the HTML.
        assert!(!html.contains("<script>alert"));
        assert!(!html.contains("<img onerror"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("&lt;img onerror"));
    }

    #[test]
    fn bash_execution_export_escapes_command_output_and_metadata() {
        let messages = vec![Message::BashExecution(pi_ai::BashExecutionMessage {
            command: "printf '<script>'".into(),
            output: "<img onerror=alert(1)>".into(),
            exit_code: Some(7),
            cancelled: false,
            truncated: true,
            full_output_path: Some("/tmp/<full>.log".into()),
            timestamp: 1,
            exclude_from_context: Some(true),
        })];
        let html = render_messages_html(&messages, &ExportMetadata::default(), &ExportOptions::default());
        assert!(html.contains("printf &#39;&lt;script&gt;&#39;"));
        assert!(html.contains("&lt;img onerror=alert(1)&gt;"));
        assert!(html.contains("(exit 7)"));
        assert!(html.contains("Output truncated. Full output: /tmp/&lt;full&gt;.log"));
        assert!(!html.contains("printf '<script>'"));
        assert!(!html.contains("<img onerror"));
    }

    #[test]
    fn custom_message_export_distinguishes_visible_and_suppresses_hidden() {
        let messages = vec![
            Message::Custom(pi_ai::CustomMessage {
                custom_type: "release-note".into(),
                content: "<visible>".into(),
                display: true,
                details: Some(serde_json::json!({"secret":"metadata"})),
                timestamp: 1,
            }),
            Message::Custom(pi_ai::CustomMessage {
                custom_type: "todo-error-reminder".into(),
                content: "hidden reminder".into(),
                display: false,
                details: None,
                timestamp: 2,
            }),
        ];
        let html = render_messages_html(&messages, &ExportMetadata::default(), &ExportOptions::default());
        assert!(html.contains("release-note"));
        assert!(html.contains("&lt;visible&gt;"));
        assert!(!html.contains("todo-error-reminder"));
        assert!(!html.contains("hidden reminder"));
        assert!(!html.contains("metadata"));
    }

    #[test]
    fn persisted_custom_message_export_respects_display() {
        let directory = tempfile::tempdir().expect("temp directory");
        let path = directory.path().join("custom.jsonl");
        fs::write(&path, concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"s1\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n",
            "{\"type\":\"custom_message\",\"id\":\"a\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01Z\",\"customType\":\"visible-note\",\"content\":\"shown\",\"display\":true,\"details\":{\"private\":true}}\n",
            "{\"type\":\"custom_message\",\"id\":\"b\",\"parentId\":\"a\",\"timestamp\":\"2026-01-01T00:00:02Z\",\"customType\":\"hidden-note\",\"content\":\"secret reminder\",\"display\":false}\n"
        )).expect("write session");
        let tree = load_session_tree(&path).expect("load session");
        let html = render_tree_html(&tree, &ExportMetadata::default(), &ExportOptions::default());
        assert!(html.contains("visible-note"));
        assert!(html.contains("shown"));
        assert!(!html.contains("hidden-note"));
        assert!(!html.contains("secret reminder"));
        assert!(!html.contains("private"));
    }

    #[test]
    fn html_is_self_contained() {
        let messages = vec![Message::user_text("hello", 0)];
        let html = render_messages_html(
            &messages,
            &ExportMetadata::default(),
            &ExportOptions::default(),
        );
        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("<style>"));
        assert!(html.contains("<script>"));
    }

    #[test]
    fn jsonl_export_filters_to_branch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        fs::write(
            &path,
            "{\"type\":\"session\",\"version\":3,\"id\":\"s1\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n\
             {\"type\":\"message\",\"id\":\"a\",\"parentId\":null,\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}],\"timestamp\":0}}\n\
             {\"type\":\"message\",\"id\":\"b\",\"parentId\":\"a\",\"timestamp\":\"2024-01-01T00:00:02Z\",\"message\":{\"role\":\"assistant\",\"content\":[],\"api\":\"x\",\"provider\":\"x\",\"model\":\"x\",\"stopReason\":\"stop\",\"timestamp\":0}}\n\
             {\"type\":\"message\",\"id\":\"c\",\"parentId\":\"a\",\"timestamp\":\"2024-01-01T00:00:03Z\",\"message\":{\"role\":\"assistant\",\"content\":[],\"api\":\"x\",\"provider\":\"x\",\"model\":\"y\",\"stopReason\":\"stop\",\"timestamp\":0}}\n",
        )
        .unwrap();
        let tree = load_session_tree(&path).unwrap();
        let branch_ids: HashSet<String> = tree.branch(None).iter().map(|e| e.id.clone()).collect();
        let jsonl = filter_branch_jsonl(&path, &branch_ids).unwrap();
        assert!(jsonl.contains("\"id\":\"a\""));
        assert!(jsonl.contains("\"id\":\"c\""));
        assert!(!jsonl.contains("\"id\":\"b\""));
    }

    #[test]
    fn atomic_write_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.html");
        atomic_write(&path, "<html></html>").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "<html></html>");
        let entries = fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(entries, 1);
    }

    #[test]
    fn resolve_output_derives_from_session() {
        let path = Path::new("/tmp/sessions/2024_test.jsonl");
        let out = resolve_output(path, None, "html").unwrap();
        assert_eq!(out, PathBuf::from("/tmp/sessions/2024_test.html"));
    }

    #[test]
    fn resolve_output_uses_explicit() {
        let out = resolve_output(
            Path::new("/tmp/x.jsonl"),
            Some(Path::new("/tmp/explicit.html")),
            "html",
        )
        .unwrap();
        assert_eq!(out, PathBuf::from("/tmp/explicit.html"));
    }

    #[test]
    fn export_session_html_produces_valid_output() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        fs::write(
            &path,
            "{\"type\":\"session\",\"version\":3,\"id\":\"s1\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n\
             {\"type\":\"message\",\"id\":\"a\",\"parentId\":null,\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hello & <world>\"}],\"timestamp\":0}}\n",
        )
        .unwrap();
        let out = dir.path().join("export.html");
        let result = export_session_html(&path, Some(&out), &ExportOptions::default()).unwrap();
        assert_eq!(result, out);
        let html = fs::read_to_string(&out).unwrap();
        assert!(html.contains("hello &amp; &lt;world&gt;"));
        assert!(!html.contains("hello & <world>"));
        assert!(html.contains("data-theme=\"dark\""));
    }

    #[test]
    fn export_session_jsonl_produces_valid_output() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        fs::write(
            &path,
            "{\"type\":\"session\",\"version\":3,\"id\":\"s1\",\"timestamp\":\"2024-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n\
             {\"type\":\"message\",\"id\":\"a\",\"parentId\":null,\"timestamp\":\"2024-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}],\"timestamp\":0}}\n",
        )
        .unwrap();
        let out = dir.path().join("branch.jsonl");
        let result = export_session_jsonl(&path, Some(&out)).unwrap();
        assert_eq!(result, out);
        let content = fs::read_to_string(&out).unwrap();
        assert!(content.contains("\"type\":\"session\""));
        assert!(content.contains("\"id\":\"a\""));
    }
}