//! Canonical workflow domain types and durable workflow management.

use std::fmt;

use serde::{Deserialize, Serialize};

mod detail;
pub mod manager;
mod store;
pub mod supervisor;

pub use detail::*;
pub use manager::*;
pub use supervisor::*;

/// Opaque stable identity for one workflow.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkflowId(String);

impl WorkflowId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkflowId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<String> for WorkflowId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for WorkflowId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

/// Durable lifecycle state of a workflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStatus {
    Queued,
    Planning,
    Running,
    Paused,
    Integrating,
    Completed,
    Failed,
    Cancelled,
    Conflicted,
}

impl WorkflowStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Planning => "planning",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Integrating => "integrating",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Conflicted => "conflicted",
        }
    }

    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Planning | Self::Running | Self::Paused | Self::Integrating
        )
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Conflicted
        )
    }
}

impl fmt::Display for WorkflowStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Explicit ownership metadata attached to work delegated by a workflow.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowTaskOwnership {
    pub workflow_id: String,
    pub todo_task_id: String,
    pub generation: u64,
}
