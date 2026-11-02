mod agent_loader_convert;
pub mod application;
mod ask;
pub mod auth;
pub mod collab;
pub mod dependency_map;
pub mod encrypt;
pub mod export;
mod fs;
pub mod share;
pub use fs::{FsError, FsOperation, SessionFileIdentity};
pub mod compaction;
pub mod extensions;
pub mod goal;
pub mod handoff;
pub mod hooks;
pub mod import;
pub mod isolate;
pub mod live;
pub mod llama;
pub mod loop_scheduler;
pub mod markdown;
pub(crate) mod mcp;
pub mod mcp_import;
pub mod memory;
pub mod oauth;
pub mod operator_http;
pub mod orchestration;
pub mod packages;
pub(crate) mod persona_skills;
pub mod plugin;
pub mod process;
pub mod prompt_templates;
pub mod quickjs_host;
pub mod redact;
pub mod resolve;
pub mod resource_manager;
pub mod resources;
pub mod retry_fallback;
pub mod sandbox;
pub mod sdk;
pub mod selector;
pub mod self_update_http;
pub mod session;
pub mod session_catalog;
pub mod session_store;
pub mod session_migration;
pub mod settings;
mod settings_watch;
pub mod settings_catalog;
pub mod system_prompt;
pub mod todo;
pub mod tool_presentation;
pub mod tools;
pub mod truncate;
pub mod trust;
pub mod workflow;
pub mod workflow_worktree;
pub mod workspace;
pub(crate) use ask::AskRuntime;
pub use auth::*;
pub use export::*;
pub use memory::MemoryConfig;
pub use oauth::*;
pub use pi_sandbox::HostHttpClass;
pub use share::*;

/// Whether an application error represents a sandboxed system operation whose target was absent.
#[must_use]
pub fn is_sandbox_not_found(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<pi_sandbox::SandboxError>(),
        Some(pi_sandbox::SandboxError::System {
            kind: pi_sandbox::SandboxIoKind::NotFound,
            ..
        })
    )
}

pub use application::*;
pub use compaction::*;
pub use extensions::*;
pub use goal::*;
pub use handoff::*;
pub use hooks::*;
pub use import::*;
pub use isolate::*;
pub use llama::*;
pub use loop_scheduler::*;
pub use markdown::*;
pub use orchestration::*;
pub use packages::*;
pub use persona_skills::{PersonaSkill, PersonaSkillStore};
pub use plugin::*;
pub use process::*;
pub use prompt_templates::*;
pub use quickjs_host::*;
pub use resolve::*;
pub use resource_manager::*;
pub use resources::*;
pub use retry_fallback::*;
pub use sandbox::*;
pub use sdk::*;
pub use selector::*;
pub use session::*;
pub use session_catalog::*;
pub use session_store::*;
pub use settings::*;
pub use settings_catalog::*;
pub use system_prompt::*;
pub use todo::*;
pub use tool_presentation::*;
pub use tools::*;
pub use truncate::*;
pub use trust::*;
pub use workflow::*;
pub use workspace::*;
