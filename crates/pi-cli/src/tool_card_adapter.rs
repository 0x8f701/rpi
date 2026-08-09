//! Ratatui-neutral tool-card rows reduced from application events.

use pi_ai::BashExecutionMessage;
use pi_coding::redact::redact_secrets;
use pi_coding::{
    ApplicationEvent, ToolCallViewStatus, ToolCard, ToolPresentationState,
    compact_tool_arguments, redact_value,
};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCardRowRole { Command, Content, Details, Status, Error }

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCardRow { pub tool_call_id: String, pub role: ToolCardRowRole, pub text: String }

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCardRows {
    pub tool_call_id: String, pub tool_name: String, pub ordinal: u64,
    pub arguments_summary: String, pub code_language: Option<String>, pub status: ToolCallViewStatus,
    pub is_partial: bool, pub is_error: bool, pub cancelled: bool,
    pub truncated: bool, pub omitted_content_lines: usize, pub rows: Vec<ToolCardRow>,
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
                    name: item.get("name").and_then(serde_json::Value::as_str).map(str::to_owned),
                    agent: item.get("agent").and_then(serde_json::Value::as_str).map(str::to_owned),
                    task: task.to_owned(),
                })
            })
            .collect::<Vec<_>>();
        return (!children.is_empty()).then(|| TaskDelegationRequest {
            context: arguments.get("context").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned(),
            children,
        });
    }
    let task = arguments.get("task")?.as_str()?.trim();
    (!task.is_empty()).then(|| TaskDelegationRequest {
        context: String::new(),
        children: vec![TaskDelegationChild {
            name: arguments.get("name").and_then(serde_json::Value::as_str).map(str::to_owned),
            agent: arguments.get("agent").and_then(serde_json::Value::as_str).map(str::to_owned),
            task: task.to_owned(),
        }],
    })
}

#[derive(Clone, Debug, Default)]
pub struct ToolCardPresentationAdapter { projection: ToolPresentationState }

impl ToolCardPresentationAdapter {
    #[must_use] pub fn new() -> Self { Self::default() }
    pub fn apply_application_event(&mut self, event: &ApplicationEvent) { if let ApplicationEvent::Agent(event) = event { self.projection.apply_event(event); } }
    pub fn apply_agent_event(&mut self, event: &pi_agent::AgentEvent) { self.projection.apply_event(event); }
    pub fn apply_tool_result(&mut self, result: &pi_ai::ToolResultMessage) { self.projection.apply_tool_result(result); }
    #[must_use] pub fn projection(&self) -> &ToolPresentationState { &self.projection }
    #[must_use] pub fn rows(&self, id: &str, expanded: bool) -> Option<ToolCardRows> { self.projection.get(id).map(|card| card_rows(card, expanded)) }
    #[must_use] pub fn rows_in_source_order(&self, expanded: bool) -> Vec<ToolCardRows> { self.projection.cards_in_source_order().into_iter().map(|card| card_rows(card, expanded)).collect() }
    #[must_use] pub fn compact_rows(&self) -> Vec<ToolCardRows> { self.rows_in_source_order(false) }
    #[must_use] pub fn expanded_rows(&self, id: &str) -> Option<ToolCardRows> { self.rows(id, true) }
    #[must_use] pub fn expanded_rows_in_source_order(&self) -> Vec<ToolCardRows> { self.rows_in_source_order(true) }
    pub fn clear(&mut self) { self.projection.clear(); }

    #[must_use]
    pub fn bash_execution_rows(message: &BashExecutionMessage, expanded: bool) -> ToolCardRows {
        let status = if message.cancelled { ToolCallViewStatus::Cancelled } else if message.exit_code.is_some_and(|code| code != 0) { ToolCallViewStatus::Failed } else { ToolCallViewStatus::Succeeded };
        let mut details = serde_json::Map::new();
        if let Some(code) = message.exit_code { details.insert("exitCode".into(), code.into()); }
        if message.cancelled { details.insert("cancelled".into(), true.into()); }
        if message.truncated { details.insert("truncated".into(), true.into()); }
        if let Some(path) = &message.full_output_path { details.insert("fullOutputPath".into(), path.clone().into()); }
        let ordinal = u64::try_from(message.timestamp).unwrap_or_default();
        let card = ToolCard {
            tool_call_id: format!("bash-execution-{}", message.timestamp), tool_name: "bash".into(), ordinal,
            first_observation_ordinal: ordinal, status, arguments: serde_json::json!({"command": message.command}),
            content: if message.output.is_empty() { Vec::new() } else { vec![pi_ai::ContentBlock::text(message.output.clone())] },
            details: serde_json::Value::Object(details), is_error: matches!(status, ToolCallViewStatus::Failed | ToolCallViewStatus::Cancelled),
            cancelled: message.cancelled, error_message: None, has_message_result: true, has_execution_end: true,
        };
        let mut rows = card_rows(&card, expanded);
        if message.cancelled { rows.rows.push(row(&card, ToolCardRowRole::Status, "Cancelled".into())); }
        else if let Some(code) = message.exit_code.filter(|code| *code != 0) { rows.rows.push(row(&card, ToolCardRowRole::Status, format!("Exit: {code}"))); }
        rows
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
    if !card.tool_name.eq_ignore_ascii_case("read") { return None; }
    let path = card.arguments.get("path").and_then(serde_json::Value::as_str)?;
    let rest = path.strip_prefix("skill://")?;
    if rest.is_empty() || rest.contains('/') { return None; }
    let mut view = parse_skill_frontmatter(content)?;
    if view.name.is_empty() { view.name = rest.to_owned(); }
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
        if line.is_empty() || line.starts_with('#') { continue; }
        let Some((key, raw)) = line.split_once(':') else { continue };
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
                if trimmed.is_empty() { continue; }
                if !block.is_empty() { block.push(if folded { ' ' } else { '\n' }); }
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
    Some(SkillCardView { name: name.unwrap_or_default(), description: description.unwrap_or_default(), body })
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
        value.split_once(" #").map_or(value, |(plain, _)| plain.trim())
    }
}

fn card_rows(card: &ToolCard, expanded: bool) -> ToolCardRows {
    let view = card.expanded_view();
    let mut rows = vec![row(card, ToolCardRowRole::Command, tool_title(&card.tool_name))];
    let content = tool_content_text(card, &view.content_text);
    let skill = skill_card_view(card, &content);
    if let Some(skill_view) = &skill {
        rows[0].text.clone_from(&skill_view.name);
    }
    let content = skill.as_ref().map_or(content, SkillCardView::prose);
    let mut content_lines = content.lines().collect::<Vec<_>>();
    if card.tool_name.eq_ignore_ascii_case("read") {
        // The read tool appends a file-level truncation notice
        // (`[N more lines in file. Use offset=… to continue.]`) to its
        // result when the file continues past the returned lines. The card
        // folds instead — the fold footer is the only "more lines" notice,
        // so the offset line never reaches the card rows.
        content_lines.retain(|line| !is_file_truncation_notice(line));
    }
    // Compact output budget. The bash card is bounded by the 20-row total
    // card budget: 1 top border + up to 4 command rows + 1 " Output "
    // separator + 1 fold hint + 1 status row + 1 bottom border leaves 11 rows
    // for content + hint; the hint row shares that budget, so content gets
    // BASH_CARD_OUTPUT_LIMIT and the hint keeps one row.
    const BASH_CARD_OUTPUT_LIMIT: usize = 10;
    const DEFAULT_CARD_OUTPUT_LIMIT: usize = 6;
    let limit = if card.tool_name.eq_ignore_ascii_case("bash") {
        BASH_CARD_OUTPUT_LIMIT
    } else {
        DEFAULT_CARD_OUTPUT_LIMIT
    };
    let visible = if expanded || content_lines.len() <= limit {
        &content_lines[..]
    } else if skill.is_some() {
        // Skill reads fold top-down so the description paragraph and the head
        // of the body stay visible in compact mode.
        &content_lines[..limit]
    } else {
        &content_lines[content_lines.len() - limit..]
    };
    rows.extend(visible.iter().map(|line| row(card, ToolCardRowRole::Content, (*line).to_owned())));
    let omitted_content_lines = content_lines.len().saturating_sub(visible.len());
    if expanded {
        let details = serde_json::to_string_pretty(&redact_value(&view.details)).unwrap_or_else(|_| view.details.to_string());
        if !matches!(details.as_str(), "{}" | "[]" | "null" | "") { rows.extend(details.lines().map(|line| row(card, ToolCardRowRole::Details, line.to_owned()))); }
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
    ToolCardRows { tool_call_id: card.tool_call_id.clone(), tool_name: card.tool_name.clone(), ordinal: card.ordinal,
        arguments_summary: compact_tool_arguments(&card.arguments), code_language, status: card.status, is_partial: card.is_partial(),
        is_error: card.is_error, cancelled: card.cancelled, truncated: omitted_content_lines > 0, omitted_content_lines, rows,
        skill_name: skill.as_ref().map(|skill_view| skill_view.name.clone()), bash_command }
}

fn tool_code_language(tool_name: &str, arguments: &serde_json::Value) -> Option<String> {
    if !matches!(tool_name.to_ascii_lowercase().as_str(), "read" | "write" | "edit") {
        return None;
    }
    let path = ["path", "file"]
        .into_iter()
        .find_map(|key| arguments.get(key).and_then(serde_json::Value::as_str))?;
    let extension = std::path::Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    let language = match extension.as_str() {
        "rs" => "rust",
        "sh" | "bash" | "zsh" | "fish" | "ksh" => "sh",
        "json" | "jsonc" | "json5" => "json",
        "ts" => "typescript",
        "tsx" | "js" | "jsx" | "py" | "go" | "java" | "c" | "cpp" | "cs" | "swift"
        | "rb" | "php" | "scala" | "toml" | "yaml" | "yml" | "sql" | "lua" | "zig"
        | "dart" | "r" | "hs" | "ex" | "clj" => extension.as_str(),
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
    let selected = if card.tool_name.eq_ignore_ascii_case("edit") { card.details.get("diff").and_then(serde_json::Value::as_str) }
        else if card.tool_name.eq_ignore_ascii_case("write") { card.arguments.get("content").and_then(serde_json::Value::as_str) } else { None };
    selected.map_or_else(|| fallback.to_owned(), |text| redact_value(&serde_json::Value::String(text.to_owned())).as_str().unwrap_or_default().to_owned())
}

fn row(card: &ToolCard, role: ToolCardRowRole, text: String) -> ToolCardRow { ToolCardRow { tool_call_id: card.tool_call_id.clone(), role, text } }

#[cfg(test)]
mod tests {
    use super::*;
    use pi_agent::{AgentEvent, AgentToolResult};
    use pi_ai::{ContentBlock, Message, ToolResultMessage, now_millis};
    use serde_json::json;

    fn app(event: AgentEvent) -> ApplicationEvent { ApplicationEvent::Agent(event) }
    fn start(id: &str, name: &str, arguments: serde_json::Value) -> ApplicationEvent { app(AgentEvent::ToolExecutionStart { tool_call_id: id.into(), tool_name: name.into(), arguments }) }
    fn update(id: &str, name: &str, text: &str) -> ApplicationEvent { app(AgentEvent::ToolExecutionUpdate { tool_call_id: id.into(), tool_name: name.into(), arguments: serde_json::Value::Null, partial_result: AgentToolResult::text(text) }) }
    fn end(id: &str, name: &str, text: &str, details: serde_json::Value, is_error: bool) -> ApplicationEvent { let mut result = AgentToolResult::text(text); result.details = details; app(AgentEvent::ToolExecutionEnd { tool_call_id: id.into(), tool_name: name.into(), result, is_error }) }
    fn result(id: &str, name: &str, text: &str, details: Option<serde_json::Value>, is_error: bool) -> ApplicationEvent { app(AgentEvent::MessageEnd { message: Message::ToolResult(ToolResultMessage { tool_call_id: id.into(), tool_name: name.into(), content: vec![ContentBlock::text(text)], usage: None, details, added_tool_names: Vec::new(), is_error, timestamp: now_millis() }) }) }
    fn content(rows: &ToolCardRows) -> Vec<String> { rows.rows.iter().filter(|row| row.role == ToolCardRowRole::Content).map(|row| row.text.clone()).collect() }
    fn title(rows: &ToolCardRows) -> &str { rows.rows.iter().find(|row| row.role == ToolCardRowRole::Command).map_or("", |row| row.text.as_str()) }

    #[test]
    fn concurrent_same_name_preserves_ids_order_and_results() {
        let mut a = ToolCardPresentationAdapter::new();
        for e in [start("a","read",json!({"path":"a"})), start("b","read",json!({"path":"b"})), update("b","read","partial-b"), end("b","read","body-b",json!({}),false), end("a","read","body-a",json!({}),false)] { a.apply_application_event(&e); }
        assert_eq!(a.compact_rows().iter().map(|r| r.tool_call_id.as_str()).collect::<Vec<_>>(), ["a","b"]);
        assert_eq!(content(&a.rows("b",false).unwrap()), vec!["body-b"]);
    }

    #[test]
    fn terminal_orphan_reconciliation_and_redaction() {
        let secret = "credential-redaction-fixture-value";
        let mut a = ToolCardPresentationAdapter::new();
        for e in [start("c","bash",json!({"command":"sleep"})), end("c","bash","Operation aborted",json!({"cancelled":true}),true), result("c","bash","Operation aborted",Some(json!({"cancelled":true,"signal":"SIGINT"})),true), result("o","read","recovered",None,false), start("s","http",json!({"api_key":secret,"password":"hunter2"})), end("s","http",&format!("token={secret}"),json!({"authorization":secret}),true)] { a.apply_application_event(&e); }
        assert_eq!(a.projection().len(), 3);
        assert_eq!(a.rows("c",false).unwrap().status, ToolCallViewStatus::Cancelled);
        assert_eq!(a.rows("o",false).unwrap().status, ToolCallViewStatus::OrphanRepaired);
        let serialized = serde_json::to_string(&a.expanded_rows("s").unwrap()).unwrap();
        assert!(!serialized.contains(secret)); assert!(!serialized.contains("hunter2")); assert!(serialized.contains("[REDACTED]"));
    }

    #[test]
    fn short_empty_long_and_expanded_bash_match_capture() {
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start("s","bash",json!({"command":"printf short-output"}))); a.apply_application_event(&end("s","bash","short-output",serde_json::Value::Null,false));
        let short = a.rows("s",false).unwrap(); assert_eq!(title(&short), "Bash"); assert_eq!(short.arguments_summary,"printf short-output"); assert_eq!(content(&short),vec!["short-output"]); assert!(!short.truncated);
        a.apply_application_event(&start("e","bash",json!({"command":"true"}))); a.apply_application_event(&end("e","bash","",serde_json::Value::Null,false)); let empty = a.rows("e",false).unwrap(); assert_eq!(title(&empty), "Bash"); assert!(content(&empty).is_empty());
        let body=(1..=30).map(|n|n.to_string()).collect::<Vec<_>>().join("\n"); a.apply_application_event(&start("l","bash",json!({"command":"seq 1 30"}))); a.apply_application_event(&end("l","bash",&body,serde_json::Value::Null,false));
        let compact=a.rows("l",false).unwrap(); assert_eq!(title(&compact), "Bash"); assert_eq!(compact.omitted_content_lines,20); assert_eq!(content(&compact),(21..=30).map(|n|n.to_string()).collect::<Vec<_>>()); assert_eq!(content(&a.rows("l",true).unwrap()).len(),30);
        a.apply_application_event(&start("t","task",json!({"prompt":"inspect"}))); let task = a.rows("t",false).unwrap(); assert_eq!(title(&task), "Task");
    }

    #[test]
    fn bash_command_field_keeps_multiline_command_and_redacts() {
        let mut a = ToolCardPresentationAdapter::new();
        let command = "CA=/tmp/oh-my-pi\n# session-context\nbuildrg -n \"export fu\"";
        a.apply_application_event(&start("m","bash",json!({"command": command})));
        a.apply_application_event(&end("m","bash","out",serde_json::Value::Null,false));
        let rows = a.rows("m",false).unwrap();
        let bash_command = rows.bash_command.as_deref().expect("bash card carries the command");
        assert_eq!(bash_command, command, "full multi-line command must survive untruncated");
        assert_eq!(bash_command.lines().count(), 3, "all three logical lines preserved");
        // The title compact stays the 60-char summary; the untruncated
        // command lives in bash_command so the card renders line-by-line.
        assert!(rows.arguments_summary.len() <= 60, "title compact still bounded: {}", rows.arguments_summary);
        // Secrets are scrubbed before the card leaves the adapter.
        let secret = ["s", "k-", "abcdefghijklmnop1234"].concat();
        let secret_command = format!("curl -H 'Authorization: Bearer {secret}' /api");
        a.apply_application_event(&start("sec","bash",json!({"command": secret_command})));
        a.apply_application_event(&end("sec","bash","",serde_json::Value::Null,false));
        let serialized = serde_json::to_string(&a.expanded_rows("sec").unwrap()).unwrap();
        assert!(!serialized.contains(&secret), "secret leaked: {serialized}");
        assert!(serialized.contains("[REDACTED]"));
        // Non-bash cards carry no bash_command.
        a.apply_application_event(&start("r","read",json!({"path": "src/main.rs"})));
        a.apply_application_event(&end("r","read","body",serde_json::Value::Null,false));
        assert_eq!(a.rows("r",false).unwrap().bash_command, None);
    }

    #[test]
    fn task_request_preserves_shared_context_and_children() {
        let request = task_delegation_request(&json!({
            "context": "# Goal\nShip it\n\n# Constraints\nBe precise",
            "tasks": [
                {"name": "Alpha", "agent": "reviewer", "task": "Review the adapter"},
                {"name": "Beta", "task": "Render the card"}
            ]
        })).expect("task batch");
        assert_eq!(request.children.len(), 2);
        assert_eq!(request.children[0].name.as_deref(), Some("Alpha"));
        assert_eq!(request.children[0].agent.as_deref(), Some("reviewer"));
        assert!(request.context.contains("# Constraints"));
    }
    #[test]
    fn edit_read_write_and_foreground_status_are_specific() {
        let mut a=ToolCardPresentationAdapter::new(); a.apply_application_event(&start("x","edit",json!({"path":"f"}))); a.apply_application_event(&end("x","edit","success",json!({"diff":"@@\n-old\n+new"}),false)); let edit=a.rows("x",false).unwrap(); assert_eq!(title(&edit), "Edit"); assert!(content(&edit).contains(&"@@".into()));
        let body=(1..=8).map(|n|format!("line-{n}")).collect::<Vec<_>>().join("\n"); a.apply_application_event(&start("r","read",json!({"path":"f"}))); a.apply_application_event(&end("r","read",&body,serde_json::Value::Null,false)); let read=a.rows("r",false).unwrap(); assert_eq!(title(&read), "Read"); assert_eq!(read.omitted_content_lines,2);
        a.apply_application_event(&start("w","write",json!({"path":"f","content":body}))); a.apply_application_event(&end("w","write","Successfully wrote",serde_json::Value::Null,false)); let write=a.rows("w",false).unwrap(); assert_eq!(title(&write), "Write"); assert_eq!(write.omitted_content_lines,2);
        let failed=BashExecutionMessage{command:"false".into(),output:String::new(),exit_code:Some(7),cancelled:false,truncated:false,full_output_path:None,timestamp:42,exclude_from_context:None}; let rows=ToolCardPresentationAdapter::bash_execution_rows(&failed,false); assert_eq!(title(&rows), "Bash"); assert!(rows.rows.iter().any(|r|r.role==ToolCardRowRole::Status&&r.text=="Exit: 7"));
    }
    #[test]
    fn read_code_language_uses_only_trusted_path_extensions() {
        let mut adapter = ToolCardPresentationAdapter::new();
        adapter.apply_application_event(&start("rust", "read", json!({"path":"src/lib.rs"})));
        adapter.apply_application_event(&end("rust", "read", "fn main() {}", serde_json::Value::Null, false));
        assert_eq!(adapter.rows("rust", false).unwrap().code_language.as_deref(), Some("rust"));

        adapter.apply_application_event(&start("plain", "read", json!({"path":"notes.unknown"})));
        adapter.apply_application_event(&end("plain", "read", "fn main() {}", serde_json::Value::Null, false));
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
        assert_eq!(title(&card), "research", "skill name must become the card title");
        let content_rows = content(&card);
        assert_eq!(content_rows[0], "Deep-dive codebase researcher. Produces structured docs.", "description must be the first body paragraph, unquoted");
        assert!(content_rows.iter().any(|line| line == "# Research"));
        assert!(content_rows.iter().any(|line| line == "Body line."));
        let all = content_rows.join("\n");
        assert!(!all.contains("name:"), "raw frontmatter name must not render: {all}");
        assert!(!all.contains("description:"), "raw frontmatter description must not render: {all}");
        assert!(!all.contains("---"), "frontmatter delimiters must not render: {all}");
        // Expanded keeps the same prose shape without truncation.
        assert_eq!(content(&a.rows("s", true).unwrap()).len(), 5);
    }

    #[test]
    fn long_skill_read_folds_top_down_and_keeps_description() {
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start("s", "read", json!({"path": "skill://long"})));
        let body = (1..=20).map(|n| format!("body-{n}")).collect::<Vec<_>>().join("\n");
        a.apply_application_event(&end("s", "read", &format!("---\nname: long\ndescription: \"Lead description.\"\n---\n{body}"), serde_json::Value::Null, false));
        let card = a.rows("s", false).unwrap();
        assert_eq!(title(&card), "long");
        let content_rows = content(&card);
        assert_eq!(content_rows[0], "Lead description.");
        assert_eq!(content_rows.get(1).map(String::as_str), Some(""));
        assert_eq!(content_rows.get(2).map(String::as_str), Some("body-1"), "compact folds from the top so the head of the body stays visible");
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
            content_rows[0],
            "Folded line one folds into a single line",
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
        assert!(!all.contains("description:"), "raw frontmatter must not render: {all}");
    }

    #[test]
    fn skill_subresource_and_frontmatter_less_reads_stay_raw() {
        let mut a = ToolCardPresentationAdapter::new();
        // A sub-resource of a skill is a plain file read, not the skill itself.
        a.apply_application_event(&start("sub", "read", json!({"path": "skill://research/asset.md"})));
        a.apply_application_event(&end("sub", "read", "---\nname: asset\ndescription: \"not the skill\"\n---\nasset body", serde_json::Value::Null, false));
        let sub = a.rows("sub", false).unwrap();
        assert_eq!(sub.skill_name, None);
        assert_eq!(title(&sub), "Read");
        assert!(content(&sub).iter().any(|line| line == "name: asset"), "sub-resource content stays raw");
        // A failed/empty skill read without frontmatter stays raw too.
        a.apply_application_event(&start("gone", "read", json!({"path": "skill://missing"})));
        a.apply_application_event(&end("gone", "read", "not found", serde_json::Value::Null, false));
        let gone = a.rows("gone", false).unwrap();
        assert_eq!(gone.skill_name, None);
        assert_eq!(title(&gone), "Read");
        assert_eq!(content(&gone), vec!["not found"]);
    }

    #[test]
    fn file_truncation_notice_matches_only_read_contract_lines() {
        // The three file-level notices `render_read_result` appends when the
        // file continues past the returned lines.
        assert!(is_file_truncation_notice("[4320 more lines in file. Use offset=3885 to continue.]"));
        assert!(is_file_truncation_notice("[Showing lines 1-2000 of 6320. Use offset=2001 to continue.]"));
        assert!(is_file_truncation_notice("[Showing lines 1-2000 of 6320 (512KB limit). Use offset=2001 to continue.]"));
        // Plain content, the render-level fold footer, and the read tool's
        // single-line-too-big diagnostic (no "Use offset=" continuation
        // contract) are not file-level notices.
        assert!(!is_file_truncation_notice("line-7"));
        assert!(!is_file_truncation_notice("… 5 more lines ⟦Ctrl+O: Expand⟧"));
        assert!(!is_file_truncation_notice("[Line 3 is 1MB, exceeds 512KB limit. Use bash: sed -n '3p' f | head -c 524288]"));
    }

    #[test]
    fn read_card_drops_file_offset_notice_and_folds_without_it() {
        let mut a = ToolCardPresentationAdapter::new();
        a.apply_application_event(&start("trunc", "read", json!({"path": "big.txt"})));
        let body = (1..=10).map(|n| format!("line-{n}")).collect::<Vec<_>>().join("\n");
        a.apply_application_event(&end("trunc", "read", &format!("{body}\n\n[4320 more lines in file. Use offset=3885 to continue.]"), serde_json::Value::Null, false));
        let compact = a.rows("trunc", false).unwrap();
        let compact_lines = content(&compact);
        assert!(
            compact_lines.iter().all(|line| !is_file_truncation_notice(line)),
            "file offset notice must never reach card rows: {compact_lines:?}"
        );
        assert_eq!(compact.omitted_content_lines, 5, "12 raw lines - 1 notice - 6 visible");
        assert!(compact.truncated, "card still folds when the notice is dropped");
        let expanded = content(&a.rows("trunc", true).unwrap());
        assert!(
            expanded.iter().all(|line| !is_file_truncation_notice(line)),
            "expanded card stays free of the offset notice: {expanded:?}"
        );
        assert_eq!(expanded.len(), 11, "12 raw lines - 1 notice");
    }
}
