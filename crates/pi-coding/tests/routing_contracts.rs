use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures_util::FutureExt;
use parking_lot::Mutex;
use pi_agent::{AbortController, ThinkingLevel, ToolCallContext};
use pi_ai::{AssistantMessage, AssistantMessageEvent, Context, Model, StopReason};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, AgentRuntimeSettings, AgentStatus,
    Application, ChildSessionFactory, JobStatus, OrchestrationConfig, OrchestrationEvent,
    OrchestrationRuntime, OrchestrationSkill, ResourceManager, ResourceManagerOptions,
    SelectorSettings, Session, SessionOptions, SkillSource, TaskSpawn,
};
use serde_json::{Value, json};

const DANGER_SETTINGS: &str = r#"{"sandbox":{"mode":"danger-full-access"}}"#;
const AUTO_ROUTE_SETTINGS: &str = r#"{
    "sandbox":{"mode":"danger-full-access"},
    "orchestration":{"tasks":true},
    "selector":{"autoMode":"auto"}
}"#;

async fn attach_resources_with_settings(
    session: &Session,
    cwd: &std::path::Path,
    settings: &str,
) -> Result<tempfile::TempDir> {
    let agent_dir = tempfile::tempdir()?;
    std::fs::write(agent_dir.path().join("settings.json"), settings)?;
    let mut options = ResourceManagerOptions::new(cwd);
    options.agent_dir = agent_dir.path().to_path_buf();
    options.project_trust_override = Some(true);
    options.disable_extensions = true;
    options.disable_skills = true;
    options.disable_prompt_templates = true;
    options.disable_themes = true;
    options.disable_context_files = true;
    session
        .attach_resources(ResourceManager::new(options)?)
        .await?;
    session.set_session_dir(agent_dir.path().join("sessions"));
    session.start_new_recording()?;
    Ok(agent_dir)
}

async fn attach_hermetic_resources(
    session: &Session,
    cwd: &std::path::Path,
) -> Result<tempfile::TempDir> {
    attach_resources_with_settings(session, cwd, DANGER_SETTINGS).await
}

fn model() -> Model {
    Model {
        id: "routing-model".to_owned(),
        name: "Routing model".to_owned(),
        api: pi_ai::API_FAUX.to_owned(),
        provider: "faux".to_owned(),
        ..Model::default()
    }
}

fn agent(name: &str, description: &str, prompt: &str, autoload_skills: &[&str]) -> AgentDefinition {
    AgentDefinition {
        name: name.to_owned(),
        description: description.to_owned(),
        system_prompt: prompt.to_owned(),
        tools: Some(Vec::new()),
        autoload_skills: autoload_skills
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
        model: None,
        thinking_level: Some(ThinkingLevel::Off),
        max_turns: None,
        max_tool_calls: None,
        timeout_secs: None,
        disallowed_tools: Vec::new(),
        capability_ceiling: None,
        source: AgentDefinitionSource::User,
        path: None,
        trusted: true,
        kind: pi_coding::AgentDefinitionKind::Agent,
        personality: None,
        soft_budget: None,
        knowledge_roots: Vec::new(),
        user_proxy: false,
    }
}

fn skill(root: &std::path::Path, name: &str, description: &str, body: &str) -> OrchestrationSkill {
    let base_dir = root.join(name);
    std::fs::create_dir_all(&base_dir).expect("skill directory");
    let file_path = base_dir.join("SKILL.md");
    std::fs::write(
        &file_path,
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
    )
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
        managed_project_boundary: None,
        managed_root: Some(root.to_path_buf()),
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
                        writer
                            .push(AssistantMessageEvent::Done {
                                reason: StopReason::Stop,
                                message: message.clone(),
                            })
                            .await;
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
                auth_manager: None,
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
            agent(
                "reviewer",
                "Review Rust security vulnerabilities and patches",
                "REVIEWER_PROMPT",
                &["rust-review"],
            ),
            agent(
                "writer",
                "Write release documentation and migration guides",
                "WRITER_PROMPT",
                &["docs-writing"],
            ),
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

fn runtime_with_agents(
    artifacts: &std::path::Path,
    agents: Vec<AgentDefinition>,
) -> OrchestrationRuntime {
    let mut config = OrchestrationConfig::new(AgentCatalog::from_agents(agents), artifacts)
        .with_selector_settings(selector_settings());
    config.default_agent = config.catalog.agents()[0].name.clone();
    config.parent_model = model();
    OrchestrationRuntime::new(config, Arc::new(|_| Box::pin(async { unreachable!() })))
        .expect("runtime")
}

fn tool<'a>(tools: &'a [pi_agent::AgentTool], name: &str) -> &'a pi_agent::AgentTool {
    tools
        .iter()
        .find(|candidate| candidate.name == name)
        .unwrap_or_else(|| panic!("missing tool {name}"))
}

fn context(arguments: Value) -> ToolCallContext {
    let (_, abort) = AbortController::new();
    ToolCallContext {
        tool_call_id: "routing-contract".to_owned(),
        arguments,
        on_update: Arc::new(|_| {}),
        abort,
        model: None,
        approval_token: None,
    }
}

async fn wait_for_spawn(
    runtime: &OrchestrationRuntime,
    spawn: &TaskSpawn,
) -> Result<pi_coding::JobSnapshot> {
    let jobs = runtime
        .wait_jobs(
            std::slice::from_ref(&spawn.job_id),
            Some(std::time::Duration::from_secs(5)),
            None,
        )
        .await?;
    Ok(jobs.into_iter().next().expect("settled job"))
}

#[tokio::test]
async fn task_routes_user_text_to_enabled_agent_and_injects_competing_skills_once() -> Result<()> {
    let root = tempfile::tempdir()?;
    let skills = vec![
        skill(
            root.path(),
            "rust-review",
            "Review Rust code for memory safety",
            "RUST_REVIEW_BODY",
        ),
        skill(
            root.path(),
            "security-audit",
            "Audit security vulnerabilities",
            "SECURITY_AUDIT_BODY",
        ),
        skill(
            root.path(),
            "docs-writing",
            "Write release documentation",
            "DOCS_WRITING_BODY",
        ),
    ];
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let runtime = runtime(root.path(), skills, BTreeMap::new(), contexts.clone());
    let tools = runtime.agent_tools("Main", 0);
    let task = tool(&tools, "task");
    assert!(
        task.description.contains("reviewer —"),
        "{}",
        task.description
    );
    assert!(
        task.description.contains("writer —"),
        "{}",
        task.description
    );

    let result = (task.execute)(context(json!({ "name": "ReviewChild", "task": "Review Rust security vulnerabilities in this patch" }))).await?;
    let spawns: Vec<TaskSpawn> = serde_json::from_value(result.details)?;
    assert_eq!(spawns.len(), 1);
    assert_eq!(spawns[0].agent, "reviewer");
    assert_eq!(
        wait_for_spawn(&runtime, &spawns[0]).await?.agent,
        "reviewer"
    );

    let captured = contexts.lock();
    assert_eq!(captured.len(), 1);
    let prompt = &captured[0].system_prompt;
    assert!(prompt.starts_with("REVIEWER_PROMPT"), "{prompt}");
    assert!(!prompt.contains("WRITER_PROMPT"), "{prompt}");
    assert_eq!(prompt.matches("RUST_REVIEW_BODY").count(), 1, "{prompt}");
    assert_eq!(prompt.matches("SECURITY_AUDIT_BODY").count(), 1, "{prompt}");
    assert_eq!(prompt.matches("DOCS_WRITING_BODY").count(), 0, "{prompt}");
    assert_eq!(
        prompt
            .matches("<autoloaded_skill name=\"rust-review\"")
            .count(),
        1
    );
    assert_eq!(
        prompt
            .matches("<autoloaded_skill name=\"security-audit\"")
            .count(),
        1
    );
    assert!(prompt.contains("<location>skill://rust-review</location>"));
    assert!(prompt.contains("<location>skill://security-audit</location>"));
    assert_eq!(
        captured[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["task", "hub"]
    );
    drop(captured);
    runtime.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn disabled_agents_are_not_advertised_or_auto_selected_and_explicit_enabled_choice_wins()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let skills = vec![
        skill(
            root.path(),
            "rust-review",
            "Review Rust code for memory safety",
            "RUST_REVIEW_BODY",
        ),
        skill(
            root.path(),
            "docs-writing",
            "Write release documentation",
            "DOCS_WRITING_BODY",
        ),
    ];
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let mut settings = BTreeMap::new();
    settings.insert(
        "reviewer".to_owned(),
        AgentRuntimeSettings {
            enabled: Some(false),
            model: None,
            tools: None,
        },
    );
    let runtime = runtime(root.path(), skills, settings, contexts.clone());
    let tools = runtime.agent_tools("Main", 0);
    let task = tool(&tools, "task");
    assert!(
        !task.description.contains("reviewer —"),
        "{}",
        task.description
    );
    assert!(
        task.description.contains("writer —"),
        "{}",
        task.description
    );
    assert_eq!(
        runtime.select_agent("review Rust security vulnerabilities", None),
        "writer"
    );
    assert_eq!(
        runtime.select_agent("review Rust security vulnerabilities", Some("writer")),
        "writer"
    );

    let error = (task.execute)(context(json!({ "name": "DisabledChild", "agent": "reviewer", "task": "review Rust security vulnerabilities" }))).await.expect_err("disabled explicit agent must fail");
    let message = error.to_string();
    assert!(
        message.contains("reviewer") && message.contains("disabled"),
        "{message}"
    );

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

#[tokio::test]
async fn exact_agent_mentions_route_before_overlapping_skills_and_ambiguous_mentions_fail()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let research = skill(
        root.path(),
        "research",
        "Research topics for a researcher study",
        "RESEARCH_BODY",
    );
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![
            agent(
                "researcher",
                "Research and study assigned topics",
                "RESEARCHER_PROMPT",
                &[],
            ),
            agent("writer", "Write assigned content", "WRITER_PROMPT", &[]),
        ]),
        root.path(),
    )
    .with_selector_settings(selector_settings());
    config.default_agent = "writer".to_owned();
    config.skills = vec![research];
    config.parent_model = model();
    let runtime = OrchestrationRuntime::new(config, recording_factory(contexts.clone()))?;
    assert_eq!(
        runtime.select_agent("Have researcher study this", None),
        "researcher"
    );
    assert_eq!(
        runtime.select_agent("Use research for this", None),
        "writer",
        "generic skill text must not force the overlapping agent"
    );

    let mut events = runtime.subscribe();
    let task = tool(&runtime.agent_tools("Main", 0), "task").clone();
    let result = (task.execute)(context(json!({
        "name": "ResearchChild",
        "task": "Have researcher study this"
    })))
    .await?;
    let spawns: Vec<TaskSpawn> = serde_json::from_value(result.details)?;
    assert_eq!(spawns.len(), 1);
    assert_eq!(spawns[0].agent, "researcher");
    assert_eq!(spawns[0].agent_id, "ResearchChild");

    let mut saw_agent = false;
    let mut saw_job = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while !(saw_agent && saw_job) && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Ok(OrchestrationEvent::AgentUpdated { agent, .. }))
                if agent.id == "ResearchChild" =>
            {
                assert_eq!(agent.display_name, "researcher");
                assert!(matches!(
                    agent.status,
                    AgentStatus::Queued
                        | AgentStatus::Running
                        | AgentStatus::Idle
                        | AgentStatus::Parked
                ));
                saw_agent = true;
            }
            Ok(Ok(OrchestrationEvent::JobUpdated { job, .. })) if job.id == spawns[0].job_id => {
                assert_eq!(job.agent, "researcher");
                assert_eq!(job.agent_id, "ResearchChild");
                assert!(matches!(
                    job.status,
                    JobStatus::Queued | JobStatus::Running | JobStatus::Completed
                ));
                saw_job = true;
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }
    assert!(saw_agent, "exact mention spawn must publish AgentUpdated");
    assert!(saw_job, "exact mention spawn must publish JobUpdated");

    wait_for_spawn(&runtime, &spawns[0]).await?;
    let captured = contexts.lock();
    assert_eq!(captured.len(), 1);
    assert!(captured[0].system_prompt.starts_with("RESEARCHER_PROMPT"));
    assert!(
        !captured[0].system_prompt.contains("RESEARCH_BODY"),
        "exact agent mention must not autoload the overlapping skill body"
    );
    drop(captured);
    runtime.shutdown().await;

    Ok(())
}

#[test]
fn missing_ambiguous_and_untrusted_skills_fail_actionably() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut missing_config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![agent(
            "reviewer",
            "Review Rust code",
            "REVIEWER_PROMPT",
            &["missing-skill"],
        )]),
        root.path(),
    );
    missing_config.default_agent = "reviewer".to_owned();
    let missing = OrchestrationRuntime::new(
        missing_config,
        Arc::new(|_| Box::pin(async { unreachable!() })),
    )
    .err()
    .expect("missing skill must fail")
    .to_string();
    assert!(
        missing.contains("missing-skill") && missing.contains("undiscovered"),
        "{missing}"
    );

    let first = skill(root.path(), "duplicate", "First duplicate", "FIRST_BODY");
    let mut second = first.clone();
    second.file_path = root.path().join("other/SKILL.md");
    second.base_dir = root.path().join("other");
    std::fs::create_dir_all(&second.base_dir)?;
    std::fs::write(&second.file_path, "SECOND_BODY")?;
    let mut ambiguous_config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![agent(
            "reviewer",
            "Review Rust code",
            "REVIEWER_PROMPT",
            &[],
        )]),
        root.path(),
    );
    ambiguous_config.default_agent = "reviewer".to_owned();
    ambiguous_config.skills = vec![first, second];
    let ambiguous = OrchestrationRuntime::new(
        ambiguous_config,
        Arc::new(|_| Box::pin(async { unreachable!() })),
    )
    .err()
    .expect("duplicate skill names must fail")
    .to_string();
    assert!(
        ambiguous.contains("duplicate") && ambiguous.contains("ambiguous"),
        "{ambiguous}"
    );
    assert!(ambiguous.contains("unique name"), "{ambiguous}");

    let mut untrusted = skill(
        root.path(),
        "untrusted",
        "Untrusted project skill",
        "SECRET_BODY",
    );
    untrusted.source = SkillSource::Project;
    untrusted.trusted = false;
    let mut untrusted_config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![agent(
            "reviewer",
            "Review Rust code",
            "REVIEWER_PROMPT",
            &[],
        )]),
        root.path(),
    );
    untrusted_config.default_agent = "reviewer".to_owned();
    untrusted_config.skills = vec![untrusted];
    let untrusted_error = OrchestrationRuntime::new(
        untrusted_config,
        Arc::new(|_| Box::pin(async { unreachable!() })),
    )
    .err()
    .expect("untrusted skill must fail")
    .to_string();
    assert!(
        untrusted_error.contains("untrusted") && untrusted_error.contains("not trusted"),
        "{untrusted_error}"
    );
    Ok(())
}

/// The workflow catalog diagnostics fail actionably when an
/// objective explicitly delegates to an agent that is absent or disabled in
/// the catalog (never a silent fallback to `task`), while present agents and
/// skill invocations pass.
#[tokio::test]
async fn workflow_catalog_diagnostics_fail_for_missing_and_disabled_agents() -> Result<()> {
    let root = tempfile::tempdir()?;
    let research = skill(
        root.path(),
        "research",
        "Research topics for a researcher study",
        "RESEARCH_BODY",
    );
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![
            agent("researcher", "Research topics", "RESEARCHER_PROMPT", &[]),
            agent("writer", "Write content", "WRITER_PROMPT", &[]),
            agent("cj", "Handle concise delegated work", "CJ_PROMPT", &[]),
        ]),
        root.path(),
    );
    config.default_agent = "writer".to_owned();
    config.skills = vec![research];
    config.parent_model = model();
    let runtime =
        OrchestrationRuntime::new(config, Arc::new(|_| Box::pin(async { unreachable!() })))?;

    // Present agent + action clause: valid.
    runtime
        .validate_delegation_agents("你让researcher仔细调研pi-coding-agent")
        .expect("a present exact agent must validate");
    // English delegation to a present agent: valid.
    runtime
        .validate_delegation_agents("Have researcher study this")
        .expect("an English delegation to a present agent must validate");
    runtime
        .validate_delegation_agents("Ask researcher to study this")
        .expect("a configured agent after ask must validate");
    runtime
        .validate_delegation_agents("Delegate to researcher to study this")
        .expect("delegate may take the optional to particle");
    runtime
        .validate_delegation_agents("让cj处理")
        .expect("a configured short CJK delegation target must validate");
    runtime
        .validate_delegation_agents("Ask (researcher) to study this")
        .expect("punctuation around a configured target must preserve its identity");
    // Skill invocation is not an agent delegation: valid.
    runtime
        .validate_delegation_agents("请使用research技能")
        .expect("a skill invocation must not be flagged as a missing agent");
    // No delegation construction: valid.
    runtime
        .validate_delegation_agents("researcher 是做什么的？")
        .expect("an informational mention must validate");
    runtime
        .validate_delegation_agents("仔细调研pi-coding-agent")
        .expect("a delegation-free objective must validate");
    // Ordinary instruction verbs never fabricate missing agents, even when
    // their object looks like an identifier or a common word.
    runtime
        .validate_delegation_agents("run python3 --version")
        .expect("an ordinary run instruction must validate");
    runtime
        .validate_delegation_agents("run ghost-agent now")
        .expect("a standalone unknown weak-verb object must remain ordinary");
    runtime
        .validate_delegation_agents("run content through the formatter")
        .expect("agent description words must not become configured identities");
    runtime
        .validate_delegation_agents("make sure the workflow passes")
        .expect("a make-sure instruction must validate");
    runtime
        .validate_delegation_agents("make sure integration stays clean")
        .expect("a make-sure instruction must validate");
    runtime
        .validate_delegation_agents("ensure the build is green")
        .expect("an ensure instruction must validate");
    runtime
        .validate_delegation_agents("check that tests pass")
        .expect("a check instruction must validate");
    // Positive nearby control: the same ordinary verb DOES validate when the
    // object names an actual configured agent or skill.
    runtime
        .validate_delegation_agents("run researcher to investigate the pi-coding-agent")
        .expect("a run instruction naming a configured agent must validate");
    runtime
        .validate_delegation_agents("make researcher handle the analysis")
        .expect("a make instruction naming a configured agent must validate");
    runtime
        .validate_delegation_agents("run research across the codebase")
        .expect("an instruction naming a configured skill must validate");
    // Missing agent named by a CJK delegation: actionable failure.
    let error = runtime
        .validate_delegation_agents("你让ghost-agent仔细调研pi-coding-agent")
        .expect_err("a missing explicit agent must fail actionably")
        .to_string();
    assert!(error.contains("ghost-agent"), "{error}");
    assert!(error.contains("not defined"), "{error}");
    assert!(error.contains("~/.rpi/agents"), "{error}");
    // Missing agent named by an English delegation: actionable failure.
    let error = runtime
        .validate_delegation_agents("Have ghost-agent study this")
        .expect_err("a missing explicit English agent must fail actionably")
        .to_string();
    assert!(
        error.contains("ghost-agent") && error.contains("not defined"),
        "{error}"
    );
    let error = runtime
        .validate_delegation_agents("Delegate to ghost-agent to study this")
        .expect_err("delegate to a missing agent must diagnose the target, not to")
        .to_string();
    assert!(
        error.contains("ghost-agent") && error.contains("not defined"),
        "{error}"
    );
    assert!(!error.contains("agent(s) \"to\""), "{error}");
    let error = runtime
        .validate_delegation_agents("Delegate to (ghost-agent) to study this")
        .expect_err("punctuation around a missing target must preserve the target")
        .to_string();
    assert!(
        error.contains("ghost-agent") && error.contains("not defined"),
        "{error}"
    );

    // Plural CJK delegation: EVERY named agent is reported, never just the
    // first (the second name is joined by a conjunction, not an action
    // clause).
    let error = runtime
        .validate_delegation_agents("你让ghost-one和ghost-two一起调研pi-coding-agent")
        .expect_err("every missing explicit agent must be reported")
        .to_string();
    assert!(error.contains("ghost-one"), "{error}");
    assert!(error.contains("ghost-two"), "{error}");
    assert!(error.contains("not defined"), "{error}");
    assert!(error.contains("~/.rpi/agents"), "{error}");
    let error = runtime
        .validate_delegation_agents("让ghost-agent处理")
        .expect_err("a missing short CJK delegation target must fail actionably")
        .to_string();
    assert!(
        error.contains("ghost-agent") && error.contains("not defined"),
        "{error}"
    );

    // Plural English delegation: the conjunction chain reports EVERY named
    // agent too, and stops before the action clause.
    let error = runtime
        .validate_delegation_agents("Have ghost-one and ghost-two study this")
        .expect_err("every missing explicit English agent must be reported")
        .to_string();
    assert!(error.contains("ghost-one"), "{error}");
    assert!(error.contains("ghost-two"), "{error}");
    assert!(error.contains("not defined"), "{error}");

    // A comma/conjunction list is parsed completely. For a weak verb, one
    // configured agent anchors the list and every missing member on either
    // side is still diagnosed.
    let error = runtime
        .validate_delegation_agents("run ghost-one, writer and ghost-two now")
        .expect_err("an anchored weak-verb list must report every missing target")
        .to_string();
    assert!(error.contains("ghost-one"), "{error}");
    assert!(error.contains("ghost-two"), "{error}");
    assert!(error.contains("not defined"), "{error}");

    // An exact configured skill identity also anchors the complete weak list,
    // while the skill itself remains exempt from agent diagnostics.
    let error = runtime
        .validate_delegation_agents("run ghost-one, research and ghost-two now")
        .expect_err("an exact skill identity must anchor all missing co-targets")
        .to_string();
    assert!(error.contains("ghost-one"), "{error}");
    assert!(error.contains("ghost-two"), "{error}");
    assert!(!error.contains("agent(s) \"research\""), "{error}");
    // Disabled agent named by a delegation: actionable failure.
    let mut disabled_settings = BTreeMap::new();
    disabled_settings.insert(
        "researcher".to_owned(),
        AgentRuntimeSettings {
            enabled: Some(false),
            model: None,
            tools: None,
        },
    );
    let mut disabled_config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![
            agent("researcher", "Research topics", "RESEARCHER_PROMPT", &[]),
            agent("writer", "Write content", "WRITER_PROMPT", &[]),
        ]),
        root.path(),
    )
    .with_agent_settings(disabled_settings);
    disabled_config.default_agent = "writer".to_owned();
    disabled_config.parent_model = model();
    let disabled_runtime = OrchestrationRuntime::new(
        disabled_config,
        Arc::new(|_| Box::pin(async { unreachable!() })),
    )?;
    let error = disabled_runtime
        .validate_delegation_agents("你让researcher仔细调研")
        .expect_err("a disabled explicit agent must fail actionably")
        .to_string();
    assert!(
        error.contains("researcher") && error.contains("disabled"),
        "{error}"
    );
    // A weak verb naming an exact configured agent is observable here: the
    // validator must recognize the target and report that it is disabled.
    let error = disabled_runtime
        .validate_delegation_agents("run researcher to investigate this")
        .expect_err("a configured weak-verb target must reach disabled validation")
        .to_string();
    assert!(
        error.contains("researcher") && error.contains("disabled"),
        "{error}"
    );
    // Mixed disabled/absent delegation: BOTH conditions are reported.
    let error = disabled_runtime
        .validate_delegation_agents("Have researcher and ghost-agent study this")
        .expect_err("a mixed disabled/absent delegation must report both")
        .to_string();
    assert!(
        error.contains("researcher") && error.contains("disabled"),
        "{error}"
    );
    assert!(
        error.contains("ghost-agent") && error.contains("not defined"),
        "{error}"
    );
    runtime.shutdown().await;
    disabled_runtime.shutdown().await;
    Ok(())
}
