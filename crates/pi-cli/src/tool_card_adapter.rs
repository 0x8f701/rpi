//! Ratatui-neutral tool-card rows reduced from application events.

use pi_ai::BashExecutionMessage;
use pi_coding::markdown::{display_width, fit_text};
use pi_coding::redact::{redact_bounded, redact_secrets};
use pi_coding::{
    ApplicationEvent, ToolCallViewStatus, ToolCard, ToolPresentationState, compact_tool_arguments,
    redact_value,
};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCardRowRole {
    Command,
    Content,
    Details,
    Status,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCardRow {
    pub tool_call_id: String,
    pub role: ToolCardRowRole,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCardRows {
    pub tool_call_id: String,
    pub tool_name: String,
    pub ordinal: u64,
    pub arguments_summary: String,
    /// Redacted one-line headline derived from [`ToolCardSummary`]: path/counts
    /// for read/write/edit, `{query} · N matches · M files` for grep/ast_grep,
    /// compact phase/task counts for todo, otherwise `{title} {arguments}`.
    /// TUI compact titles can render this instead of reconstructing from rows.
    /// `card_rows` always fills it; serde defaults empty for older payloads.
    #[serde(default)]
    pub summary: String,
    pub code_language: Option<String>,
    pub status: ToolCallViewStatus,
    pub is_partial: bool,
    pub is_error: bool,
    pub cancelled: bool,
    pub truncated: bool,
    pub omitted_content_lines: usize,
    pub rows: Vec<ToolCardRow>,
    /// Frontmatter `name` of the skill read when the card is a `skill://` read
    /// rendered as prose (title + description paragraph + body). `None` for
    /// every other card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_name: Option<String>,
    /// Full bash command (redacted, untruncated — multi-line commands keep
    /// their line structure) for the `$` command rows. `None` for every other
    /// card. `arguments_summary` stays the 60-char title compact; the bash
    /// card renders the command from this field so embedded newlines survive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bash_command: Option<String>,
    /// Original arguments retained for tool-specific rendering. The adapter
    /// stores the redacted value so presentation never needs raw tool input.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub arguments: serde_json::Value,
    /// Authoritative redacted tool-result details for specialized cards.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskDelegationChild {
    pub name: Option<String>,
    pub agent: Option<String>,
    pub task: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskDelegationRequest {
    pub context: String,
    pub children: Vec<TaskDelegationChild>,
}

#[must_use]
pub fn task_delegation_request(arguments: &serde_json::Value) -> Option<TaskDelegationRequest> {
    if let Some(tasks) = arguments.get("tasks").and_then(serde_json::Value::as_array) {
        let children = tasks
            .iter()
            .filter_map(|item| {
                let task = item.get("task")?.as_str()?.trim();
                (!task.is_empty()).then(|| TaskDelegationChild {
                    name: item
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    agent: item
                        .get("agent")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    task: task.to_owned(),
                })
            })
            .collect::<Vec<_>>();
        return (!children.is_empty()).then(|| TaskDelegationRequest {
            context: arguments
                .get("context")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            children,
        });
    }
    let task = arguments.get("task")?.as_str()?.trim();
    (!task.is_empty()).then(|| TaskDelegationRequest {
        context: String::new(),
        children: vec![TaskDelegationChild {
            name: arguments
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            agent: arguments
                .get("agent")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            task: task.to_owned(),
        }],
    })
}

/// Display-ready Goal / Constraints / Contract bodies split from a Task
/// delegation `context`, mirroring the Web Task card's structured default
/// view. Each body is bounded; raw `# Heading` lines never reach the rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskDelegationSections {
    pub goal: String,
    pub target: String,
    pub constraints: String,
    pub contract: String,
}

/// Character budget for one Task context section (Web `TASK_SECTION_CHAR_LIMIT`).
pub const TASK_SECTION_CHAR_LIMIT: usize = 480;
/// Character budget for a Task child target summary (Web `TASK_TARGET_CHAR_LIMIT`).
pub const TASK_TARGET_CHAR_LIMIT: usize = 220;

/// Bound free-form text to a character budget without mid-code-unit cuts.
/// The budget covers the whole bounded body: when truncated, the output is
/// exactly `limit` characters and ends with `…`, so the ellipsis never
/// shrinks the total below the advertised cap (mirrors the Web `boundText`
/// budget semantics).
#[must_use]
pub fn bound_display_text(text: &str, limit: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    if limit <= 1 {
        return "…".to_owned();
    }
    let mut bounded = text.chars().take(limit - 1).collect::<String>();
    // A truncated body must end with the ellipsis AND total exactly `limit`
    // characters: when the 479-char prefix ends inside a whitespace run,
    // extend past it so the trimmed tail cannot shrink the total below the
    // budget.
    let mut rest = text.chars().skip(limit - 1);
    while bounded.chars().last().is_some_and(char::is_whitespace) {
        bounded.pop();
        if let Some(next) = rest.next() {
            bounded.push(next);
        } else {
            break;
        }
    }
    bounded.push('…');
    bounded
}

/// The three recognized Task context section kinds (Web `TaskContextSections`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TaskSectionKey {
    Goal,
    Target,
    Constraints,
    Contract,
}

/// ATX heading title: optional indent ≤3, 1–6 hashes, required space, optional
/// trailing hashes. Returns `None` for non-heading lines (mirrors the Web
/// `parseHeading`).
fn atx_heading_title(line: &str) -> Option<String> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let hashes = rest.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let after = rest.get(hashes..)?;
    let after = after.strip_prefix([' ', '\t'])?;
    let title = strip_trailing_hashes(after).trim();
    (!title.is_empty()).then(|| title.to_owned())
}

/// Strip an optional trailing `[ \t]+#+[ \t]*` closing sequence from an ATX
/// heading title (`## Goal ##` → `Goal`); a lone `#` not preceded by
/// whitespace (`foo#bar`) is content, not a closer.
fn strip_trailing_hashes(title: &str) -> &str {
    let bytes = title.as_bytes();
    let mut end = bytes.len();
    while end > 0 && matches!(bytes[end - 1], b' ' | b'\t') {
        end -= 1;
    }
    let hashes_end = end;
    while end > 0 && bytes[end - 1] == b'#' {
        end -= 1;
    }
    if end == hashes_end || end == 0 || !matches!(bytes[end - 1], b' ' | b'\t') {
        return title;
    }
    let mut trimmed = end;
    while trimmed > 0 && matches!(bytes[trimmed - 1], b' ' | b'\t') {
        trimmed -= 1;
    }
    &title[..trimmed]
}

/// Normalize a heading title to one of the recognized section keys, matching
/// the Web aliases case-insensitively (goal/goals, target/targets,
/// constraints/constraint/"constraints and preferences",
/// contract/acceptance/"acceptance criteria").
fn normalize_task_section_key(title: &str) -> Option<TaskSectionKey> {
    let key = title
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                ' '
            }
        })
        .collect::<String>();
    let key = key.split_whitespace().collect::<Vec<_>>().join(" ");
    match key.as_str() {
        "goal" | "goals" => Some(TaskSectionKey::Goal),
        "target" | "targets" => Some(TaskSectionKey::Target),
        "constraints"
        | "constraint"
        | "constraints preferences"
        | "constraints and preferences" => Some(TaskSectionKey::Constraints),
        "contract" | "acceptance" | "acceptance criteria" => Some(TaskSectionKey::Contract),
        _ if key.starts_with("goal ") => Some(TaskSectionKey::Goal),
        _ if key.starts_with("target ") => Some(TaskSectionKey::Target),
        _ if key.starts_with("constraint") => Some(TaskSectionKey::Constraints),
        _ if key.starts_with("contract") => Some(TaskSectionKey::Contract),
        _ => None,
    }
}

/// Split a task `context` string into Goal / Target / Constraints / Contract
/// bodies. Unrecognized leading prose folds into Goal so a free-form briefing
/// still surfaces; known headings are matched case-insensitively with ATX
/// levels; every body is bounded to [`TASK_SECTION_CHAR_LIMIT`] (mirrors the
/// Web `parseTaskContextSections`).
#[must_use]
pub fn task_delegation_sections(context: &str) -> TaskDelegationSections {
    let normalized = context.replace("\r\n", "\n").replace('\r', "\n");
    let mut goal = Vec::new();
    let mut target = Vec::new();
    let mut constraints = Vec::new();
    let mut contract = Vec::new();
    let mut leading = Vec::new();
    let mut current: Option<TaskSectionKey> = None;
    for line in normalized.lines() {
        let key = atx_heading_title(line).and_then(|title| normalize_task_section_key(&title));
        if let Some(key) = key {
            current = Some(key);
            continue;
        }
        match current {
            Some(TaskSectionKey::Goal) => goal.push(line),
            Some(TaskSectionKey::Target) => target.push(line),
            Some(TaskSectionKey::Constraints) => constraints.push(line),
            Some(TaskSectionKey::Contract) => contract.push(line),
            None => leading.push(line),
        }
    }
    if leading.iter().any(|line| !line.trim().is_empty()) {
        let mut all = leading;
        all.extend(goal);
        goal = all;
    }
    TaskDelegationSections {
        goal: bound_display_text(&goal.join("\n"), TASK_SECTION_CHAR_LIMIT),
        target: bound_display_text(&target.join("\n"), TASK_SECTION_CHAR_LIMIT),
        constraints: bound_display_text(&constraints.join("\n"), TASK_SECTION_CHAR_LIMIT),
        contract: bound_display_text(&contract.join("\n"), TASK_SECTION_CHAR_LIMIT),
    }
}

#[derive(Clone, Debug, Default)]
pub struct ToolCardPresentationAdapter {
    projection: ToolPresentationState,
}

impl ToolCardPresentationAdapter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    pub fn apply_application_event(&mut self, event: &ApplicationEvent) {
        if let ApplicationEvent::Agent(event) = event {
            self.projection.apply_event(event);
        }
    }
    pub fn apply_agent_event(&mut self, event: &pi_agent::AgentEvent) {
        self.projection.apply_event(event);
    }
    pub fn apply_tool_result(&mut self, result: &pi_ai::ToolResultMessage) {
        self.projection.apply_tool_result(result);
    }
    #[must_use]
    pub fn projection(&self) -> &ToolPresentationState {
        &self.projection
    }
    #[must_use]
    pub fn rows(&self, id: &str, expanded: bool) -> Option<ToolCardRows> {
        self.projection
            .get(id)
            .map(|card| card_rows(card, expanded))
    }
    #[must_use]
    pub fn rows_in_source_order(&self, expanded: bool) -> Vec<ToolCardRows> {
        self.projection
            .cards_in_source_order()
            .into_iter()
            .map(|card| card_rows(card, expanded))
            .collect()
    }
    #[must_use]
    pub fn compact_rows(&self) -> Vec<ToolCardRows> {
        self.rows_in_source_order(false)
    }
    #[must_use]
    pub fn expanded_rows(&self, id: &str) -> Option<ToolCardRows> {
        self.rows(id, true)
    }
    #[must_use]
    pub fn expanded_rows_in_source_order(&self) -> Vec<ToolCardRows> {
        self.rows_in_source_order(true)
    }
    #[must_use]
    pub fn summary(&self, id: &str) -> Option<ToolCardSummary> {
        self.projection.get(id).map(tool_card_summary)
    }
    pub fn clear(&mut self) {
        self.projection.clear();
    }

    #[must_use]
    pub fn bash_execution_rows(message: &BashExecutionMessage, expanded: bool) -> ToolCardRows {
        let status = if message.cancelled {
            ToolCallViewStatus::Cancelled
        } else if message.exit_code.is_some_and(|code| code != 0) {
            ToolCallViewStatus::Failed
        } else {
            ToolCallViewStatus::Succeeded
        };
        let mut details = serde_json::Map::new();
        if let Some(code) = message.exit_code {
            details.insert("exitCode".into(), code.into());
        }
        if message.cancelled {
            details.insert("cancelled".into(), true.into());
        }
        if message.truncated {
            details.insert("truncated".into(), true.into());
        }
        if let Some(path) = &message.full_output_path {
            details.insert("fullOutputPath".into(), path.clone().into());
        }
        let ordinal = u64::try_from(message.timestamp).unwrap_or_default();
        let mut output = if !message.output.is_empty() {
            message.output.clone()
        } else if message.cancelled {
            "Command aborted".to_owned()
        } else if let Some(code) = message.exit_code.filter(|code| *code != 0) {
            format!("Command exited with code {code}")
        } else {
            String::new()
        };
        // Keep the exit or cancel trailer when the transcript omitted it.
        let terminal_line = output
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .map(str::trim);
        let trailer = if message.cancelled && terminal_line != Some("Command aborted") {
            Some("Command aborted".to_owned())
        } else {
            message.exit_code.filter(|code| *code != 0).and_then(|code| {
                let line = format!("Command exited with code {code}");
                (terminal_line != Some(line.as_str())).then_some(line)
            })
        };
        if let Some(trailer) = trailer.as_deref() {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(trailer);
        }
        let card = ToolCard {
            tool_call_id: format!("bash-execution-{}", message.timestamp),
            tool_name: "bash".into(),
            ordinal,
            first_observation_ordinal: ordinal,
            status,
            arguments: serde_json::json!({"command": message.command}),
            content: if output.is_empty() {
                Vec::new()
            } else {
                vec![pi_ai::ContentBlock::text(output)]
            },
            details: serde_json::Value::Object(details),
            is_error: matches!(
                status,
                ToolCallViewStatus::Failed | ToolCallViewStatus::Cancelled
            ),
            cancelled: message.cancelled,
            error_message: None,
            has_message_result: true,
            has_execution_end: true,
        };
        card_rows(&card, expanded)
    }
}

/// One `skill://` read card reduced to prose: frontmatter `name` becomes the
/// card title, `description` becomes the first body paragraph, and the
/// `SKILL.md` body follows. Raw `name:`/`description:` YAML lines never reach
/// the row model.
struct SkillCardView {
    name: String,
    description: String,
    body: String,
}

impl SkillCardView {
    fn prose(&self) -> String {
        if self.description.is_empty() {
            self.body.clone()
        } else {
            format!("{}\n\n{}", self.description, self.body)
        }
    }
}

/// Detects a bare `skill://<name>` read whose content carries frontmatter and
/// returns the prose view. Sub-resource reads (`skill://<name>/…`) and
/// frontmatter-less content keep their raw text.
fn skill_card_view(card: &ToolCard, content: &str) -> Option<SkillCardView> {
    if !card.tool_name.eq_ignore_ascii_case("read") {
        return None;
    }
    let path = card
        .arguments
        .get("path")
        .and_then(serde_json::Value::as_str)?;
    let rest = path.strip_prefix("skill://")?;
    if rest.is_empty() || rest.contains('/') {
        return None;
    }
    let mut view = parse_skill_frontmatter(content)?;
    if view.name.is_empty() {
        view.name = rest.to_owned();
    }
    Some(view)
}

/// Minimal `SKILL.md` frontmatter reader: extracts `name` and `description`
/// (plain, quoted, or simple block scalars) plus the body after the closing
/// `---` delimiter. Block scalars follow YAML folding: `|` keeps line breaks,
/// `>` folds them into spaces. Mirrors the semantics of the discovery parser
/// in `pi_coding::resources` (quotes stripped, ` #` comments removed) without
/// depending on its crate-private implementation.
fn parse_skill_frontmatter(content: &str) -> Option<SkillCardView> {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let rest = normalized.strip_prefix("---\n")?;
    let header_end = rest.find("\n---")?;
    let header = &rest[..header_end];
    let body = rest[header_end + 4..].trim().to_owned();
    let mut name = None;
    let mut description = None;
    let lines = header.split('\n').collect::<Vec<_>>();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index].trim();
        index += 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, raw)) = line.split_once(':') else {
            continue;
        };
        let raw = raw.trim();
        let value = if raw.starts_with(['|', '>']) {
            // YAML block scalars: `|` keeps line breaks, `>` folds them into
            // spaces (simple chomping: indentation stripped, blank lines
            // dropped).
            let folded = raw.starts_with('>');
            let mut block = String::new();
            while index < lines.len() && lines[index].starts_with([' ', '\t']) {
                let trimmed = lines[index].trim();
                index += 1;
                if trimmed.is_empty() {
                    continue;
                }
                if !block.is_empty() {
                    block.push(if folded { ' ' } else { '\n' });
                }
                block.push_str(trimmed);
            }
            block
        } else {
            strip_frontmatter_scalar(raw).to_owned()
        };
        match key {
            "name" => name = Some(value),
            "description" => description = Some(value),
            _ => {}
        }
    }
    Some(SkillCardView {
        name: name.unwrap_or_default(),
        description: description.unwrap_or_default(),
        body,
    })
}

/// Strips surrounding quotes or a trailing ` #` comment from one frontmatter
/// scalar (same rules as skill discovery).
fn strip_frontmatter_scalar(value: &str) -> &str {
    let value = value.trim();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        &value[1..value.len() - 1]
    } else {
        value
            .split_once(" #")
            .map_or(value, |(plain, _)| plain.trim())
    }
}

const HUB_COMPACT_FAILURE_PREVIEW_LIMIT: usize = 1;
const HUB_EXPANDED_FAILURE_PREVIEW_LIMIT: usize = 3;
const HUB_SAFE_DIAGNOSTIC_CHAR_LIMIT: usize = 120;
const HUB_SAFE_ROW_CHAR_LIMIT: usize = 180;
const HUB_RECEIVED_BODY_CHAR_LIMIT: usize = 180;
const HUB_RECEIVED_BODY_LINE_LIMIT: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HubFailureCategory {
    Failure,
    Validation,
}

impl HubFailureCategory {
    fn label(self) -> &'static str {
        match self {
            Self::Failure => "failure",
            Self::Validation => "validation",
        }
    }

    fn fallback(self) -> &'static str {
        match self {
            Self::Failure => "operation failed",
            Self::Validation => "output validation failed",
        }
    }
}

#[derive(Default)]
struct HubRowProjection {
    rows: Vec<(ToolCardRowRole, String)>,
    omitted: usize,
}

fn hub_failure_category(structured_output_invalid: bool) -> HubFailureCategory {
    if structured_output_invalid {
        HubFailureCategory::Validation
    } else {
        HubFailureCategory::Failure
    }
}

fn hub_safe_diagnostic(text: &str) -> String {
    let normalized = crate::ansi::ansi_plain_text(&redact_secrets(text)).to_ascii_lowercase();
    let contains = |needles: &[&str]| needles.iter().any(|needle| normalized.contains(needle));
    let reason = if contains(&[
        "schema validation",
        "output validation",
        "invalid output",
        "parse error",
        "malformed output",
    ]) {
        "output validation failed"
    } else if contains(&[
        "permission denied",
        "access denied",
        "unauthorized",
        "forbidden",
    ]) {
        "permission denied"
    } else if contains(&["timed out", "timeout", "deadline exceeded"]) {
        "operation timed out"
    } else if contains(&["sandbox", "seccomp", "landlock"]) {
        "sandbox blocked the operation"
    } else if contains(&["rate limit", "quota", "resource exhausted", "token limit"]) {
        "resource limit reached"
    } else if normalized.contains("provider")
        && contains(&["failed", "failure", "error", "unavailable"])
    {
        "provider failed"
    } else if contains(&["transport", "connection", "network", "socket", "dns"])
        && contains(&["failed", "failure", "error", "unavailable"])
    {
        "connection or transport failed"
    } else if contains(&["no recipient", "no recipients"]) {
        "no recipient was available"
    } else if contains(&[
        "not found",
        "unknown target",
        "unknown orchestration agent",
        "absent",
    ]) {
        "required target was not found"
    } else if contains(&["required", "missing parameter"]) {
        "required input is missing"
    } else if contains(&["unsupported", "invalid", "must be", "cannot "]) {
        "request parameters are invalid"
    } else if contains(&[
        "cancelled",
        "canceled",
        "aborted",
        "interrupted",
        "panic",
        "crash",
    ]) {
        "operation was interrupted"
    } else {
        "operation failed"
    };
    bound_display_text(reason, HUB_SAFE_DIAGNOSTIC_CHAR_LIMIT)
}

fn hub_typed_details<'a>(
    details: &'a serde_json::Value,
    operation: &str,
) -> Option<&'a serde_json::Map<String, serde_json::Value>> {
    let record = details.as_object()?;
    (record.get("op").and_then(serde_json::Value::as_str) == Some(operation)).then_some(record)
}

fn hub_successful_wait_body_projection(
    card: &ToolCard,
    details: &serde_json::Value,
) -> Option<HubRowProjection> {
    if !matches!(
        card.status,
        ToolCallViewStatus::Succeeded | ToolCallViewStatus::OrphanRepaired
    ) {
        return None;
    }
    let message = hub_typed_details(details, "wait")?
        .get("message")?
        .as_object()?;
    let id = message.get("id").and_then(serde_json::Value::as_str)?;
    let from = message.get("from").and_then(serde_json::Value::as_str)?;
    if id.is_empty() || from.is_empty() {
        return None;
    }
    let body = message.get("body").and_then(serde_json::Value::as_str)?;
    let plain = crate::ansi::ansi_plain_text(body);
    let redacted = redact_secrets(&plain);
    let lines = redacted.lines().collect::<Vec<_>>();
    let line_truncated = lines.len() > HUB_RECEIVED_BODY_LINE_LIMIT;
    let visible = lines
        .iter()
        .take(HUB_RECEIVED_BODY_LINE_LIMIT)
        .copied()
        .collect::<Vec<_>>();
    let joined = visible.join("\n");
    let char_truncated = joined.chars().count() > HUB_RECEIVED_BODY_CHAR_LIMIT;
    let mut bounded = if char_truncated {
        redact_bounded(&joined, HUB_RECEIVED_BODY_CHAR_LIMIT - 1)
    } else {
        joined
    };
    if char_truncated || line_truncated {
        bounded.push('…');
    }
    let visible = bounded.lines().map(str::to_owned).collect::<Vec<_>>();
    (!visible.is_empty()).then(|| HubRowProjection {
        rows: visible
            .into_iter()
            .map(|line| (ToolCardRowRole::Content, line))
            .collect(),
        omitted: 0,
    })
}

fn hub_failed_job_projection(
    operation: &str,
    details: &serde_json::Value,
    expanded: bool,
) -> Option<HubRowProjection> {
    if !matches!(operation, "jobs" | "wait") {
        return None;
    }
    let jobs = hub_typed_details(details, operation)?
        .get("jobs")?
        .as_array()?;
    let preview_limit = if expanded {
        HUB_EXPANDED_FAILURE_PREVIEW_LIMIT
    } else {
        HUB_COMPACT_FAILURE_PREVIEW_LIMIT
    };
    let mut failed = 0usize;
    let mut previews = Vec::new();
    for job in jobs {
        if job.get("status").and_then(serde_json::Value::as_str) != Some("failed") {
            continue;
        }
        failed += 1;
        if previews.len() >= preview_limit {
            continue;
        }
        let error = job
            .pointer("/result/error")
            .and_then(serde_json::Value::as_str);
        let structured_output_invalid = job
            .pointer("/result/structuredOutput/valid")
            .and_then(serde_json::Value::as_bool)
            == Some(false);
        let category = hub_failure_category(structured_output_invalid);
        let diagnostic = error
            .map(hub_safe_diagnostic)
            .filter(|diagnostic| diagnostic != "operation failed")
            .unwrap_or_else(|| category.fallback().to_owned());
        previews.push((category, diagnostic));
    }
    if failed == 0 {
        return None;
    }
    let noun = if failed == 1 { "job" } else { "jobs" };
    let mut rows = vec![(ToolCardRowRole::Status, format!("Failed {noun}: {failed}"))];
    rows.extend(previews.into_iter().map(|(category, diagnostic)| {
        (
            ToolCardRowRole::Error,
            bound_display_text(
                &format!(
                    "{} · {diagnostic}; retry or inspect failed jobs",
                    category.label()
                ),
                HUB_SAFE_ROW_CHAR_LIMIT,
            ),
        )
    }));
    let omitted = failed.saturating_sub(preview_limit);
    if expanded && omitted > 0 {
        let noun = if omitted == 1 { "job" } else { "jobs" };
        rows.push((
            ToolCardRowRole::Status,
            format!("{omitted} more failed {noun} omitted"),
        ));
    }
    Some(HubRowProjection { rows, omitted })
}

fn hub_wait_timeout_projection(
    card: &ToolCard,
    details: &serde_json::Value,
) -> Option<HubRowProjection> {
    if !matches!(
        card.status,
        ToolCallViewStatus::Succeeded | ToolCallViewStatus::OrphanRepaired
    ) || card
        .arguments
        .get("timeoutMs")
        .and_then(serde_json::Value::as_u64)
        .is_none()
    {
        return None;
    }
    let details = hub_typed_details(details, "wait")?;
    let waits_for_jobs = card
        .arguments
        .get("ids")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|ids| !ids.is_empty());
    let message = if waits_for_jobs {
        let jobs = details.get("jobs")?.as_array()?;
        let all_unsettled = !jobs.is_empty()
            && jobs.iter().all(|job| {
                matches!(
                    job.get("status").and_then(serde_json::Value::as_str),
                    Some("queued" | "running")
                )
            });
        all_unsettled.then_some("Wait timed out · no job settled. Retry wait or inspect jobs.")
    } else {
        details
            .get("message")
            .is_some_and(serde_json::Value::is_null)
            .then_some("Wait timed out · no message arrived. Retry wait.")
    }?;
    Some(HubRowProjection {
        rows: vec![(ToolCardRowRole::Status, message.to_owned())],
        omitted: 0,
    })
}

fn hub_operation_failure_projection(
    card: &ToolCard,
    operation: &str,
    content: &str,
) -> Option<HubRowProjection> {
    if card.status != ToolCallViewStatus::Failed {
        return None;
    }
    let label = match operation {
        "send" => "Send",
        "ask" => "Ask",
        "wait" => "Wait",
        "inbox" => "Inbox",
        "list" => "Agent list",
        "jobs" => "Jobs",
        "cancel" => "Cancel",
        "read_history" => "History",
        _ => "Hub operation",
    };
    let action = if operation == "wait" {
        "Retry wait."
    } else {
        "Check inputs and retry."
    };
    let diagnostic = if matches!(
        operation,
        "send" | "ask" | "wait" | "inbox" | "list" | "jobs" | "cancel" | "read_history"
    ) {
        hub_safe_diagnostic(content)
    } else {
        "operation failed".to_owned()
    };
    let text = bound_display_text(
        &format!("{label} failed · {diagnostic}. {action}"),
        HUB_SAFE_ROW_CHAR_LIMIT,
    );
    Some(HubRowProjection {
        rows: vec![(ToolCardRowRole::Error, text)],
        omitted: 0,
    })
}

fn hub_row_projection(
    card: &ToolCard,
    expanded: bool,
    details: &serde_json::Value,
    content: &str,
) -> HubRowProjection {
    let operation = card
        .arguments
        .get("op")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    hub_failed_job_projection(operation, details, expanded)
        .or_else(|| hub_operation_failure_projection(card, operation, content))
        .or_else(|| {
            (operation == "wait")
                .then(|| hub_successful_wait_body_projection(card, details))
                .flatten()
        })
        .or_else(|| {
            (operation == "wait")
                .then(|| hub_wait_timeout_projection(card, details))
                .flatten()
        })
        .unwrap_or_default()
}

fn card_rows(card: &ToolCard, expanded: bool) -> ToolCardRows {
    let is_read = card.tool_name.eq_ignore_ascii_case("read");
    let (view_content, view_details) = if is_read {
        (card_content_text(card), serde_json::Value::Null)
    } else {
        let view = card.expanded_view();
        (view.content_text, view.details)
    };
    let mut rows = vec![row(
        card,
        ToolCardRowRole::Command,
        tool_title(&card.tool_name),
    )];
    let mut content = tool_content_text(card, &view_content);
    let skill = skill_card_view(card, &content);
    if let Some(skill_view) = &skill {
        rows[0].text.clone_from(&skill_view.name);
    }
    content = skill.as_ref().map_or(content, SkillCardView::prose);
    let is_bash = card.tool_name.eq_ignore_ascii_case("bash");
    let is_glob = card.tool_name.eq_ignore_ascii_case("glob");
    let is_hub = card.tool_name.eq_ignore_ascii_case("hub");
    let mut status = card.status;
    let mut bash_footer = None;
    if is_bash {
        let terminal_line = content
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .map(str::trim);
        let canonical_abort = terminal_line == Some("Command aborted");
        let canonical_exit = terminal_line
            .and_then(|line| line.strip_prefix("Command exited with code "))
            .and_then(|code| code.parse::<i32>().ok());
        let canonical_timeout = terminal_line
            .and_then(|line| line.strip_prefix("Command timed out after "))
            .and_then(|rest| rest.strip_suffix(" seconds"))
            .filter(|seconds| seconds.parse::<f64>().is_ok())
            .map(str::to_owned);
        let has_canonical_timeout = canonical_timeout.is_some();
        if canonical_abort {
            status = ToolCallViewStatus::Cancelled;
        }
        let background_timeout = card
            .arguments
            .get("timeout")
            .and_then(serde_json::Value::as_f64)
            .filter(|timeout| timeout.is_finite() && *timeout > 0.0)
            .map(|timeout| timeout.to_string());
        let is_background = card
            .arguments
            .get("background")
            .and_then(serde_json::Value::as_bool)
            == Some(true);
        let background_id = is_background
            .then(|| view_details.get("id").and_then(serde_json::Value::as_str))
            .flatten()
            .filter(|id| !id.is_empty());
        if let Some(id) = background_id {
            let canonical = format!("Process started: {id} (visible in /ps)");
            if content.trim() == canonical {
                content.clear();
                let mut fields = vec![format!("Backgrounded: {id}")];
                if let Some(timeout) = background_timeout {
                    fields.push(format!("Timeout: {timeout}s"));
                }
                bash_footer = Some(format!("⟦{}⟧", fields.join(" | ")));
            }
        } else {
            if canonical_abort || canonical_exit.is_some() || has_canonical_timeout {
                let mut lines = content.lines().collect::<Vec<_>>();
                while lines.last().is_some_and(|line| line.trim().is_empty()) {
                    lines.pop();
                }
                lines.pop();
                while lines.last().is_some_and(|line| line.trim().is_empty()) {
                    lines.pop();
                }
                content = lines.join("\n");
            }
            if !status.is_partial() {
                let mut fields = Vec::new();
                if let Some(timeout) = canonical_timeout {
                    fields.push(format!("Timeout: {timeout}s"));
                } else if status == ToolCallViewStatus::Cancelled {
                    fields.push("Cancelled".to_owned());
                } else if let Some(code) = canonical_exit.filter(|code| *code != 0) {
                    fields.push(format!("Exit: {code}"));
                }
                if !fields.is_empty() {
                    bash_footer = Some(format!("⟦{}⟧", fields.join(" | ")));
                }
            }
        }
    }
    let hub_projection =
        is_hub.then(|| hub_row_projection(card, expanded, &view_details, &content));
    let mut omitted_content_lines = hub_projection
        .as_ref()
        .map_or(0, |projection| projection.omitted);
    let todo_lines = todo_compact_lines(card, &content, expanded);
    // Glob cards only render the file tree for runs that produced (or are
    // streaming) path results: a failed/cancelled glob falls through to plain
    // content so an error message never gets painted as a file-tree row.
    let glob_success_like = is_glob
        && matches!(
            card.status,
            ToolCallViewStatus::Succeeded
                | ToolCallViewStatus::OrphanRepaired
                | ToolCallViewStatus::Running
                | ToolCallViewStatus::Streaming
        );
    let glob_lines = glob_success_like.then(|| glob_tree_lines(&content));
    // Verified-empty glob (terminal success, no paths): compact keeps the
    // body empty so the headline `0 files` is the whole story; expanded
    // surfaces the safe visible `No files found` line. The raw model-facing
    // `No files found matching pattern` sentinel never reaches the rows.
    let glob_empty_expanded = is_glob
        && expanded
        && matches!(
            card.status,
            ToolCallViewStatus::Succeeded | ToolCallViewStatus::OrphanRepaired
        )
        && glob_result_paths(&content).is_empty();
    let mut content_lines = if is_hub {
        Vec::new()
    } else if let Some(todo_lines) = &todo_lines {
        todo_lines.iter().map(String::as_str).collect::<Vec<_>>()
    } else if glob_empty_expanded {
        vec!["No files found"]
    } else if let Some(glob_lines) = &glob_lines {
        glob_lines.iter().map(String::as_str).collect::<Vec<_>>()
    } else {
        content.lines().collect::<Vec<_>>()
    };
    if todo_lines.is_none() && card.tool_name.eq_ignore_ascii_case("read") {
        content_lines.retain(|line| !is_file_truncation_notice(line));
    }
    const BASH_CARD_OUTPUT_LIMIT: usize = 10;
    const BASH_CARD_EXPANDED_OUTPUT_LIMIT: usize = 30;
    const DEFAULT_CARD_OUTPUT_LIMIT: usize = 6;
    let is_todo = todo_lines.is_some();
    let limit = if is_bash {
        if expanded {
            BASH_CARD_EXPANDED_OUTPUT_LIMIT
        } else {
            BASH_CARD_OUTPUT_LIMIT
        }
    } else {
        DEFAULT_CARD_OUTPUT_LIMIT
    };
    let fold_head = skill.is_some() || is_todo || is_glob;
    let visible = if is_todo || (expanded && !is_bash) || content_lines.len() <= limit {
        &content_lines[..]
    } else if fold_head {
        &content_lines[..limit]
    } else {
        &content_lines[content_lines.len() - limit..]
    };
    rows.extend(
        visible
            .iter()
            .map(|line| row(card, ToolCardRowRole::Content, (*line).to_owned())),
    );
    if let Some(projection) = hub_projection {
        rows.extend(
            projection
                .rows
                .into_iter()
                .map(|(role, text)| row(card, role, text)),
        );
    }
    if !is_hub {
        omitted_content_lines = if is_todo && !expanded {
            todo_hidden_task_count(card)
        } else {
            content_lines.len().saturating_sub(visible.len())
        };
    }
    if expanded && !is_read && !is_todo && !is_bash && !is_glob && !is_hub {
        let details = serde_json::to_string_pretty(&view_details)
            .unwrap_or_else(|_| view_details.to_string());
        if !matches!(details.as_str(), "{}" | "[]" | "null" | "") {
            rows.extend(
                details
                    .lines()
                    .map(|line| row(card, ToolCardRowRole::Details, line.to_owned())),
            );
        }
    }
    if let Some(footer) = bash_footer {
        rows.push(row(card, ToolCardRowRole::Status, footer));
    }
    let code_language = tool_code_language(&card.tool_name, &card.arguments);
    let bash_command = card
        .tool_name
        .eq_ignore_ascii_case("bash")
        .then(|| {
            card.arguments
                .get("command")
                .and_then(serde_json::Value::as_str)
                .map(redact_secrets)
        })
        .flatten();
    ToolCardRows {
        tool_call_id: card.tool_call_id.clone(),
        tool_name: card.tool_name.clone(),
        ordinal: card.ordinal,
        arguments_summary: compact_tool_arguments(&card.arguments),
        summary: tool_card_summary_text(card),
        code_language,
        status,
        is_partial: status.is_partial(),
        is_error: matches!(
            status,
            ToolCallViewStatus::Failed | ToolCallViewStatus::Cancelled
        ),
        cancelled: status == ToolCallViewStatus::Cancelled,
        truncated: omitted_content_lines > 0,
        omitted_content_lines,
        rows,
        skill_name: skill.as_ref().map(|skill_view| skill_view.name.clone()),
        bash_command,
        arguments: redact_value(&card.arguments),
        details: view_details,
    }
}

fn tool_code_language(tool_name: &str, arguments: &serde_json::Value) -> Option<String> {
    if !matches!(
        tool_name.to_ascii_lowercase().as_str(),
        "read" | "write" | "edit"
    ) {
        return None;
    }
    let path = ["path", "file"]
        .into_iter()
        .find_map(|key| arguments.get(key).and_then(serde_json::Value::as_str))?;
    let extension = std::path::Path::new(path)
        .extension()?
        .to_str()?
        .to_ascii_lowercase();
    let language = match extension.as_str() {
        "rs" => "rust",
        "sh" | "bash" | "zsh" | "fish" | "ksh" => "sh",
        "json" | "jsonc" | "json5" => "json",
        "ts" => "typescript",
        "tsx" | "js" | "jsx" | "py" | "go" | "java" | "c" | "cpp" | "cs" | "swift" | "rb"
        | "php" | "scala" | "toml" | "yaml" | "yml" | "sql" | "lua" | "zig" | "dart" | "r"
        | "hs" | "ex" | "clj" => extension.as_str(),
        "cc" => "cpp",
        "kt" => "kotlin",
        "pl" => "perl",
        "exs" => "elixir",
        _ => return None,
    };
    Some(language.to_owned())
}
fn tool_title(tool_name: &str) -> String {
    tool_name
        .split(['_', '-'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// True when `line` is the read tool's file-level truncation notice — the
/// `[N more lines in file. Use offset=… to continue.]` and the
/// `[Showing lines X-Y of Z … Use offset=… to continue.]` contracts that
/// `pi_coding::tools::render_read_result` appends when the file continues
/// past the returned lines. The read card drops these lines and folds
/// instead, so the fold footer (`… N more lines ⟦Ctrl+O: Expand⟧`) stays
/// the single "more lines" notice.
fn is_file_truncation_notice(line: &str) -> bool {
    let line = line.trim();
    line.starts_with('[')
        && line.ends_with(']')
        && line.contains("Use offset=")
        && line.contains("to continue.")
        && (line.contains("more lines in file") || line.contains("Showing lines"))
}

fn tool_content_text(card: &ToolCard, fallback: &str) -> String {
    let selected = if card.tool_name.eq_ignore_ascii_case("edit") {
        card.details.get("diff").and_then(serde_json::Value::as_str)
    } else if card.tool_name.eq_ignore_ascii_case("write") {
        card.arguments
            .get("content")
            .and_then(serde_json::Value::as_str)
    } else {
        None
    };
    selected.map_or_else(
        || fallback.to_owned(),
        |text| {
            redact_value(&serde_json::Value::String(text.to_owned()))
                .as_str()
                .unwrap_or_default()
                .to_owned()
        },
    )
}

fn card_content_text(card: &ToolCard) -> String {
    let text = card
        .content
        .iter()
        .filter_map(block_text)
        .collect::<Vec<_>>()
        .join("\n");
    redact_value(&serde_json::Value::String(text))
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn row(card: &ToolCard, role: ToolCardRowRole, text: String) -> ToolCardRow {
    ToolCardRow {
        tool_call_id: card.tool_call_id.clone(),
        role,
        text,
    }
}

/// Tool-specific one-line header projection for a card. Raw details stay
/// folded; this is only the headline data (path, counts) so compact cards can
/// render "what ran on what, how much" without expanding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ToolCardSummary {
    Read {
        path: String,
        line_range: Option<String>,
        lines_returned: usize,
    },
    Search {
        query: String,
        match_count: usize,
        files_matched: usize,
    },
    Edit {
        path: String,
        hunks: usize,
        added_lines: usize,
        removed_lines: usize,
    },
    Write {
        path: String,
        bytes_written: Option<usize>,
        lines_written: usize,
    },
    Generic {
        title: String,
        arguments_summary: String,
    },
}

/// Tools whose result rows are `<path>:<line>: <text>` matches (grep-style).
const SEARCH_TOOL_NAMES: [&str; 2] = ["grep", "ast_grep"];

/// Parses the `[Showing lines X-Y of Z …]` / `[N more lines in file. Use
/// offset=… to continue.]` notices `render_read_result` appends (the read
/// tool publishes no structured line-range details).
fn read_line_range(content: &str) -> Option<String> {
    let notice = content.lines().rev().find_map(|line| {
        let line = line.trim();
        (line.starts_with('[') && line.ends_with(']')).then_some(line)
    })?;
    if let Some((_, range)) = notice.split_once("Showing lines ") {
        return range.split(" of ").next().map(str::to_owned);
    }
    // The `[N more lines in file …]` contract means the returned window ends
    // right before the offset it advertises.
    let offset = notice.split_once("Use offset=").and_then(|(_, rest)| {
        rest.chars()
            .take_while(|character| character.is_ascii_digit())
            .collect::<String>()
            .parse::<usize>()
            .ok()
    })?;
    (offset > 1).then(|| format!("1-{}", offset - 1))
}

/// Lines the read card actually returns: everything before the appended
/// truncation notice and its blank separator.
fn read_lines_returned(content: &str) -> usize {
    let mut lines: Vec<&str> = content.lines().collect();
    if let Some(index) = lines
        .iter()
        .position(|line| is_file_truncation_notice(line))
    {
        lines.truncate(index);
        if lines.last().is_some_and(|line| line.trim().is_empty()) {
            lines.pop();
        }
    }
    lines.len()
}

fn count_diff_lines(diff: &str, prefix: char) -> usize {
    diff.lines()
        .filter(|line| {
            let mut chars = line.chars();
            chars.next() == Some(prefix)
                // Unified-patch `+++`/`---` headers are not content lines.
                && chars.next() != Some(prefix)
                && !matches!(*line, "+" | "-")
        })
        .count()
}

fn edit_hunks(patch: &str) -> usize {
    patch.lines().filter(|line| line.starts_with("@@")).count()
}

/// Match rows in a grep-style result: every output line except the blank
/// separator and the appended `[N matches limit reached …]` notice.
fn grep_match_row_count(text: &str) -> usize {
    text.lines()
        .filter(|line| !line.trim().is_empty() && !(line.starts_with('[') && line.ends_with(']')))
        .count()
}

fn glob_result_paths(content: &str) -> Vec<&str> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !(line.starts_with('[') && line.ends_with(']')))
        .filter(|line| *line != "No files found matching pattern")
        .collect()
}

fn glob_file_icon(path: &str) -> &'static str {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
    match ext {
        "tsv" | "csv" => "📑",
        "md" | "markdown" => "📝",
        "txt" | "text" | "log" => "🗒",
        _ => "📄",
    }
}

fn glob_tree_lines(content: &str) -> Vec<String> {
    let paths = glob_result_paths(content);
    let last = paths.len().saturating_sub(1);
    paths
        .into_iter()
        .enumerate()
        .map(|(index, path)| {
            let branch = if index == last { "└─" } else { "├─" };
            format!("{branch} {} {path}", glob_file_icon(path))
        })
        .collect()
}

/// Display-cell budget for the glob compact-title target so a long absolute
/// search root never blows out the one-line headline (OMP `Glob <bounded
/// target>` parity). Measured in terminal cells (CJK wide = 2), not chars,
/// so a wide path cannot overflow the title row.
const GLOB_TARGET_CELL_LIMIT: usize = 48;

/// Bounded display target for a glob card headline: the explicit search
/// `path` when given, else the glob `pattern`, else `.` (the tool's cwd
/// default). The result is bounded to [`GLOB_TARGET_CELL_LIMIT`] terminal
/// cells via the shared grapheme-preserving truncator ([`fit_text`]), so a
/// CJK/emoji/combining-mark target is never split mid-cluster and never
/// exceeds one title row. Raw JSON / empty inputs never reach the title.
fn glob_target(path: &str, pattern: &str) -> String {
    let raw = if !path.is_empty() {
        path
    } else if !pattern.is_empty() {
        pattern
    } else {
        "."
    };
    fit_text(raw, GLOB_TARGET_CELL_LIMIT)
}

/// Compact one-line glob headline (the OMP `Glob <target> …` form). A
/// verified-empty run — terminal success with no path results — reports
/// `0 files`; a non-empty run reports `N files`; an incomplete (still
/// running/streaming) run reports `…` without a count; a failed/cancelled
/// run reports no count so the status glyph conveys the error and the
/// title never falsely claims `0 files`.
fn glob_compact_title(status: ToolCallViewStatus, target: &str, content: &str) -> String {
    match status {
        ToolCallViewStatus::Succeeded | ToolCallViewStatus::OrphanRepaired => {
            let n = glob_result_paths(content).len();
            format!("Glob {target} {n} files")
        }
        ToolCallViewStatus::Running | ToolCallViewStatus::Streaming => format!("Glob {target} …"),
        ToolCallViewStatus::Failed | ToolCallViewStatus::Cancelled => format!("Glob {target}"),
    }
}

fn tool_card_summary(card: &ToolCard) -> ToolCardSummary {
    match card.tool_name.to_ascii_lowercase().as_str() {
        "read" => {
            let path = argument_str(&card.arguments, &["path"]).unwrap_or_default();
            if matches!(
                card.status,
                ToolCallViewStatus::Failed | ToolCallViewStatus::Cancelled
            ) {
                // Failed/cancelled reads return no lines: the card body is the
                // error or cancel sentence, so range/count projections would
                // count that sentence as returned lines. The headline keeps
                // only the path; the status glyph conveys the outcome.
                ToolCardSummary::Read {
                    path,
                    line_range: None,
                    lines_returned: 0,
                }
            } else {
                let content = tool_content_text(card, &card_content_text(card));
                ToolCardSummary::Read {
                    path,
                    line_range: read_line_range(&content),
                    lines_returned: read_lines_returned(&content),
                }
            }
        }
        "write" => {
            let written = card
                .arguments
                .get("content")
                .and_then(serde_json::Value::as_str);
            ToolCardSummary::Write {
                path: argument_str(&card.arguments, &["path"]).unwrap_or_default(),
                bytes_written: written.map(str::len),
                lines_written: written.map_or(0, |text| text.lines().count()),
            }
        }
        "edit" => {
            let patch = card
                .details
                .get("patch")
                .and_then(serde_json::Value::as_str);
            let diff = card.details.get("diff").and_then(serde_json::Value::as_str);
            ToolCardSummary::Edit {
                path: argument_str(&card.arguments, &["path"]).unwrap_or_default(),
                hunks: patch.map(edit_hunks).unwrap_or_default(),
                added_lines: diff
                    .map(|diff| count_diff_lines(diff, '+'))
                    .unwrap_or_default(),
                removed_lines: diff
                    .map(|diff| count_diff_lines(diff, '-'))
                    .unwrap_or_default(),
            }
        }
        "glob" => {
            let path = argument_str(&card.arguments, &["path"]).unwrap_or_default();
            let pattern = argument_str(&card.arguments, &["pattern"]).unwrap_or_default();
            let target = glob_target(&path, &pattern);
            let content = tool_content_text(card, &card_content_text(card));
            let title = glob_compact_title(card.status, &target, &content);
            ToolCardSummary::Generic {
                title,
                arguments_summary: String::new(),
            }
        }
        name if SEARCH_TOOL_NAMES.contains(&name) => {
            let text = card
                .content
                .iter()
                .filter_map(|block| block_text(block))
                .collect::<Vec<_>>()
                .join("\n");
            let files_matched = text
                .lines()
                .filter(|line| {
                    !line.trim().is_empty() && !(line.starts_with('[') && line.ends_with(']'))
                })
                .map(|line| line.split(':').next().unwrap_or_default())
                .collect::<Vec<_>>();
            ToolCardSummary::Search {
                query: argument_str(&card.arguments, &["pattern", "query"]).unwrap_or_default(),
                match_count: grep_match_row_count(&text),
                files_matched: dedup_count(&files_matched),
            }
        }
        _ => ToolCardSummary::Generic {
            title: tool_title(&card.tool_name),
            arguments_summary: compact_tool_arguments(&card.arguments),
        },
    }
}

/// Redacted one-line headline for a card. Secrets in paths/queries are scrubbed
/// before the string leaves the adapter.
fn tool_card_summary_text(card: &ToolCard) -> String {
    let summary = tool_card_summary(card);
    // Failed/cancelled reads return no lines: the headline keeps only the
    // path so the error/cancel sentence is never counted as returned lines.
    let formatted = match (&summary, card.status) {
        (
            ToolCardSummary::Read { path, .. },
            ToolCallViewStatus::Failed | ToolCallViewStatus::Cancelled,
        ) => path.clone(),
        _ => format_tool_card_summary(&summary),
    };
    redact_secrets(&formatted)
}

/// Visible one-line headline for a [`ToolCardSummary`]. Production TUI titles
/// (`render_tool_card`) paint `{marker} {summary}` from [`ToolCardRows::summary`],
/// which is this formatter's redacted output.
#[must_use]
pub fn format_tool_card_summary(summary: &ToolCardSummary) -> String {
    match summary {
        ToolCardSummary::Read {
            path,
            line_range,
            lines_returned,
        } => {
            let range = line_range
                .as_deref()
                .map_or(String::new(), |range| format!(" · lines {range}"));
            format!("{path}{range} · {lines_returned} returned")
        }
        ToolCardSummary::Search {
            query,
            match_count,
            files_matched,
        } => {
            format!("{query} · {match_count} matches · {files_matched} files")
        }
        ToolCardSummary::Edit {
            path,
            hunks,
            added_lines,
            removed_lines,
        } => {
            format!("{path} · {hunks} hunks · +{added_lines} −{removed_lines}")
        }
        ToolCardSummary::Write {
            path,
            bytes_written,
            lines_written,
        } => {
            let bytes = bytes_written.map_or(String::new(), |bytes| format!(" · {bytes} bytes"));
            format!("{path}{bytes} · {lines_written} lines")
        }
        ToolCardSummary::Generic {
            title,
            arguments_summary,
        } => {
            if arguments_summary.is_empty() {
                title.clone()
            } else {
                format!("{title} {arguments_summary}")
            }
        }
    }
}

impl ToolCardRows {
    /// OMP glyph on the card title: `☑` for ordinary Todo, `🔍` for glob,
    /// `✎` for succeeded edit/write, `•` for other running/success, `✘` failed,
    /// `■` cancelled, `↻` orphan-repaired. Production `render_tool_card` paints
    /// this marker.

    #[must_use]
    pub fn status_glyph(&self) -> &'static str {
        match self.status {
            ToolCallViewStatus::Running
            | ToolCallViewStatus::Streaming
            | ToolCallViewStatus::Succeeded => {
                if self.tool_name.eq_ignore_ascii_case("todo") {
                    "☑"
                } else if self.tool_name.eq_ignore_ascii_case("glob") {
                    "🔍"
                } else if self.tool_name.eq_ignore_ascii_case("read")
                    && !self
                        .arguments
                        .get("path")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .starts_with("skill://")
                {
                    "●"
                } else if matches!(self.tool_name.as_str(), "edit" | "write") {
                    "✎"
                } else {
                    "•"
                }
            }
            ToolCallViewStatus::Failed => "✘",
            ToolCallViewStatus::Cancelled => "■",
            ToolCallViewStatus::OrphanRepaired => "↻",
        }
    }

    /// Frame color used by production `render_tool_card`.
    ///
    /// Matches OMP `renderOutputBlock` default chrome
    /// (`src/tui/output-block.ts`): running/pending use accent, success uses
    /// dim (gray), error/warning keep their colors. Bash cards previously
    /// painted the whole frame with `bashMode` (`#00ff88` readoutGreen);
    /// OMP reserves that token for composer `!` mode, not settled card
    /// chrome. Non-bash success keeps `tool_card_border`, the visible
    /// neutral already required against the dark background.

    #[must_use]
    pub fn frame_color(&self, theme: crate::theme::Theme) -> ratatui::style::Color {
        match self.status {
            ToolCallViewStatus::Failed => theme.error,
            ToolCallViewStatus::Cancelled => theme.warning,
            ToolCallViewStatus::Running | ToolCallViewStatus::Streaming => theme.border_accent,
            ToolCallViewStatus::Succeeded | ToolCallViewStatus::OrphanRepaired => {
                if self.tool_name.eq_ignore_ascii_case("bash") {
                    theme.dim
                } else {
                    theme.tool_card_border
                }
            }
        }
    }
}

/// Per-task content budget for compact Todo rows (Web `TODO_TASK_CHAR_LIMIT`).
const TODO_TASK_CHAR_LIMIT: usize = 240;
/// Dependency-waiting reason budget (Web `TASK_ACTIVITY_CHAR_LIMIT`).
const TODO_WAITING_CHAR_LIMIT: usize = 140;
/// Compact transcript-card cap for ordinary Todo task rows.
const TODO_COMPACT_TASK_LIMIT: usize = 8;

struct TodoCompactTask {
    content: String,
    status: String,
    waiting_on: Vec<String>,
}

struct TodoCompactPhase {
    name: String,
    tasks: Vec<TodoCompactTask>,
}

struct TodoCardSnapshot {
    phases: Vec<TodoCompactPhase>,
}

fn todo_task_is_completed(status: &str) -> bool {
    status == "completed"
}

fn todo_task_marker(status: &str) -> &'static str {
    if todo_task_is_completed(status) {
        "☑"
    } else {
        "☐"
    }
}

fn todo_total_tasks(snapshot: &TodoCardSnapshot) -> usize {
    snapshot.phases.iter().map(|phase| phase.tasks.len()).sum()
}

fn todo_phase_progress(phase: &TodoCompactPhase) -> (usize, usize) {
    let total = phase.tasks.len();
    let completed = phase
        .tasks
        .iter()
        .filter(|task| todo_task_is_completed(&task.status))
        .count();
    (completed, total)
}

fn todo_selected_task_keys(snapshot: &TodoCardSnapshot) -> Vec<(usize, usize)> {
    let mut active = Vec::new();
    let mut open = Vec::new();
    let mut completed = Vec::new();
    for (phase_idx, phase) in snapshot.phases.iter().enumerate() {
        for (task_idx, task) in phase.tasks.iter().enumerate() {
            if task.status == "in_progress" {
                active.push((phase_idx, task_idx));
            } else if todo_task_is_completed(&task.status) {
                completed.push((phase_idx, task_idx));
            } else {
                open.push((phase_idx, task_idx));
            }
        }
    }
    if active.is_empty() && open.is_empty() {
        return completed
            .into_iter()
            .take(TODO_COMPACT_TASK_LIMIT)
            .collect();
    }
    let mut selected = Vec::new();
    for key in active.into_iter().chain(open) {
        if selected.len() >= TODO_COMPACT_TASK_LIMIT {
            break;
        }
        selected.push(key);
    }
    if selected.len() < TODO_COMPACT_TASK_LIMIT {
        if let Some(recent) = completed.last().copied() {
            selected.push(recent);
        }
    }
    selected
}

fn todo_hidden_task_count(card: &ToolCard) -> usize {
    let snapshot = todo_card_snapshot(card);
    todo_total_tasks(&snapshot).saturating_sub(todo_selected_task_keys(&snapshot).len())
}

fn todo_format_task_row(task: &TodoCompactTask, last: bool) -> String {
    let branch = if last { "└─" } else { "├─" };
    let mut line = format!(
        "{branch} {} {}",
        todo_task_marker(&task.status),
        task.content
    );
    if !task.waiting_on.is_empty() {
        line.push_str(" · waiting on ");
        line.push_str(&task.waiting_on.join(", "));
    }
    line
}

fn todo_structured_lines(
    snapshot: &TodoCardSnapshot,
    selected: Option<&[(usize, usize)]>,
) -> Vec<String> {
    let show_phases = snapshot.phases.len() > 1;
    let mut lines = Vec::new();
    for (phase_idx, phase) in snapshot.phases.iter().enumerate() {
        let mut task_indices: Vec<usize> = match selected {
            Some(selected) => selected
                .iter()
                .filter(|(item_phase, _)| *item_phase == phase_idx)
                .map(|(_, task_idx)| *task_idx)
                .collect(),
            None => (0..phase.tasks.len()).collect(),
        };
        task_indices.sort_unstable();
        task_indices.dedup();
        if task_indices.is_empty() {
            continue;
        }
        if show_phases && !phase.name.is_empty() {
            let (completed, total) = todo_phase_progress(phase);
            lines.push(bound_display_text(
                &format!("{} · {completed}/{total}", phase.name),
                TODO_TASK_CHAR_LIMIT,
            ));
        }
        let last_idx = task_indices.len() - 1;
        for (index, task_idx) in task_indices.into_iter().enumerate() {
            lines.push(todo_format_task_row(
                &phase.tasks[task_idx],
                index == last_idx,
            ));
        }
    }
    lines
}

fn todo_card_snapshot(card: &ToolCard) -> TodoCardSnapshot {
    let phases = parse_todo_phases(card.details.get("phases"))
        .or_else(|| todo_phases_from_init_args(&card.arguments))
        .unwrap_or_default();
    TodoCardSnapshot { phases }
}

fn parse_todo_phases(value: Option<&serde_json::Value>) -> Option<Vec<TodoCompactPhase>> {
    let serde_json::Value::Array(items) = value? else {
        return None;
    };
    let mut phases = Vec::new();
    for item in items {
        let Some(record) = item.as_object() else {
            continue;
        };
        let name = record
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        let mut tasks = Vec::new();
        if let Some(serde_json::Value::Array(raw_tasks)) = record.get("tasks") {
            for raw in raw_tasks {
                if let Some(task) = todo_task_from_value(raw) {
                    tasks.push(task);
                }
            }
        }
        if name.is_empty() && tasks.is_empty() {
            continue;
        }
        phases.push(TodoCompactPhase { name, tasks });
    }
    (!phases.is_empty()).then_some(phases)
}

fn todo_task_from_value(value: &serde_json::Value) -> Option<TodoCompactTask> {
    let record = value.as_object()?;
    let content = record
        .get("content")
        .and_then(serde_json::Value::as_str)?
        .trim();
    if content.is_empty() {
        return None;
    }
    let status = record
        .get("status")
        .and_then(serde_json::Value::as_str)
        .filter(|status| {
            matches!(
                *status,
                "pending" | "in_progress" | "completed" | "abandoned"
            )
        })
        .unwrap_or("pending")
        .to_owned();
    let mut waiting_on = Vec::new();
    if let Some(serde_json::Value::Array(reasons)) = record.get("waitingOn") {
        for reason in reasons {
            let Some(text) = reason
                .get("content")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
            else {
                continue;
            };
            if !text.is_empty() {
                waiting_on.push(bound_display_text(text, TODO_WAITING_CHAR_LIMIT));
            }
        }
    }
    Some(TodoCompactTask {
        content: bound_display_text(content, TODO_TASK_CHAR_LIMIT),
        status,
        waiting_on,
    })
}

fn todo_phases_from_init_args(arguments: &serde_json::Value) -> Option<Vec<TodoCompactPhase>> {
    if argument_str(arguments, &["op"]).as_deref() != Some("init") {
        return None;
    }
    if let Some(serde_json::Value::Array(list)) = arguments.get("list") {
        let mut phases = Vec::new();
        for item in list {
            if let Some(phase) = todo_phase_from_init_value(item) {
                phases.push(phase);
            }
        }
        return (!phases.is_empty()).then_some(phases);
    }
    todo_phase_from_init_value(arguments).map(|phase| vec![phase])
}

fn todo_phase_from_init_value(value: &serde_json::Value) -> Option<TodoCompactPhase> {
    let record = value.as_object()?;
    let name = record
        .get("phase")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    let mut tasks = Vec::new();
    if let Some(serde_json::Value::Array(items)) = record.get("items") {
        for item in items {
            let Some(content) = item.as_str().map(str::trim) else {
                continue;
            };
            if content.is_empty() {
                continue;
            }
            tasks.push(TodoCompactTask {
                content: bound_display_text(content, TODO_TASK_CHAR_LIMIT),
                status: "pending".to_owned(),
                waiting_on: Vec::new(),
            });
        }
    }
    if name.is_empty() && tasks.is_empty() {
        return None;
    }
    Some(TodoCompactPhase { name, tasks })
}

/// Structured ordinary Todo body: `☑`/`☐` tree rows from `details.phases`
/// or an `init` args preview. Compact keeps at most eight task rows,
/// preferring active/open tasks and at most one recent completed task when
/// space remains. Phase names render only when more than one phase exists.
/// Remaining-items prose and raw JSON never become card content when a
/// snapshot parses. `None` for every non-todo card so callers keep the
/// ordinary result-text path.
fn todo_compact_lines(card: &ToolCard, content: &str, expanded: bool) -> Option<Vec<String>> {
    if !card.tool_name.eq_ignore_ascii_case("todo") {
        return None;
    }
    let snapshot = todo_card_snapshot(card);
    let mut lines = Vec::new();
    if card.is_error && !card.cancelled {
        let error = content.trim();
        lines.push(if error.is_empty() {
            "todo op failed".to_owned()
        } else {
            bound_display_text(error, TODO_TASK_CHAR_LIMIT)
        });
    }
    let selected = if expanded {
        None
    } else {
        Some(todo_selected_task_keys(&snapshot))
    };
    lines.extend(todo_structured_lines(&snapshot, selected.as_deref()));
    if lines.is_empty() {
        if card.status.is_partial() {
            return Some(Vec::new());
        }
        let fallback = content.trim();
        if !fallback.is_empty() {
            lines.push(bound_display_text(fallback, TODO_TASK_CHAR_LIMIT));
        }
    }
    Some(lines)
}

/// Reads the first present string field from `arguments` (`path` vs `file`
/// style aliases mirror `tool_code_language`).
fn argument_str(arguments: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        arguments
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    })
}

fn block_text(block: &pi_ai::ContentBlock) -> Option<&str> {
    match block {
        pi_ai::ContentBlock::Text { text, .. } => Some(text),
        _ => None,
    }
}

/// Counts distinct non-empty entries preserving first-seen order semantics.
fn dedup_count(entries: &[&str]) -> usize {
    let mut seen = Vec::new();
    for entry in entries {
        if !entry.is_empty() && !seen.contains(entry) {
            seen.push(entry);
        }
    }
    seen.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_agent::{AgentEvent, AgentToolResult};
    use pi_ai::{ContentBlock, Message, ToolResultMessage, now_millis};
    use serde_json::json;
    use unicode_segmentation::UnicodeSegmentation;

    fn app(event: AgentEvent) -> ApplicationEvent {
        ApplicationEvent::Agent(event)
    }
    fn start(id: &str, name: &str, arguments: serde_json::Value) -> ApplicationEvent {
        app(AgentEvent::ToolExecutionStart {
            tool_call_id: id.into(),
            tool_name: name.into(),
            arguments,
        })
    }
    fn update(id: &str, name: &str, text: &str) -> ApplicationEvent {
        app(AgentEvent::ToolExecutionUpdate {
            tool_call_id: id.into(),
            tool_name: name.into(),
            arguments: serde_json::Value::Null,
            partial_result: AgentToolResult::text(text),
        })
    }
    fn end(
        id: &str,
        name: &str,
        text: &str,
        details: serde_json::Value,
        is_error: bool,
    ) -> ApplicationEvent {
        let mut result = AgentToolResult::text(text);
        result.details = details;
        app(AgentEvent::ToolExecutionEnd {
            tool_call_id: id.into(),
            tool_name: name.into(),
            result,
            is_error,
        })
    }
    fn result(
        id: &str,
        name: &str,
        text: &str,
        details: Option<serde_json::Value>,
        is_error: bool,
    ) -> ApplicationEvent {
        app(AgentEvent::MessageEnd {
            message: Message::ToolResult(ToolResultMessage {
                tool_call_id: id.into(),
                tool_name: name.into(),
                content: vec![ContentBlock::text(text)],
                usage: None,
                details,
                added_tool_names: Vec::new(),
                is_error,
                timestamp: now_millis(),
            }),
        })
    }
    fn content(rows: &ToolCardRows) -> Vec<String> {
        rows.rows
            .iter()
            .filter(|row| row.role == ToolCardRowRole::Content)
            .map(|row| row.text.clone())
            .collect()
    }
    fn title(rows: &ToolCardRows) -> &str {
        rows.rows
            .iter()
            .find(|row| row.role == ToolCardRowRole::Command)
            .map_or("", |row| row.text.as_str())
    }

    #[test]
    fn concurrent_same_name_preserves_ids_order_and_results() {
        let mut a = ToolCardPresentationAdapter::new();
        for e in [
            start("a", "read", json!({"path":"a"})),
            start("b", "read", json!({"path":"b"})),
            update("b", "read", "partial-b"),
            end("b", "read", "body-b", json!({}), false),
            end("a", "read", "body-a", json!({}), false),
        ] {
            a.apply_application_event(&e);
        }
        assert_eq!(
            a.compact_rows()
                .iter()
                .map(|r| r.tool_call_id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(content(&a.rows("b", false).unwrap()), vec!["body-b"]);
    }

    #[test]
    fn terminal_orphan_reconciliation_and_redaction() {
        let secret = "credential-redaction-fixture-value";
        let mut a = ToolCardPresentationAdapter::new();
        for e in [
            start("c", "bash", json!({"command":"sleep"})),
            end(
                "c",
                "bash",
                "Operation aborted",
                json!({"cancelled":true}),
                true,
            ),
            result(
                "c",
                "bash",
                "Operation aborted",
                Some(json!({"cancelled":true,"signal":"SIGINT"})),
                true,
            ),
            result("o", "read", "recovered", None, false),
            start("s", "http", json!({"api_key":secret,"password":"hunter2"})),
            end(
                "s",
                "http",
                &format!("token={secret}"),
                json!({"authorization":secret}),
                true,
            ),
        ] {
            a.apply_application_event(&e);
        }
        assert_eq!(a.projection().len(), 3);
        assert_eq!(
            a.rows("c", false).unwrap().status,
            ToolCallViewStatus::Cancelled
        );
        assert_eq!(
            a.rows("o", false).unwrap().status,
            ToolCallViewStatus::OrphanRepaired
        );
        let serialized = serde_json::to_string(&a.expanded_rows("s").unwrap()).unwrap();
        assert!(!serialized.contains(secret));
        assert!(!serialized.contains("hunter2"));
        assert!(serialized.contains("[REDACTED]"));
    }

    #[test]
    fn short_empty_long_and_expanded_bash_match_capture() {
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start(
            "s",
            "bash",
            json!({"command":"printf short-output"}),
        ));
        a.apply_application_event(&end(
            "s",
            "bash",
            "short-output",
            serde_json::Value::Null,
            false,
        ));
        let short = a.rows("s", false).unwrap();
        assert_eq!(title(&short), "Bash");
        assert_eq!(short.arguments_summary, "printf short-output");
        assert_eq!(content(&short), vec!["short-output"]);
        assert!(!short.truncated);
        a.apply_application_event(&start("e", "bash", json!({"command":"true"})));
        a.apply_application_event(&end("e", "bash", "", serde_json::Value::Null, false));
        let empty = a.rows("e", false).unwrap();
        assert_eq!(title(&empty), "Bash");
        assert!(content(&empty).is_empty());
        let body = (1..=30)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        a.apply_application_event(&start("l", "bash", json!({"command":"seq 1 30"})));
        a.apply_application_event(&end("l", "bash", &body, serde_json::Value::Null, false));
        let compact = a.rows("l", false).unwrap();
        assert_eq!(title(&compact), "Bash");
        assert_eq!(compact.omitted_content_lines, 20);
        assert_eq!(
            content(&compact),
            (21..=30).map(|n| n.to_string()).collect::<Vec<_>>()
        );
        assert_eq!(content(&a.rows("l", true).unwrap()).len(), 30);
        a.apply_application_event(&start("t", "task", json!({"prompt":"inspect"})));
        let task = a.rows("t", false).unwrap();
        assert_eq!(title(&task), "Task");
    }

    #[test]
    fn bash_command_field_keeps_multiline_command_and_redacts() {
        let mut a = ToolCardPresentationAdapter::new();
        let command = "CA=/tmp/oh-my-pi\n# session-context\nbuildrg -n \"export fu\"";
        a.apply_application_event(&start("m", "bash", json!({"command": command})));
        a.apply_application_event(&end("m", "bash", "out", serde_json::Value::Null, false));
        let rows = a.rows("m", false).unwrap();
        let bash_command = rows
            .bash_command
            .as_deref()
            .expect("bash card carries the command");
        assert_eq!(
            bash_command, command,
            "full multi-line command must survive untruncated"
        );
        assert_eq!(
            bash_command.lines().count(),
            3,
            "all three logical lines preserved"
        );
        // The title compact stays the 60-char summary; the untruncated
        // command lives in bash_command so the card renders line-by-line.
        assert!(
            rows.arguments_summary.len() <= 60,
            "title compact still bounded: {}",
            rows.arguments_summary
        );
        // Secrets are scrubbed before the card leaves the adapter.
        let secret = ["s", "k-", "abcdefghijklmnop1234"].concat();
        let secret_command = format!("curl -H 'Authorization: Bearer {secret}' /api");
        a.apply_application_event(&start("sec", "bash", json!({"command": secret_command})));
        a.apply_application_event(&end("sec", "bash", "", serde_json::Value::Null, false));
        let serialized = serde_json::to_string(&a.expanded_rows("sec").unwrap()).unwrap();
        assert!(!serialized.contains(&secret), "secret leaked: {serialized}");
        assert!(serialized.contains("[REDACTED]"));
        // Non-bash cards carry no bash_command.
        a.apply_application_event(&start("r", "read", json!({"path": "src/main.rs"})));
        a.apply_application_event(&end("r", "read", "body", serde_json::Value::Null, false));
        assert_eq!(a.rows("r", false).unwrap().bash_command, None);
    }

    #[test]
    fn hub_send_success_keeps_redacted_typed_metadata_without_body_rows() {
        let secret = ["s", "k-", "abcdefghijklmnop1234"].concat();
        let mut adapter = ToolCardPresentationAdapter::new();
        adapter.apply_application_event(&start(
            "hub",
            "hub",
            json!({
                "op": "send",
                "to": "DeepSeekRenderingFinal",
                "message": format!("token={secret}")
            }),
        ));
        adapter.apply_application_event(&end(
            "hub",
            "hub",
            "- DeepSeekRenderingFinal: woken",
            json!({"op":"send","receipts":[{"to":"DeepSeekRenderingFinal","outcome":"woken"}]}),
            false,
        ));
        let card = adapter.rows("hub", false).expect("hub card");
        assert_eq!(card.arguments["op"], "send");
        assert_eq!(card.arguments["to"], "DeepSeekRenderingFinal");
        assert!(!card.arguments.to_string().contains(&secret));
        assert!(card.arguments.to_string().contains("[REDACTED]"));
        assert_eq!(card.details["receipts"][0]["outcome"], "woken");
        assert!(
            card.rows
                .iter()
                .all(|row| row.role == ToolCardRowRole::Command),
            "successful Hub send remains metadata-only: {:?}",
            card.rows
        );
    }

    #[test]
    fn hub_successful_wait_projects_exact_redacted_bounded_received_body() {
        let exact_body = "beta-to-main-tmux";
        let mut adapter = ToolCardPresentationAdapter::new();
        adapter.apply_application_event(&start("wait", "hub", json!({"op":"wait","from":"Beta"})));
        adapter.apply_application_event(&end(
            "wait",
            "hub",
            "[raw-message-id] Beta: model-facing-prose-must-not-render",
            json!({
                "op": "wait",
                "message": {
                    "id": "raw-message-id",
                    "from": "Beta",
                    "to": "Main",
                    "body": exact_body,
                    "timestamp": 7
                }
            }),
            false,
        ));
        for expanded in [false, true] {
            let rows = adapter.rows("wait", expanded).expect("successful Hub wait");
            assert_eq!(
                hub_visible_rows(&rows),
                vec![(ToolCardRowRole::Content, exact_body.to_owned())],
                "the typed received body is the only visible wait result row",
            );
            assert!(
                !rows
                    .rows
                    .iter()
                    .any(|row| row.text.contains("model-facing-prose"))
            );
        }

        let secret = ["s", "k-", "abcdefghijklmnop1234"].concat();
        let long_body = format!(
            "token={secret} {}",
            "λ".repeat(HUB_RECEIVED_BODY_CHAR_LIMIT + 80)
        );
        let mut bounded = ToolCardPresentationAdapter::new();
        bounded.apply_application_event(&start(
            "long-wait",
            "hub",
            json!({"op":"wait","from":"Beta"}),
        ));
        bounded.apply_application_event(&end(
            "long-wait",
            "hub",
            "raw long result prose",
            json!({
                "op": "wait",
                "message": {
                    "id": "long-message-id",
                    "from": "Beta",
                    "to": "Main",
                    "body": long_body,
                    "timestamp": 8
                }
            }),
            false,
        ));
        for expanded in [false, true] {
            let rows = bounded
                .rows("long-wait", expanded)
                .expect("bounded Hub wait");
            let visible = hub_visible_rows(&rows);
            assert_eq!(
                visible.len(),
                1,
                "one bounded received-body row: {visible:?}"
            );
            assert_eq!(visible[0].0, ToolCardRowRole::Content);
            assert_eq!(visible[0].1.chars().count(), HUB_RECEIVED_BODY_CHAR_LIMIT);
            assert!(
                visible[0].1.ends_with('…'),
                "truncation is explicit: {visible:?}"
            );
            assert!(
                visible[0].1.contains("[REDACTED]"),
                "secret is redacted before bounding: {visible:?}"
            );
            assert!(
                !visible[0].1.contains(&secret),
                "secret leaked: {visible:?}"
            );
        }

        let mut multiline = ToolCardPresentationAdapter::new();
        multiline.apply_application_event(&start(
            "multiline-wait",
            "hub",
            json!({"op":"wait","from":"Beta"}),
        ));
        multiline.apply_application_event(&end(
            "multiline-wait",
            "hub",
            "raw multiline result prose",
            json!({
                "op": "wait",
                "message": {
                    "id": "multiline-message-id",
                    "from": "Beta",
                    "to": "Main",
                    "body": (1..=12).map(|line| format!("line-{line}")).collect::<Vec<_>>().join("\n"),
                    "timestamp": 9
                }
            }),
            false,
        ));
        for expanded in [false, true] {
            let rows = multiline
                .rows("multiline-wait", expanded)
                .expect("line-bounded Hub wait");
            let visible = hub_visible_rows(&rows);
            assert_eq!(
                visible.len(),
                HUB_RECEIVED_BODY_LINE_LIMIT,
                "received body line cap: {visible:?}"
            );
            assert_eq!(visible[0].1, "line-1");
            assert_eq!(visible[HUB_RECEIVED_BODY_LINE_LIMIT - 1].1, "line-6…");
        }
    }
    fn hub_visible_rows(rows: &ToolCardRows) -> Vec<(ToolCardRowRole, String)> {
        rows.rows
            .iter()
            .filter(|row| row.role != ToolCardRowRole::Command)
            .map(|row| (row.role, row.text.clone()))
            .collect()
    }

    fn assert_hub_rows_exclude(rows: &ToolCardRows, forbidden: &[&str]) {
        let visible = hub_visible_rows(rows);
        let rendered = visible
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for sentinel in forbidden {
            assert!(
                !rendered.contains(sentinel),
                "Hub row leaked {sentinel:?}: {visible:?}"
            );
        }
        assert!(
            visible
                .iter()
                .all(|(_, text)| text.chars().count() <= HUB_SAFE_ROW_CHAR_LIMIT),
            "Hub row exceeded bound: {visible:?}"
        );
        assert!(
            rows.rows
                .iter()
                .all(|row| row.role != ToolCardRowRole::Content
                    && row.role != ToolCardRowRole::Details),
            "non-successful-wait Hub projection must never emit raw content/details: {:?}",
            rows.rows
        );
    }

    #[test]
    fn hub_jobs_projects_bounded_failed_count_and_safe_previews() {
        let forbidden = [
            "job-hostile-id",
            "agent-hostile-id",
            "BODY-HOSTILE-SENTINEL",
            "/home/hostile/private",
            "model-hostile-name",
            "Grok",
            "reviewer",
            "assignment-hostile-text",
        ];
        let mut adapter = ToolCardPresentationAdapter::new();
        adapter.apply_application_event(&start(
            "jobs",
            "hub",
            json!({"op":"jobs","ids":["job-hostile-id"]}),
        ));
        adapter.apply_application_event(&end(
            "jobs",
            "hub",
            "raw jobs prose BODY-HOSTILE-SENTINEL /home/hostile/private",
            json!({
                "op": "jobs",
                "jobs": [
                    {
                        "id": "job-hostile-id",
                        "agentId": "agent-hostile-id",
                        "agent": "model-hostile-name",
                        "description": "assignment-hostile-text",
                        "status": "failed",
                        "result": {
                            "output": "BODY-HOSTILE-SENTINEL",
                            "error": "Grok provider failed at /home/hostile/private for job-hostile-id reviewer role assignment-hostile-text"
                        }
                    },
                    {
                        "id": "second-id",
                        "status": "failed",
                        "result": {"structuredOutput": {"valid": false, "error": "BODY-HOSTILE-SENTINEL"}}
                    },
                    {
                        "id": "third-id",
                        "status": "failed",
                        "result": {"error": "permission denied for /home/hostile/private"}
                    },
                    {
                        "id": "fourth-id",
                        "status": "failed",
                        "result": {"error": "socket transport unavailable"}
                    },
                    {
                        "id": "soft-budget-id",
                        "status": "completed",
                        "softBudgetExhausted": true,
                        "result": {"error": "must not be counted"}
                    }
                ]
            }),
            false,
        ));

        let compact = adapter.rows("jobs", false).expect("compact Hub jobs");
        assert_eq!(
            hub_visible_rows(&compact),
            vec![
                (ToolCardRowRole::Status, "Failed jobs: 4".to_owned()),
                (
                    ToolCardRowRole::Error,
                    "failure · provider failed; retry or inspect failed jobs".to_owned()
                ),
            ]
        );
        assert_eq!(compact.omitted_content_lines, 3);
        assert!(compact.truncated);
        assert_hub_rows_exclude(&compact, &forbidden);

        let expanded = adapter.rows("jobs", true).expect("expanded Hub jobs");
        assert_eq!(
            hub_visible_rows(&expanded),
            vec![
                (ToolCardRowRole::Status, "Failed jobs: 4".to_owned()),
                (
                    ToolCardRowRole::Error,
                    "failure · provider failed; retry or inspect failed jobs".to_owned()
                ),
                (
                    ToolCardRowRole::Error,
                    "validation · output validation failed; retry or inspect failed jobs"
                        .to_owned()
                ),
                (
                    ToolCardRowRole::Error,
                    "failure · permission denied; retry or inspect failed jobs".to_owned()
                ),
                (
                    ToolCardRowRole::Status,
                    "1 more failed job omitted".to_owned()
                ),
            ]
        );
        assert_eq!(expanded.omitted_content_lines, 1);
        assert!(expanded.truncated);
        assert_hub_rows_exclude(&expanded, &forbidden);
    }

    #[test]
    fn hub_wait_projects_typed_timeout_and_operation_failure_rows() {
        let forbidden = [
            "00000000-0000-7000-8000-0000000000ab",
            "BODY-HOSTILE-SENTINEL",
            "/home/hostile/private",
            "model-hostile-name",
            "Grok",
            "reviewer",
        ];

        let mut message_wait = ToolCardPresentationAdapter::new();
        message_wait.apply_application_event(&start(
            "message-wait",
            "hub",
            json!({"op":"wait","timeoutMs":500,"from":"00000000-0000-7000-8000-0000000000ab"}),
        ));
        message_wait.apply_application_event(&end(
            "message-wait",
            "hub",
            "No message before timeout.",
            json!({"op":"wait","message":null}),
            false,
        ));
        for expanded in [false, true] {
            let rows = message_wait
                .rows("message-wait", expanded)
                .expect("typed message timeout");
            assert_eq!(
                hub_visible_rows(&rows),
                vec![(
                    ToolCardRowRole::Status,
                    "Wait timed out · no message arrived. Retry wait.".to_owned()
                )]
            );
            assert!(
                !rows.is_error,
                "typed wait timeout is a warning/status, not failure"
            );
            assert_hub_rows_exclude(&rows, &forbidden);
        }

        let mut immediate_wait = ToolCardPresentationAdapter::new();
        immediate_wait.apply_application_event(&start(
            "immediate-wait",
            "hub",
            json!({"op":"wait","timeoutMs":0}),
        ));
        immediate_wait.apply_application_event(&end(
            "immediate-wait",
            "hub",
            "No message before timeout.",
            json!({"op":"wait","message":null}),
            false,
        ));
        let immediate = immediate_wait
            .rows("immediate-wait", false)
            .expect("zero-duration typed timeout");
        assert_eq!(
            hub_visible_rows(&immediate),
            vec![(
                ToolCardRowRole::Status,
                "Wait timed out · no message arrived. Retry wait.".to_owned()
            )]
        );
        assert!(!immediate.is_error);

        let mut job_wait = ToolCardPresentationAdapter::new();
        job_wait.apply_application_event(&start(
            "job-wait",
            "hub",
            json!({"op":"wait","ids":["00000000-0000-7000-8000-0000000000ab"],"timeoutMs":500}),
        ));
        job_wait.apply_application_event(&end(
            "job-wait",
            "hub",
            "No job completed before timeout. BODY-HOSTILE-SENTINEL",
            json!({"op":"wait","jobs":[{"id":"00000000-0000-7000-8000-0000000000ab","status":"running","description":"BODY-HOSTILE-SENTINEL"}]}),
            false,
        ));
        for expanded in [false, true] {
            let rows = job_wait
                .rows("job-wait", expanded)
                .expect("typed job timeout");
            assert_eq!(
                hub_visible_rows(&rows),
                vec![(
                    ToolCardRowRole::Status,
                    "Wait timed out · no job settled. Retry wait or inspect jobs.".to_owned()
                )]
            );
            assert!(
                !rows.is_error,
                "typed job wait timeout is a warning/status, not failure"
            );
            assert_hub_rows_exclude(&rows, &forbidden);
        }

        let mut malformed_job_wait = ToolCardPresentationAdapter::new();
        malformed_job_wait.apply_application_event(&start(
            "empty-job-wait",
            "hub",
            json!({"op":"wait","ids":["00000000-0000-7000-8000-0000000000ab"],"timeoutMs":500}),
        ));
        malformed_job_wait.apply_application_event(&end(
            "empty-job-wait",
            "hub",
            "No job completed before timeout.",
            json!({"op":"wait","jobs":[]}),
            false,
        ));
        let malformed_rows = malformed_job_wait
            .rows("empty-job-wait", false)
            .expect("malformed empty jobs wait");
        assert!(
            hub_visible_rows(&malformed_rows).is_empty(),
            "empty jobs are not typed timeout evidence: {:?}",
            malformed_rows.rows
        );
        assert_hub_rows_exclude(&malformed_rows, &forbidden);

        let mut failed_wait = ToolCardPresentationAdapter::new();
        failed_wait.apply_application_event(&start(
            "failed-wait",
            "hub",
            json!({"op":"wait","timeoutMs":500}),
        ));
        failed_wait.apply_application_event(&end(
            "failed-wait",
            "hub",
            "Grok provider failed at /home/hostile/private reviewer role model-hostile-name model BODY-HOSTILE-SENTINEL",
            serde_json::Value::Null,
            true,
        ));
        for expanded in [false, true] {
            let rows = failed_wait
                .rows("failed-wait", expanded)
                .expect("failed Hub wait");
            assert_eq!(
                hub_visible_rows(&rows),
                vec![(
                    ToolCardRowRole::Error,
                    "Wait failed · provider failed. Retry wait.".to_owned()
                )]
            );
            assert!(rows.is_error, "operation failure remains a failure");
            assert_hub_rows_exclude(&rows, &forbidden);
        }

        let mut secret_failure = ToolCardPresentationAdapter::new();
        let secret = ["s", "k-", "abcdefghijklmnop1234"].concat();
        secret_failure.apply_application_event(&start(
            "secret-failure",
            "hub",
            json!({"op":"cancel","ids":["job-hostile-id"]}),
        ));
        secret_failure.apply_application_event(&end(
            "secret-failure",
            "hub",
            &format!("permission denied for job-hostile-id at https://example.invalid/private token={secret}\u{1b}[31m"),
            serde_json::Value::Null,
            true,
        ));
        for expanded in [false, true] {
            let rows = secret_failure
                .rows("secret-failure", expanded)
                .expect("redacted operation failure");
            assert_eq!(
                hub_visible_rows(&rows),
                vec![(
                    ToolCardRowRole::Error,
                    "Cancel failed · permission denied. Check inputs and retry.".to_owned()
                )]
            );
            assert_hub_rows_exclude(
                &rows,
                &[
                    "job-hostile-id",
                    "https://example.invalid",
                    &secret,
                    "\u{1b}",
                ],
            );
        }
    }

    #[test]
    fn failed_hub_wait_with_message_details_keeps_safe_error_projection() {
        let mut adapter = ToolCardPresentationAdapter::new();
        adapter.apply_application_event(&start("failed-message-wait", "hub", json!({"op":"wait"})));
        adapter.apply_application_event(&end(
            "failed-message-wait",
            "hub",
            "connection failed",
            json!({
                "op": "wait",
                "message": {
                    "id": "must-not-project",
                    "from": "SensitiveSender",
                    "to": "Main",
                    "body": "must-not-project-body"
                }
            }),
            true,
        ));
        for expanded in [false, true] {
            let rows = adapter
                .rows("failed-message-wait", expanded)
                .expect("failed wait with message details");
            assert_eq!(
                hub_visible_rows(&rows),
                vec![(
                    ToolCardRowRole::Error,
                    "Wait failed · connection or transport failed. Retry wait.".to_owned()
                )],
            );
            assert_hub_rows_exclude(
                &rows,
                &[
                    "SensitiveSender",
                    "must-not-project",
                    "must-not-project-body",
                ],
            );
        }
    }

    #[test]
    fn hub_malformed_fallback_is_fixed_actionable_and_metadata_only() {
        let forbidden = [
            "HOSTILE-OP-SENTINEL",
            "BODY-HOSTILE-SENTINEL",
            "00000000-0000-7000-8000-0000000000ab",
            "/home/hostile/private",
            "model-hostile-name",
            "assignment-hostile-text",
        ];
        let mut adapter = ToolCardPresentationAdapter::new();
        adapter.apply_application_event(&start(
            "malformed",
            "hub",
            json!({
                "op": "HOSTILE-OP-SENTINEL /home/hostile/private",
                "ids": ["00000000-0000-7000-8000-0000000000ab"],
                "message": "BODY-HOSTILE-SENTINEL",
                "model": "model-hostile-name",
                "assignment": "assignment-hostile-text"
            }),
        ));
        adapter.apply_application_event(&end(
            "malformed",
            "hub",
            "unsupported hub operation HOSTILE-OP-SENTINEL at /home/hostile/private for model-hostile-name BODY-HOSTILE-SENTINEL",
            json!({"malformed":"assignment-hostile-text","id":"00000000-0000-7000-8000-0000000000ab"}),
            true,
        ));

        for expanded in [false, true] {
            let rows = adapter
                .rows("malformed", expanded)
                .expect("malformed Hub fallback");
            assert_eq!(
                hub_visible_rows(&rows),
                vec![(
                    ToolCardRowRole::Error,
                    "Hub operation failed · operation failed. Check inputs and retry.".to_owned()
                )]
            );
            assert_hub_rows_exclude(&rows, &forbidden);
        }

        let mut empty_fallback = ToolCardPresentationAdapter::new();
        empty_fallback.apply_application_event(&start(
            "empty-fallback",
            "hub",
            json!({"op":"cancel","ids":["job-hostile-id"]}),
        ));
        empty_fallback.apply_application_event(&end(
            "empty-fallback",
            "hub",
            "job-hostile-id /home/hostile/private",
            serde_json::Value::Null,
            true,
        ));
        for expanded in [false, true] {
            let rows = empty_fallback
                .rows("empty-fallback", expanded)
                .expect("empty safe fallback");
            assert_eq!(
                hub_visible_rows(&rows),
                vec![(
                    ToolCardRowRole::Error,
                    "Cancel failed · operation failed. Check inputs and retry.".to_owned()
                )]
            );
            assert_hub_rows_exclude(&rows, &["job-hostile-id", "/home/hostile/private"]);
        }
    }

    #[test]
    fn task_request_preserves_shared_context_and_children() {
        let request = task_delegation_request(&json!({
            "context": "# Goal\nShip it\n\n# Constraints\nBe precise",
            "tasks": [
                {"name": "Alpha", "agent": "reviewer", "task": "Review the adapter"},
                {"name": "Beta", "task": "Render the card"}
            ]
        }))
        .expect("task batch");
        assert_eq!(request.children.len(), 2);
        assert_eq!(request.children[0].name.as_deref(), Some("Alpha"));
        assert_eq!(request.children[0].agent.as_deref(), Some("reviewer"));
        assert!(request.context.contains("# Constraints"));
    }

    #[test]
    fn task_sections_split_long_chinese_goal_constraints_contract() {
        let goal = "验证用户给出的长中文 Task delegation（Goal/Constraints/Contract + DeepSeek child）在 Web transcript 中是否正确、可读、无溢出地结构化渲染。";
        let constraints = "只用 DeepSeek。共享工作树并发修改；不得回滚、格式化、提交或运行全套测试。先读现有 Task card 实现与现有真实 E2E；不要重复实现。默认只改测试/E2E；保留 redaction/bounds/raw collapsed。";
        let contract = "正确渲染：标题 Task；Goal/Constraints/Contract 独立区块；child name/agent/target/status；长中文正常 wrap；无水平 overflow；raw JSON 默认折叠；desktop 和 390px mobile 可读。";
        let sections = task_delegation_sections(&format!(
            "# Goal\n{goal}\n\n# Constraints\n{constraints}\n\n# Contract\n{contract}"
        ));
        assert_eq!(
            sections.goal, goal,
            "Goal body must hold the long Chinese verbatim"
        );
        assert_eq!(
            sections.constraints, constraints,
            "Constraints body must hold the long Chinese verbatim"
        );
        assert_eq!(
            sections.contract, contract,
            "Contract body must hold the long Chinese verbatim"
        );
        assert!(
            !sections.goal.contains("只用 DeepSeek"),
            "sections must not cross-contaminate"
        );
        assert!(
            !sections.constraints.contains("验证用户给出的"),
            "sections must not cross-contaminate"
        );
        assert!(sections.goal.chars().count() <= TASK_SECTION_CHAR_LIMIT);
        assert!(sections.constraints.chars().count() <= TASK_SECTION_CHAR_LIMIT);
        assert!(sections.contract.chars().count() <= TASK_SECTION_CHAR_LIMIT);
    }

    #[test]
    fn task_sections_bound_overlong_bodies_and_fold_leading_prose() {
        // An overlong section is bounded with a trailing ellipsis.
        let long = "长".repeat(TASK_SECTION_CHAR_LIMIT + 120);
        let sections = task_delegation_sections(&format!("# Goal\n{long}"));
        assert_eq!(sections.goal.chars().count(), TASK_SECTION_CHAR_LIMIT);
        assert!(
            sections.goal.ends_with('…'),
            "bounded body ends with ellipsis"
        );
        assert!(
            sections
                .goal
                .chars()
                .all(|character| character == '长' || character == '…')
        );

        // Leading unrecognized prose folds into Goal so a free-form briefing
        // still surfaces.
        let sections =
            task_delegation_sections("briefing without headings\n\n# Constraints\nBe precise");
        assert!(
            sections.goal.contains("briefing without headings"),
            "leading prose folds into Goal"
        );
        assert_eq!(sections.constraints, "Be precise");
        assert!(sections.contract.is_empty());

        // A context with no content yields no sections at all.
        let sections = task_delegation_sections("   \n\n");
        assert!(
            sections.goal.is_empty()
                && sections.constraints.is_empty()
                && sections.contract.is_empty()
        );

        // Regression: a truncated body whose 479-char prefix ends inside a
        // whitespace run must still total exactly the budget with the
        // ellipsis — the trimmed tail must not shrink the total below
        // TASK_SECTION_CHAR_LIMIT (the real long-Chinese fixture straddles a
        // space at index 478).
        let straddle = format!(
            "{} {} {}",
            "长".repeat(TASK_SECTION_CHAR_LIMIT - 2),
            "尾".repeat(40),
            "结"
        );
        let sections = task_delegation_sections(&format!("# Goal\n{straddle}"));
        assert_eq!(
            sections.goal.chars().count(),
            TASK_SECTION_CHAR_LIMIT,
            "whitespace-straddle total"
        );
        assert!(sections.goal.ends_with('…'), "whitespace-straddle ellipsis");
    }

    #[test]
    fn task_sections_match_heading_aliases_and_ignore_unknown_headings() {
        let sections = task_delegation_sections(
            "## Goal: ship\n\nthe objective\n\n### Constraints & Preferences\n\nprecise work\n\n#### Acceptance Criteria\n\none card",
        );
        assert!(
            sections.goal.contains("the objective"),
            "Goal: alias with trailing prose: {sections:?}"
        );
        assert_eq!(
            sections.constraints, "precise work",
            "Constraints & Preferences alias: {sections:?}"
        );
        assert_eq!(
            sections.contract, "one card",
            "Acceptance Criteria alias: {sections:?}"
        );
        assert!(
            !sections.goal.contains("Constraints"),
            "heading lines never leak into bodies: {sections:?}"
        );

        // Unknown headings are body content of the current section, not new
        // sections (Web parity).
        let sections = task_delegation_sections("# Goal\none\n\n## Notes\nmore");
        assert_eq!(sections.goal, "one\n\n## Notes\nmore");
        assert!(sections.constraints.is_empty() && sections.contract.is_empty());

        // ATX edge cases: trailing hashes stripped, missing space is not a heading.
        let sections = task_delegation_sections("## Goal ##\nobjective\n\n##Goal\nnot a heading");
        assert_eq!(
            sections.goal, "objective\n\n##Goal\nnot a heading",
            "##Goal must stay body text"
        );
    }
    #[test]
    fn edit_read_write_and_foreground_status_are_specific() {
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start("x", "edit", json!({"path":"f"})));
        a.apply_application_event(&end(
            "x",
            "edit",
            "success",
            json!({"diff":"@@\n-old\n+new"}),
            false,
        ));
        let edit = a.rows("x", false).unwrap();
        assert_eq!(title(&edit), "Edit");
        assert!(content(&edit).contains(&"@@".into()));
        let body = (1..=8)
            .map(|n| format!("line-{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        a.apply_application_event(&start("r", "read", json!({"path":"f"})));
        a.apply_application_event(&end("r", "read", &body, serde_json::Value::Null, false));
        let read = a.rows("r", false).unwrap();
        assert_eq!(title(&read), "Read");
        assert_eq!(
            content(&read),
            (3..=8).map(|n| format!("line-{n}")).collect::<Vec<_>>()
        );
        assert_eq!(read.omitted_content_lines, 2);
        a.apply_application_event(&start("w", "write", json!({"path":"f","content":body})));
        a.apply_application_event(&end(
            "w",
            "write",
            "Successfully wrote",
            serde_json::Value::Null,
            false,
        ));
        let write = a.rows("w", false).unwrap();
        assert_eq!(title(&write), "Write");
        assert_eq!(write.omitted_content_lines, 2);
        let failed = BashExecutionMessage {
            command: "false".into(),
            output: String::new(),
            exit_code: Some(7),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            timestamp: 42,
            exclude_from_context: None,
        };
        let rows = ToolCardPresentationAdapter::bash_execution_rows(&failed, false);
        assert_eq!(title(&rows), "Bash");
        assert!(
            rows.rows
                .iter()
                .any(|r| r.role == ToolCardRowRole::Status && r.text == "⟦Exit: 7⟧")
        );
    }
    #[test]
    fn read_code_language_uses_only_trusted_path_extensions() {
        let mut adapter = ToolCardPresentationAdapter::new();
        adapter.apply_application_event(&start("rust", "read", json!({"path":"src/lib.rs"})));
        adapter.apply_application_event(&end(
            "rust",
            "read",
            "fn main() {}",
            serde_json::Value::Null,
            false,
        ));
        assert_eq!(
            adapter
                .rows("rust", false)
                .unwrap()
                .code_language
                .as_deref(),
            Some("rust")
        );

        adapter.apply_application_event(&start("plain", "read", json!({"path":"notes.unknown"})));
        adapter.apply_application_event(&end(
            "plain",
            "read",
            "fn main() {}",
            serde_json::Value::Null,
            false,
        ));
        assert_eq!(adapter.rows("plain", false).unwrap().code_language, None);

        adapter.apply_application_event(&start("http", "http", json!({"path":"src/lib.rs"})));
        assert_eq!(adapter.rows("http", false).unwrap().code_language, None);
    }

    #[test]
    fn skill_read_card_uses_name_title_and_description_prose_without_frontmatter() {
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start("s", "read", json!({"path": "skill://research"})));
        a.apply_application_event(&end("s", "read", "---\nname: research\ndescription: \"Deep-dive codebase researcher. Produces structured docs.\"\n---\n# Research\n\nBody line.", serde_json::Value::Null, false));
        let card = a.rows("s", false).unwrap();
        assert_eq!(card.skill_name.as_deref(), Some("research"));
        assert_eq!(
            title(&card),
            "research",
            "skill name must become the card title"
        );
        let content_rows = content(&card);
        assert_eq!(
            content_rows[0], "Deep-dive codebase researcher. Produces structured docs.",
            "description must be the first body paragraph, unquoted"
        );
        assert!(content_rows.iter().any(|line| line == "# Research"));
        assert!(content_rows.iter().any(|line| line == "Body line."));
        let all = content_rows.join("\n");
        assert!(
            !all.contains("name:"),
            "raw frontmatter name must not render: {all}"
        );
        assert!(
            !all.contains("description:"),
            "raw frontmatter description must not render: {all}"
        );
        assert!(
            !all.contains("---"),
            "frontmatter delimiters must not render: {all}"
        );
        // Expanded keeps the same prose shape without truncation.
        assert_eq!(content(&a.rows("s", true).unwrap()).len(), 5);
    }

    #[test]
    fn long_skill_read_folds_top_down_and_keeps_description() {
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start("s", "read", json!({"path": "skill://long"})));
        let body = (1..=20)
            .map(|n| format!("body-{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        a.apply_application_event(&end(
            "s",
            "read",
            &format!("---\nname: long\ndescription: \"Lead description.\"\n---\n{body}"),
            serde_json::Value::Null,
            false,
        ));
        let card = a.rows("s", false).unwrap();
        assert_eq!(title(&card), "long");
        let content_rows = content(&card);
        assert_eq!(content_rows[0], "Lead description.");
        assert_eq!(content_rows.get(1).map(String::as_str), Some(""));
        assert_eq!(
            content_rows.get(2).map(String::as_str),
            Some("body-1"),
            "compact folds from the top so the head of the body stays visible"
        );
        assert_eq!(card.omitted_content_lines, 16, "22 prose lines - 6 visible");
        assert_eq!(content(&a.rows("s", true).unwrap()).len(), 22);
    }

    #[test]
    fn skill_block_scalar_descriptions_follow_yaml_folding() {
        let mut a = ToolCardPresentationAdapter::new();
        // `>` folds continuation lines into a single space-joined line.
        a.apply_application_event(&start("fold", "read", json!({"path": "skill://folded"})));
        a.apply_application_event(&end("fold", "read", "---\nname: folded\ndescription: >\n  Folded line one\n  folds into a\n  single line\n---\n# Folded\n\nBody.", serde_json::Value::Null, false));
        let card = a.rows("fold", false).unwrap();
        assert_eq!(title(&card), "folded");
        let content_rows = content(&card);
        assert_eq!(
            content_rows[0], "Folded line one folds into a single line",
            "`>` must fold with spaces, not newlines"
        );

        // `|` keeps the line breaks of a literal block.
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start("lit", "read", json!({"path": "skill://literal"})));
        a.apply_application_event(&end("lit", "read", "---\nname: literal\ndescription: |\n  Literal line one\n  keeps the newline\n---\n# Literal\n\nBody.", serde_json::Value::Null, false));
        let card = a.rows("lit", false).unwrap();
        assert_eq!(title(&card), "literal");
        let content_rows = content(&card);
        assert_eq!(content_rows[0], "Literal line one");
        assert_eq!(content_rows[1], "keeps the newline");
        let all = content_rows.join("\n");
        assert!(
            !all.contains("description:"),
            "raw frontmatter must not render: {all}"
        );
    }

    #[test]
    fn skill_subresource_and_frontmatter_less_reads_stay_raw() {
        let mut a = ToolCardPresentationAdapter::new();
        // A sub-resource of a skill is a plain file read, not the skill itself.
        a.apply_application_event(&start(
            "sub",
            "read",
            json!({"path": "skill://research/asset.md"}),
        ));
        a.apply_application_event(&end(
            "sub",
            "read",
            "---\nname: asset\ndescription: \"not the skill\"\n---\nasset body",
            serde_json::Value::Null,
            false,
        ));
        let sub = a.rows("sub", false).unwrap();
        assert_eq!(sub.skill_name, None);
        assert_eq!(title(&sub), "Read");
        assert!(
            content(&sub).iter().any(|line| line == "name: asset"),
            "sub-resource content stays raw"
        );
        // A failed/empty skill read without frontmatter stays raw too.
        a.apply_application_event(&start("gone", "read", json!({"path": "skill://missing"})));
        a.apply_application_event(&end(
            "gone",
            "read",
            "not found",
            serde_json::Value::Null,
            false,
        ));
        let gone = a.rows("gone", false).unwrap();
        assert_eq!(gone.skill_name, None);
        assert_eq!(title(&gone), "Read");
        assert_eq!(content(&gone), vec!["not found"]);
    }

    #[test]
    fn file_truncation_notice_matches_only_read_contract_lines() {
        // The three file-level notices `render_read_result` appends when the
        // file continues past the returned lines.
        assert!(is_file_truncation_notice(
            "[4320 more lines in file. Use offset=3885 to continue.]"
        ));
        assert!(is_file_truncation_notice(
            "[Showing lines 1-2000 of 6320. Use offset=2001 to continue.]"
        ));
        assert!(is_file_truncation_notice(
            "[Showing lines 1-2000 of 6320 (512KB limit). Use offset=2001 to continue.]"
        ));
        // Plain content, the render-level fold footer, and the read tool's
        // single-line-too-big diagnostic (no "Use offset=" continuation
        // contract) are not file-level notices.
        assert!(!is_file_truncation_notice("line-7"));
        assert!(!is_file_truncation_notice(
            "… 5 more lines ⟦Ctrl+O: Expand⟧"
        ));
        assert!(!is_file_truncation_notice(
            "[Line 3 is 1MB, exceeds 512KB limit. Use bash: sed -n '3p' f | head -c 524288]"
        ));
    }

    #[test]
    fn read_card_drops_file_offset_notice_and_folds_source_rows() {
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start("trunc", "read", json!({"path": "big.txt"})));
        let body = (1..=10)
            .map(|n| format!("line-{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        a.apply_application_event(&end(
            "trunc",
            "read",
            &format!("{body}\n\n[4320 more lines in file. Use offset=3885 to continue.]"),
            serde_json::Value::Null,
            false,
        ));
        let compact = a.rows("trunc", false).unwrap();
        let compact_lines = content(&compact);
        assert_eq!(
            compact_lines,
            vec!["line-6", "line-7", "line-8", "line-9", "line-10", ""]
        );
        assert_eq!(compact.omitted_content_lines, 5);
        assert!(compact.truncated);
        assert!(
            compact
                .rows
                .iter()
                .all(|row| row.role != ToolCardRowRole::Details)
        );
        let expanded = a.rows("trunc", true).unwrap();
        assert_eq!(
            content(&expanded),
            (1..=10)
                .map(|n| format!("line-{n}"))
                .chain(std::iter::once(String::new()))
                .collect::<Vec<_>>()
        );
        assert!(
            expanded
                .rows
                .iter()
                .all(|row| row.role != ToolCardRowRole::Details)
        );
    }

    #[test]
    fn read_card_never_materializes_media_details() {
        let mut adapter = ToolCardPresentationAdapter::new();
        adapter.apply_application_event(&start("media", "read", json!({"path": "clip.mp4"})));
        let media_marker = "binary-payload-sentinel".repeat(128 * 1024);
        adapter.apply_application_event(&end(
            "media",
            "read",
            "Video attachment loaded",
            json!({"media": [{"mimeType": "video/mp4", "data": media_marker}]}),
            false,
        ));

        let expanded = adapter.rows("media", true).expect("expanded read card");
        assert_eq!(content(&expanded), vec!["Video attachment loaded"]);
        assert_eq!(expanded.details, serde_json::Value::Null);
        assert!(
            expanded
                .rows
                .iter()
                .all(|row| row.role != ToolCardRowRole::Details)
        );
        assert!(
            !serde_json::to_string(&expanded)
                .expect("serialize card")
                .contains("binary-payload-sentinel")
        );
    }
    #[test]
    fn read_summary_reports_path_line_range_and_lines_returned() {
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start("r", "read", json!({"path": "src/lib.rs"})));
        let body = (0..10)
            .map(|n| format!("line-{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        a.apply_application_event(&end(
            "r",
            "read",
            &format!("{body}\n\n[4320 more lines in file. Use offset=11 to continue.]"),
            serde_json::Value::Null,
            false,
        ));
        match a.summary("r").expect("read card") {
            ToolCardSummary::Read {
                path,
                line_range,
                lines_returned,
            } => {
                assert_eq!(path, "src/lib.rs");
                assert_eq!(
                    line_range.as_deref(),
                    Some("1-10"),
                    "offset-continuation contract yields the returned window"
                );
                assert_eq!(
                    lines_returned, 10,
                    "truncation notice is not counted as content"
                );
            }
            other => panic!("expected Read summary, got {other:?}"),
        }

        // A byte-capped read publishes the `[Showing lines X-Y of Z …]`
        // notice instead; the range comes from that text.
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start(
            "c",
            "read",
            json!({"path": "big.log", "offset": 40}),
        ));
        a.apply_application_event(&end(
            "c",
            "read",
            "mid\n\n[Showing lines 40-60 of 500. Use offset=61 to continue.]",
            json!({"truncation": {"truncated": true}}),
            false,
        ));
        match a.summary("c").unwrap() {
            ToolCardSummary::Read {
                line_range,
                lines_returned,
                ..
            } => {
                assert_eq!(line_range.as_deref(), Some("40-60"));
                assert_eq!(
                    lines_returned, 1,
                    "notice and its blank separator are not content"
                );
            }
            other => panic!("expected Read summary, got {other:?}"),
        }

        // A fully-contained read has no range notice.
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start("f", "read", json!({"path": "small.txt"})));
        a.apply_application_event(&end(
            "f",
            "read",
            "one\ntwo",
            serde_json::Value::Null,
            false,
        ));
        match a.summary("f").unwrap() {
            ToolCardSummary::Read {
                path,
                line_range,
                lines_returned,
            } => {
                assert_eq!(path, "small.txt");
                assert_eq!(line_range, None);
                assert_eq!(lines_returned, 2);
            }
            other => panic!("expected Read summary, got {other:?}"),
        }
    }

    #[test]
    fn failed_read_summary_omits_range_and_count_but_rows_keep_error_body() {
        // An offset-beyond-end read fails with a one-sentence error body. The
        // headline keeps the path and never counts that sentence as returned
        // lines; the rows still carry the error body.
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start(
            "off",
            "read",
            json!({"path": "small.txt", "offset": 99}),
        ));
        a.apply_application_event(&end(
            "off",
            "read",
            "Offset 99 is beyond end of file (3 lines total)",
            serde_json::Value::Null,
            true,
        ));
        let summary = a.summary("off").expect("failed read card");
        match &summary {
            ToolCardSummary::Read {
                path,
                line_range,
                lines_returned,
            } => {
                assert_eq!(path, "small.txt");
                assert_eq!(*line_range, None);
                assert_eq!(*lines_returned, 0);
            }
            other => panic!("expected Read summary, got {other:?}"),
        }
        let rows = a.rows("off", false).expect("failed read rows");
        assert_eq!(rows.status, ToolCallViewStatus::Failed);
        assert!(
            !rows.summary.contains("returned"),
            "card summary never counts the error sentence: {}",
            rows.summary
        );
        assert!(
            rows.rows
                .iter()
                .any(|row| row.text.contains("beyond end of file")),
            "error body stays available through rows"
        );
        let serialized = serde_json::to_string(&summary).unwrap();
        assert!(serialized.contains(r#""kind":"read""#), "{serialized}");

        // A cancelled read gets the same treatment.
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start("cancel", "read", json!({"path": "notes.md"})));
        a.apply_application_event(&end(
            "cancel",
            "read",
            "Operation aborted",
            json!({"cancelled": true}),
            true,
        ));
        let cancelled_summary = a.summary("cancel").unwrap();
        match &cancelled_summary {
            ToolCardSummary::Read {
                path,
                line_range,
                lines_returned,
            } => {
                assert_eq!(path, "notes.md");
                assert_eq!(*line_range, None);
                assert_eq!(*lines_returned, 0);
            }
            other => panic!("expected Read summary, got {other:?}"),
        }
        assert_eq!(
            a.rows("cancel", false).unwrap().status,
            ToolCallViewStatus::Cancelled
        );
        assert!(
            a.rows("cancel", false)
                .unwrap()
                .rows
                .iter()
                .any(|row| row.text.contains("Operation aborted")),
            "cancel body stays available through rows"
        );

        // A successful one-line read still reports its count, and the `read`
        // kind keeps its existing serialization shape.
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start("one", "read", json!({"path": "solo.txt"})));
        a.apply_application_event(&end(
            "one",
            "read",
            "only line",
            serde_json::Value::Null,
            false,
        ));
        let summary = a.summary("one").unwrap();
        match &summary {
            ToolCardSummary::Read {
                path,
                line_range,
                lines_returned,
            } => {
                assert_eq!(path, "solo.txt");
                assert_eq!(*line_range, None);
                assert_eq!(*lines_returned, 1);
            }
            other => panic!("expected Read summary, got {other:?}"),
        }
        let headline = format_tool_card_summary(&summary);
        assert_eq!(
            headline, "solo.txt · 1 returned",
            "success headline keeps the count: {headline}"
        );
        let serialized = serde_json::to_string(&summary).unwrap();
        assert!(
            serialized.contains(r#""kind":"read""#) && serialized.contains(r#""lines_returned":1"#),
            "existing read serialization is preserved: {serialized}"
        );
    }

    #[test]
    fn search_summary_counts_matches_and_distinct_files() {
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start("g", "grep", json!({"pattern": "fn ", "path": "."})));
        a.apply_application_event(&end(
            "g",
            "grep",
            "src/a.rs:1: fn alpha()\nsrc/a.rs:7: fn beta()\nsrc/b.rs:3: fn gamma()\n\n[100 matches limit reached. Use limit=200 for more]",
            json!({"matchLimitReached": 100}),
            false,
        ));
        match a.summary("g").expect("grep card") {
            ToolCardSummary::Search {
                query,
                match_count,
                files_matched,
            } => {
                assert_eq!(query, "fn ");
                assert_eq!(match_count, 3, "every output row is a match row");
                assert_eq!(files_matched, 2, "distinct file prefixes only");
            }
            other => panic!("expected Search summary, got {other:?}"),
        }
    }

    #[test]
    fn edit_summary_derives_hunks_and_add_remove_counts_from_details() {
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start("e", "edit", json!({"path": "f.rs"})));
        let diff = "-2 two\n+2 TWO\n 3 three\n-4 four\n+4 FOUR\n";
        a.apply_application_event(&end("e", "edit", "ok", json!({"diff": diff, "patch": "--- f.rs\n+++ f.rs\n@@ -1,4 +1,4 @@\n@@ -9,6 +9,7 @@\n"}), false));
        match a.summary("e").expect("edit card") {
            ToolCardSummary::Edit {
                path,
                hunks,
                added_lines,
                removed_lines,
            } => {
                assert_eq!(path, "f.rs");
                assert_eq!(hunks, 2, "@@ headers count hunks");
                assert_eq!(added_lines, 2);
                assert_eq!(removed_lines, 2);
            }
            other => panic!("expected Edit summary, got {other:?}"),
        }

        // Missing details degrade to zeros instead of failing the projection.
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start("bare", "edit", json!({"path": "g.rs"})));
        a.apply_application_event(&end("bare", "edit", "ok", serde_json::Value::Null, false));
        match a.summary("bare").unwrap() {
            ToolCardSummary::Edit {
                hunks,
                added_lines,
                removed_lines,
                ..
            } => {
                assert_eq!((hunks, added_lines, removed_lines), (0, 0, 0));
            }
            other => panic!("expected Edit summary, got {other:?}"),
        }
    }

    #[test]
    fn write_summary_reports_path_and_written_size_from_arguments() {
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start(
            "w",
            "write",
            json!({"path": "out.txt", "content": "alpha\nbeta\n"}),
        ));
        a.apply_application_event(&end(
            "w",
            "write",
            "Successfully wrote 11 bytes to out.txt",
            serde_json::Value::Null,
            false,
        ));
        match a.summary("w").expect("write card") {
            ToolCardSummary::Write {
                path,
                bytes_written,
                lines_written,
            } => {
                assert_eq!(path, "out.txt");
                assert_eq!(
                    bytes_written,
                    Some(11),
                    "byte size from the written content argument"
                );
                assert_eq!(
                    lines_written, 2,
                    "trailing newline does not create an extra logical line"
                );
            }
            other => panic!("expected Write summary, got {other:?}"),
        }

        // Empty content writes zero bytes and zero lines.
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start(
            "we",
            "write",
            json!({"path": "empty.txt", "content": ""}),
        ));
        a.apply_application_event(&end(
            "we",
            "write",
            "Successfully wrote 0 bytes to empty.txt",
            serde_json::Value::Null,
            false,
        ));
        match a.summary("we").unwrap() {
            ToolCardSummary::Write {
                bytes_written,
                lines_written,
                ..
            } => {
                assert_eq!(bytes_written, Some(0));
                assert_eq!(lines_written, 0);
            }
            other => panic!("expected Write summary, got {other:?}"),
        }
    }

    #[test]
    fn unknown_and_mcp_tools_fall_back_to_generic_summary() {
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start("t", "task", json!({"prompt": "inspect"})));
        match a.summary("t").expect("task card") {
            ToolCardSummary::Generic {
                title,
                arguments_summary,
            } => {
                assert_eq!(title, "Task");
                assert_eq!(
                    arguments_summary, "prompt=inspect",
                    "non-preferred keys render as key=value compacts"
                );
            }
            other => panic!("expected Generic summary, got {other:?}"),
        }
        // MCP-style dotted names stay generic too.
        a.apply_application_event(&start("mcp", "mcp__server__query", json!({"q": "x"})));
        match a.summary("mcp").expect("mcp card") {
            ToolCardSummary::Generic { title, .. } => assert_eq!(title, "Mcp Server Query"),
            other => panic!("expected Generic summary, got {other:?}"),
        }
    }

    #[test]
    fn summary_survives_serialization_roundtrip_with_tagged_kind() {
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start("s", "grep", json!({"pattern": "p"})));
        a.apply_application_event(&end(
            "s",
            "grep",
            "a.rs:1: p",
            serde_json::Value::Null,
            false,
        ));
        let serialized = serde_json::to_string(&a.summary("s").unwrap()).unwrap();
        assert!(
            serialized.contains(r#""kind":"search""#),
            "tagged enum serializes its kind: {serialized}"
        );
    }

    #[test]
    fn glob_verified_empty_compact_count_and_expanded_safe_visible() {
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start(
            "e",
            "glob",
            json!({"pattern": "*.none", "path": "fixture-dir"}),
        ));
        a.apply_application_event(&end(
            "e",
            "glob",
            "No files found matching pattern",
            serde_json::Value::Null,
            false,
        ));
        let title = match a.summary("e").expect("glob empty summary") {
            ToolCardSummary::Generic {
                title,
                arguments_summary,
            } => {
                assert_eq!(arguments_summary, "", "glob title carries no args compact");
                title
            }
            other => panic!("expected Generic glob summary, got {other:?}"),
        };
        assert_eq!(
            title, "Glob fixture-dir 0 files",
            "verified empty reports `0 files` in the OMP space form"
        );
        let compact = a.rows("e", false).expect("compact");
        assert!(
            content(&compact).is_empty(),
            "compact empty body stays empty; the headline is the whole story"
        );
        assert!(
            !compact
                .rows
                .iter()
                .any(|r| r.text.contains("No files found matching pattern")),
            "raw model-facing sentinel never reaches compact rows"
        );
        assert!(
            !compact
                .rows
                .iter()
                .any(|r| r.role == ToolCardRowRole::Details),
            "no raw JSON details in compact"
        );
        let expanded = a.rows("e", true).expect("expanded");
        assert_eq!(
            content(&expanded),
            vec!["No files found".to_owned()],
            "expanded surfaces the safe visible text"
        );
        assert!(
            !expanded
                .rows
                .iter()
                .any(|r| r.role == ToolCardRowRole::Details),
            "expanded never exposes raw JSON details"
        );
    }

    #[test]
    fn glob_non_empty_reports_count_and_file_tree() {
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start("n", "glob", json!({"pattern": "*", "path": "src"})));
        a.apply_application_event(&end(
            "n",
            "glob",
            "src/a.rs\nsrc/b.txt",
            serde_json::Value::Null,
            false,
        ));
        let title = match a.summary("n").unwrap() {
            ToolCardSummary::Generic { title, .. } => title,
            other => panic!("expected Generic, got {other:?}"),
        };
        assert_eq!(title, "Glob src 2 files");
        let compact = a.rows("n", false).unwrap();
        assert_eq!(
            content(&compact).len(),
            2,
            "file tree renders one row per result"
        );
        assert!(content(&compact).iter().any(|t| t.contains("a.rs")));
        assert!(content(&compact).iter().any(|t| t.contains("b.txt")));
        assert!(
            content(&compact).iter().any(|t| t.contains("└─")),
            "last entry uses the └─ branch"
        );
        assert!(
            !content(&compact).iter().any(|t| t.contains(" files")),
            "body keeps the file list, not the headline count"
        );
    }

    #[test]
    fn glob_incomplete_running_ellipsis_without_count() {
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start("r", "glob", json!({"pattern": "*", "path": "src"})));
        let title = match a.summary("r").unwrap() {
            ToolCardSummary::Generic { title, .. } => title,
            other => panic!("expected Generic, got {other:?}"),
        };
        assert_eq!(
            title, "Glob src …",
            "incomplete run shows ellipsis, never a count"
        );
        assert!(
            !title.contains("files"),
            "incomplete run must not claim `0 files`"
        );
        assert_eq!(
            a.rows("r", false).unwrap().status,
            ToolCallViewStatus::Running
        );
    }

    #[test]
    fn glob_error_distinct_from_verified_empty() {
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start(
            "err",
            "glob",
            json!({"pattern": "*.rs", "path": "nope"}),
        ));
        a.apply_application_event(&end(
            "err",
            "glob",
            "Path not found",
            serde_json::Value::Null,
            true,
        ));
        let title = match a.summary("err").unwrap() {
            ToolCardSummary::Generic { title, .. } => title,
            other => panic!("expected Generic, got {other:?}"),
        };
        assert_eq!(
            title, "Glob nope",
            "error reports no count; the status glyph conveys failure"
        );
        assert!(
            !title.contains("files"),
            "error must not be reported as `0 files`"
        );
        let compact = a.rows("err", false).unwrap();
        assert_eq!(compact.status, ToolCallViewStatus::Failed);
        assert!(
            content(&compact).iter().any(|t| t == "Path not found"),
            "error text surfaces as plain content"
        );
        assert!(
            !content(&compact)
                .iter()
                .any(|t| t.contains("└─") || t.contains("├─")),
            "error text is never painted as a file-tree row"
        );
    }

    #[test]
    fn glob_narrow_target_is_bounded_in_compact_title() {
        let long = format!("workspace/{}", "x".repeat(80));
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start(
            "b",
            "glob",
            json!({"pattern": "*", "path": long.clone()}),
        ));
        a.apply_application_event(&end("b", "glob", "a.rs", serde_json::Value::Null, false));
        let title = match a.summary("b").unwrap() {
            ToolCardSummary::Generic { title, .. } => title,
            other => panic!("expected Generic, got {other:?}"),
        };
        assert!(title.starts_with("Glob "), "OMP space form");
        assert!(title.ends_with(" 1 files"));
        let target = title
            .strip_prefix("Glob ")
            .unwrap()
            .strip_suffix(" 1 files")
            .unwrap();
        assert!(
            display_width(target) <= GLOB_TARGET_CELL_LIMIT,
            "target bounded to the display-cell budget: {target} ({} cells)",
            display_width(target)
        );
        assert!(
            target.ends_with('…'),
            "bounded target ends with ellipsis: {target}"
        );
    }

    #[test]
    fn glob_default_cwd_target_is_dot() {
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start("d", "glob", json!({})));
        a.apply_application_event(&end(
            "d",
            "glob",
            "No files found matching pattern",
            serde_json::Value::Null,
            false,
        ));
        let title = match a.summary("d").unwrap() {
            ToolCardSummary::Generic { title, .. } => title,
            other => panic!("expected Generic, got {other:?}"),
        };
        assert_eq!(
            title, "Glob . 0 files",
            "cwd-default target is the OMP `.` literal"
        );
    }

    #[test]
    fn glob_summary_never_uses_find_alias_or_raw_json() {
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start(
            "j",
            "glob",
            json!({"pattern": "*.rs", "path": "src", "limit": 12}),
        ));
        a.apply_application_event(&end(
            "j",
            "glob",
            "src/a.rs",
            json!({"resultLimitReached": 1}),
            false,
        ));
        let summary = a.summary("j").unwrap();
        let serialized = serde_json::to_string(&summary).unwrap();
        assert!(
            !serialized.contains("find"),
            "no Find alias/variant is introduced: {serialized}"
        );
        match &summary {
            ToolCardSummary::Generic {
                title,
                arguments_summary,
            } => {
                assert!(title.contains("Glob "), "title keeps the Glob name");
                assert!(
                    !title.contains("Find") && !arguments_summary.contains("Find"),
                    "no Find alias in the title or args compact"
                );
            }
            other => panic!("expected Generic, got {other:?}"),
        }
        let expanded = a.rows("j", true).unwrap();
        assert!(
            !expanded
                .rows
                .iter()
                .any(|r| r.role == ToolCardRowRole::Details),
            "raw JSON details stay folded out of the public body"
        );
    }

    #[test]
    fn glob_narrow_target_bounds_cjk_to_display_cell_budget() {
        // Each CJK char is 2 display cells; a char-count budget would let a
        // wide path overflow the title row. The bound must be in cells.
        let cjk = "工作区目录".repeat(20); // 5 wide chars * 20 = 200 cells (>> 48)
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start(
            "k",
            "glob",
            json!({"pattern": "*", "path": cjk.clone()}),
        ));
        a.apply_application_event(&end("k", "glob", "a.rs", serde_json::Value::Null, false));
        let title = match a.summary("k").unwrap() {
            ToolCardSummary::Generic { title, .. } => title,
            other => panic!("expected Generic, got {other:?}"),
        };
        let target = title
            .strip_prefix("Glob ")
            .unwrap()
            .strip_suffix(" 1 files")
            .unwrap();
        assert!(
            display_width(target) <= GLOB_TARGET_CELL_LIMIT,
            "CJK target bounded to display cells: {target} ({} cells)",
            display_width(target)
        );
        assert!(
            target.ends_with('…'),
            "bounded target ends with ellipsis: {target}"
        );
        // No wide grapheme is split: every remaining cluster (before the
        // ellipsis) is a whole 2-cell CJK char, never a half glyph.
        let kept = target.strip_suffix('…').unwrap();
        for cluster in UnicodeSegmentation::graphemes(kept, true) {
            assert!(
                cluster.chars().count() == 1 && display_width(cluster) == 2,
                "no wide char split: {cluster:?} in {kept:?}"
            );
        }
    }

    #[test]
    fn glob_narrow_target_preserves_zwj_and_combining_clusters() {
        // ZWJ family emoji (one 2-cell cluster) and a base char + combining
        // mark (one 1-cell cluster) must never be split at the bound.
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}";
        assert_eq!(
            display_width(family),
            2,
            "fixture: one ZWJ family cluster is 2 cells"
        );
        let accented = "e\u{308}"; // e + combining diaeresis, one 1-cell cluster
        assert_eq!(display_width(accented), 1);
        // Combining-mark clusters first so they actually appear in the
        // bounded result; ZWJ family clusters follow so both kinds are
        // exercised (a char-count bound would split the family ZWJ).
        let long = format!("{}{}", accented.repeat(40), family.repeat(30)); // 40 + 60 cells
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start(
            "z",
            "glob",
            json!({"pattern": "*", "path": long.clone()}),
        ));
        a.apply_application_event(&end("z", "glob", "a.rs", serde_json::Value::Null, false));
        let title = match a.summary("z").unwrap() {
            ToolCardSummary::Generic { title, .. } => title,
            other => panic!("expected Generic, got {other:?}"),
        };
        let target = title
            .strip_prefix("Glob ")
            .unwrap()
            .strip_suffix(" 1 files")
            .unwrap();
        assert!(
            display_width(target) <= GLOB_TARGET_CELL_LIMIT,
            "cluster target bounded to display cells: {target} ({} cells)",
            display_width(target)
        );
        assert!(
            target.ends_with('…'),
            "bounded target ends with ellipsis: {target}"
        );
        let kept = target.strip_suffix('…').unwrap();
        // The combining-mark cluster survived whole at the start: a char-count
        // split would have separated the base `e` from its combining diaeresis.
        assert!(
            kept.starts_with(accented),
            "combining-mark cluster kept whole: {kept:?}"
        );
        // No ZWJ family is split: the kept part contains at least one whole
        // family cluster and never ends with a dangling joiner.
        assert!(
            kept.contains(family),
            "a whole ZWJ family cluster survived: {kept:?}"
        );
        assert!(
            !kept.ends_with('\u{200d}'),
            "ZWJ never leads the ellipsis boundary: {kept:?}"
        );
        // No bare combining mark leads the boundary: the last cluster before
        // the ellipsis occupies at least one display cell (combining marks and
        // ZWJ have zero width on their own).
        let last_cluster = UnicodeSegmentation::graphemes(kept, true)
            .next_back()
            .unwrap_or("");
        assert!(
            display_width(last_cluster) > 0,
            "no zero-width cluster (combining mark / ZWJ) leads the bound: {last_cluster:?}"
        );
        // Every remaining cluster is whole: re-iterating the graphemes of the
        // kept prefix reproduces the exact byte slice fit_text wrote.
        let mut cursor = 0usize;
        for cluster in UnicodeSegmentation::graphemes(kept, true) {
            assert_eq!(
                &kept[cursor..cursor + cluster.len()],
                cluster,
                "cluster {cluster:?} is contiguous in {kept:?}"
            );
            cursor += cluster.len();
        }
        assert_eq!(
            cursor,
            kept.len(),
            "grapheme iteration covers the whole kept prefix"
        );
    }

    #[test]
    fn glob_streaming_ellipsis_without_count() {
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start("s", "glob", json!({"pattern": "*", "path": "src"})));
        a.apply_application_event(&update("s", "glob", "src/a.rs"));
        let title = match a.summary("s").unwrap() {
            ToolCardSummary::Generic { title, .. } => title,
            other => panic!("expected Generic, got {other:?}"),
        };
        assert_eq!(
            title, "Glob src …",
            "streaming run shows ellipsis, never a count"
        );
        assert!(
            !title.contains("files"),
            "streaming run must not claim `0 files`"
        );
        assert_eq!(
            a.rows("s", false).unwrap().status,
            ToolCallViewStatus::Streaming
        );
    }

    #[test]
    fn glob_cancelled_distinct_from_verified_empty() {
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start(
            "c",
            "glob",
            json!({"pattern": "*.rs", "path": "nope"}),
        ));
        a.apply_application_event(&end(
            "c",
            "glob",
            "Operation aborted",
            json!({"cancelled": true}),
            true,
        ));
        a.apply_application_event(&result(
            "c",
            "glob",
            "Operation aborted",
            Some(json!({"cancelled": true})),
            true,
        ));
        let title = match a.summary("c").unwrap() {
            ToolCardSummary::Generic { title, .. } => title,
            other => panic!("expected Generic, got {other:?}"),
        };
        assert_eq!(
            title, "Glob nope",
            "cancelled run reports no count; the status glyph conveys the cancel"
        );
        assert!(
            !title.contains("files"),
            "cancelled run must not be reported as `0 files`"
        );
        assert_eq!(
            a.rows("c", false).unwrap().status,
            ToolCallViewStatus::Cancelled
        );
        let compact = a.rows("c", false).unwrap();
        assert!(
            !content(&compact)
                .iter()
                .any(|t| t.contains("└─") || t.contains("├─")),
            "cancel text is never painted as a file-tree row"
        );
    }

    #[test]
    fn glob_orphan_repaired_reports_count_like_success() {
        // A result arriving without a prior live start (history/repair path)
        // is terminal and carries a real result, so it reports the count —
        // not the incomplete `…` form. With no path arg the target is `.`.
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&result("o", "glob", "src/a.rs\nsrc/b.txt", None, false));
        let title = match a.summary("o").unwrap() {
            ToolCardSummary::Generic { title, .. } => title,
            other => panic!("expected Generic, got {other:?}"),
        };
        assert_eq!(
            title, "Glob . 2 files",
            "orphan-repaired reports the count, target defaults to `.`"
        );
        assert!(
            !title.contains('…'),
            "a terminal repaired run is not incomplete"
        );
        assert_eq!(
            a.rows("o", false).unwrap().status,
            ToolCallViewStatus::OrphanRepaired
        );
    }

    fn card_with(name: &str, status: ToolCallViewStatus) -> ToolCardRows {
        ToolCardRows {
            tool_call_id: format!("{name}-{status:?}"),
            tool_name: name.to_owned(),
            ordinal: 1,
            arguments_summary: String::new(),
            summary: String::new(),
            code_language: None,
            status,
            is_partial: matches!(
                status,
                ToolCallViewStatus::Running | ToolCallViewStatus::Streaming
            ),
            is_error: matches!(
                status,
                ToolCallViewStatus::Failed | ToolCallViewStatus::Cancelled
            ),
            cancelled: status == ToolCallViewStatus::Cancelled,
            truncated: false,
            omitted_content_lines: 0,
            rows: Vec::new(),
            skill_name: None,
            bash_command: None,
            arguments: serde_json::Value::Null,
            details: serde_json::Value::Null,
        }
    }

    #[test]
    fn frame_color_restrains_success_chrome_and_keeps_states_distinct() {
        // OMP `renderOutputBlock` (`src/tui/output-block.ts`): running/pending
        // use accent, success uses dim, error/warning keep their colors.
        // Neon `bashMode` (#00ff88) is composer `!` mode, not settled chrome.
        let theme = crate::theme::DARK;
        let neon = theme.bash_mode;
        assert_eq!(neon, ratatui::style::Color::Rgb(0x00, 0xff, 0x88));

        let bash_success = card_with("bash", ToolCallViewStatus::Succeeded).frame_color(theme);
        let bash_running = card_with("bash", ToolCallViewStatus::Running).frame_color(theme);
        let bash_streaming = card_with("bash", ToolCallViewStatus::Streaming).frame_color(theme);
        let bash_error = card_with("bash", ToolCallViewStatus::Failed).frame_color(theme);
        let bash_cancelled = card_with("bash", ToolCallViewStatus::Cancelled).frame_color(theme);
        let bash_repaired =
            card_with("bash", ToolCallViewStatus::OrphanRepaired).frame_color(theme);
        let read_success = card_with("read", ToolCallViewStatus::Succeeded).frame_color(theme);
        let read_running = card_with("read", ToolCallViewStatus::Running).frame_color(theme);
        let read_error = card_with("read", ToolCallViewStatus::Failed).frame_color(theme);
        let read_cancelled = card_with("read", ToolCallViewStatus::Cancelled).frame_color(theme);

        assert_eq!(bash_success, theme.dim);
        assert_ne!(bash_success, neon);
        assert_eq!(bash_repaired, theme.dim);
        assert_eq!(bash_running, theme.border_accent);
        assert_eq!(bash_streaming, theme.border_accent);
        assert_ne!(bash_running, neon);
        assert_eq!(bash_error, theme.error);
        assert_eq!(bash_cancelled, theme.warning);
        assert_eq!(read_success, theme.tool_card_border);
        assert_eq!(read_running, theme.border_accent);
        assert_eq!(read_error, theme.error);
        assert_eq!(read_cancelled, theme.warning);

        assert_ne!(bash_success, bash_running);
        assert_ne!(bash_success, bash_error);
        assert_ne!(bash_success, bash_cancelled);
        assert_ne!(bash_running, bash_error);
        assert_ne!(bash_running, bash_cancelled);
        assert_ne!(bash_error, bash_cancelled);
        assert_ne!(theme.dim, theme.error);
        assert_ne!(theme.dim, theme.warning);
        assert_ne!(theme.dim, theme.border_accent);
        assert_ne!(theme.error, theme.warning);
    }
}
