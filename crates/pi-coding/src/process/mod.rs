mod backend;
mod log;
mod manager;
mod tool;

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use log::{ProcessLogChunk, ProcessLogs};
pub use manager::ProcessManager;
pub use tool::process_tool;

pub const DEFAULT_PROCESS_OUTPUT_BYTES: usize = 1024 * 1024;
pub const DEFAULT_MAX_PROCESSES: usize = 16;
pub const DEFAULT_LOG_READ_BYTES: usize = 256 * 1024;
pub const MAX_PROCESS_LABEL_BYTES: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProcessId(String);

impl ProcessId {
    fn generate() -> Self {
        Self(Uuid::now_v7().to_string())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProcessId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProcessOwnerId(String);

impl ProcessOwnerId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn generate() -> Self {
        Self(Uuid::now_v7().to_string())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProcessOwnerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    Starting,
    Running,
    Stopping,
    Exited,
    TimedOut,
    Expired,
    Failed,
}

impl ProcessState {
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Exited | Self::TimedOut | Self::Expired | Self::Failed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStream {
    Stdout,
    Stderr,
    Combined,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ProcessSignal {
    Sigint,
    Sigterm,
    Sighup,
    Sigquit,
    Sigkill,
}

impl Default for ProcessSignal {
    fn default() -> Self {
        Self::Sigterm
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ProcessKey {
    Enter,
    Tab,
    Escape,
    CtrlC,
    CtrlD,
    Up,
    Down,
    Left,
    Right,
}

impl ProcessKey {
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Enter => b"\r",
            Self::Tab => b"\t",
            Self::Escape => b"\x1b",
            Self::CtrlC => b"\x03",
            Self::CtrlD => b"\x04",
            Self::Up => b"\x1b[A",
            Self::Down => b"\x1b[B",
            Self::Right => b"\x1b[C",
            Self::Left => b"\x1b[D",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessTerminalSize {
    pub rows: u16,
    pub cols: u16,
}

impl Default for ProcessTerminalSize {
    fn default() -> Self {
        Self { rows: 24, cols: 80 }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessSpawnSpec {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub env: BTreeMap<String, Option<String>>,
    #[serde(default)]
    pub tty: bool,
    #[serde(default)]
    pub terminal_size: Option<ProcessTerminalSize>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub output_bytes: Option<usize>,
}

impl fmt::Debug for ProcessSpawnSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessSpawnSpec")
            .field("argc", &self.argv.len())
            .field("cwd", &self.cwd)
            .field("environment_entry_count", &self.env.len())
            .field("tty", &self.tty)
            .field("terminal_size", &self.terminal_size)
            .field("label", &self.label)
            .field("timeout_ms", &self.timeout_ms)
            .field("output_bytes", &self.output_bytes)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessInfo {
    pub id: ProcessId,
    pub owner_id: ProcessOwnerId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub state: ProcessState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub tty: bool,
    pub started_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exited_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub output_start_cursor: u64,
    pub output_cursor: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum ProcessEvent {
    ProcessStarted {
        process: ProcessInfo,
    },
    ProcessOutput {
        id: ProcessId,
        owner_id: ProcessOwnerId,
        stream: ProcessStream,
        start_cursor: u64,
        cursor: u64,
        data_base64: String,
    },
    ProcessExited {
        process: ProcessInfo,
    },
}

#[derive(Clone, Debug)]
pub struct ProcessManagerConfig {
    pub max_processes: usize,
    pub max_output_bytes: usize,
    pub idle_timeout: Option<Duration>,
    pub idle_scan_interval: Duration,
    pub terminate_grace: Duration,
}

impl Default for ProcessManagerConfig {
    fn default() -> Self {
        Self {
            max_processes: DEFAULT_MAX_PROCESSES,
            max_output_bytes: DEFAULT_PROCESS_OUTPUT_BYTES,
            idle_timeout: Some(Duration::from_secs(30 * 60)),
            idle_scan_interval: Duration::from_secs(30),
            terminate_grace: Duration::from_secs(1),
        }
    }
}
