use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use futures_util::FutureExt;
use parking_lot::Mutex;
use pi_agent::{AbortController, ThinkingLevel, ToolCallContext};
use pi_ai::{AssistantMessage, AssistantMessageEvent, Context, Model, StopReason};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, AgentRuntimeSettings, ChildSessionFactory,
    OrchestrationConfig, OrchestrationRuntime, OrchestrationSkill, SelectorSettings, Session,
    SessionOptions, SkillSource, TaskSpawn,
};
use serde_json::{Value, json};

fn model() -> Model {
    Model {
        id: "routing-model".to_owned(),
        name: "Routing model".to_owned(),
        api: "routing-api".to_owned(),
        provider: "routing-provider".to_owned(),
        ..Model::default()
    }
}

fn agent(name: &str, description: &str, prompt: &str, autoload_skills: &[&str]) -> AgentDefinition {
    AgentDefinition {
        name: name.to_owned(),
        description: description.to_owned(),
        system_prompt: prompt.to_owned(),
        tools: Some(Vec::new()),
        autoload_skills: autoload_skills.iter().map(|name| (*name).to_owned()).collect(),
        model: None,
        thinking_level: Some(ThinkingLevel::Off),
        source: AgentDefinitionSource::User,
        path: None,
        trusted: true,
    }
}

fn skill(root: &std::path::Path, name: &str, description: &str, body: &str) -> OrchestrationSkill {
    let base_dir = root.join(name);
    std::fs::create_dir_all(&base_dir).expect("skill directory");
    let file_path = base_dir.join("SKILL.md");
    std::fs::write(&file_path, format!("---\nname: {name}\ndescription: {description}\n---\n{body}"))
        .expect("skill file");
    OrchestrationSkill {
        name: name.to_owned(),
        description: description.to_owned(),
        file_path,
        base_dir,
        globs: Vec::new(),
        always_apply: false,
        hidden: false,
        disable_model_invocation: false,
        source: SkillSource::User,
        trusted: true,
    }
}

fn selector_settings() -> SelectorSettings {
    SelectorSettings {
        min_score: 0,
        autoload_threshold: 1,
        auto_select_threshold: 1,
        confidence_margin: 0,
        ..SelectorSettings::default()
    }
}

fn recording_factory(contexts: Arc<Mutex<Vec<Context>>>) -> ChildSessionFactory {
    Arc::new(move |request| {
        let contexts = contexts.clone();
        Box::pin(async move {
            let stream: pi_agent::StreamFn = Arc::new(move |model, context, _| {
                let contexts = contexts.clone();
                async move {
                    contexts.lock().push(context);
                    let events = pi_ai::new_assistant_message_event_stream();
                    let writer = events.clone();
                    tokio::spawn(async move {
                        let mut message = AssistantMessage::pending(&model);
                        message.stop_reason = StopReason::Stop;
                        writer.push(AssistantMessageEvent::Done { reason: StopReason::Stop, message: message.clone() }).await;
                        writer.end(Some(message)).await;
                    });
                    events
                }
                .boxed()
            });
            Session::new(SessionOptions {
                model: request.model,
                cwd: std::env::current_dir().expect("current directory"),
                system_prompt: request.system_prompt,
                thinking_level: request.thinking_level.unwrap_or(ThinkingLevel::Off),
                api_key: "routing-key".to_owned(),
                compaction: None,
                stream_options: Default::default(),
                tools: Some(request.orchestration_tools),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: Some(stream),
                auth_resolver: None,
            })
        })
    })
}

fn runtime(
    artifacts: &std::path::Path,
    skills: Vec<OrchestrationSkill>,
    agent_settings: BTreeMap<String, AgentRuntimeSettings>,
    contexts: Arc<Mutex<Vec<Context>>>,
) -> OrchestrationRuntime {
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![
            agent("reviewer", "Review Rust security vulnerabilities and patches", "REVIEWER_PROMPT", &["rust-review"]),
            agent("writer", "Write release documentation and migration guides", "WRITER_PROMPT", &["docs-writing"]),
        ]),
        artifacts,
    )
    .with_selector_settings(selector_settings());
    config.default_agent = "writer".to_owned();
    config.skills = skills;
    config.agent_settings = agent_settings;
    config.parent_model = model();
    OrchestrationRuntime::new(config, recording_factory(contexts)).expect("runtime")
}

fn tool<'a>(tools: &'a [pi_agent::AgentTool], name: &str) -> &'a pi_agent::AgentTool {
    tools.iter().find(|candidate| candidate.name == name).unwrap_or_else(|| panic!("missing tool {name}"))
}

fn context(arguments: Value) -> ToolCallContext {
    let (_, abort) = AbortController::new();
    ToolCallContext { tool_call_id: "routing-contract".to_owned(), arguments, on_update: Arc::new(|_| {}), abort, model: None }
}

async fn wait_for_spawn(runtime: &OrchestrationRuntime, spawn: &TaskSpawn) -> Result<pi_coding::JobSnapshot> {
    let jobs = runtime.wait_jobs(std::slice::from_ref(&spawn.job_id), Some(std::time::Duration::from_secs(5)), None).await?;
    Ok(jobs.into_iter().next().expect("settled job"))
}

#[tokio::test]
async fn task_routes_user_text_to_enabled_agent_and_injects_competing_skills_once() -> Result<()> {
    let root = tempfile::tempdir()?;
    let skills = vec![
        skill(root.path(), "rust-review", "Review Rust code for memory safety", "RUST_REVIEW_BODY"),
        skill(root.path(), "security-audit", "Audit security vulnerabilities", "SECURITY_AUDIT_BODY"),
        skill(root.path(), "docs-writing", "Write release documentation", "DOCS_WRITING_BODY"),
    ];
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let runtime = runtime(root.path(), skills, BTreeMap::new(), contexts.clone());
    let tools = runtime.agent_tools("Main", 0);
    let task = tool(&tools, "task");
    assert!(task.description.contains("reviewer —"), "{}", task.description);
    assert!(task.description.contains("writer —"), "{}", task.description);

    let result = (task.execute)(context(json!({ "name": "ReviewChild", "task": "Review Rust security vulnerabilities in this patch" }))).await?;
    let spawns: Vec<TaskSpawn> = serde_json::from_value(result.details)?;
    assert_eq!(spawns.len(), 1);
    assert_eq!(spawns[0].agent, "reviewer");
    assert_eq!(wait_for_spawn(&runtime, &spawns[0]).await?.agent, "reviewer");

    let captured = contexts.lock();
    assert_eq!(captured.len(), 1);
    let prompt = &captured[0].system_prompt;
    assert!(prompt.starts_with("REVIEWER_PROMPT"), "{prompt}");
    assert!(!prompt.contains("WRITER_PROMPT"), "{prompt}");
    assert_eq!(prompt.matches("RUST_REVIEW_BODY").count(), 1, "{prompt}");
    assert_eq!(prompt.matches("SECURITY_AUDIT_BODY").count(), 1, "{prompt}");
    assert_eq!(prompt.matches("DOCS_WRITING_BODY").count(), 0, "{prompt}");
    assert_eq!(prompt.matches("<autoloaded_skill name=\"rust-review\"").count(), 1);
    assert_eq!(prompt.matches("<autoloaded_skill name=\"security-audit\"").count(), 1);
    assert!(prompt.contains("<location>skill://rust-review</location>"));
    assert!(prompt.contains("<location>skill://security-audit</location>"));
    assert_eq!(captured[0].tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(), ["task", "hub"]);
    drop(captured);
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn disabled_agents_are_not_advertised_or_auto_selected_and_explicit_enabled_choice_wins() -> Result<()> {
    let root = tempfile::tempdir()?;
    let skills = vec![
        skill(root.path(), "rust-review", "Review Rust code for memory safety", "RUST_REVIEW_BODY"),
        skill(root.path(), "docs-writing", "Write release documentation", "DOCS_WRITING_BODY"),
    ];
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let mut settings = BTreeMap::new();
    settings.insert("reviewer".to_owned(), AgentRuntimeSettings { enabled: Some(false), model: None,
                tools: None,
            });
    let runtime = runtime(root.path(), skills, settings, contexts.clone());
    let tools = runtime.agent_tools("Main", 0);
    let task = tool(&tools, "task");
    assert!(!task.description.contains("reviewer —"), "{}", task.description);
    assert!(task.description.contains("writer —"), "{}", task.description);
    assert_eq!(runtime.select_agent("review Rust security vulnerabilities", None), "writer");
    assert_eq!(runtime.select_agent("review Rust security vulnerabilities", Some("writer")), "writer");

    let error = (task.execute)(context(json!({ "name": "DisabledChild", "agent": "reviewer", "task": "review Rust security vulnerabilities" }))).await.expect_err("disabled explicit agent must fail");
    let message = error.to_string();
    assert!(message.contains("reviewer") && message.contains("disabled"), "{message}");

    let result = (task.execute)(context(json!({ "name": "WriterChild", "agent": "writer", "task": "review Rust security vulnerabilities" }))).await?;
    let spawns: Vec<TaskSpawn> = serde_json::from_value(result.details)?;
    assert_eq!(spawns[0].agent, "writer");
    wait_for_spawn(&runtime, &spawns[0]).await?;
    let captured = contexts.lock();
    assert_eq!(captured.len(), 1);
    assert!(captured[0].system_prompt.starts_with("WRITER_PROMPT"));
    drop(captured);
    runtime.shutdown().await;
    Ok(())
}

#[test]
fn missing_ambiguous_and_untrusted_skills_fail_actionably() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut missing_config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![agent("reviewer", "Review Rust code", "REVIEWER_PROMPT", &["missing-skill"])]),
        root.path(),
    );
    missing_config.default_agent = "reviewer".to_owned();
    let missing = OrchestrationRuntime::new(missing_config, Arc::new(|_| Box::pin(async { unreachable!() })))
        .err()
        .expect("missing skill must fail")
        .to_string();
    assert!(missing.contains("missing-skill") && missing.contains("undiscovered"), "{missing}");

    let first = skill(root.path(), "duplicate", "First duplicate", "FIRST_BODY");
    let mut second = first.clone();
    second.file_path = root.path().join("other/SKILL.md");
    second.base_dir = root.path().join("other");
    std::fs::create_dir_all(&second.base_dir)?;
    std::fs::write(&second.file_path, "SECOND_BODY")?;
    let mut ambiguous_config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![agent("reviewer", "Review Rust code", "REVIEWER_PROMPT", &[])]),
        root.path(),
    );
    ambiguous_config.default_agent = "reviewer".to_owned();
    ambiguous_config.skills = vec![first, second];
    let ambiguous = OrchestrationRuntime::new(ambiguous_config, Arc::new(|_| Box::pin(async { unreachable!() })))
        .err()
        .expect("duplicate skill names must fail")
        .to_string();
    assert!(ambiguous.contains("duplicate") && ambiguous.contains("ambiguous"), "{ambiguous}");
    assert!(ambiguous.contains("unique name"), "{ambiguous}");

    let mut untrusted = skill(root.path(), "untrusted", "Untrusted project skill", "SECRET_BODY");
    untrusted.source = SkillSource::Project;
    untrusted.trusted = false;
    let mut untrusted_config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![agent("reviewer", "Review Rust code", "REVIEWER_PROMPT", &[])]),
        root.path(),
    );
    untrusted_config.default_agent = "reviewer".to_owned();
    untrusted_config.skills = vec![untrusted];
    let untrusted_error = OrchestrationRuntime::new(untrusted_config, Arc::new(|_| Box::pin(async { unreachable!() })))
        .err()
        .expect("untrusted skill must fail")
        .to_string();
    assert!(untrusted_error.contains("untrusted") && untrusted_error.contains("not trusted"), "{untrusted_error}");
    Ok(())
}
