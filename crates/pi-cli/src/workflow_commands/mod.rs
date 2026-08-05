//! `/workflow` parser, typed requests, human formatting, and manager adapter surface.
//!
//! Canonical identity, status, snapshots, and lifecycle live in `pi_coding::workflow`.
//! This module is the CLI adapter: parse the exact slash contract, format English-only
//! output, and execute against a live [`WorkflowManager`] (never fabricated status).

use anyhow::{Result, anyhow, bail};
use pi_coding::{
    Application, WorkflowCreateRequest, WorkflowId, WorkflowIntegration, WorkflowManager,
    WorkflowSnapshot, WorkflowStatus,
};

/// Canonical usage advertised by the builtin registry and parse errors.
pub const WORKFLOW_USAGE: &str =
    "/workflow [list|show [id|name]|create <objective>|create <name> <objective>|pause|resume|cancel|integrate|remove]";

/// Selector accepted by show/lifecycle subcommands (`id` or `name`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowSelector(pub String);

impl WorkflowSelector {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Typed `/workflow` request produced by the CLI parser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InteractiveWorkflowCommand {
    /// Bare `/workflow` — open the dedicated workflows page (TUI) / list precursor.
    OpenPage,
    List,
    Show {
        selector: Option<WorkflowSelector>,
    },
    Create {
        name: String,
        objective: String,
    },
    Pause {
        selector: Option<WorkflowSelector>,
    },
    Resume {
        selector: Option<WorkflowSelector>,
    },
    Cancel {
        selector: Option<WorkflowSelector>,
    },
    Integrate {
        selector: Option<WorkflowSelector>,
    },
    Remove {
        selector: Option<WorkflowSelector>,
    },
}

/// CLI-facing summary projected losslessly from [`WorkflowSnapshot`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowSummary {
    pub id: WorkflowId,
    pub name: String,
    pub objective: String,
    pub status: WorkflowStatus,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
    pub supervisor_agent_id: Option<String>,
    pub generation: u64,
    pub failure: Option<String>,
    pub integration: Option<String>,
}

impl From<&WorkflowSnapshot> for WorkflowSummary {
    fn from(snapshot: &WorkflowSnapshot) -> Self {
        Self {
            id: snapshot.workflow_id.clone(),
            name: snapshot.name.clone(),
            objective: snapshot.objective.clone(),
            status: snapshot.status,
            worktree_path: snapshot.worktree_label.clone(),
            branch: snapshot.branch.clone(),
            supervisor_agent_id: snapshot.supervisor_agent_id.clone(),
            generation: snapshot.generation,
            failure: snapshot.failure.as_ref().map(|failure| failure.message.clone()),
            integration: format_integration(&snapshot.integration),
        }
    }
}

impl From<WorkflowSnapshot> for WorkflowSummary {
    fn from(snapshot: WorkflowSnapshot) -> Self {
        Self::from(&snapshot)
    }
}

fn format_integration(integration: &WorkflowIntegration) -> Option<String> {
    match integration {
        WorkflowIntegration::None => None,
        WorkflowIntegration::Applied { result_commit } => {
            Some(format!("applied · {result_commit}"))
        }
        WorkflowIntegration::Conflicted { conflicts } => {
            if conflicts.is_empty() {
                Some("conflicted".to_owned())
            } else {
                Some(format!("conflicted · {}", conflicts.join(", ")))
            }
        }
    }
}

/// Effect returned by adapter execution (TUI intercepts [`Self::OpenPage`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkflowCommandEffect {
    OpenPage,
    Message(String),
}

/// Async runtime port over a live workflow manager.
#[async_trait::async_trait]
pub trait WorkflowCommandPort: Send + Sync {
    async fn list(&self) -> Result<Vec<WorkflowSummary>>;
    async fn get(&self, selector: &str) -> Result<WorkflowSummary>;
    async fn selected(&self) -> Result<Option<WorkflowSummary>>;
    async fn create(&self, name: String, objective: String) -> Result<WorkflowSummary>;
    async fn pause(&self, selector: Option<&str>) -> Result<WorkflowSummary>;
    async fn resume(&self, selector: Option<&str>) -> Result<WorkflowSummary>;
    async fn cancel(&self, selector: Option<&str>) -> Result<WorkflowSummary>;
    async fn integrate(&self, selector: Option<&str>) -> Result<WorkflowSummary>;
    async fn remove(&self, selector: Option<&str>) -> Result<WorkflowSummary>;
}

/// Live manager adapter. Lifecycle methods resolve the target, then call the
/// generation-gated manager APIs (`pause`/`resume`/`cancel`/`integrate`/`remove`).
#[derive(Clone, Debug)]
pub struct ManagerWorkflowPort {
    manager: WorkflowManager,
}

impl ManagerWorkflowPort {
    #[must_use]
    pub fn new(manager: WorkflowManager) -> Self {
        Self { manager }
    }

    #[must_use]
    pub fn manager(&self) -> &WorkflowManager {
        &self.manager
    }

    fn resolve_snapshot(&self, selector: Option<&str>) -> Result<WorkflowSnapshot> {
        match selector {
            Some(selector) => match self.manager.get(&WorkflowId::new(selector)) {
                Ok(snapshot) => Ok(snapshot),
                Err(_) => self.manager.get_by_name(selector),
            },
            None => self
                .manager
                .selected()
                .ok_or_else(|| anyhow!("no workflow selected; pass id or name")),
        }
    }
}

#[async_trait::async_trait]
impl WorkflowCommandPort for ManagerWorkflowPort {
    async fn list(&self) -> Result<Vec<WorkflowSummary>> {
        Ok(self
            .manager
            .list()
            .iter()
            .map(WorkflowSummary::from)
            .collect())
    }

    async fn get(&self, selector: &str) -> Result<WorkflowSummary> {
        Ok(WorkflowSummary::from(self.resolve_snapshot(Some(selector))?))
    }

    async fn selected(&self) -> Result<Option<WorkflowSummary>> {
        Ok(self.manager.selected().map(WorkflowSummary::from))
    }

    async fn create(&self, name: String, objective: String) -> Result<WorkflowSummary> {
        let snapshot = self
            .manager
            .create(WorkflowCreateRequest { name, objective })
            .await?;
        Ok(WorkflowSummary::from(snapshot))
    }

    async fn pause(&self, selector: Option<&str>) -> Result<WorkflowSummary> {
        let current = self.resolve_snapshot(selector)?;
        let snapshot = self
            .manager
            .pause(&current.workflow_id, current.generation)
            .await?;
        Ok(WorkflowSummary::from(snapshot))
    }

    async fn resume(&self, selector: Option<&str>) -> Result<WorkflowSummary> {
        let current = self.resolve_snapshot(selector)?;
        let snapshot = self
            .manager
            .resume(&current.workflow_id, current.generation)
            .await?;
        Ok(WorkflowSummary::from(snapshot))
    }

    async fn cancel(&self, selector: Option<&str>) -> Result<WorkflowSummary> {
        let current = self.resolve_snapshot(selector)?;
        let snapshot = self
            .manager
            .cancel(&current.workflow_id, current.generation)
            .await?;
        Ok(WorkflowSummary::from(snapshot))
    }

    async fn integrate(&self, selector: Option<&str>) -> Result<WorkflowSummary> {
        let current = self.resolve_snapshot(selector)?;
        let snapshot = self
            .manager
            .integrate(&current.workflow_id, current.generation)
            .await?;
        Ok(WorkflowSummary::from(snapshot))
    }

    async fn remove(&self, selector: Option<&str>) -> Result<WorkflowSummary> {
        let current = self.resolve_snapshot(selector)?;
        let snapshot = self
            .manager
            .remove(&current.workflow_id, current.generation)
            .await?;
        Ok(WorkflowSummary::from(snapshot))
    }
}

/// Resolve the Application-owned workflow manager port.
///
/// Requires `Application::attach_workflow_manager` (architect cutover). Fails closed
/// when no manager is attached — never invents status or opens a shadow store.
pub fn application_workflow_port(
    application: &Application,
) -> Result<ManagerWorkflowPort> {
    Ok(ManagerWorkflowPort::new(application.workflow_manager()?))
}

/// Parse a domain status wire string.
pub fn parse_workflow_status(value: &str) -> Result<WorkflowStatus> {
    match value {
        "queued" => Ok(WorkflowStatus::Queued),
        "planning" => Ok(WorkflowStatus::Planning),
        "running" => Ok(WorkflowStatus::Running),
        "paused" => Ok(WorkflowStatus::Paused),
        "integrating" => Ok(WorkflowStatus::Integrating),
        "completed" => Ok(WorkflowStatus::Completed),
        "failed" => Ok(WorkflowStatus::Failed),
        "cancelled" => Ok(WorkflowStatus::Cancelled),
        "conflicted" => Ok(WorkflowStatus::Conflicted),
        other => bail!("unknown workflow status {other:?}"),
    }
}

/// Parse `/workflow` arguments into a typed request.
///
/// Contract:
/// - bare → [`InteractiveWorkflowCommand::OpenPage`]
/// - `list`
/// - `show [id|name]`
/// - `create <objective>` or `create <name> <objective>` (quoted args via [`pi_coding::parse_command_args`])
/// - `pause|resume|cancel|integrate|remove [id|name]`
pub fn parse_interactive_workflow_command(
    argument: Option<&str>,
) -> Result<InteractiveWorkflowCommand> {
    let argument = argument.unwrap_or_default().trim();
    if argument.is_empty() {
        return Ok(InteractiveWorkflowCommand::OpenPage);
    }

    let arguments = pi_coding::parse_command_args(argument);
    let mut parts = arguments.iter().map(String::as_str);
    let operation = parts
        .next()
        .ok_or_else(|| anyhow!("usage: {WORKFLOW_USAGE}"))?;

    match operation {
        "list" => no_trailing(parts, InteractiveWorkflowCommand::List),
        "show" => {
            let selector = optional_selector(&mut parts)?;
            no_trailing(parts, InteractiveWorkflowCommand::Show { selector })
        }
        "create" => parse_create(parts),
        "pause" => {
            let selector = optional_selector(&mut parts)?;
            no_trailing(parts, InteractiveWorkflowCommand::Pause { selector })
        }
        "resume" => {
            let selector = optional_selector(&mut parts)?;
            no_trailing(parts, InteractiveWorkflowCommand::Resume { selector })
        }
        "cancel" => {
            let selector = optional_selector(&mut parts)?;
            no_trailing(parts, InteractiveWorkflowCommand::Cancel { selector })
        }
        "integrate" => {
            let selector = optional_selector(&mut parts)?;
            no_trailing(parts, InteractiveWorkflowCommand::Integrate { selector })
        }
        "remove" => {
            let selector = optional_selector(&mut parts)?;
            no_trailing(parts, InteractiveWorkflowCommand::Remove { selector })
        }
        other => bail!("unknown workflow subcommand {other:?}; usage: {WORKFLOW_USAGE}"),
    }
}

fn optional_selector<'a>(
    parts: &mut impl Iterator<Item = &'a str>,
) -> Result<Option<WorkflowSelector>> {
    Ok(parts.next().map(|value| WorkflowSelector(value.to_owned())))
}

fn no_trailing<'a>(
    mut parts: impl Iterator<Item = &'a str>,
    command: InteractiveWorkflowCommand,
) -> Result<InteractiveWorkflowCommand> {
    if let Some(extra) = parts.next() {
        bail!("unexpected argument {extra:?}; usage: {WORKFLOW_USAGE}");
    }
    Ok(command)
}

fn parse_create<'a>(parts: impl Iterator<Item = &'a str>) -> Result<InteractiveWorkflowCommand> {
    let arguments = parts.collect::<Vec<_>>();
    let (name, objective) = match arguments.as_slice() {
        [] => bail!("workflow create requires <objective> or <name> <objective>"),
        [objective] => ((*objective).to_owned(), (*objective).to_owned()),
        [name, objectives @ ..] => ((*name).to_owned(), objectives.join(" ")),
    };
    let name = name.trim();
    if name.is_empty() {
        bail!("workflow name must not be empty");
    }
    let objective = objective.trim();
    if objective.is_empty() {
        bail!("workflow objective must not be empty");
    }
    Ok(InteractiveWorkflowCommand::Create {
        name: name.to_owned(),
        objective: objective.to_owned(),
    })
}

/// Execute a typed workflow command against a runtime port.
pub async fn execute_interactive_workflow_command(
    port: &impl WorkflowCommandPort,
    command: InteractiveWorkflowCommand,
) -> Result<WorkflowCommandEffect> {
    match command {
        InteractiveWorkflowCommand::OpenPage => Ok(WorkflowCommandEffect::OpenPage),
        InteractiveWorkflowCommand::List => {
            let items = port.list().await?;
            Ok(WorkflowCommandEffect::Message(format_workflow_list(&items)))
        }
        InteractiveWorkflowCommand::Show { selector } => {
            let summary = match selector {
                Some(selector) => port.get(selector.as_str()).await?,
                None => port
                    .selected()
                    .await?
                    .ok_or_else(|| anyhow!("no workflow selected; pass id or name"))?,
            };
            Ok(WorkflowCommandEffect::Message(format_workflow_detail(
                &summary,
            )))
        }
        InteractiveWorkflowCommand::Create { name, objective } => {
            let summary = port.create(name, objective).await?;
            Ok(WorkflowCommandEffect::Message(format_workflow_detail(
                &summary,
            )))
        }
        InteractiveWorkflowCommand::Pause { selector } => {
            let summary = port.pause(selector_str(selector.as_ref())).await?;
            Ok(WorkflowCommandEffect::Message(format_workflow_summary(
                &summary,
            )))
        }
        InteractiveWorkflowCommand::Resume { selector } => {
            let summary = port.resume(selector_str(selector.as_ref())).await?;
            Ok(WorkflowCommandEffect::Message(format_workflow_summary(
                &summary,
            )))
        }
        InteractiveWorkflowCommand::Cancel { selector } => {
            let summary = port.cancel(selector_str(selector.as_ref())).await?;
            Ok(WorkflowCommandEffect::Message(format_workflow_summary(
                &summary,
            )))
        }
        InteractiveWorkflowCommand::Integrate { selector } => {
            let summary = port.integrate(selector_str(selector.as_ref())).await?;
            Ok(WorkflowCommandEffect::Message(format_workflow_summary(
                &summary,
            )))
        }
        InteractiveWorkflowCommand::Remove { selector } => {
            let summary = port.remove(selector_str(selector.as_ref())).await?;
            Ok(WorkflowCommandEffect::Message(format!(
                "removed {}",
                format_workflow_summary(&summary)
            )))
        }
    }
}

/// Execute `/workflow` against the Application-bound manager.
pub async fn execute_interactive_workflow_on_application(
    application: &Application,
    command: InteractiveWorkflowCommand,
) -> Result<WorkflowCommandEffect> {
    let port = application_workflow_port(application)?;
    execute_interactive_workflow_command(&port, command).await
}

fn selector_str(selector: Option<&WorkflowSelector>) -> Option<&str> {
    selector.map(WorkflowSelector::as_str)
}

/// Compact conversation-header line: `Workflows · A active · T total`.
#[must_use]
pub fn format_workflows_header(active: usize, total: usize) -> String {
    format!("Workflows · {active} active · {total} total")
}

/// Count active workflows for the compact header.
#[must_use]
pub fn count_active_workflows(items: &[WorkflowSummary]) -> usize {
    items
        .iter()
        .filter(|item| item.status.is_active())
        .count()
}

#[must_use]
pub fn format_workflow_list(items: &[WorkflowSummary]) -> String {
    if items.is_empty() {
        return "No workflows.".to_owned();
    }
    let active = count_active_workflows(items);
    let mut lines = vec![format_workflows_header(active, items.len())];
    for item in items {
        lines.push(format_workflow_summary(item));
    }
    lines.join("\n")
}

#[must_use]
pub fn format_workflow_summary(item: &WorkflowSummary) -> String {
    format!(
        "{} · {} · {} · {}",
        item.status.as_str(),
        item.name,
        item.id.as_str(),
        item.objective
    )
}

#[must_use]
pub fn format_workflow_detail(item: &WorkflowSummary) -> String {
    let mut lines = vec![
        format!("Name: {}", item.name),
        format!("Id: {}", item.id.as_str()),
        format!("Status: {}", item.status.as_str()),
        format!("Objective: {}", item.objective),
        format!("Generation: {}", item.generation),
    ];
    if let Some(path) = item
        .worktree_path
        .as_deref()
        .and_then(display_worktree_label)
    {
        lines.push(format!("Worktree: {path}"));
    }
    if let Some(branch) = &item.branch {
        lines.push(format!("Branch: {branch}"));
    }
    if let Some(supervisor) = &item.supervisor_agent_id {
        lines.push(format!("Supervisor: {supervisor}"));
    }
    if let Some(failure) = &item.failure {
        lines.push(format!("Failure: {failure}"));
    }
    if let Some(integration) = &item.integration {
        lines.push(format!("Integration: {integration}"));
    }
    lines.join("\n")
}

/// Display label for a worktree path: never an absolute filesystem path.
#[must_use]
pub fn display_worktree_label(path: &str) -> Option<String> {
    crate::workflow_rpc::redact_worktree_path(path)
}

#[cfg(test)]
mod tests;
