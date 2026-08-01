//! Durable, session-scoped goal state machine.
//!
//! Goals are deliberately separate from todos, plans, scheduled loops, and
//! conversation messages. A goal describes why future turns may continue; this
//! module only records state and returns a continuation decision. It never
//! starts a turn.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{SessionRecorder, SessionTree};

/// Custom session entry type used by the append-only goal journal.
pub const GOAL_SESSION_CUSTOM_TYPE: &str = "pi.goal.event";
/// Current typed payload version for [`GOAL_SESSION_CUSTOM_TYPE`].
pub const GOAL_SESSION_ENTRY_VERSION: u32 = 1;
/// Maximum UTF-8 objective size. Goal events store both the transition and its
/// resulting snapshot, so this leaves ample headroom under the 8 MiB session
/// record limit.
pub const MAX_GOAL_OBJECTIVE_BYTES: usize = 64 * 1024;


/// The public lifecycle of a session goal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalLifecycle {
    Active,
    Paused,
    Completed,
    Dropped,
}

impl GoalLifecycle {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Dropped)
    }
}

/// Why an otherwise unfinished goal is paused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalPauseReason {
    Manual,
    BudgetExhausted,
    ResumeSafety,
}

/// Cumulative usage charged to a goal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalUsage {
    pub tokens_used: u64,
    pub active_time_seconds: u64,
}

/// A monotonic usage increment reported after work has run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalUsageDelta {
    pub tokens: u64,
    pub active_time_seconds: u64,
}

impl GoalUsageDelta {
    #[must_use]
    pub const fn new(tokens: u64, active_time_seconds: u64) -> Self {
        Self {
            tokens,
            active_time_seconds,
        }
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.tokens == 0 && self.active_time_seconds == 0
    }
}

/// The one current goal owned by a session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Goal {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_goal_id: Option<String>,
    pub objective: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    pub lifecycle: GoalLifecycle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_reason: Option<GoalPauseReason>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub usage: GoalUsage,
}

impl Goal {
    #[must_use]
    pub fn remaining_tokens(&self) -> Option<u64> {
        self.token_budget
            .map(|budget| budget.saturating_sub(self.usage.tokens_used))
    }
}

/// Session-owned goal state. A session has zero or one current goal.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<Goal>,
    pub revision: u64,
}

/// The semantic operation stored in each append-only goal event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GoalEventKind {
    Created,
    ForkCloned { source: Goal },
    Paused { reason: GoalPauseReason },
    Resumed,
    Completed,
    Dropped,
    UsageUpdated { delta: GoalUsageDelta },
}

/// A typed, revisioned goal journal event. `goal` is the resulting snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalEvent {
    pub revision: u64,
    pub timestamp: DateTime<Utc>,
    pub kind: GoalEventKind,
    pub goal: Goal,
}

/// Versioned data stored inside a `custom` session record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalSessionEntry {
    pub version: u32,
    pub event: GoalEvent,
}

/// Pure continuation result for callers that decide whether to start a turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum GoalContinuationDecision {
    NoGoal,
    Continue {
        goal_id: String,
        objective: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remaining_tokens: Option<u64>,
        revision: u64,
    },
    Paused {
        goal_id: String,
        reason: GoalPauseReason,
        revision: u64,
    },
    Terminal {
        goal_id: String,
        lifecycle: GoalLifecycle,
        revision: u64,
    },
}

/// Goal state-machine and journal failures.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GoalError {
    #[error("no current goal")]
    NoCurrentGoal,
    #[error("goal objective must not be empty")]
    EmptyObjective,
    #[error("a current goal already exists")]
    GoalAlreadyExists,
    #[error("goal objective must be at most {max_bytes} UTF-8 bytes")]
    ObjectiveTooLong { max_bytes: usize },
    #[error("goal token budget must be positive")]
    InvalidTokenBudget,
    #[error("goal usage update must charge tokens or active time")]
    EmptyUsageUpdate,
    #[error("goal usage overflow")]
    UsageOverflow,
    #[error("cannot {operation} a goal in the {lifecycle:?} lifecycle")]
    InvalidTransition {
        operation: &'static str,
        lifecycle: GoalLifecycle,
    },
    #[error("cannot resume a goal after its token budget is exhausted")]
    BudgetExhausted,
    #[error("failed to persist goal event: {0}")]
    Persistence(String),
    #[error("invalid goal journal: {0}")]
    InvalidJournal(String),
}

pub type GoalPersistFn = Arc<dyn Fn(&GoalEvent) -> Result<(), GoalError> + Send + Sync>;
pub type GoalClockFn = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Cloneable, linearizable owner of the current goal.
#[derive(Clone)]
pub struct GoalRuntime {
    state: Arc<Mutex<GoalState>>,
    persist: GoalPersistFn,
    clock: GoalClockFn,
}

impl Default for GoalRuntime {
    fn default() -> Self {
        Self::memory()
    }
}

impl GoalRuntime {
    /// Creates an empty, in-memory goal runtime.
    #[must_use]
    pub fn memory() -> Self {
        Self::with_components(
            GoalState::default(),
            Arc::new(|_| Ok(())),
            Arc::new(Utc::now),
        )
    }

    /// Creates an empty runtime with an append callback.
    #[must_use]
    pub fn with_persistence(persist: GoalPersistFn) -> Self {
        Self::with_components(GoalState::default(), persist, Arc::new(Utc::now))
    }

    /// Creates an empty runtime with injectable persistence and time.
    #[must_use]
    pub fn with_clock_and_persistence(clock: GoalClockFn, persist: GoalPersistFn) -> Self {
        Self::with_components(GoalState::default(), persist, clock)
    }

    /// Replays an event sequence into an in-memory runtime.
    pub fn from_events(events: &[GoalEvent]) -> Result<Self, GoalError> {
        Self::from_events_with_components(events, Arc::new(|_| Ok(())), Arc::new(Utc::now))
    }

    /// Restores and durably appends goal events through an existing recorder.
    ///
    /// Each commit compare-and-appends against a fresh recorder topology token.
    /// The goal revision is independently checked against the active branch
    /// while the recorder mutex prevents navigation from redirecting the append.
    pub fn from_session_recorder(recorder: SessionRecorder) -> Result<Self, GoalError> {
        let (tree, _) = recorder
            .tree_with_append_token()
            .map_err(|error| GoalError::Persistence(error.to_string()))?;
        let events = goal_events_from_session_tree(&tree)?;
        let journal = recorder.clone();
        let persist: GoalPersistFn = Arc::new(move |event| {
            let token = journal.append_token();
            let entry = GoalSessionEntry {
                version: GOAL_SESSION_ENTRY_VERSION,
                event: event.clone(),
            };
            journal
                .record_custom_entry_durable_checked(
                    &token,
                    GOAL_SESSION_CUSTOM_TYPE,
                    &entry,
                    |tree| {
                        let mut durable_events = goal_events_from_session_tree(tree)
                            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                        durable_events.push(event.clone());
                        GoalRuntime::from_events(&durable_events)
                            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                        Ok(())
                    },
                )
                .map_err(|error| GoalError::Persistence(error.to_string()))?;
            Ok(())
        });
        Self::from_events_with_components(&events, persist, Arc::new(Utc::now))
    }

    fn from_events_with_components(
        events: &[GoalEvent],
        persist: GoalPersistFn,
        clock: GoalClockFn,
    ) -> Result<Self, GoalError> {
        let mut state = GoalState::default();
        for event in events {
            replay_event(&mut state, event)?;
        }
        Ok(Self::with_components(state, persist, clock))
    }

    fn with_components(state: GoalState, persist: GoalPersistFn, clock: GoalClockFn) -> Self {
        Self {
            state: Arc::new(Mutex::new(state)),
            persist,
            clock,
        }
    }

    /// Returns a consistent snapshot without changing the journal.
    #[must_use]
    pub fn get(&self) -> GoalState {
        self.state.lock().clone()
    }

    /// Creates the session's only goal. Existing goals are never replaced.
    pub fn create(
        &self,
        objective: impl Into<String>,
        token_budget: Option<u64>,
    ) -> Result<Goal, GoalError> {
        let objective = objective.into();
        let objective = objective.trim();
        if objective.is_empty() {
            return Err(GoalError::EmptyObjective);
        }
        if objective.len() > MAX_GOAL_OBJECTIVE_BYTES {
            return Err(GoalError::ObjectiveTooLong {
                max_bytes: MAX_GOAL_OBJECTIVE_BYTES,
            });
        }
        if token_budget == Some(0) {
            return Err(GoalError::InvalidTokenBudget);
        }

        let mut state = self.state.lock();
        if state.current.is_some() {
            return Err(GoalError::GoalAlreadyExists);
        }
        let timestamp = (self.clock)();
        let goal = Goal {
            id: Uuid::now_v7().to_string(),
            origin_goal_id: None,
            objective: objective.to_owned(),
            token_budget,
            lifecycle: GoalLifecycle::Active,
            pause_reason: None,
            created_at: timestamp,
            updated_at: timestamp,
            usage: GoalUsage::default(),
        };
        self.commit(&mut state, GoalEventKind::Created, goal)
    }

    /// Creates the goal owned by a forked session from a source snapshot.
    /// Active work is safety-paused until the resumed session explicitly
    /// continues it; already paused or terminal lifecycle state is preserved.
    pub fn fork_clone(&self, source: &Goal) -> Result<Goal, GoalError> {
        validate_goal(source, 0)?;
        let mut state = self.state.lock();
        if state.current.as_ref().is_some_and(|current| current != source) {
            return Err(GoalError::GoalAlreadyExists);
        }
        let timestamp = (self.clock)();
        let (lifecycle, pause_reason) = if source.lifecycle == GoalLifecycle::Active {
            (GoalLifecycle::Paused, Some(GoalPauseReason::ResumeSafety))
        } else {
            (source.lifecycle, source.pause_reason)
        };
        let goal = Goal {
            id: Uuid::now_v7().to_string(),
            origin_goal_id: Some(
                source
                    .origin_goal_id
                    .clone()
                    .unwrap_or_else(|| source.id.clone()),
            ),
            objective: source.objective.clone(),
            token_budget: source.token_budget,
            lifecycle,
            pause_reason,
            created_at: timestamp,
            updated_at: timestamp,
            usage: source.usage,
        };
        self.commit(
            &mut state,
            GoalEventKind::ForkCloned {
                source: source.clone(),
            },
            goal,
        )
    }

    /// Safety-pauses active work while a session is being resumed. Existing
    /// paused or terminal states are left unchanged.
    pub fn pause_on_resume(&self) -> Result<Goal, GoalError> {
        let mut state = self.state.lock();
        let current = current_goal(&state)?;
        if current.lifecycle != GoalLifecycle::Active {
            return Ok(current.clone());
        }
        let mut goal = current.clone();
        goal.lifecycle = GoalLifecycle::Paused;
        goal.pause_reason = Some(GoalPauseReason::ResumeSafety);
        goal.updated_at = self.next_timestamp(goal.updated_at);
        self.commit(
            &mut state,
            GoalEventKind::Paused {
                reason: GoalPauseReason::ResumeSafety,
            },
            goal,
        )
    }

    /// Pauses active work. Repeating the same transition is idempotent.
    pub fn pause(&self) -> Result<Goal, GoalError> {
        let mut state = self.state.lock();
        let current = current_goal(&state)?;
        match current.lifecycle {
            GoalLifecycle::Active => {
                let mut goal = current.clone();
                goal.lifecycle = GoalLifecycle::Paused;
                goal.pause_reason = Some(GoalPauseReason::Manual);
                goal.updated_at = self.next_timestamp(goal.updated_at);
                self.commit(
                    &mut state,
                    GoalEventKind::Paused {
                        reason: GoalPauseReason::Manual,
                    },
                    goal,
                )
            }
            GoalLifecycle::Paused => Ok(current.clone()),
            lifecycle => Err(GoalError::InvalidTransition {
                operation: "pause",
                lifecycle,
            }),
        }
    }

    /// Resumes a manually paused goal. An exhausted immutable budget cannot be
    /// resumed, and repeating an active transition is idempotent.
    pub fn resume(&self) -> Result<Goal, GoalError> {
        let mut state = self.state.lock();
        let current = current_goal(&state)?;
        match current.lifecycle {
            GoalLifecycle::Active => Ok(current.clone()),
            GoalLifecycle::Paused
                if current.pause_reason == Some(GoalPauseReason::BudgetExhausted) =>
            {
                Err(GoalError::BudgetExhausted)
            }
            GoalLifecycle::Paused => {
                let mut goal = current.clone();
                goal.lifecycle = GoalLifecycle::Active;
                goal.pause_reason = None;
                goal.updated_at = self.next_timestamp(goal.updated_at);
                self.commit(&mut state, GoalEventKind::Resumed, goal)
            }
            lifecycle => Err(GoalError::InvalidTransition {
                operation: "resume",
                lifecycle,
            }),
        }
    }

    /// Marks an active or paused goal completed. Completion is explicit and is
    /// never inferred from budget exhaustion, process exit, or empty output.
    pub fn complete(&self) -> Result<Goal, GoalError> {
        let mut state = self.state.lock();
        let current = current_goal(&state)?;
        match current.lifecycle {
            GoalLifecycle::Completed => Ok(current.clone()),
            GoalLifecycle::Dropped => Err(GoalError::InvalidTransition {
                operation: "complete",
                lifecycle: GoalLifecycle::Dropped,
            }),
            GoalLifecycle::Active | GoalLifecycle::Paused => {
                let mut goal = current.clone();
                goal.lifecycle = GoalLifecycle::Completed;
                goal.pause_reason = None;
                goal.updated_at = self.next_timestamp(goal.updated_at);
                self.commit(&mut state, GoalEventKind::Completed, goal)
            }
        }
    }

    /// Permanently drops an active or paused goal.
    pub fn drop(&self) -> Result<Goal, GoalError> {
        let mut state = self.state.lock();
        let current = current_goal(&state)?;
        match current.lifecycle {
            GoalLifecycle::Dropped => Ok(current.clone()),
            GoalLifecycle::Completed => Err(GoalError::InvalidTransition {
                operation: "drop",
                lifecycle: GoalLifecycle::Completed,
            }),
            GoalLifecycle::Active | GoalLifecycle::Paused => {
                let mut goal = current.clone();
                goal.lifecycle = GoalLifecycle::Dropped;
                goal.pause_reason = None;
                goal.updated_at = self.next_timestamp(goal.updated_at);
                self.commit(&mut state, GoalEventKind::Dropped, goal)
            }
        }
    }

    /// Charges monotonic usage. Crossing the budget changes `active` to
    /// `paused(budget_exhausted)`; it never changes the goal to `completed`.
    pub fn update_usage(&self, delta: GoalUsageDelta) -> Result<Goal, GoalError> {
        if delta.is_empty() {
            return Err(GoalError::EmptyUsageUpdate);
        }
        let mut state = self.state.lock();
        let current = current_goal(&state)?;
        let mut goal = current.clone();
        goal.usage.tokens_used = goal
            .usage
            .tokens_used
            .checked_add(delta.tokens)
            .ok_or(GoalError::UsageOverflow)?;
        goal.usage.active_time_seconds = goal
            .usage
            .active_time_seconds
            .checked_add(delta.active_time_seconds)
            .ok_or(GoalError::UsageOverflow)?;
        if !goal.lifecycle.is_terminal()
            && goal
                .token_budget
                .is_some_and(|budget| goal.usage.tokens_used >= budget)
        {
            goal.lifecycle = GoalLifecycle::Paused;
            goal.pause_reason = Some(GoalPauseReason::BudgetExhausted);
        }
        goal.updated_at = self.next_timestamp(goal.updated_at);
        self.commit(
            &mut state,
            GoalEventKind::UsageUpdated { delta },
            goal,
        )
    }

    /// Computes the next action without queuing, starting, or mutating a turn.
    #[must_use]
    pub fn continuation_decision(&self) -> GoalContinuationDecision {
        let state = self.state.lock();
        let Some(goal) = &state.current else {
            return GoalContinuationDecision::NoGoal;
        };
        match goal.lifecycle {
            GoalLifecycle::Active => GoalContinuationDecision::Continue {
                goal_id: goal.id.clone(),
                objective: goal.objective.clone(),
                remaining_tokens: goal.remaining_tokens(),
                revision: state.revision,
            },
            GoalLifecycle::Paused => GoalContinuationDecision::Paused {
                goal_id: goal.id.clone(),
                reason: goal.pause_reason.unwrap_or(GoalPauseReason::Manual),
                revision: state.revision,
            },
            lifecycle => GoalContinuationDecision::Terminal {
                goal_id: goal.id.clone(),
                lifecycle,
                revision: state.revision,
            },
        }
    }

    fn next_timestamp(&self, previous: DateTime<Utc>) -> DateTime<Utc> {
        (self.clock)().max(previous)
    }

    fn commit(
        &self,
        state: &mut GoalState,
        kind: GoalEventKind,
        goal: Goal,
    ) -> Result<Goal, GoalError> {
        let revision = state
            .revision
            .checked_add(1)
            .ok_or(GoalError::UsageOverflow)?;
        let event = GoalEvent {
            revision,
            timestamp: goal.updated_at,
            kind,
            goal: goal.clone(),
        };
        (self.persist)(&event)?;
        state.revision = revision;
        state.current = Some(goal.clone());
        Ok(goal)
    }
}

/// Decodes goal events on the active session branch. Unrelated custom entries
/// are ignored; malformed or future goal entries fail closed.
pub fn goal_events_from_session_tree(tree: &SessionTree) -> Result<Vec<GoalEvent>, GoalError> {
    tree.branch(None)
        .into_iter()
        .filter(|entry| {
            entry.entry_type == "custom"
                && entry.custom_type.as_deref() == Some(GOAL_SESSION_CUSTOM_TYPE)
        })
        .map(|entry| {
            let data = entry.data.clone().ok_or_else(|| {
                GoalError::InvalidJournal(format!("goal entry {} has no data", entry.id))
            })?;
            let persisted: GoalSessionEntry = serde_json::from_value(data).map_err(|error| {
                GoalError::InvalidJournal(format!(
                    "goal entry {} cannot be decoded: {error}",
                    entry.id
                ))
            })?;
            if persisted.version != GOAL_SESSION_ENTRY_VERSION {
                return Err(GoalError::InvalidJournal(format!(
                    "goal entry {} has unsupported version {}",
                    entry.id, persisted.version
                )));
            }
            Ok(persisted.event)
        })
        .collect()
}


fn current_goal(state: &GoalState) -> Result<&Goal, GoalError> {
    state.current.as_ref().ok_or(GoalError::NoCurrentGoal)
}

fn replay_event(state: &mut GoalState, event: &GoalEvent) -> Result<(), GoalError> {
    let expected_revision = state
        .revision
        .checked_add(1)
        .ok_or_else(|| GoalError::InvalidJournal("revision overflow".to_owned()))?;
    if event.revision != expected_revision {
        return Err(GoalError::InvalidJournal(format!(
            "expected revision {expected_revision}, found {}",
            event.revision
        )));
    }
    if event.timestamp != event.goal.updated_at {
        return Err(GoalError::InvalidJournal(format!(
            "revision {} timestamp does not match its goal snapshot",
            event.revision
        )));
    }
    validate_goal(&event.goal, event.revision)?;

    match (&state.current, &event.kind) {
        (None, GoalEventKind::Created) => {
            if event.goal.origin_goal_id.is_some()
                || event.goal.lifecycle != GoalLifecycle::Active
                || event.goal.pause_reason.is_some()
                || event.goal.usage != GoalUsage::default()
                || event.goal.created_at != event.goal.updated_at
            {
                return Err(invalid_event(event, "invalid created snapshot"));
            }
        }
        (None, GoalEventKind::ForkCloned { source }) => {
            validate_fork_cloned(source, event)?;
        }
        (None, _) => return Err(invalid_event(event, "first event is not created")),
        (Some(previous), GoalEventKind::ForkCloned { source }) => {
            if previous != source {
                return Err(invalid_event(event, "fork clone source does not match current goal"));
            }
            validate_fork_cloned(source, event)?;
        }
        (Some(_), GoalEventKind::Created) => {
            return Err(invalid_event(event, "created event replaces an existing goal"));
        }
        (Some(previous), kind) => validate_replayed_transition(previous, event, kind)?,
    }

    state.revision = event.revision;
    state.current = Some(event.goal.clone());
    Ok(())
}

fn validate_goal(goal: &Goal, revision: u64) -> Result<(), GoalError> {
    if goal.id.is_empty() {
        return Err(GoalError::InvalidJournal(format!(
            "revision {revision} has an empty goal id"
        )));
    }
    if goal.origin_goal_id.as_deref().is_some_and(|origin| origin.is_empty() || origin == goal.id) {
        return Err(GoalError::InvalidJournal(format!(
            "revision {revision} has an invalid origin goal id"
        )));
    }
    if goal.objective.trim().is_empty() || goal.objective.trim() != goal.objective {
        return Err(GoalError::InvalidJournal(format!(
            "revision {revision} has an invalid objective"
        )));
    }
    if goal.objective.len() > MAX_GOAL_OBJECTIVE_BYTES {
        return Err(GoalError::InvalidJournal(format!(
            "revision {revision} objective exceeds {MAX_GOAL_OBJECTIVE_BYTES} UTF-8 bytes"
        )));
    }
    if goal.token_budget == Some(0) {
        return Err(GoalError::InvalidJournal(format!(
            "revision {revision} has a zero token budget"
        )));
    }
    if goal.updated_at < goal.created_at {
        return Err(GoalError::InvalidJournal(format!(
            "revision {revision} moves time before goal creation"
        )));
    }
    match goal.lifecycle {
        GoalLifecycle::Active if goal.pause_reason.is_some() => {
            return Err(GoalError::InvalidJournal(format!(
                "revision {revision} has a pause reason while active"
            )));
        }
        GoalLifecycle::Paused if goal.pause_reason.is_none() => {
            return Err(GoalError::InvalidJournal(format!(
                "revision {revision} is paused without a reason"
            )));
        }
        GoalLifecycle::Completed | GoalLifecycle::Dropped if goal.pause_reason.is_some() => {
            return Err(GoalError::InvalidJournal(format!(
                "revision {revision} has a pause reason after termination"
            )));
        }
        _ => {}
    }
    if goal.lifecycle == GoalLifecycle::Active
        && goal
            .token_budget
            .is_some_and(|budget| goal.usage.tokens_used >= budget)
    {
        return Err(GoalError::InvalidJournal(format!(
            "revision {revision} is active after exhausting its budget"
        )));
    }
    if goal.pause_reason == Some(GoalPauseReason::BudgetExhausted)
        && !goal
            .token_budget
            .is_some_and(|budget| goal.usage.tokens_used >= budget)
    {
        return Err(GoalError::InvalidJournal(format!(
            "revision {revision} claims budget exhaustion before the budget"
        )));
    }
    Ok(())
}

fn validate_fork_cloned(source: &Goal, event: &GoalEvent) -> Result<(), GoalError> {
    validate_goal(source, event.revision)?;
    let goal = &event.goal;
    let expected_origin = source
        .origin_goal_id
        .as_ref()
        .unwrap_or(&source.id);
    let (expected_lifecycle, expected_pause_reason) =
        if source.lifecycle == GoalLifecycle::Active {
            (GoalLifecycle::Paused, Some(GoalPauseReason::ResumeSafety))
        } else {
            (source.lifecycle, source.pause_reason)
        };
    if goal.id == source.id
        || goal.origin_goal_id.as_ref() != Some(expected_origin)
        || goal.objective != source.objective
        || goal.token_budget != source.token_budget
        || goal.lifecycle != expected_lifecycle
        || goal.pause_reason != expected_pause_reason
        || goal.usage != source.usage
        || goal.created_at != goal.updated_at
    {
        return Err(invalid_event(event, "invalid fork clone snapshot"));
    }
    Ok(())
}

fn validate_replayed_transition(
    previous: &Goal,
    event: &GoalEvent,
    kind: &GoalEventKind,
) -> Result<(), GoalError> {
    let goal = &event.goal;
    if goal.id != previous.id
        || goal.origin_goal_id != previous.origin_goal_id
        || goal.objective != previous.objective
        || goal.token_budget != previous.token_budget
        || goal.created_at != previous.created_at
        || goal.updated_at < previous.updated_at
    {
        return Err(invalid_event(event, "immutable goal fields changed"));
    }
    if previous.lifecycle.is_terminal() && !matches!(kind, GoalEventKind::UsageUpdated { .. }) {
        return Err(invalid_event(event, "event follows a terminal lifecycle"));
    }

    match kind {
        GoalEventKind::Paused { reason } => {
            if previous.lifecycle != GoalLifecycle::Active
                || goal.lifecycle != GoalLifecycle::Paused
                || goal.pause_reason != Some(*reason)
                || goal.usage != previous.usage
            {
                return Err(invalid_event(event, "invalid pause transition"));
            }
        }
        GoalEventKind::Resumed => {
            if previous.lifecycle != GoalLifecycle::Paused
                || previous.pause_reason == Some(GoalPauseReason::BudgetExhausted)
                || goal.lifecycle != GoalLifecycle::Active
                || goal.pause_reason.is_some()
                || goal.usage != previous.usage
            {
                return Err(invalid_event(event, "invalid resume transition"));
            }
        }
        GoalEventKind::Completed => {
            if goal.lifecycle != GoalLifecycle::Completed
                || goal.pause_reason.is_some()
                || goal.usage != previous.usage
            {
                return Err(invalid_event(event, "invalid completion transition"));
            }
        }
        GoalEventKind::Dropped => {
            if goal.lifecycle != GoalLifecycle::Dropped
                || goal.pause_reason.is_some()
                || goal.usage != previous.usage
            {
                return Err(invalid_event(event, "invalid drop transition"));
            }
        }
        GoalEventKind::UsageUpdated { delta } => {
            let expected_tokens = previous
                .usage
                .tokens_used
                .checked_add(delta.tokens)
                .ok_or_else(|| invalid_event(event, "token usage overflow"))?;
            let expected_time = previous
                .usage
                .active_time_seconds
                .checked_add(delta.active_time_seconds)
                .ok_or_else(|| invalid_event(event, "active time overflow"))?;
            if delta.is_empty()
                || goal.usage.tokens_used != expected_tokens
                || goal.usage.active_time_seconds != expected_time
            {
                return Err(invalid_event(event, "invalid usage transition"));
            }
            let exhausted = goal
                .token_budget
                .is_some_and(|budget| goal.usage.tokens_used >= budget);
            if previous.lifecycle.is_terminal() {
                if goal.lifecycle != previous.lifecycle
                    || goal.pause_reason != previous.pause_reason
                {
                    return Err(invalid_event(event, "terminal usage changed the lifecycle"));
                }
            } else if exhausted {
                if goal.lifecycle != GoalLifecycle::Paused
                    || goal.pause_reason != Some(GoalPauseReason::BudgetExhausted)
                {
                    return Err(invalid_event(event, "exhausted usage did not pause"));
                }
            } else if goal.lifecycle != previous.lifecycle
                || goal.pause_reason != previous.pause_reason
            {
                return Err(invalid_event(event, "usage changed the lifecycle"));
            }
        }
        GoalEventKind::Created | GoalEventKind::ForkCloned { .. } => {
            unreachable!("creation events handled before transition validation")
        }
    }
    Ok(())
}

fn invalid_event(event: &GoalEvent, reason: &str) -> GoalError {
    GoalError::InvalidJournal(format!("revision {}: {reason}", event.revision))
}
