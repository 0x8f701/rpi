//! Context compaction settings, token estimation, cut-point selection, and
//! conversation/file-ops serialization helpers. Port of `coding/compaction.go`
//! (the stateless helpers only; the Session-bound compact/summarize/generate*
//! methods live in `session.rs`).

use std::{collections::BTreeSet, sync::LazyLock};

use pi_ai::{ContentBlock, ContentList, Message, StopReason, Usage};

use crate::resources::utf16_len;

/// Configures automatic context-window compaction (pi `CompactionSettings`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionSettings {
    pub enabled: bool,
    pub reserve_tokens: i64,
    pub keep_recent_tokens: i64,
}

/// File-operation metadata retained alongside a compaction checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionDetails {
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// Result returned by an explicit manual compaction.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionResult {
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_tokens_after: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<CompactionDetails>,
}

/// pi's defaults (`DEFAULT_COMPACTION_SETTINGS`).
pub const DEFAULT_COMPACTION_SETTINGS: CompactionSettings = CompactionSettings {
    enabled: true,
    reserve_tokens: 16384,
    keep_recent_tokens: 20000,
};

/// Per-image char estimate used by the token heuristic (pi `ESTIMATED_IMAGE_CHARS`).
pub const ESTIMATED_IMAGE_CHARS: i64 = 4800;

/// Tool results are truncated to this many characters when serialized for
/// summarization (pi `TOOL_RESULT_MAX_CHARS`).
pub const TOOL_RESULT_MAX_CHARS: i64 = 2000;

/// pi's `SUMMARIZATION_SYSTEM_PROMPT` (utils.ts:168), byte-for-byte.
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI coding assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

/// pi's `SUMMARIZATION_PROMPT`.
pub const SUMMARIZATION_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\nUse this EXACT format:\n\n## Goal\n[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned by user]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Current work]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [Ordered list of what should happen next]\n\n## Critical Context\n- [Any data, examples, or references needed to continue]\n- [Or \"(none)\" if not applicable]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// pi's `UPDATE_SUMMARIZATION_PROMPT` (compaction.ts:487), used when a previous
/// compaction summary exists. Byte-for-byte from the npm build.
pub const UPDATE_SUMMARIZATION_PROMPT: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\nUpdate the existing structured summary with new information. RULES:\n- PRESERVE all existing information from the previous summary\n- ADD new progress, decisions, and context from the new messages\n- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed\n- UPDATE \"Next Steps\" based on what was accomplished\n- PRESERVE exact file paths, function names, and error messages\n- If something is no longer relevant, you may remove it\n\nUse this EXACT format:\n\n## Goal\n[Preserve existing goals, add new ones if the task expanded]\n\n## Constraints & Preferences\n- [Preserve existing, add new ones discovered]\n\n## Progress\n### Done\n- [x] [Include previously done items AND newly completed items]\n\n### In Progress\n- [ ] [Current work - update based on progress]\n\n### Blocked\n- [Current blockers - remove if resolved]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale] (preserve all previous, add new)\n\n## Next Steps\n1. [Update based on current state]\n\n## Critical Context\n- [Preserve important context, add new if needed]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// pi's `TURN_PREFIX_SUMMARIZATION_PROMPT` (compaction.ts:725), used for the
/// prefix of a split turn. Byte-for-byte.
pub const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = "This is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.\n\nSummarize the prefix to provide context for the retained suffix:\n\n## Original Request\n[What did the user ask for in this turn?]\n\n## Early Progress\n- [Key decisions and work done in the prefix]\n\n## Context for Suffix\n- [Information needed to understand the retained recent work]\n\nBe concise. Focus on what's needed to understand the kept suffix.";

/// Estimates the token cost of a message (pi `estimateTokens`: char count / 4,
/// rounded up).
pub fn estimate_message_tokens(m: &Message) -> i64 {
    let chars = match m {
        Message::User(um) => content_chars(&um.content),
        Message::Assistant(am) => assistant_chars(&am.content),
        Message::ToolResult(tr) => content_chars(&tr.content),
        Message::BashExecution(bash) => utf16_len(&bash.command) + utf16_len(&bash.output),
        Message::Custom(custom) => content_chars(&custom.content.to_blocks()),
        Message::BranchSummary(summary) => utf16_len(&summary.summary),
        Message::CompactionSummary(summary) => utf16_len(&summary.summary),
    };
    (chars + 3) / 4 // ceil(chars / 4) for non-negative chars
}

fn assistant_chars(content: &ContentList) -> i64 {
    let mut chars = 0;
    for c in content {
        match c {
            ContentBlock::Text { text, .. } => chars += utf16_len(text),
            ContentBlock::Thinking { thinking, .. } => chars += utf16_len(thinking),
            ContentBlock::ToolCall(tc) => {
                let args = serde_json::to_string(&tc.arguments).map_or(0, |s| utf16_len(&s));
                chars += utf16_len(&tc.name) + args;
            }
            _ => {}
        }
    }
    chars
}

fn content_chars(content: &ContentList) -> i64 {
    let mut chars = 0;
    for c in content {
        match c {
            ContentBlock::Text { text, .. } => chars += utf16_len(text),
            ContentBlock::Image { .. } => chars += ESTIMATED_IMAGE_CHARS, // fixed estimate per inline image
            _ => {}
        }
    }
    chars
}

/// Sums estimated tokens across messages (pure heuristic).
pub fn estimate_context_tokens(messages: &[Message]) -> i64 {
    messages.iter().map(|m| estimate_message_tokens(m)).sum()
}

/// Blends the real token usage reported by the last assistant turn with a
/// heuristic estimate of the trailing messages (pi `estimateContextTokens`).
/// Far more accurate than the pure char/4 heuristic on large contexts. Skips
/// aborted/error and all-zero-usage messages when picking the usage anchor.
pub fn estimate_context_tokens_usage_aware(messages: &[Message]) -> i64 {
    let mut last_idx: i64 = -1;
    let mut last_usage_total = 0i64;
    for (i, m) in messages.iter().enumerate() {
        let Message::Assistant(am) = m else { continue };
        if am.stop_reason == StopReason::Aborted || am.stop_reason == StopReason::Error {
            continue;
        }
        let t = context_tokens_from_usage(&am.usage);
        if t > 0 {
            last_idx = i as i64;
            last_usage_total = t;
        }
    }
    if last_idx == -1 {
        return estimate_context_tokens(messages);
    }
    let mut total = last_usage_total;
    for m in messages.iter().skip(last_idx as usize + 1) {
        total += estimate_message_tokens(m);
    }
    total
}

fn context_tokens_from_usage(u: &Usage) -> i64 {
    if u.total_tokens > 0 {
        return u.total_tokens;
    }
    u.input + u.output + u.cache_read + u.cache_write
}

/// Reports whether the context exceeds the safe budget.
pub fn should_compact(context_tokens: i64, context_window: i64, s: &CompactionSettings) -> bool {
    if !s.enabled || context_window <= 0 {
        return false;
    }
    context_tokens > context_window - s.reserve_tokens
}

static CONTEXT_OVERFLOW_PATTERNS: LazyLock<Vec<regex::Regex>> = LazyLock::new(|| {
    [
        r"prompt is too long", r"request_too_large", r"input is too long for requested model",
        r"exceeds the context window", r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))",
        r"input token count.*exceeds the maximum", r"maximum prompt length is \d+", r"reduce the length of the messages",
        r"maximum context length is \d+ tokens", r"exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?",
        r"input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)", r"exceeds the limit of \d+",
        r"exceeds the available context size", r"greater than the context length", r"context window exceeds limit",
        r"exceeded model token limit", r"too large for model with \d+ maximum context length",
        r"prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?", r"model_context_window_exceeded",
        r"prompt too long; exceeded (?:max )?context length", r"range of input length should be", r"context[_ ]length[_ ]exceeded",
        r"too many tokens", r"token limit exceeded", r"^4(?:00|13)\s*(?:status code)?\s*\(no body\)",
    ]
    .into_iter()
    .map(|pattern| regex::RegexBuilder::new(pattern).case_insensitive(true).build().expect("valid overflow pattern"))
    .collect()
});

static NON_OVERFLOW_PATTERNS: LazyLock<Vec<regex::Regex>> = LazyLock::new(|| {
    [r"^(Throttling error|Service unavailable):", r"rate limit", r"too many requests"]
        .into_iter()
        .map(|pattern| regex::RegexBuilder::new(pattern).case_insensitive(true).build().expect("valid non-overflow pattern"))
        .collect()
});

static RETRYABLE_ERROR_PATTERNS: LazyLock<Vec<regex::Regex>> = LazyLock::new(|| {
    [
        r"overloaded", r"rate.?limit", r"too many requests", r"429", r"500", r"502", r"503", r"504", r"524",
        r"service.?unavailable", r"server.?error", r"internal.?error", r"provider.?returned.?error", r"network.?error",
        r"connection.?error", r"connection.?refused", r"connection.?lost", r"other side closed", r"fetch failed",
        r"getaddrinfo", r"ENOTFOUND", r"EAI_AGAIN", r"upstream.?connect", r"reset before headers", r"socket hang up",
        r"socket connection was closed", r"timed? out", r"timeout", r"terminated", r"websocket.?closed", r"websocket.?error",
        r"ended without", r"stream ended before message_stop", r"stream ended before a terminal response event",
        r"http2 request did not get a response", r"retry delay", r"you can retry your request", r"try your request again",
        r"please retry your request", r"ResourceExhausted",
    ]
    .into_iter()
    .map(|pattern| regex::RegexBuilder::new(pattern).case_insensitive(true).build().expect("valid retry pattern"))
    .collect()
});

static NON_RETRYABLE_LIMIT_PATTERNS: LazyLock<Vec<regex::Regex>> = LazyLock::new(|| {
    [
        r"GoUsageLimitError", r"FreeUsageLimitError", r"Monthly usage limit reached", r"available balance",
        r"insufficient_quota", r"out of budget", r"quota exceeded", r"billing",
    ]
    .into_iter()
    .map(|pattern| regex::RegexBuilder::new(pattern).case_insensitive(true).build().expect("valid limit pattern"))
    .collect()
});

/// Returns whether an assistant result indicates a context-window overflow.
pub fn is_context_overflow(message: &pi_ai::AssistantMessage, context_window: i64) -> bool {
    if message.stop_reason == StopReason::Error
        && let Some(error) = message.error_message.as_deref()
        && !NON_OVERFLOW_PATTERNS.iter().any(|pattern| pattern.is_match(error))
        && CONTEXT_OVERFLOW_PATTERNS.iter().any(|pattern| pattern.is_match(error))
    {
        return true;
    }

    let input_tokens = message.usage.input + message.usage.cache_read;
    if context_window > 0 && message.stop_reason == StopReason::Stop && input_tokens > context_window {
        return true;
    }
    context_window > 0
        && message.stop_reason == StopReason::Length
        && message.usage.output == 0
        && input_tokens as f64 >= context_window as f64 * 0.99
}

/// Classifies transient provider and transport errors for bounded retry.
pub fn is_retryable_assistant_error(message: &pi_ai::AssistantMessage) -> bool {
    let Some(error) = message.error_message.as_deref().filter(|_| message.stop_reason == StopReason::Error) else {
        return false;
    };
    !NON_RETRYABLE_LIMIT_PATTERNS.iter().any(|pattern| pattern.is_match(error))
        && RETRYABLE_ERROR_PATTERNS.iter().any(|pattern| pattern.is_match(error))
}

/// Result of `find_cut_point` (pi `CutPointResult`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CutPointResult {
    /// Index of the first message to keep.
    pub first_kept_index: usize,
    /// The user message starting the turn being split, or `None`.
    pub turn_start_index: Option<usize>,
    /// True when the cut lands mid-turn (not on a user message).
    pub is_split_turn: bool,
}

/// Ports pi's `findCutPoint` + `findValidCutPoints` to a flat message list. Valid
/// cut points are any non-tool-result message (a kept run must never start on a
/// tool result). Walking backwards from the newest message, tokens accumulate
/// until `keep_recent_tokens` is reached; the cut then snaps FORWARD to the
/// first valid cut point at or after the crossing index (so a boundary
/// tool-result goes into the summarized portion). If the budget is never
/// reached, the cut defaults to the first valid cut point (keep everything).
/// Only messages in `[start, end)` are considered.
pub fn find_cut_point(messages: &[Message], start: usize, end: usize, keep_recent_tokens: i64) -> CutPointResult {
    let mut cut_points: Vec<usize> = Vec::new();
    for i in start..end {
        if !is_tool_result(&messages[i]) {
            cut_points.push(i);
        }
    }
    if cut_points.is_empty() {
        return CutPointResult { first_kept_index: start, turn_start_index: None, is_split_turn: false };
    }

    // Walk backwards from newest, accumulating estimated message sizes.
    let mut acc: i64 = 0;
    let mut cut_index = cut_points[0]; // default: keep from first message
    for i in (start..end).rev() {
        acc += estimate_message_tokens(&messages[i]);
        if acc >= keep_recent_tokens {
            // Snap to the closest valid cut point at or after this index.
            for &c in &cut_points {
                if c >= i {
                    cut_index = c;
                    break;
                }
            }
            break;
        }
    }

    // A cut not on a turn-start message splits the turn started by the nearest
    // preceding user-like message. Bash executions begin turns upstream.
    let cut_is_turn_start = is_turn_start(&messages[cut_index]);
    let mut turn_start: Option<usize> = None;
    if !cut_is_turn_start {
        for i in (start..=cut_index).rev() {
            if is_turn_start(&messages[i]) {
                turn_start = Some(i);
                break;
            }
        }
    }
    CutPointResult {
        first_kept_index: cut_index,
        turn_start_index: turn_start,
        is_split_turn: !cut_is_turn_start && turn_start.is_some(),
    }
}

fn is_turn_start(m: &Message) -> bool {
    matches!(m, Message::User(_) | Message::BashExecution(_) | Message::Custom(_) | Message::BranchSummary(_) | Message::CompactionSummary(_))
}

fn is_tool_result(m: &Message) -> bool {
    matches!(m, Message::ToolResult(_))
}

/// Builds the compacted view: the checkpoint summary message followed by the
/// messages after `prefix_len` (pi: compaction entry + kept entries).
pub fn apply_checkpoint(summary: &str, messages: &[Message], prefix_len: usize) -> Vec<Message> {
    let timestamp = chrono::Utc::now().timestamp_millis();
    let checkpoint = Message::CompactionSummary(pi_ai::CompactionSummaryMessage {
        summary: summary.to_owned(),
        tokens_before: 0,
        timestamp,
    });
    let mut out = Vec::with_capacity(1 + messages.len().saturating_sub(prefix_len));
    out.push(checkpoint);
    out.extend(messages[prefix_len..].iter().cloned());
    out
}

/// Converts stored agent messages to provider-compatible LLM roles.
pub fn messages_as_llm(messages: &[Message]) -> Vec<Message> {
    pi_ai::messages_to_llm(messages)
}

/// Serializes LLM messages to text for summarization so the model treats it as
/// content to summarize, not a conversation to continue (pi `serializeConversation`).
/// Tool results are truncated to `TOOL_RESULT_MAX_CHARS`.
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for m in messages {
        match m {
            Message::User(um) => {
                let c = text_of(&um.content);
                if !c.is_empty() {
                    parts.push(format!("[User]: {c}"));
                }
            }
            Message::Assistant(am) => parts.extend(serialize_assistant(&am.content)),
            Message::ToolResult(tr) => {
                let c = text_of(&tr.content);
                if !c.is_empty() {
                    parts.push(format!("[Tool result]: {}", truncate_for_summary(&c, TOOL_RESULT_MAX_CHARS)));
                }
            }
            Message::BashExecution(_) | Message::Custom(_) | Message::BranchSummary(_) | Message::CompactionSummary(_) => unreachable!("messages_as_llm projects session messages before serialization"),
        }
    }
    parts.join("\n\n")
}

fn serialize_assistant(content: &ContentList) -> Vec<String> {
    let mut text_parts: Vec<String> = Vec::new();
    let mut thinking_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<String> = Vec::new();
    for c in content {
        match c {
            ContentBlock::Text { text, .. } => text_parts.push(text.clone()),
            ContentBlock::Thinking { thinking, .. } => thinking_parts.push(thinking.clone()),
            ContentBlock::ToolCall(tc) => {
                // JS Object.entries preserves insertion order; serde_json::Value
                // objects use a BTreeMap (sorted keys), matching Go's sorted
                // json.Marshal, so the serialization is stable.
                let entries = tc.arguments.as_object().map(|obj| {
                    let mut keys: Vec<&String> = obj.keys().collect();
                    keys.sort();
                    keys.iter()
                        .filter_map(|k| {
                            obj.get(*k)
                                .and_then(|v| serde_json::to_string(v).ok())
                                .map(|v| format!("{}={v}", k))
                        })
                        .collect::<Vec<String>>()
                });
                match entries {
                    Some(e) if !e.is_empty() => tool_calls.push(format!("{}({})", tc.name, e.join(", "))),
                    _ => tool_calls.push(format!("{}()", tc.name)),
                }
            }
            _ => {}
        }
    }
    let mut parts: Vec<String> = Vec::new();
    if !thinking_parts.is_empty() {
        parts.push(format!("[Assistant thinking]: {}", thinking_parts.join("\n")));
    }
    if !text_parts.is_empty() {
        parts.push(format!("[Assistant]: {}", text_parts.join("\n")));
    }
    if !tool_calls.is_empty() {
        parts.push(format!("[Assistant tool calls]: {}", tool_calls.join("; ")));
    }
    parts
}

fn text_of(content: &ContentList) -> String {
    let mut b = String::new();
    for c in content {
        if let ContentBlock::Text { text, .. } = c {
            b.push_str(text);
        }
    }
    b
}

/// Truncates text to `max_chars` UTF-16 code units, appending a marker (pi
/// `truncateForSummary`; JS `.length`/`.slice` count UTF-16 units). A surrogate
/// pair on the boundary is dropped whole rather than split, so the output is
/// always valid UTF-8.
pub fn truncate_for_summary(text: &str, max_chars: i64) -> String {
    let length = utf16_len(text);
    if length <= max_chars {
        return text.to_string();
    }
    format!(
        "{}\n\n[... {} more characters truncated]",
        slice_utf16(text, max_chars),
        length - max_chars
    )
}

/// Returns the longest prefix of `s` holding at most `n` UTF-16 code units
/// without splitting a rune (an astral rune counts as 2 units and is excluded
/// entirely when it straddles the boundary).
fn slice_utf16(s: &str, n: i64) -> &str {
    let mut units: i64 = 0;
    for (i, r) in s.char_indices() {
        let w = if r as u32 > 0xFFFF { 2 } else { 1 };
        if units + w > n {
            return &s[..i];
        }
        units += w;
    }
    s
}

// ---------------------------------------------------------------------------
// File ops
// ---------------------------------------------------------------------------

/// pi's `FileOperations` (utils.ts `createFileOps`).
#[derive(Default)]
struct FileOps {
    read: BTreeSet<String>,
    written: BTreeSet<String>,
    edited: BTreeSet<String>,
}

impl FileOps {
    /// Computes the final sorted file lists: modified = edited ∪ written;
    /// readFiles excludes any file that was also modified.
    fn lists(&self) -> (Vec<String>, Vec<String>) {
        let mut modified: BTreeSet<String> = self.edited.iter().cloned().collect();
        for f in &self.written {
            modified.insert(f.clone());
        }
        let read_files: Vec<String> = self
            .read
            .iter()
            .filter(|f| !modified.contains(*f))
            .cloned()
            .collect();
        let modified_files: Vec<String> = modified.iter().cloned().collect();
        // BTreeSet iteration is already sorted ascending.
        (read_files, modified_files)
    }
}

/// Collects file paths from read/write/edit tool calls in an assistant message
/// (pi `extractFileOpsFromMessage`).
fn extract_file_ops_from_message(m: &Message, ops: &mut FileOps) {
    let Message::Assistant(am) = m else { return };
    for c in &am.content {
        let ContentBlock::ToolCall(tc) = c else { continue };
        let Some(path) = tc.arguments.get("path").and_then(|v| v.as_str()) else { continue };
        if path.is_empty() {
            continue;
        }
        match tc.name.as_str() {
            "read" => {
                ops.read.insert(path.to_string());
            }
            "write" => {
                ops.written.insert(path.to_string());
            }
            "edit" => {
                ops.edited.insert(path.to_string());
            }
            _ => {}
        }
    }
}

/// Derives the read-only and modified file lists from read/edit/write tool calls
/// in the given messages.
pub fn compute_file_lists(messages: &[Message]) -> (Vec<String>, Vec<String>) {
    let mut ops = FileOps::default();
    for m in messages {
        extract_file_ops_from_message(m, &mut ops);
    }
    ops.lists()
}

/// Formats read/modified file lists as XML tags appended to the summary (pi
/// `formatFileOperations`).
pub fn format_file_operations(read_files: &[String], modified_files: &[String]) -> String {
    let mut sections: Vec<String> = Vec::new();
    if !read_files.is_empty() {
        sections.push(format!("<read-files>\n{}\n</read-files>", read_files.join("\n")));
    }
    if !modified_files.is_empty() {
        sections.push(format!("<modified-files>\n{}\n</modified-files>", modified_files.join("\n")));
    }
    if sections.is_empty() {
        return String::new();
    }
    format!("\n\n{}", sections.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::{AssistantMessage, ContentBlock, Message, StopReason, ToolCall, ToolResultMessage, Usage};

    fn user_text(text: &str, ts: i64) -> Message {
        Message::user_text(text, ts)
    }

    /// Builds an assistant message with empty Option fields (matching pi's defaults).
    fn am(content: Vec<ContentBlock>, stop_reason: StopReason, usage: Usage, ts: i64) -> AssistantMessage {
        AssistantMessage {
            content,
            api: String::new(),
            provider: String::new(),
            model: String::new(),
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage,
            stop_reason,
            error_message: None,
            raw_stop_reason: None,
            timestamp: ts,
        }
    }
    fn assistant_text(text: &str, ts: i64) -> Message {
        Message::Assistant(am(
            vec![ContentBlock::Text { text: text.to_string(), text_signature: None }],
            StopReason::Stop,
            Usage::default(),
            ts,
        ))
    }

    #[test]
    fn summarization_system_prompt_byte_for_byte() {
        let want = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI coding assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";
        assert_eq!(SUMMARIZATION_SYSTEM_PROMPT, want);
    }

    #[test]
    fn estimate_image_chars() {
        assert_eq!(ESTIMATED_IMAGE_CHARS, 4800);
        let content = vec![
            ContentBlock::Image { data: "abc".to_string(), mime_type: "image/png".to_string() },
            ContentBlock::Text { text: "hello".to_string(), text_signature: None },
        ];
        assert_eq!(content_chars(&content), 4805);
        let msg = Message::User(pi_ai::UserMessage {
            content: vec![ContentBlock::Image { data: "x".to_string(), mime_type: "image/png".to_string() }],
            timestamp: 1,
        });
        assert_eq!(estimate_message_tokens(&msg), 1200, "ceil(4800/4)");
    }

    #[test]
    fn bash_execution_counts_raw_content_and_projects_for_summarization() {
        let visible = Message::BashExecution(pi_ai::BashExecutionMessage {
            command: "echo ok".into(), output: "ok".into(), exit_code: Some(0),
            cancelled: false, truncated: false, full_output_path: None,
            timestamp: 1, exclude_from_context: None,
        });
        let excluded = Message::BashExecution(pi_ai::BashExecutionMessage {
            command: "secret".into(), output: "hidden".into(), exit_code: Some(0),
            cancelled: false, truncated: false, full_output_path: None,
            timestamp: 2, exclude_from_context: Some(true),
        });
        assert_eq!(estimate_message_tokens(&visible), 3);
        let llm = messages_as_llm(&[visible, excluded]);
        assert_eq!(llm, vec![Message::user_text("Ran `echo ok`\n```\nok\n```", 1)]);
        assert_eq!(serialize_conversation(&llm), "[User]: Ran `echo ok`\n```\nok\n```");
    }

    #[test]
    fn estimates_text_with_javascript_utf16_lengths() {
        assert_eq!(estimate_message_tokens(&Message::user_text("αβ", 1)), 1);
        assert_eq!(estimate_message_tokens(&Message::user_text("😀😀😀", 1)), 2);

        let model = pi_ai::Model::default();
        let mut assistant = pi_ai::AssistantMessage::pending(&model);
        assistant.content = vec![
            ContentBlock::text("😀"),
            ContentBlock::thinking("αβ"),
            ContentBlock::ToolCall(ToolCall {
                id: "t".to_owned(),
                name: "αβ😀".to_owned(),
                arguments: serde_json::json!({"é": "😀"}),
                thought_signature: None,
            }),
        ];
        let expected_chars = utf16_len("😀")
            + utf16_len("αβ")
            + utf16_len("αβ😀")
            + utf16_len(&serde_json::to_string(&serde_json::json!({"é": "😀"})).unwrap());
        assert_eq!(estimate_message_tokens(&Message::Assistant(assistant)), (expected_chars + 3) / 4);
    }

    #[test]
    fn should_compact_threshold() {
        let s = CompactionSettings { enabled: true, reserve_tokens: 16384, keep_recent_tokens: 20000 };
        assert!(!should_compact(100, 200000, &s), "small context should not compact");
        assert!(should_compact(190000, 200000, &s), "above window-reserve should compact");
        let off = CompactionSettings { enabled: false, reserve_tokens: 16384, keep_recent_tokens: 20000 };
        assert!(!should_compact(190000, 200000, &off), "disabled must never trigger");
        // reserve larger than window: RHS negative → any positive context compacts.
        let big_reserve = CompactionSettings { enabled: true, reserve_tokens: 200, keep_recent_tokens: 20000 };
        assert!(should_compact(50, 100, &big_reserve), "reserve>window must still compact positive context");
    }

    #[test]
    fn classifies_overflow_and_transient_errors() {
        let model = pi_ai::Model::default();
        let mut overflow = pi_ai::AssistantMessage::pending(&model);
        overflow.stop_reason = StopReason::Error;
        overflow.error_message = Some("Your input exceeds the context window of this model".to_owned());
        assert!(is_context_overflow(&overflow, 100_000));
        overflow.error_message = Some("Throttling error: Too many tokens, please retry".to_owned());
        assert!(!is_context_overflow(&overflow, 100_000));

        let mut silent = pi_ai::AssistantMessage::pending(&model);
        silent.stop_reason = StopReason::Stop;
        silent.usage.input = 101;
        assert!(is_context_overflow(&silent, 100));

        let mut transient = pi_ai::AssistantMessage::pending(&model);
        transient.stop_reason = StopReason::Error;
        transient.error_message = Some("503 Service unavailable".to_owned());
        assert!(is_retryable_assistant_error(&transient));
        transient.error_message = Some("429 insufficient_quota: billing limit".to_owned());
        assert!(!is_retryable_assistant_error(&transient));
    }

    #[test]
    fn find_cut_point_snaps_forward() {
        let big = "x".repeat(4000); // ~1000 tokens each
        let messages = vec![
            user_text(&big, 1),
            Message::Assistant(am(
                vec![ContentBlock::ToolCall(ToolCall {
                    id: "t".to_string(),
                    name: "read".to_string(),
                    arguments: serde_json::json!({}),
                    thought_signature: None,
                })],
                StopReason::ToolUse,
                Usage::default(),
                2,
            )),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "t".to_string(),
                tool_name: "read".to_string(),
                content: vec![ContentBlock::Text { text: big.clone(), text_signature: None }],
                usage: None,
                details: None,
                added_tool_names: Vec::new(),
                is_error: false,
                timestamp: 3,
            }),
            user_text(&big, 4),
        ];
        // Walking back: user@3 (1000) then toolResult@2 (1000) crosses 1500 at index 2.
        // First cut point >= 2 is the user at index 3 (forward snap).
        let cp = find_cut_point(&messages, 0, messages.len(), 1500);
        assert_eq!(cp.first_kept_index, 3, "expected forward snap to index 3");
        assert!(!cp.is_split_turn, "cut on a user message must not be a split turn");
    }

    #[test]
    fn find_cut_point_keep_everything_edge() {
        let messages = vec![user_text("a", 1), assistant_text("b", 2)];
        let cp = find_cut_point(&messages, 0, messages.len(), 1_000_000);
        assert_eq!(cp.first_kept_index, 0, "expected keep-everything cut at 0");

        // No valid cut points (all tool results) => startIndex.
        let tr = vec![Message::ToolResult(ToolResultMessage {
            tool_call_id: "t".to_string(),
            tool_name: "read".to_string(),
            content: vec![ContentBlock::Text { text: "x".to_string(), text_signature: None }],
            usage: None,
            details: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: 1,
        })];
        let cp = find_cut_point(&tr, 0, tr.len(), 1);
        assert_eq!(cp.first_kept_index, 0);
        assert!(!cp.is_split_turn);
    }

    #[test]
    fn find_cut_point_split_turn() {
        let big = "x".repeat(4000);
        let messages = vec![
            user_text(&big, 1),
            assistant_text(&big, 2),
            user_text(&big, 3),
            assistant_text(&big, 4),
        ];
        // keep 400: crossing at the last assistant (index 3), a valid cut point itself.
        let cp = find_cut_point(&messages, 0, messages.len(), 400);
        assert_eq!(cp.first_kept_index, 3);
        assert!(cp.is_split_turn);
        assert_eq!(cp.turn_start_index, Some(2));
    }

    #[test]
    fn usage_estimate_skips_aborted_and_error() {
        let valid = Message::Assistant(am(
            vec![ContentBlock::Text { text: "ok".to_string(), text_signature: None }],
            StopReason::Stop,
            Usage { total_tokens: 50000, ..Usage::default() },
            1,
        ));
        let aborted = Message::Assistant(am(
            vec![ContentBlock::Text { text: "partial".to_string(), text_signature: None }],
            StopReason::Aborted,
            Usage { total_tokens: 70000, ..Usage::default() },
            2,
        ));
        let mut errored = am(vec![], StopReason::Error, Usage { total_tokens: 90000, ..Usage::default() }, 3);
        errored.error_message = Some("boom".to_string());
        let errored = Message::Assistant(errored);
        let got = estimate_context_tokens_usage_aware(&[valid.clone(), aborted.clone(), errored.clone()]);
        assert!(got >= 50000 && got < 70000, "anchor at valid usage 50000 (+trailing), got {got}");

        let got2 = estimate_context_tokens_usage_aware(&[user_text("short", 1), aborted, errored]);
        assert!(got2 < 1000, "aborted/error must not anchor: {got2}");
    }

    #[test]
    fn usage_estimate_skips_all_zero_usage() {
        let valid = Message::Assistant(am(
            vec![ContentBlock::Text { text: "ok".to_string(), text_signature: None }],
            StopReason::Stop,
            Usage { total_tokens: 50000, ..Usage::default() },
            1,
        ));
        let zero_usage = Message::Assistant(am(
            vec![ContentBlock::Text { text: "huge response with no usage".to_string(), text_signature: None }],
            StopReason::Stop,
            Usage::default(),
            2,
        ));
        let got = estimate_context_tokens_usage_aware(&[valid, zero_usage.clone()]);
        assert!(got >= 50000, "all-zero must not override valid anchor: {got}");

        let only = vec![user_text("short", 1), zero_usage];
        let got = estimate_context_tokens_usage_aware(&only);
        let pure = estimate_context_tokens(&only);
        assert_eq!(got, pure, "all-zero usage must fall back to pure heuristic ({pure}), got {got}");
    }

    #[test]
    fn truncate_for_summary_utf16() {
        // 1500 two-byte runes = 3000 bytes but only 1500 UTF-16 units: no truncation.
        let input = "é".repeat(1500);
        assert_eq!(truncate_for_summary(&input, 2000), input);

        // 1999 ASCII + astral rune (2 units) = 2001 units: truncate 1 unit; the
        // surrogate pair on the boundary is dropped whole, never split.
        let input = "a".repeat(1999) + "\u{1F648}";
        let got = truncate_for_summary(&input, 2000);
        let want = "a".repeat(1999) + "\n\n[... 1 more characters truncated]";
        assert_eq!(got, want);

        // Plain over-limit keeps exactly maxChars units.
        let input = "b".repeat(2500);
        let got = truncate_for_summary(&input, 2000);
        assert!(got.starts_with(&"b".repeat(2000)));
        assert!(got.ends_with("[... 500 more characters truncated]"));
    }

    #[test]
    fn usage_aware_token_estimate() {
        let messages = vec![
            user_text("short", 1),
            Message::Assistant(am(
                vec![ContentBlock::Text { text: "ok".to_string(), text_signature: None }],
                StopReason::Stop,
                Usage { total_tokens: 50000, ..Usage::default() },
                2,
            )),
            user_text("a follow-up question", 3),
        ];
        let got = estimate_context_tokens_usage_aware(&messages);
        assert!(got >= 50000, "usage-aware should be >= reported 50000, got {got}");
        let pure = estimate_context_tokens(&messages);
        assert!(got > pure * 10, "usage-aware ({got}) should dominate pure ({pure})");
    }

    #[test]
    fn default_compaction_settings() {
        assert!(DEFAULT_COMPACTION_SETTINGS.enabled);
        assert_eq!(DEFAULT_COMPACTION_SETTINGS.reserve_tokens, 16384);
        assert_eq!(DEFAULT_COMPACTION_SETTINGS.keep_recent_tokens, 20000);
    }

    #[test]
    fn file_ops_lists_and_format() {
        // read a.go, edit b.go, write b.go (modified), read c.go (read-only).
        let mk = |name: &str, args: serde_json::Value| {
            Message::Assistant(am(
                vec![ContentBlock::ToolCall(ToolCall {
                    id: String::new(),
                    name: name.to_string(),
                    arguments: args,
                    thought_signature: None,
                })],
                StopReason::Stop,
                Usage::default(),
                0,
            ))
        };
        let messages = vec![
            mk("read", serde_json::json!({ "path": "/a/a.go" })),
            mk("read", serde_json::json!({ "path": "/b/b.go" })),
            mk("edit", serde_json::json!({ "path": "/b/b.go" })),
            mk("write", serde_json::json!({ "path": "/b/b.go" })),
            mk("read", serde_json::json!({ "path": "/c/c.go" })),
        ];
        let (read, modified) = compute_file_lists(&messages);
        assert_eq!(read, vec!["/a/a.go".to_string(), "/c/c.go".to_string()]);
        assert_eq!(modified, vec!["/b/b.go".to_string()]);
        let f = format_file_operations(&read, &modified);
        assert!(f.contains("<read-files>\n/a/a.go\n/c/c.go\n</read-files>"));
        assert!(f.contains("<modified-files>\n/b/b.go\n</modified-files>"));
    }
}