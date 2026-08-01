use std::{collections::{HashMap, HashSet}, fmt, sync::{Arc, atomic::{AtomicBool, Ordering}}};
use anyhow::{Result, anyhow};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus { Pending, InProgress, Completed, Abandoned }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoItem { pub content: String, pub status: TodoStatus }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoPhase { pub name: String, pub tasks: Vec<TodoItem> }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoCompletionTransition { pub phase: String, pub content: String }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStorage { Session, Memory }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoState { pub phases: Vec<TodoPhase>, pub storage: TodoStorage }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoInitPhase { pub phase: String, pub items: Vec<String> }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TodoOp {
    Init { #[serde(default, skip_serializing_if = "Option::is_none")] list: Option<Vec<TodoInitPhase>>, #[serde(default, skip_serializing_if = "Option::is_none")] items: Option<Vec<String>>, #[serde(default, skip_serializing_if = "Option::is_none")] phase: Option<String> },
    Start { task: String },
    Done { #[serde(default, skip_serializing_if = "Option::is_none")] task: Option<String>, #[serde(default, skip_serializing_if = "Option::is_none")] phase: Option<String> },
    Drop { #[serde(default, skip_serializing_if = "Option::is_none")] task: Option<String>, #[serde(default, skip_serializing_if = "Option::is_none")] phase: Option<String> },
    Rm { #[serde(default, skip_serializing_if = "Option::is_none")] task: Option<String>, #[serde(default, skip_serializing_if = "Option::is_none")] phase: Option<String> },
    Append { phase: String, items: Vec<String> }, View,
}
impl TodoOp {
    #[must_use] pub const fn is_view(&self) -> bool { matches!(self, Self::View) }
    #[must_use] pub const fn name(&self) -> &'static str {
        match self {
            Self::Init { .. } => "init",
            Self::Start { .. } => "start",
            Self::Done { .. } => "done",
            Self::Drop { .. } => "drop",
            Self::Rm { .. } => "rm",
            Self::Append { .. } => "append",
            Self::View => "view",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoApplyResult { pub phases: Vec<TodoPhase>, #[serde(default, skip_serializing_if = "Vec::is_empty")] pub completed_tasks: Vec<TodoCompletionTransition>, pub summary: String }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoToolDetails { pub phases: Vec<TodoPhase>, pub storage: TodoStorage, #[serde(default)] pub completed_tasks: Vec<TodoCompletionTransition> }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TodoOperationError { errors: Vec<String> }
impl TodoOperationError { fn new(errors: Vec<String>) -> Self { Self { errors } } #[must_use] pub fn errors(&self) -> &[String] { &self.errors } }
impl fmt::Display for TodoOperationError { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "Errors: {}", self.errors.join("; ")) } }
impl std::error::Error for TodoOperationError {}

pub type TodoStorageFn = Arc<dyn Fn() -> TodoStorage + Send + Sync>;
pub type TodoPersistFn = Arc<dyn Fn(&TodoState) -> Result<()> + Send + Sync>;
pub(crate) const TODO_ERROR_MARKER: &str = "__piTodoError";
#[derive(Clone)]
pub struct TodoRuntime { phases: Arc<Mutex<Vec<TodoPhase>>>, storage: TodoStorageFn, persist: TodoPersistFn, reminder_pending: Arc<AtomicBool> }
impl Default for TodoRuntime { fn default() -> Self { Self::memory() } }
impl TodoRuntime {
    #[must_use] pub fn memory() -> Self { Self::with_persistence(Arc::new(|| TodoStorage::Memory), Arc::new(|_| Ok(()))) }
    #[must_use] pub fn with_persistence(storage: TodoStorageFn, persist: TodoPersistFn) -> Self { Self { phases: Arc::new(Mutex::new(Vec::new())), storage, persist, reminder_pending: Arc::new(AtomicBool::new(false)) } }
    #[must_use] pub fn state(&self) -> TodoState { TodoState { phases: self.phases.lock().clone(), storage: (self.storage)() } }
    pub fn apply(&self, op: TodoOp) -> Result<TodoApplyResult> {
        let mut state = self.phases.lock(); let previous = state.clone();
        if op.is_view() { return Ok(TodoApplyResult { summary: format_todo_summary(&previous, &[], true), phases: previous, completed_tasks: Vec::new() }); }
        let mut updated = previous.clone(); let mut errors = Vec::new(); apply_operation(&mut updated, &op, &mut errors);
        if !errors.is_empty() { return Err(TodoOperationError::new(errors).into()); }
        normalize_todo_phases(&mut updated); let completed_tasks = completion_transitions(&previous, &updated);
        (self.persist)(&TodoState { phases: updated.clone(), storage: (self.storage)() })?; *state = updated.clone();
        Ok(TodoApplyResult { summary: format_todo_summary(&updated, &[], false), phases: updated, completed_tasks })
    }
    pub fn set_phases(&self, mut phases: Vec<TodoPhase>) -> Result<TodoApplyResult> {
        let errors = validate_unique_phases(&phases, ""); if !errors.is_empty() { return Err(TodoOperationError::new(errors).into()); }
        normalize_todo_phases(&mut phases); let mut state = self.phases.lock(); let completed_tasks = completion_transitions(&state, &phases);
        (self.persist)(&TodoState { phases: phases.clone(), storage: (self.storage)() })?; *state = phases.clone();
        Ok(TodoApplyResult { summary: format_todo_summary(&phases, &[], false), phases, completed_tasks })
    }
    pub fn restore_open(&self, phases: Vec<TodoPhase>) -> Result<()> {
        let mut open = phases.into_iter().map(|phase| TodoPhase { name: phase.name, tasks: phase.tasks.into_iter().filter(|task| matches!(task.status, TodoStatus::Pending | TodoStatus::InProgress)).collect() }).collect::<Vec<_>>();
        let errors = validate_unique_phases(&open, ""); if !errors.is_empty() { return Err(TodoOperationError::new(errors).into()); }
        normalize_todo_phases(&mut open); *self.phases.lock() = open; Ok(())
    }
    pub fn schedule_reminder(&self) { self.reminder_pending.store(true, Ordering::Release); }
    #[must_use] pub fn reminder_pending(&self) -> bool { self.reminder_pending.load(Ordering::Acquire) }
    #[must_use] pub fn take_reminder(&self) -> bool { self.reminder_pending.swap(false, Ordering::AcqRel) }
}

#[must_use]
pub fn normalize_todo_phases(phases: &mut [TodoPhase]) {
    let mut saw = false; for task in phases.iter_mut().flat_map(|phase| phase.tasks.iter_mut()) { if task.status == TodoStatus::InProgress { if saw { task.status = TodoStatus::Pending; } else { saw = true; } } }
    if saw { return; } if let Some(task) = phases.iter_mut().flat_map(|phase| phase.tasks.iter_mut()).find(|task| task.status == TodoStatus::Pending) { task.status = TodoStatus::InProgress; }
}

fn apply_operation(phases: &mut Vec<TodoPhase>, op: &TodoOp, errors: &mut Vec<String>) {
    match op {
        TodoOp::Init { list, items, phase } => {
            let init = list.clone().or_else(|| items.as_ref().filter(|items| !items.is_empty()).map(|items| vec![TodoInitPhase { phase: phase.clone().unwrap_or_else(|| "Tasks".to_owned()), items: items.clone() }]));
            let Some(init) = init else { errors.push("Missing list for init operation".to_owned()); return; };
            for entry in &init { if entry.items.is_empty() { errors.push(format!("Missing items for phase \"{}\" in init list", entry.phase)); } }
            errors.extend(validate_init_list(&init)); *phases = init.into_iter().map(|entry| TodoPhase { name: entry.phase, tasks: entry.items.into_iter().map(|content| TodoItem { content, status: TodoStatus::Pending }).collect() }).collect();
        }
        TodoOp::Start { task } => { let Some((pi, ti)) = resolve_task(phases, task, errors) else { return; }; for (p, phase) in phases.iter_mut().enumerate() { for (t, item) in phase.tasks.iter_mut().enumerate() { if item.status == TodoStatus::InProgress && (p, t) != (pi, ti) { item.status = TodoStatus::Pending; } } } phases[pi].tasks[ti].status = TodoStatus::InProgress; }
        TodoOp::Done { task, phase } => for (pi, ti) in resolve_targets(phases, task, phase, errors) { phases[pi].tasks[ti].status = TodoStatus::Completed; },
        TodoOp::Drop { task, phase } => for (pi, ti) in resolve_targets(phases, task, phase, errors) { phases[pi].tasks[ti].status = TodoStatus::Abandoned; },
        TodoOp::Rm { task, phase } => {
            if let Some(task) = task.as_deref().filter(|task| !task.is_empty()) { let Some((pi, ti)) = resolve_task(phases, task, errors) else { return; }; phases[pi].tasks.remove(ti); }
            else if let Some(phase) = phase.as_deref().filter(|phase| !phase.is_empty()) { let Some(pi) = resolve_phase(phases, phase, errors) else { return; }; phases[pi].tasks.clear(); }
            else { for phase in phases { phase.tasks.clear(); } }
        }
        TodoOp::Append { phase, items } => {
            if phase.trim().is_empty() {
                errors.push("Missing phase name for append operation".to_owned());
            } else if phase.trim() != phase {
                errors.push("Phase name must not have leading or trailing whitespace".to_owned());
            }
            if items.is_empty() { errors.push("Missing items for append operation".to_owned()); }
            for content in items {
                if content.trim().is_empty() {
                    errors.push("Task content must not be empty".to_owned());
                } else if content.trim() != content {
                    errors.push(format!("Task \"{content}\" must not have leading or trailing whitespace"));
                }
            }
            if !errors.is_empty() { return; }
            let existing = phases.iter().flat_map(|phase| phase.tasks.iter()).map(|task| task.content.as_str()).collect::<HashSet<_>>(); let mut seen = HashSet::new();
            for content in items { if existing.contains(content.as_str()) || !seen.insert(content.as_str()) { errors.push(format!("Task \"{content}\" already exists")); } } if !errors.is_empty() { return; }
            let pi = phases.iter().position(|candidate| candidate.name == *phase).unwrap_or_else(|| { phases.push(TodoPhase { name: phase.clone(), tasks: Vec::new() }); phases.len() - 1 });
            phases[pi].tasks.extend(items.iter().cloned().map(|content| TodoItem { content, status: TodoStatus::Pending }));
        }
        TodoOp::View => {}
    }
}

fn resolve_targets(phases: &[TodoPhase], task: &Option<String>, phase: &Option<String>, errors: &mut Vec<String>) -> Vec<(usize, usize)> {
    if let Some(task) = task.as_deref().filter(|task| !task.is_empty()) { return resolve_task(phases, task, errors).into_iter().collect(); }
    if let Some(phase) = phase.as_deref().filter(|phase| !phase.is_empty()) { let Some(pi) = resolve_phase(phases, phase, errors) else { return Vec::new(); }; return (0..phases[pi].tasks.len()).map(|ti| (pi, ti)).collect(); }
    phases.iter().enumerate().flat_map(|(pi, phase)| (0..phase.tasks.len()).map(move |ti| (pi, ti))).collect()
}
fn resolve_task(phases: &[TodoPhase], content: &str, errors: &mut Vec<String>) -> Option<(usize, usize)> {
    if content.is_empty() { errors.push("Missing task content".to_owned()); return None; }
    for (pi, phase) in phases.iter().enumerate() { if let Some(ti) = phase.tasks.iter().position(|task| task.content == content) { return Some((pi, ti)); } }
    if content.strip_prefix("task-").is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())) { errors.push(format!("Task \"{content}\" not found. Tasks are referenced by content, not by IDs — pass the task's full text from the previous result.")); }
    else { let hint = if phases.iter().all(|phase| phase.tasks.is_empty()) { " (todo list is empty — was it replaced or not yet created?)" } else { "" }; errors.push(format!("Task \"{content}\" not found{hint}")); } None
}
fn resolve_phase(phases: &[TodoPhase], name: &str, errors: &mut Vec<String>) -> Option<usize> { if name.is_empty() { errors.push("Missing phase name".to_owned()); return None; } let index = phases.iter().position(|phase| phase.name == name); if index.is_none() { errors.push(format!("Phase \"{name}\" not found")); } index }
fn validate_init_list(list: &[TodoInitPhase]) -> Vec<String> { validate_unique_phases(&list.iter().map(|entry| TodoPhase { name: entry.phase.clone(), tasks: entry.items.iter().cloned().map(|content| TodoItem { content, status: TodoStatus::Pending }).collect() }).collect::<Vec<_>>(), " in init list") }
fn validate_unique_phases(phases: &[TodoPhase], suffix: &str) -> Vec<String> {
    let mut errors = Vec::new();
    let mut names = HashSet::new();
    let mut tasks = HashSet::new();
    for phase in phases {
        if phase.name.trim().is_empty() {
            errors.push(format!("Phase name must not be empty{suffix}"));
        } else if phase.name.trim() != phase.name {
            errors.push(format!("Phase \"{}\" must not have leading or trailing whitespace{suffix}", phase.name));
        }
        if !names.insert(phase.name.as_str()) { errors.push(format!("Duplicate phase \"{}\"{suffix}", phase.name)); }
        for task in &phase.tasks {
            if task.content.trim().is_empty() {
                errors.push(format!("Task content must not be empty{suffix}"));
            } else if task.content.trim() != task.content {
                errors.push(format!("Task \"{}\" must not have leading or trailing whitespace{suffix}", task.content));
            }
            if !tasks.insert(task.content.as_str()) { errors.push(format!("Duplicate task \"{}\"{suffix}", task.content)); }
        }
    }
    errors
}
fn completion_transitions(previous: &[TodoPhase], updated: &[TodoPhase]) -> Vec<TodoCompletionTransition> { let old = previous.iter().flat_map(|phase| phase.tasks.iter().map(move |task| (format!("{}\0{}", phase.name, task.content), task.status))).collect::<HashMap<_, _>>(); updated.iter().flat_map(|phase| phase.tasks.iter().filter_map(|task| { if task.status != TodoStatus::Completed { return None; } let prior = old.get(&format!("{}\0{}", phase.name, task.content))?; (*prior != TodoStatus::Completed).then(|| TodoCompletionTransition { phase: phase.name.clone(), content: task.content.clone() }) })).collect() }

#[must_use]
pub fn todo_phases_to_markdown(phases: &[TodoPhase]) -> String { if phases.is_empty() { return "# Todos\n".to_owned(); } let mut lines = Vec::new(); for (index, phase) in phases.iter().enumerate() { if index > 0 { lines.push(String::new()); } lines.push(format!("# {}", phase.name)); for task in &phase.tasks { let marker = match task.status { TodoStatus::Pending => " ", TodoStatus::InProgress => "/", TodoStatus::Completed => "x", TodoStatus::Abandoned => "-" }; lines.push(format!("- [{marker}] {}", task.content)); } } format!("{}\n", lines.join("\n")) }
pub fn parse_todo_markdown(markdown: &str) -> Result<Vec<TodoPhase>> {
    let mut phases: Vec<TodoPhase> = Vec::new(); let mut current = None; let mut errors = Vec::new();
    for (line_index, raw) in markdown.lines().enumerate() { let line = raw.trim(); if line.is_empty() { continue; } if let Some(name) = parse_heading(line) { phases.push(TodoPhase { name: name.to_owned(), tasks: Vec::new() }); current = Some(phases.len() - 1); continue; } if let Some((marker, content)) = parse_check_item(line) { let status = match marker { "" | " " => Some(TodoStatus::Pending), "x" | "X" => Some(TodoStatus::Completed), "/" | ">" => Some(TodoStatus::InProgress), "-" | "~" => Some(TodoStatus::Abandoned), _ => None }; let Some(status) = status else { errors.push(format!("Line {}: unknown status marker \"[{marker}]\" (use [ ], [x], [/], [-])", line_index + 1)); continue; }; let pi = current.unwrap_or_else(|| { phases.push(TodoPhase { name: "Todos".to_owned(), tasks: Vec::new() }); current = Some(phases.len() - 1); phases.len() - 1 }); phases[pi].tasks.push(TodoItem { content: content.to_owned(), status }); continue; } errors.push(format!("Line {}: unrecognized syntax \"{line}\"", line_index + 1)); }
    errors.extend(validate_unique_phases(&phases, "")); if !errors.is_empty() { return Err(TodoOperationError::new(errors).into()); } normalize_todo_phases(&mut phases); Ok(phases)
}
fn parse_heading(line: &str) -> Option<&str> { let hashes = line.bytes().take_while(|byte| *byte == b'#').count(); if !(1..=6).contains(&hashes) { return None; } line.get(hashes..)?.strip_prefix(' ').map(str::trim) }
fn parse_check_item(line: &str) -> Option<(&str, &str)> { let rest = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")).or_else(|| line.strip_prefix("+ "))?.strip_prefix('[')?; let close = rest.find(']')?; let marker = &rest[..close]; if marker.chars().count() > 1 { return None; } Some((marker, rest.get(close + 1..)?.strip_prefix(' ')?.trim())) }

#[must_use]
pub fn format_todo_summary(phases: &[TodoPhase], errors: &[String], read_only: bool) -> String {
    let tasks = phases.iter().flat_map(|phase| phase.tasks.iter()).collect::<Vec<_>>(); if tasks.is_empty() { if !errors.is_empty() { return format!("Errors: {}", errors.join("; ")); } return if read_only { "Todo list is empty.".to_owned() } else { "Todo list cleared.".to_owned() }; }
    let remaining = phases.iter().flat_map(|phase| phase.tasks.iter().filter_map(move |task| matches!(task.status, TodoStatus::Pending | TodoStatus::InProgress).then_some((phase.name.as_str(), task)))).collect::<Vec<_>>(); let current_index = phases.iter().position(|phase| phase.tasks.iter().any(|task| matches!(task.status, TodoStatus::Pending | TodoStatus::InProgress))).unwrap_or(phases.len() - 1); let current = &phases[current_index]; let current_done = current.tasks.iter().filter(|task| matches!(task.status, TodoStatus::Completed | TodoStatus::Abandoned)).count(); let closed = tasks.iter().filter(|task| matches!(task.status, TodoStatus::Completed | TodoStatus::Abandoned)).count(); let worked_ahead = phases.iter().enumerate().any(|(index, phase)| index > current_index && phase.tasks.iter().any(|task| matches!(task.status, TodoStatus::Completed | TodoStatus::Abandoned))); let mut lines = Vec::new(); if !errors.is_empty() { lines.push(format!("Errors: {}", errors.join("; "))); }
    if remaining.is_empty() { lines.push("Remaining items: none.".to_owned()); } else { lines.push(format!("Remaining items ({}):", remaining.len())); for (phase, task) in &remaining { lines.push(format!("  - {} [{}] ({phase})", task.content, status_name(task.status))); } }
    lines.push(format!("Overall: {closed}/{} done, {} open.", tasks.len(), remaining.len())); let suffix = if worked_ahead { " — earliest phase with open tasks; the in-progress pointer auto-advances to the earliest open task on each completion, so it can sit behind out-of-order work (nothing was un-completed)." } else { "." }; lines.push(format!("Active phase {}/{} \"{}\" ({current_done}/{}){suffix}", current_index + 1, phases.len(), current.name, current.tasks.len()));
    for phase in phases { lines.push(format!("  {}:", phase.name)); for task in &phase.tasks { let checkbox = if task.status == TodoStatus::Completed { "[X]" } else { "[ ]" }; let suffix = match task.status { TodoStatus::InProgress => " (in progress)", TodoStatus::Abandoned => " (dropped)", TodoStatus::Pending | TodoStatus::Completed => "" }; lines.push(format!("    - {checkbox} {}{suffix}", task.content)); } } lines.join("\n")
}
fn status_name(status: TodoStatus) -> &'static str { match status { TodoStatus::Pending => "pending", TodoStatus::InProgress => "in_progress", TodoStatus::Completed => "completed", TodoStatus::Abandoned => "abandoned" } }
pub(crate) fn tool_failure_result(runtime: &TodoRuntime, error: &anyhow::Error) -> (String, TodoToolDetails) { let state = runtime.state(); let text = error.to_string(); let summary = if text.starts_with("Errors: ") { text } else { format!("Errors: {text}") }; (summary, TodoToolDetails { phases: state.phases, storage: state.storage, completed_tasks: Vec::new() }) }
pub(crate) fn deserialize_todo_op(value: serde_json::Value) -> Result<TodoOp> { serde_json::from_value(value).map_err(|error| anyhow!("Invalid todo arguments: {error}")) }

#[cfg(test)]
mod tests {
    use super::*;
    fn item(content: &str, status: TodoStatus) -> TodoItem { TodoItem { content: content.to_owned(), status } }
    fn phase(name: &str, tasks: Vec<TodoItem>) -> TodoPhase { TodoPhase { name: name.to_owned(), tasks } }
    fn init(list: Vec<(&str, Vec<&str>)>) -> TodoOp { TodoOp::Init { list: Some(list.into_iter().map(|(phase, items)| TodoInitPhase { phase: phase.to_owned(), items: items.into_iter().map(str::to_owned).collect() }).collect()), items: None, phase: None } }
    #[test] fn normalization_keeps_first_active_or_promotes_first_pending() { let mut phases = vec![phase("One", vec![item("closed", TodoStatus::Completed), item("first", TodoStatus::InProgress)]), phase("Two", vec![item("second", TodoStatus::InProgress), item("third", TodoStatus::Pending)])]; normalize_todo_phases(&mut phases); assert_eq!(phases[0].tasks[1].status, TodoStatus::InProgress); assert_eq!(phases[1].tasks[0].status, TodoStatus::Pending); phases[0].tasks[1].status = TodoStatus::Completed; normalize_todo_phases(&mut phases); assert_eq!(phases[1].tasks[0].status, TodoStatus::InProgress); }
    #[test] fn all_target_operations_update_or_remove_every_task() { let runtime = TodoRuntime::memory(); runtime.apply(init(vec![("One", vec!["a", "b"]), ("Two", vec!["c"])] )).unwrap(); runtime.apply(TodoOp::Done { task: None, phase: None }).unwrap(); assert!(runtime.state().phases.iter().flat_map(|phase| &phase.tasks).all(|task| task.status == TodoStatus::Completed)); runtime.apply(TodoOp::Drop { task: None, phase: None }).unwrap(); assert!(runtime.state().phases.iter().flat_map(|phase| &phase.tasks).all(|task| task.status == TodoStatus::Abandoned)); runtime.apply(TodoOp::Rm { task: None, phase: None }).unwrap(); assert!(runtime.state().phases.iter().all(|phase| phase.tasks.is_empty())); }
    #[test] fn init_and_append_reject_global_duplicates() { let runtime = TodoRuntime::memory(); assert_eq!(runtime.apply(init(vec![("Build", vec!["same", "first"]), ("Build", vec!["same"])] )).unwrap_err().to_string(), "Errors: Duplicate phase \"Build\" in init list; Duplicate task \"same\" in init list"); assert!(runtime.state().phases.is_empty()); runtime.apply(init(vec![("Build", vec!["same"])] )).unwrap(); assert_eq!(runtime.apply(TodoOp::Append { phase: "Later".to_owned(), items: vec!["same".to_owned(), "same".to_owned()] }).unwrap_err().to_string(), "Errors: Task \"same\" already exists; Task \"same\" already exists"); assert_eq!(runtime.state().phases.len(), 1); }
    #[test]
    fn invalid_identifiers_are_accumulated_and_rolled_back() {
        let runtime = TodoRuntime::memory();
        runtime.apply(init(vec![("Build", vec!["one"])] )).unwrap();
        let before = runtime.state();
        let error = runtime
            .apply(TodoOp::Append {
                phase: " ".to_owned(),
                items: vec!["".to_owned(), " padded ".to_owned()],
            })
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "Errors: Missing phase name for append operation; Task content must not be empty; Task \" padded \" must not have leading or trailing whitespace"
        );
        assert_eq!(runtime.state(), before);

        let error = runtime
            .set_phases(vec![phase(" Build ", vec![item(" ", TodoStatus::Pending)])])
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "Errors: Phase \" Build \" must not have leading or trailing whitespace; Task content must not be empty"
        );
        assert_eq!(runtime.state(), before);
    }

    #[test] fn failed_operation_rolls_back_and_persistence_failure_does_not_commit() { let runtime = TodoRuntime::memory(); runtime.apply(init(vec![("Build", vec!["one"])] )).unwrap(); let before = runtime.state(); runtime.apply(TodoOp::Append { phase: "Build".to_owned(), items: vec!["two".to_owned(), "one".to_owned()] }).unwrap_err(); assert_eq!(runtime.state(), before); let failing = TodoRuntime::with_persistence(Arc::new(|| TodoStorage::Session), Arc::new(|_| Err(anyhow!("disk full")))); assert_eq!(failing.apply(init(vec![("Build", vec!["one"])] )).unwrap_err().to_string(), "disk full"); assert!(failing.state().phases.is_empty()); }
    #[test] fn view_is_read_only_and_does_not_normalize_or_persist() { let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0)); let seen = writes.clone(); let runtime = TodoRuntime::with_persistence(Arc::new(|| TodoStorage::Session), Arc::new(move |_| { seen.fetch_add(1, Ordering::Relaxed); Ok(()) })); *runtime.phases.lock() = vec![phase("Build", vec![item("one", TodoStatus::Pending), item("two", TodoStatus::Pending)])]; let before = runtime.state(); assert_eq!(runtime.apply(TodoOp::View).unwrap().phases, before.phases); assert_eq!(runtime.state(), before); assert_eq!(writes.load(Ordering::Relaxed), 0); }
    #[test] fn completion_transitions_include_only_newly_completed_tasks() { let runtime = TodoRuntime::memory(); runtime.set_phases(vec![phase("Build", vec![item("one", TodoStatus::InProgress), item("two", TodoStatus::Completed), item("three", TodoStatus::Abandoned)])]).unwrap(); let result = runtime.apply(TodoOp::Done { task: None, phase: Some("Build".to_owned()) }).unwrap(); assert_eq!(result.completed_tasks, vec![TodoCompletionTransition { phase: "Build".to_owned(), content: "one".to_owned() }, TodoCompletionTransition { phase: "Build".to_owned(), content: "three".to_owned() }]); }
    #[test] fn markdown_round_trip_preserves_phases_and_accepts_alias_markers() { let phases = vec![phase("Build", vec![item("one", TodoStatus::InProgress), item("two", TodoStatus::Pending)]), phase("Verify", vec![item("three", TodoStatus::Completed), item("four", TodoStatus::Abandoned)])]; assert_eq!(parse_todo_markdown(&todo_phases_to_markdown(&phases)).unwrap(), phases); let aliases = parse_todo_markdown("- [>] active\n- [~] gone\n").unwrap(); assert_eq!(aliases[0].name, "Todos"); assert_eq!(aliases[0].tasks[0].status, TodoStatus::InProgress); assert_eq!(aliases[0].tasks[1].status, TodoStatus::Abandoned); }
    #[test] fn serialization_uses_omp_wire_names_and_detail_fields() { assert_eq!(serde_json::to_value(TodoOp::Append { phase: "Build".to_owned(), items: vec!["compile".to_owned()] }).unwrap(), serde_json::json!({"op":"append","phase":"Build","items":["compile"]})); let value = serde_json::to_value(TodoToolDetails { phases: vec![phase("Build", vec![item("compile", TodoStatus::InProgress)])], storage: TodoStorage::Session, completed_tasks: vec![TodoCompletionTransition { phase: "Build".to_owned(), content: "compile".to_owned() }] }).unwrap(); assert_eq!(value["storage"], "session"); assert_eq!(value["phases"][0]["tasks"][0]["status"], "in_progress"); assert_eq!(value["completedTasks"][0]["content"], "compile"); }
    #[test] fn summary_is_stable() { assert_eq!(format_todo_summary(&[], &[], true), "Todo list is empty."); assert_eq!(format_todo_summary(&[], &[], false), "Todo list cleared."); let phases = vec![phase("Build", vec![item("compile", TodoStatus::InProgress), item("test", TodoStatus::Pending)])]; assert_eq!(format_todo_summary(&phases, &[], false), "Remaining items (2):\n  - compile [in_progress] (Build)\n  - test [pending] (Build)\nOverall: 0/2 done, 2 open.\nActive phase 1/1 \"Build\" (0/2).\n  Build:\n    - [ ] compile (in progress)\n    - [ ] test"); }
    #[test] fn resume_keeps_only_open_tasks_then_normalizes() { let runtime = TodoRuntime::memory(); runtime.restore_open(vec![phase("Build", vec![item("done", TodoStatus::Completed), item("one", TodoStatus::Pending), item("two", TodoStatus::InProgress), item("gone", TodoStatus::Abandoned)])]).unwrap(); assert_eq!(runtime.state().phases, vec![phase("Build", vec![item("one", TodoStatus::Pending), item("two", TodoStatus::InProgress)])]); }
}
