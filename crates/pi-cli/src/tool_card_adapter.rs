//! Ratatui-neutral tool-card rows reduced from application events.

use pi_ai::BashExecutionMessage;
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

fn card_rows(card: &ToolCard, expanded: bool) -> ToolCardRows {
    let view = card.expanded_view();
    let mut rows = vec![row(card, ToolCardRowRole::Command, tool_title(&card.tool_name))];
    let content = tool_content_text(card, &view.content_text);
    let content_lines = content.lines().collect::<Vec<_>>();
    let limit = if card.tool_name.eq_ignore_ascii_case("bash") { 19 } else { 6 };
    let visible = if expanded || content_lines.len() <= limit { &content_lines[..] } else { &content_lines[content_lines.len()-limit..] };
    rows.extend(visible.iter().map(|line| row(card, ToolCardRowRole::Content, (*line).to_owned())));
    let omitted_content_lines = content_lines.len().saturating_sub(visible.len());
    if expanded {
        let details = serde_json::to_string_pretty(&redact_value(&view.details)).unwrap_or_else(|_| view.details.to_string());
        if !matches!(details.as_str(), "{}" | "[]" | "null" | "") { rows.extend(details.lines().map(|line| row(card, ToolCardRowRole::Details, line.to_owned()))); }
    }
    let code_language = tool_code_language(&card.tool_name, &card.arguments);
    ToolCardRows { tool_call_id: card.tool_call_id.clone(), tool_name: card.tool_name.clone(), ordinal: card.ordinal,
        arguments_summary: compact_tool_arguments(&card.arguments), code_language, status: card.status, is_partial: card.is_partial(),
        is_error: card.is_error, cancelled: card.cancelled, truncated: omitted_content_lines > 0, omitted_content_lines, rows }
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
        let compact=a.rows("l",false).unwrap(); assert_eq!(title(&compact), "Bash"); assert_eq!(compact.omitted_content_lines,11); assert_eq!(content(&compact),(12..=30).map(|n|n.to_string()).collect::<Vec<_>>()); assert_eq!(content(&a.rows("l",true).unwrap()).len(),30);
        a.apply_application_event(&start("t","task",json!({"prompt":"inspect"}))); let task = a.rows("t",false).unwrap(); assert_eq!(title(&task), "Task");
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
}
