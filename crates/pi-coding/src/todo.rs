use std::{collections::{HashMap, HashSet, VecDeque}, fmt, sync::{Arc, atomic::{AtomicBool, Ordering}}};
use anyhow::{Result, anyhow};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Deserializer, Serialize, de};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus { Pending, InProgress, Completed, Abandoned }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoBlockedReason { pub task_id: String, pub content: String, pub status: TodoStatus }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoItem {
    #[serde(default)] pub id: String,
    pub content: String,
    pub status: TodoStatus,
    #[serde(default)] pub depends_on: Vec<String>,
    #[serde(default)] pub ready: bool,
    #[serde(default)] pub blocked_by: Vec<TodoBlockedReason>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoPhase { pub name: String, pub tasks: Vec<TodoItem> }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoCompletionTransition { pub phase: String, pub content: String }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStorage { Session, Memory }
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoState { pub phases: Vec<TodoPhase>, pub storage: TodoStorage }

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TodoStateWire { phases: Vec<TodoPhase>, storage: TodoStorage }

impl<'de> Deserialize<'de> for TodoState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let TodoStateWire { mut phases, storage } = TodoStateWire::deserialize(deserializer)?;
        prepare_todo_phases(&mut phases).map_err(de::Error::custom)?;
        Ok(Self { phases, storage })
    }
}
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
    Rm { #[serde(default, skip_serializing_if = "Option::is_none")] task: Option<String>, #[serde(default, skip_serializing_if = "Option::is_none")] phase: Option<String>, #[serde(default)] cascade: bool },
    Append { phase: String, items: Vec<String> },
    AddDependency { task: String, #[serde(rename = "dependsOn")] depends_on: Vec<String> },
    RemoveDependency { task: String, #[serde(rename = "dependsOn")] depends_on: Vec<String> },
    UpdateDependencies { task: String, #[serde(rename = "dependsOn")] depends_on: Vec<String> },
    View,
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
            Self::AddDependency { .. } => "add_dependency",
            Self::RemoveDependency { .. } => "remove_dependency",
            Self::UpdateDependencies { .. } => "update_dependencies",
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
pub(crate) struct TodoMutationTransaction {
    pub(crate) gate: Arc<Mutex<()>>,
    pub(crate) check: Arc<dyn Fn() -> Result<()> + Send + Sync>,
    pub(crate) commit: Arc<dyn Fn() + Send + Sync>,
}
#[derive(Clone)]
pub struct TodoRuntime { phases: Arc<Mutex<Vec<TodoPhase>>>, storage: TodoStorageFn, persist: TodoPersistFn, reminder_pending: Arc<AtomicBool>, transaction: Arc<RwLock<Option<TodoMutationTransaction>>> }
impl Default for TodoRuntime { fn default() -> Self { Self::memory() } }
impl TodoRuntime {
    #[must_use] pub fn memory() -> Self { Self::with_persistence(Arc::new(|| TodoStorage::Memory), Arc::new(|_| Ok(()))) }
    #[must_use] pub fn with_persistence(storage: TodoStorageFn, persist: TodoPersistFn) -> Self { Self { phases: Arc::new(Mutex::new(Vec::new())), storage, persist, reminder_pending: Arc::new(AtomicBool::new(false)), transaction: Arc::new(RwLock::new(None)) } }
    #[must_use] pub fn state(&self) -> TodoState { TodoState { phases: self.phases.lock().clone(), storage: (self.storage)() } }
    pub fn apply(&self, op: TodoOp) -> Result<TodoApplyResult> {
        if op.is_view() { return self.apply_raw(op); }
        let transaction = self.transaction.read().clone();
        let _guard = transaction.as_ref().map(|transaction| transaction.gate.lock());
        if let Some(transaction) = &transaction { (transaction.check)()?; }
        let result = self.apply_raw(op)?;
        if let Some(transaction) = &transaction { (transaction.commit)(); }
        Ok(result)
    }
    pub(crate) fn apply_raw(&self, op: TodoOp) -> Result<TodoApplyResult> {
        let mut state = self.phases.lock(); let previous = state.clone();
        if op.is_view() { return Ok(TodoApplyResult { summary: format_todo_summary(&previous, &[], true), phases: previous, completed_tasks: Vec::new() }); }
        let mut updated = previous.clone(); let mut errors = Vec::new(); apply_operation(&mut updated, &op, &mut errors);
        if errors.is_empty() { if let Err(error) = prepare_todo_phases(&mut updated) { errors.extend(error.errors); } }
        if !errors.is_empty() { return Err(TodoOperationError::new(errors).into()); }
        let completed_tasks = completion_transitions(&previous, &updated);
        (self.persist)(&TodoState { phases: updated.clone(), storage: (self.storage)() })?; *state = updated.clone();
        Ok(TodoApplyResult { summary: format_todo_summary(&updated, &[], false), phases: updated, completed_tasks })
    }
    pub fn set_phases(&self, phases: Vec<TodoPhase>) -> Result<TodoApplyResult> {
        let transaction = self.transaction.read().clone();
        let _guard = transaction.as_ref().map(|transaction| transaction.gate.lock());
        if let Some(transaction) = &transaction { (transaction.check)()?; }
        let result = self.set_phases_raw(phases)?;
        if let Some(transaction) = &transaction { (transaction.commit)(); }
        Ok(result)
    }
    pub(crate) fn set_phases_raw(&self, mut phases: Vec<TodoPhase>) -> Result<TodoApplyResult> {
        prepare_todo_phases(&mut phases).map_err(anyhow::Error::new)?;
        let mut state = self.phases.lock(); let completed_tasks = completion_transitions(&state, &phases);
        (self.persist)(&TodoState { phases: phases.clone(), storage: (self.storage)() })?; *state = phases.clone();
        Ok(TodoApplyResult { summary: format_todo_summary(&phases, &[], false), phases, completed_tasks })
    }
    pub(crate) fn set_mutation_transaction(&self, transaction: TodoMutationTransaction) { *self.transaction.write() = Some(transaction); }
    pub fn restore_state(&self, mut phases: Vec<TodoPhase>) -> Result<()> { prepare_todo_phases(&mut phases).map_err(anyhow::Error::new)?; *self.phases.lock() = phases; Ok(()) }
    pub fn restore_open(&self, phases: Vec<TodoPhase>) -> Result<()> {
        let mut open = phases.into_iter().map(|phase| TodoPhase { name: phase.name, tasks: phase.tasks.into_iter().filter(|task| matches!(task.status, TodoStatus::Pending | TodoStatus::InProgress)).collect() }).collect::<Vec<_>>();
        let errors = validate_unique_phases(&open, ""); if !errors.is_empty() { return Err(TodoOperationError::new(errors).into()); }
        let retained = open.iter().flat_map(|phase| &phase.tasks).map(|task| task.id.clone()).collect::<HashSet<_>>();
        for task in open.iter_mut().flat_map(|phase| &mut phase.tasks) { task.depends_on.retain(|dependency| retained.contains(dependency)); task.blocked_by.clear(); task.ready = false; }
        normalize_todo_phases(&mut open); *self.phases.lock() = open; Ok(())
    }
    pub fn schedule_reminder(&self) { self.reminder_pending.store(true, Ordering::Release); }
    #[must_use] pub fn reminder_pending(&self) -> bool { self.reminder_pending.load(Ordering::Acquire) }
    #[must_use] pub fn take_reminder(&self) -> bool { self.reminder_pending.swap(false, Ordering::AcqRel) }
}

#[must_use]
pub fn normalize_todo_phases(phases: &mut [TodoPhase]) {
    let saw_active = phases
        .iter()
        .flat_map(|phase| &phase.tasks)
        .any(|task| task.status == TodoStatus::InProgress);
    if saw_active { return; }
    if let Some(task) = phases
        .iter_mut()
        .flat_map(|phase| phase.tasks.iter_mut())
        .find(|task| task.status == TodoStatus::Pending && task.depends_on.is_empty())
    {
        task.status = TodoStatus::InProgress;
    }
}

fn prepare_todo_phases(phases: &mut [TodoPhase]) -> std::result::Result<(), TodoOperationError> {
    assign_missing_ids(phases); let errors = validate_todo_phases(phases); if !errors.is_empty() { return Err(TodoOperationError::new(errors)); }
    project_readiness(phases); normalize_ready_pointer(phases); project_readiness(phases); Ok(())
}
fn assign_missing_ids(phases: &mut [TodoPhase]) {
    let mut used = phases.iter().flat_map(|phase| &phase.tasks).filter(|task| !task.id.is_empty()).map(|task| task.id.clone()).collect::<HashSet<_>>();
    for (pi, phase) in phases.iter_mut().enumerate() { for (ti, task) in phase.tasks.iter_mut().enumerate() { if !task.id.is_empty() { continue; } let mut hasher = Sha256::new(); hasher.update(b"pi-todo-legacy-v1\0"); hasher.update(pi.to_le_bytes()); hasher.update(phase.name.as_bytes()); hasher.update([0]); hasher.update(ti.to_le_bytes()); hasher.update(task.content.as_bytes()); let digest = hasher.finalize(); let base = format!("task-{}", hex_prefix(&digest, 10)); let mut candidate = base.clone(); let mut suffix = 2; while used.contains(&candidate) { candidate = format!("{base}-{suffix}"); suffix += 1; } used.insert(candidate.clone()); task.id = candidate; } }
}
fn hex_prefix(bytes: &[u8], count: usize) -> String { const HEX: &[u8; 16] = b"0123456789abcdef"; let mut output = String::with_capacity(count * 2); for byte in bytes.iter().take(count) { output.push(char::from(HEX[usize::from(byte >> 4)])); output.push(char::from(HEX[usize::from(byte & 0x0f)])); } output }
fn new_task(content: String) -> TodoItem { TodoItem { id: format!("task-{}", Uuid::new_v4().simple()), content, status: TodoStatus::Pending, depends_on: Vec::new(), ready: false, blocked_by: Vec::new() } }
fn normalize_ready_pointer(phases: &mut [TodoPhase]) {
    let mut saw_ready_active = false;
    for task in phases.iter_mut().flat_map(|phase| phase.tasks.iter_mut()) {
        if task.status == TodoStatus::InProgress {
            if task.ready {
                saw_ready_active = true;
            } else {
                task.status = TodoStatus::Pending;
            }
        }
    }
    if saw_ready_active { return; }
    if let Some(task) = phases
        .iter_mut()
        .flat_map(|phase| phase.tasks.iter_mut())
        .find(|task| task.status == TodoStatus::Pending && task.ready)
    {
        task.status = TodoStatus::InProgress;
    }
}
fn project_readiness(phases: &mut [TodoPhase]) { let tasks = phases.iter().flat_map(|phase| &phase.tasks).map(|task| (task.id.clone(), (task.content.clone(), task.status))).collect::<HashMap<_, _>>(); for task in phases.iter_mut().flat_map(|phase| phase.tasks.iter_mut()) { task.blocked_by = task.depends_on.iter().filter_map(|dependency| { let (content, status) = tasks.get(dependency)?; (!matches!(status, TodoStatus::Completed | TodoStatus::Abandoned)).then(|| TodoBlockedReason { task_id: dependency.clone(), content: content.clone(), status: *status }) }).collect(); task.ready = matches!(task.status, TodoStatus::Pending | TodoStatus::InProgress) && task.blocked_by.is_empty(); } }

fn apply_operation(phases: &mut Vec<TodoPhase>, op: &TodoOp, errors: &mut Vec<String>) {
    match op {
        TodoOp::Init { list, items, phase } => {
            let init = list.clone().or_else(|| items.as_ref().filter(|items| !items.is_empty()).map(|items| vec![TodoInitPhase { phase: phase.clone().unwrap_or_else(|| "Tasks".to_owned()), items: items.clone() }]));
            let Some(init) = init else { errors.push("Missing list for init operation".to_owned()); return; };
            for entry in &init { if entry.items.is_empty() { errors.push(format!("Missing items for phase \"{}\" in init list", entry.phase)); } }
            errors.extend(validate_init_list(&init)); *phases = init.into_iter().map(|entry| TodoPhase { name: entry.phase, tasks: entry.items.into_iter().map(new_task).collect() }).collect();
        }
        TodoOp::Start { task } => {
            let Some((pi, ti)) = resolve_task(phases, task, errors) else { return; }; let target = &phases[pi].tasks[ti];
            if !target.blocked_by.is_empty() { errors.push(format!("Task \"{}\" ({}) is blocked by {}", target.content, target.id, target.blocked_by.iter().map(|dependency| format!("{} ({})", dependency.content, dependency.task_id)).collect::<Vec<_>>().join(", "))); return; }
            phases[pi].tasks[ti].status = TodoStatus::InProgress;
        }
        TodoOp::Done { task, phase } => for (pi, ti) in resolve_targets(phases, task, phase, errors) { phases[pi].tasks[ti].status = TodoStatus::Completed; },
        TodoOp::Drop { task, phase } => for (pi, ti) in resolve_targets(phases, task, phase, errors) { phases[pi].tasks[ti].status = TodoStatus::Abandoned; },
        TodoOp::Rm { task, phase, cascade } => remove_tasks(phases, task, phase, *cascade, errors),
        TodoOp::Append { phase, items } => { validate_append(phases, phase, items, errors); if !errors.is_empty() { return; } let pi = phases.iter().position(|candidate| candidate.name == *phase).unwrap_or_else(|| { phases.push(TodoPhase { name: phase.clone(), tasks: Vec::new() }); phases.len() - 1 }); phases[pi].tasks.extend(items.iter().cloned().map(new_task)); }
        TodoOp::AddDependency { task, depends_on } => update_dependencies(phases, task, depends_on, DependencyMutation::Add, errors),
        TodoOp::RemoveDependency { task, depends_on } => update_dependencies(phases, task, depends_on, DependencyMutation::Remove, errors),
        TodoOp::UpdateDependencies { task, depends_on } => update_dependencies(phases, task, depends_on, DependencyMutation::Replace, errors), TodoOp::View => {}
    }
}
fn validate_append(phases: &[TodoPhase], phase: &str, items: &[String], errors: &mut Vec<String>) {
    if phase.trim().is_empty() { errors.push("Missing phase name for append operation".to_owned()); } else if phase.trim() != phase { errors.push("Phase name must not have leading or trailing whitespace".to_owned()); } if items.is_empty() { errors.push("Missing items for append operation".to_owned()); }
    for content in items { if content.trim().is_empty() { errors.push("Task content must not be empty".to_owned()); } else if content.trim() != content { errors.push(format!("Task \"{content}\" must not have leading or trailing whitespace")); } } if !errors.is_empty() { return; }
    let existing = phases.iter().flat_map(|phase| &phase.tasks).map(|task| task.content.as_str()).collect::<HashSet<_>>(); let mut seen = HashSet::new(); for content in items { if existing.contains(content.as_str()) || !seen.insert(content.as_str()) { errors.push(format!("Task \"{content}\" already exists")); } }
}
#[derive(Clone, Copy)] enum DependencyMutation { Add, Remove, Replace }
fn update_dependencies(phases: &mut [TodoPhase], task_id: &str, dependencies: &[String], mutation: DependencyMutation, errors: &mut Vec<String>) {
    let Some((pi, ti)) = resolve_task_id(phases, task_id, errors) else { return; }; if matches!(mutation, DependencyMutation::Add | DependencyMutation::Remove) && dependencies.is_empty() { errors.push("dependsOn must contain at least one task ID".to_owned()); return; }
    let known = phases.iter().flat_map(|phase| &phase.tasks).map(|task| task.id.as_str()).collect::<HashSet<_>>(); let mut unique = HashSet::new(); for dependency in dependencies { if dependency.is_empty() { errors.push("Dependency task ID must not be empty".to_owned()); } else if !known.contains(dependency.as_str()) { errors.push(format!("Dependency task ID \"{dependency}\" not found")); } else if dependency == task_id { errors.push(format!("Task \"{task_id}\" cannot depend on itself")); } else if !unique.insert(dependency.as_str()) { errors.push(format!("Duplicate dependency task ID \"{dependency}\"")); } } if !errors.is_empty() { return; }
    let target = &mut phases[pi].tasks[ti].depends_on; match mutation { DependencyMutation::Add => for dependency in dependencies { if !target.contains(dependency) { target.push(dependency.clone()); } }, DependencyMutation::Remove => { for dependency in dependencies { if !target.contains(dependency) { errors.push(format!("Task \"{task_id}\" does not depend on \"{dependency}\"")); } } if errors.is_empty() { target.retain(|dependency| !dependencies.contains(dependency)); } }, DependencyMutation::Replace => *target = dependencies.to_vec() }
}
fn remove_tasks(phases: &mut Vec<TodoPhase>, task: &Option<String>, phase: &Option<String>, cascade: bool, errors: &mut Vec<String>) {
    let targets = if let Some(task) = task.as_deref().filter(|task| !task.is_empty()) { let Some((pi, ti)) = resolve_task(phases, task, errors) else { return; }; vec![(pi, ti)] } else if let Some(phase) = phase.as_deref().filter(|phase| !phase.is_empty()) { let Some(pi) = resolve_phase(phases, phase, errors) else { return; }; (0..phases[pi].tasks.len()).map(|ti| (pi, ti)).collect() } else { phases.iter().enumerate().flat_map(|(pi, phase)| (0..phase.tasks.len()).map(move |ti| (pi, ti))).collect() };
    let removed_ids = targets.iter().map(|(pi, ti)| phases[*pi].tasks[*ti].id.clone()).collect::<HashSet<_>>(); let dependents = phases.iter().flat_map(|phase| &phase.tasks).filter(|candidate| !removed_ids.contains(&candidate.id)).filter_map(|candidate| { let removed = candidate.depends_on.iter().filter(|dependency| removed_ids.contains(*dependency)).cloned().collect::<Vec<_>>(); (!removed.is_empty()).then(|| (candidate.id.clone(), candidate.content.clone(), removed)) }).collect::<Vec<_>>();
    if !cascade && !dependents.is_empty() { for (id, content, removed) in dependents { errors.push(format!("Cannot remove dependency target(s) {}: task \"{}\" ({}) depends on them; retry rm with cascade=true to remove those dependency edges", removed.join(", "), content, id)); } return; } if cascade { for candidate in phases.iter_mut().flat_map(|phase| &mut phase.tasks) { candidate.depends_on.retain(|dependency| !removed_ids.contains(dependency)); } }
    for (pi, phase) in phases.iter_mut().enumerate() { let mut indices = targets.iter().filter_map(|(target_phase, ti)| (*target_phase == pi).then_some(*ti)).collect::<Vec<_>>(); indices.sort_unstable_by(|left, right| right.cmp(left)); for ti in indices { phase.tasks.remove(ti); } }
}

fn resolve_targets(phases: &[TodoPhase], task: &Option<String>, phase: &Option<String>, errors: &mut Vec<String>) -> Vec<(usize, usize)> {
    if let Some(task) = task.as_deref().filter(|task| !task.is_empty()) { return resolve_task(phases, task, errors).into_iter().collect(); }
    if let Some(phase) = phase.as_deref().filter(|phase| !phase.is_empty()) { let Some(pi) = resolve_phase(phases, phase, errors) else { return Vec::new(); }; return (0..phases[pi].tasks.len()).map(|ti| (pi, ti)).collect(); }
    phases.iter().enumerate().flat_map(|(pi, phase)| (0..phase.tasks.len()).map(move |ti| (pi, ti))).collect()
}
fn resolve_task(phases: &[TodoPhase], reference: &str, errors: &mut Vec<String>) -> Option<(usize, usize)> {
    if reference.is_empty() { errors.push("Missing task ID or content".to_owned()); return None; }
    for (pi, phase) in phases.iter().enumerate() { if let Some(ti) = phase.tasks.iter().position(|task| task.id == reference || task.content == reference) { return Some((pi, ti)); } }
    let hint = if phases.iter().all(|phase| phase.tasks.is_empty()) { " (todo list is empty — was it replaced or not yet created?)" } else { "" }; errors.push(format!("Task \"{reference}\" not found by ID or content{hint}")); None
}
fn resolve_task_id(phases: &[TodoPhase], id: &str, errors: &mut Vec<String>) -> Option<(usize, usize)> { if id.is_empty() { errors.push("Missing task ID".to_owned()); return None; } for (pi, phase) in phases.iter().enumerate() { if let Some(ti) = phase.tasks.iter().position(|task| task.id == id) { return Some((pi, ti)); } } errors.push(format!("Task ID \"{id}\" not found")); None }
fn resolve_phase(phases: &[TodoPhase], name: &str, errors: &mut Vec<String>) -> Option<usize> { if name.is_empty() { errors.push("Missing phase name".to_owned()); return None; } let index = phases.iter().position(|phase| phase.name == name); if index.is_none() { errors.push(format!("Phase \"{name}\" not found")); } index }
fn validate_init_list(list: &[TodoInitPhase]) -> Vec<String> { validate_unique_phases(&list.iter().map(|entry| TodoPhase { name: entry.phase.clone(), tasks: entry.items.iter().cloned().map(|content| TodoItem { id: String::new(), content, status: TodoStatus::Pending, depends_on: Vec::new(), ready: false, blocked_by: Vec::new() }).collect() }).collect::<Vec<_>>(), " in init list") }
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
fn validate_todo_phases(phases: &[TodoPhase]) -> Vec<String> {
    let mut errors = validate_unique_phases(phases, ""); let mut ids = HashSet::new();
    for task in phases.iter().flat_map(|phase| &phase.tasks) { if task.id.trim().is_empty() { errors.push("Task ID must not be empty".to_owned()); } else if task.id.trim() != task.id { errors.push(format!("Task ID \"{}\" must not have leading or trailing whitespace", task.id)); } if !ids.insert(task.id.as_str()) { errors.push(format!("Duplicate task ID \"{}\"", task.id)); } let mut deps = HashSet::new(); for dependency in &task.depends_on { if !deps.insert(dependency.as_str()) { errors.push(format!("Task \"{}\" has duplicate dependency task ID \"{}\"", task.id, dependency)); } } }
    for task in phases.iter().flat_map(|phase| &phase.tasks) { for dependency in &task.depends_on { if !ids.contains(dependency.as_str()) { errors.push(format!("Task \"{}\" depends on missing task ID \"{}\"", task.id, dependency)); } if dependency == &task.id { errors.push(format!("Task \"{}\" cannot depend on itself", task.id)); } } }
    if errors.is_empty() && graph_contains_cycle(phases) { errors.push("Todo dependencies contain a cycle".to_owned()); } errors
}
fn graph_contains_cycle(phases: &[TodoPhase]) -> bool {
    let tasks = phases.iter().flat_map(|phase| &phase.tasks).collect::<Vec<_>>(); let mut indegree = tasks.iter().map(|task| (task.id.as_str(), task.depends_on.len())).collect::<HashMap<_, _>>(); let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for task in &tasks { for dependency in &task.depends_on { dependents.entry(dependency).or_default().push(&task.id); } }
    let mut ready = indegree.iter().filter_map(|(id, count)| (*count == 0).then_some(*id)).collect::<VecDeque<_>>(); let mut visited = 0;
    while let Some(id) = ready.pop_front() { visited += 1; if let Some(children) = dependents.get(id) { for child in children { if let Some(count) = indegree.get_mut(child) { *count -= 1; if *count == 0 { ready.push_back(child); } } } } } visited != tasks.len()
}
fn completion_transitions(previous: &[TodoPhase], updated: &[TodoPhase]) -> Vec<TodoCompletionTransition> { let old = previous.iter().flat_map(|phase| phase.tasks.iter().map(|task| (task.id.as_str(), task.status))).collect::<HashMap<_, _>>(); updated.iter().flat_map(|phase| phase.tasks.iter().filter_map(|task| { if task.status != TodoStatus::Completed { return None; } let prior = old.get(task.id.as_str())?; (*prior != TodoStatus::Completed).then(|| TodoCompletionTransition { phase: phase.name.clone(), content: task.content.clone() }) })).collect() }

#[must_use]
pub fn todo_phases_to_markdown(phases: &[TodoPhase]) -> String { if phases.is_empty() { return "# Todos\n".to_owned(); } let mut lines = Vec::new(); for (index, phase) in phases.iter().enumerate() { if index > 0 { lines.push(String::new()); } lines.push(format!("# {}", phase.name)); for task in &phase.tasks { let marker = match task.status { TodoStatus::Pending => " ", TodoStatus::InProgress => "/", TodoStatus::Completed => "x", TodoStatus::Abandoned => "-" }; lines.push(format!("- [{marker}] {}", task.content)); } } format!("{}\n", lines.join("\n")) }
pub fn parse_todo_markdown(markdown: &str) -> Result<Vec<TodoPhase>> {
    let mut phases: Vec<TodoPhase> = Vec::new(); let mut current = None; let mut errors = Vec::new();
    for (line_index, raw) in markdown.lines().enumerate() { let line = raw.trim(); if line.is_empty() { continue; } if let Some(name) = parse_heading(line) { phases.push(TodoPhase { name: name.to_owned(), tasks: Vec::new() }); current = Some(phases.len() - 1); continue; } if let Some((marker, content)) = parse_check_item(line) { let status = match marker { "" | " " => Some(TodoStatus::Pending), "x" | "X" => Some(TodoStatus::Completed), "/" | ">" => Some(TodoStatus::InProgress), "-" | "~" => Some(TodoStatus::Abandoned), _ => None }; let Some(status) = status else { errors.push(format!("Line {}: unknown status marker \"[{marker}]\" (use [ ], [x], [/], [-])", line_index + 1)); continue; }; let pi = current.unwrap_or_else(|| { phases.push(TodoPhase { name: "Todos".to_owned(), tasks: Vec::new() }); current = Some(phases.len() - 1); phases.len() - 1 }); phases[pi].tasks.push(TodoItem { id: String::new(), content: content.to_owned(), status, depends_on: Vec::new(), ready: false, blocked_by: Vec::new() }); continue; } errors.push(format!("Line {}: expected a heading or checklist item", line_index + 1)); }
    errors.extend(validate_unique_phases(&phases, "")); if !errors.is_empty() { return Err(TodoOperationError::new(errors).into()); } normalize_todo_phases(&mut phases); Ok(phases)
}
fn parse_heading(line: &str) -> Option<&str> { let hashes = line.bytes().take_while(|byte| *byte == b'#').count(); if !(1..=6).contains(&hashes) { return None; } line.get(hashes..)?.strip_prefix(' ').map(str::trim) }
fn parse_check_item(line: &str) -> Option<(&str, &str)> { let rest = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")).or_else(|| line.strip_prefix("+ "))?.strip_prefix('[')?; let close = rest.find(']')?; let marker = &rest[..close]; if marker.chars().count() > 1 { return None; } Some((marker, rest.get(close + 1..)?.strip_prefix(' ')?.trim())) }

#[must_use]
pub fn format_todo_summary(phases: &[TodoPhase], errors: &[String], read_only: bool) -> String {
    let tasks = phases.iter().flat_map(|phase| phase.tasks.iter()).collect::<Vec<_>>(); if tasks.is_empty() { if !errors.is_empty() { return format!("Errors: {}", errors.join("; ")); } return if read_only { "Todo list is empty.".to_owned() } else { "Todo list cleared.".to_owned() }; }
    let remaining = phases.iter().flat_map(|phase| phase.tasks.iter().filter_map(move |task| matches!(task.status, TodoStatus::Pending | TodoStatus::InProgress).then_some((phase.name.as_str(), task)))).collect::<Vec<_>>(); let current_index = phases.iter().position(|phase| phase.tasks.iter().any(|task| matches!(task.status, TodoStatus::Pending | TodoStatus::InProgress))).unwrap_or(phases.len() - 1); let current = &phases[current_index]; let current_done = current.tasks.iter().filter(|task| matches!(task.status, TodoStatus::Completed | TodoStatus::Abandoned)).count(); let closed = tasks.iter().filter(|task| matches!(task.status, TodoStatus::Completed | TodoStatus::Abandoned)).count(); let graph = tasks.iter().any(|task| !task.id.is_empty() || !task.depends_on.is_empty() || task.ready || !task.blocked_by.is_empty()); let mut lines = Vec::new();
    if remaining.is_empty() { lines.push("Remaining items: none.".to_owned()); } else { lines.push(format!("Remaining items ({}):", remaining.len())); for (phase, task) in &remaining { let projection = if task.id.is_empty() { String::new() } else if task.ready { format!(" id={} ready", task.id) } else { format!(" id={} blocked by {}", task.id, task.blocked_by.iter().map(|dependency| format!("{} ({})", dependency.content, dependency.task_id)).collect::<Vec<_>>().join(", ")) }; lines.push(format!("  - {} [{}] ({phase}){projection}", task.content, status_name(task.status))); } }
    lines.push(format!("Overall: {closed}/{} done, {} open.", tasks.len(), remaining.len())); let suffix = if graph { " — phase order is presentation only; any ready task may proceed." } else { "." }; lines.push(format!("Active phase {}/{} \"{}\" ({current_done}/{}){suffix}", current_index + 1, phases.len(), current.name, current.tasks.len()));
    for phase in phases { lines.push(format!("  {}:", phase.name)); for task in &phase.tasks { let checkbox = if task.status == TodoStatus::Completed { "[X]" } else { "[ ]" }; let status_suffix = match task.status { TodoStatus::InProgress => " (in progress)", TodoStatus::Abandoned => " (dropped)", TodoStatus::Pending | TodoStatus::Completed => "" }; let graph_suffix = if task.id.is_empty() { String::new() } else if task.ready { format!(" [{}; ready]", task.id) } else if task.blocked_by.is_empty() { format!(" [{}]", task.id) } else { format!(" [{}; blocked by {}]", task.id, task.blocked_by.iter().map(|dependency| dependency.task_id.as_str()).collect::<Vec<_>>().join(", ")) }; lines.push(format!("    - {checkbox} {}{status_suffix}{graph_suffix}", task.content)); } } lines.join("\n")
}
fn status_name(status: TodoStatus) -> &'static str { match status { TodoStatus::Pending => "pending", TodoStatus::InProgress => "in_progress", TodoStatus::Completed => "completed", TodoStatus::Abandoned => "abandoned" } }
pub(crate) fn tool_failure_result(runtime: &TodoRuntime, error: &anyhow::Error) -> (String, TodoToolDetails) { let state = runtime.state(); let text = error.to_string(); let summary = if text.starts_with("Errors: ") { text } else { format!("Errors: {text}") }; (summary, TodoToolDetails { phases: state.phases, storage: state.storage, completed_tasks: Vec::new() }) }
pub(crate) fn deserialize_todo_op(value: serde_json::Value) -> Result<TodoOp> { serde_json::from_value(value).map_err(|error| anyhow!("Invalid todo arguments: {error}")) }

#[cfg(test)]
mod tests {
    use super::*;
    fn item(content: &str, status: TodoStatus) -> TodoItem { TodoItem { id: String::new(), content: content.to_owned(), status, depends_on: Vec::new(), ready: false, blocked_by: Vec::new() } }
    fn graph_item(id: &str, content: &str, status: TodoStatus, depends_on: &[&str]) -> TodoItem { TodoItem { id: id.to_owned(), content: content.to_owned(), status, depends_on: depends_on.iter().map(|id| (*id).to_owned()).collect(), ready: false, blocked_by: Vec::new() } }
    fn phase(name: &str, tasks: Vec<TodoItem>) -> TodoPhase { TodoPhase { name: name.to_owned(), tasks } }
    fn init(list: Vec<(&str, Vec<&str>)>) -> TodoOp { TodoOp::Init { list: Some(list.into_iter().map(|(phase, items)| TodoInitPhase { phase: phase.to_owned(), items: items.into_iter().map(str::to_owned).collect() }).collect()), items: None, phase: None } }
    #[test] fn normalization_preserves_parallel_active_tasks_and_promotes_when_none_remain() { let mut phases = vec![phase("One", vec![item("closed", TodoStatus::Completed), item("first", TodoStatus::InProgress)]), phase("Two", vec![item("second", TodoStatus::InProgress), item("third", TodoStatus::Pending)])]; normalize_todo_phases(&mut phases); assert_eq!(phases[0].tasks[1].status, TodoStatus::InProgress); assert_eq!(phases[1].tasks[0].status, TodoStatus::InProgress); phases[0].tasks[1].status = TodoStatus::Completed; phases[1].tasks[0].status = TodoStatus::Completed; normalize_todo_phases(&mut phases); assert_eq!(phases[1].tasks[1].status, TodoStatus::InProgress); }
    #[test] fn all_target_operations_update_or_remove_every_task() { let runtime = TodoRuntime::memory(); runtime.apply(init(vec![("One", vec!["a", "b"]), ("Two", vec!["c"])] )).unwrap(); runtime.apply(TodoOp::Done { task: None, phase: None }).unwrap(); assert!(runtime.state().phases.iter().flat_map(|phase| &phase.tasks).all(|task| task.status == TodoStatus::Completed)); runtime.apply(TodoOp::Drop { task: None, phase: None }).unwrap(); assert!(runtime.state().phases.iter().flat_map(|phase| &phase.tasks).all(|task| task.status == TodoStatus::Abandoned)); runtime.apply(TodoOp::Rm { task: None, phase: None, cascade: false }).unwrap(); assert!(runtime.state().phases.iter().all(|phase| phase.tasks.is_empty())); }
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
    #[test] fn diamond_dag_projects_parallel_roots_and_topological_blockers() { let runtime = TodoRuntime::memory(); runtime.set_phases(vec![phase("Roots", vec![graph_item("a", "root a", TodoStatus::Pending, &[]), graph_item("b", "root b", TodoStatus::Pending, &[])]), phase("Join", vec![graph_item("c", "join", TodoStatus::Pending, &["a", "b"]), graph_item("d", "leaf", TodoStatus::Pending, &["c"])])]).unwrap(); let state = runtime.state(); assert!(state.phases[0].tasks[0].ready); assert!(state.phases[0].tasks[1].ready); assert_eq!(state.phases[1].tasks[0].blocked_by.len(), 2); assert_eq!(state.phases[1].tasks[1].blocked_by[0].task_id, "c"); assert!(!state.phases[1].tasks[0].ready); assert!(!state.phases[1].tasks[1].ready); }
    #[test] fn ready_normalization_preserves_multiple_unblocked_active_roots() { let runtime = TodoRuntime::memory(); runtime.set_phases(vec![phase("Roots", vec![graph_item("a", "root a", TodoStatus::InProgress, &[]), graph_item("b", "root b", TodoStatus::InProgress, &[])]), phase("Join", vec![graph_item("c", "join", TodoStatus::InProgress, &["a", "b"])])]).unwrap(); let state = runtime.state(); assert_eq!(state.phases[0].tasks[0].status, TodoStatus::InProgress); assert_eq!(state.phases[0].tasks[1].status, TodoStatus::InProgress); assert_eq!(state.phases[1].tasks[0].status, TodoStatus::Pending); assert!(state.phases[0].tasks.iter().all(|task| task.ready)); assert!(!state.phases[1].tasks[0].ready); }
    #[test] fn dependency_completion_and_drop_both_unlock_dependents() { for terminal in [TodoStatus::Completed, TodoStatus::Abandoned] { let runtime = TodoRuntime::memory(); runtime.set_phases(vec![phase("Graph", vec![graph_item("a", "root", TodoStatus::Pending, &[]), graph_item("b", "dependent", TodoStatus::Pending, &["a"])])]).unwrap(); let op = if terminal == TodoStatus::Completed { TodoOp::Done { task: Some("a".to_owned()), phase: None } } else { TodoOp::Drop { task: Some("a".to_owned()), phase: None } }; runtime.apply(op).unwrap(); let state = runtime.state(); assert_eq!(state.phases[0].tasks[0].status, terminal); assert!(state.phases[0].tasks[1].ready); assert!(state.phases[0].tasks[1].blocked_by.is_empty()); } }
    #[test] fn missing_ids_and_cycles_are_rejected_atomically() { let runtime = TodoRuntime::memory(); runtime.set_phases(vec![phase("Graph", vec![graph_item("a", "a", TodoStatus::Pending, &[]), graph_item("b", "b", TodoStatus::Pending, &["a"])])]).unwrap(); let before = runtime.state(); assert_eq!(runtime.apply(TodoOp::AddDependency { task: "missing".to_owned(), depends_on: vec!["a".to_owned()] }).unwrap_err().to_string(), "Errors: Task ID \"missing\" not found"); assert_eq!(runtime.state(), before); assert_eq!(runtime.apply(TodoOp::AddDependency { task: "a".to_owned(), depends_on: vec!["b".to_owned()] }).unwrap_err().to_string(), "Errors: Todo dependencies contain a cycle"); assert_eq!(runtime.state(), before); }
    #[test] fn dependency_mutations_and_explicit_removal_cascade_preserve_graph_validity() { let runtime = TodoRuntime::memory(); runtime.set_phases(vec![phase("Graph", vec![graph_item("a", "a", TodoStatus::Pending, &[]), graph_item("b", "b", TodoStatus::Pending, &[])])]).unwrap(); runtime.apply(TodoOp::AddDependency { task: "b".to_owned(), depends_on: vec!["a".to_owned()] }).unwrap(); runtime.apply(TodoOp::RemoveDependency { task: "b".to_owned(), depends_on: vec!["a".to_owned()] }).unwrap(); assert!(runtime.state().phases[0].tasks[1].depends_on.is_empty()); runtime.apply(TodoOp::UpdateDependencies { task: "b".to_owned(), depends_on: vec!["a".to_owned()] }).unwrap(); let before = runtime.state(); assert!(runtime.apply(TodoOp::Rm { task: Some("a".to_owned()), phase: None, cascade: false }).unwrap_err().to_string().contains("cascade=true")); assert_eq!(runtime.state(), before); runtime.apply(TodoOp::Rm { task: Some("a".to_owned()), phase: None, cascade: true }).unwrap(); let state = runtime.state(); assert_eq!(state.phases[0].tasks.len(), 1); assert!(state.phases[0].tasks[0].depends_on.is_empty()); assert!(state.phases[0].tasks[0].ready); }
    #[test] fn durable_graph_persists_and_reloads_with_projection() { let persisted = Arc::new(Mutex::new(None::<String>)); let seen = persisted.clone(); let runtime = TodoRuntime::with_persistence(Arc::new(|| TodoStorage::Session), Arc::new(move |state| { *seen.lock() = Some(serde_json::to_string(state)?); Ok(()) })); runtime.set_phases(vec![phase("Graph", vec![graph_item("a", "root", TodoStatus::InProgress, &[]), graph_item("b", "dependent", TodoStatus::Pending, &["a"])])]).unwrap(); runtime.apply(TodoOp::Done { task: Some("a".to_owned()), phase: None }).unwrap(); let json = persisted.lock().clone().expect("snapshot"); let restored: TodoState = serde_json::from_str(&json).unwrap(); assert_eq!(restored.phases[0].tasks[1].depends_on, vec!["a"]); assert!(restored.phases[0].tasks[1].ready); assert_eq!(restored.phases[0].tasks[1].status, TodoStatus::InProgress); }
    #[test] fn legacy_state_migration_assigns_deterministic_ids() { let legacy = serde_json::json!({"phases":[{"name":"Build","tasks":[{"content":"compile","status":"in_progress"},{"content":"test","status":"pending"}]}],"storage":"session"}); let first: TodoState = serde_json::from_value(legacy.clone()).unwrap(); let second: TodoState = serde_json::from_value(legacy).unwrap(); assert_eq!(first.phases[0].tasks[0].id, second.phases[0].tasks[0].id); assert_eq!(first.phases[0].tasks[1].id, second.phases[0].tasks[1].id); assert_ne!(first.phases[0].tasks[0].id, first.phases[0].tasks[1].id); assert!(first.phases[0].tasks.iter().all(|task| task.id.starts_with("task-"))); assert!(first.phases[0].tasks.iter().all(|task| task.ready)); }
    #[test] fn dependency_wire_names_are_public() { assert_eq!(serde_json::to_value(TodoOp::UpdateDependencies { task: "task-b".to_owned(), depends_on: vec!["task-a".to_owned()] }).unwrap(), serde_json::json!({"op":"update_dependencies","task":"task-b","dependsOn":["task-a"]})); let mut phases = vec![phase("Graph", vec![graph_item("a", "root", TodoStatus::Pending, &[]), graph_item("b", "child", TodoStatus::Pending, &["a"])])]; prepare_todo_phases(&mut phases).unwrap(); let value = serde_json::to_value(&phases[0].tasks[1]).unwrap(); assert_eq!(value["dependsOn"], serde_json::json!(["a"])); assert_eq!(value["ready"], false); assert_eq!(value["blockedBy"][0]["taskId"], "a"); }
}
