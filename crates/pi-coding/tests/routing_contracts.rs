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
    AgentDefinition { name: name.to_owned(),
    description: description.to_owned(),
    system_prompt: prompt.to_owned(),
    tools: Some(Vec::new()),
    autoload_skills: autoload_skills.iter().map(|name| (*name).to_owned()).collect(),
    model: None,
    thinking_level: Some(ThinkingLevel::Off),
    max_turns: None,
    max_tool_calls: None,
    timeout_secs: None,
    disallowed_tools: Vec::new(),
    capability_ceiling: None,
    source: AgentDefinitionSource::User,
    path: None, trusted: true, kind: pi_coding::AgentDefinitionKind::Agent, personality: None, soft_budget: None }
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

#[tokio::test]
async fn exact_agent_mentions_route_before_overlapping_skills_and_ambiguous_mentions_fail() -> Result<()> {
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
    assert_eq!(runtime.select_agent("Have researcher study this", None), "researcher");
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
                    AgentStatus::Queued | AgentStatus::Running | AgentStatus::Idle | AgentStatus::Parked
                ));
                saw_agent = true;
            }
            Ok(Ok(OrchestrationEvent::JobUpdated { job, .. }))
                if job.id == spawns[0].job_id =>
            {
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

    // Natural-language spawn API: verb + exact name spawns; skill-only does not.
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
    config.skills = vec![skill(
        root.path(),
        "research",
        "Research topics for a researcher study",
        "RESEARCH_BODY",
    )];
    config.parent_model = model();
    let nl_runtime = OrchestrationRuntime::new(config, recording_factory(contexts.clone()))?;
    let nl_spawns = nl_runtime
        .spawn_from_natural_language("Main", 0, "Have researcher study this")?
        .expect("verb + exact agent must spawn");
    assert_eq!(nl_spawns[0].agent, "researcher");
    assert_eq!(nl_spawns[0].agent_id, "researcher");
    wait_for_spawn(&nl_runtime, &nl_spawns[0]).await?;
    assert!(
        nl_runtime
            .spawn_from_natural_language("Main", 0, "Use research for this")?
            .is_none(),
        "skill-oriented text must remain a recommendation"
    );
    nl_runtime.shutdown().await;

    let ambiguous_runtime = runtime_with_agents(
        root.path(),
        vec![
            agent("Research-Agent", "First researcher", "FIRST_PROMPT", &[]),
            agent("research-agent", "Second researcher", "SECOND_PROMPT", &[]),
        ],
    );
    let ambiguous_task = tool(&ambiguous_runtime.agent_tools("Main", 0), "task").clone();
    let error = (ambiguous_task.execute)(context(json!({
        "task": "Have Research Agent study this"
    })))
    .await
    .expect_err("ambiguous exact agent mention must fail");
    let message = error.to_string();
    assert!(message.contains("ambiguous"), "{message}");
    assert!(message.contains("Research-Agent"), "{message}");
    assert!(message.contains("research-agent"), "{message}");
    assert!(message.contains("task.agent"), "{message}");
    let nl_ambiguous = ambiguous_runtime
        .spawn_from_natural_language("Main", 0, "Have Research Agent study this")
        .expect_err("ambiguous NL spawn must fail");
    assert!(nl_ambiguous.to_string().contains("ambiguous"), "{nl_ambiguous}");
    // Informational collision mentions never error and never spawn: a
    // question verb or a non-delegating verb list is not a delegation.
    for informational in [
        "Tell me whether Research Agent is available",
        "Have me compare Research Agent descriptions",
    ] {
        let mention = ambiguous_runtime
            .spawn_from_natural_language("Main", 0, informational)
            .expect("informational collision mention must not error");
        assert!(mention.is_none(), "{informational:?} must not spawn");
    }
    ambiguous_runtime.shutdown().await;

    // Disabled exact agent mention fails clearly (task tool and NL spawn).
    let mut disabled_settings = BTreeMap::new();
    disabled_settings.insert(
        "researcher".to_owned(),
        AgentRuntimeSettings {
            enabled: Some(false),
            model: None,
            tools: None,
        },
    );
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let mut disabled_config = OrchestrationConfig::new(
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
    .with_selector_settings(selector_settings())
    .with_agent_settings(disabled_settings);
    disabled_config.default_agent = "writer".to_owned();
    disabled_config.parent_model = model();
    let disabled_runtime =
        OrchestrationRuntime::new(disabled_config, recording_factory(contexts))?;
    let disabled_task = tool(&disabled_runtime.agent_tools("Main", 0), "task").clone();
    let disabled_error = (disabled_task.execute)(context(json!({
        "task": "Have researcher study this"
    })))
    .await
    .expect_err("disabled exact agent mention must fail");
    let disabled_message = disabled_error.to_string();
    assert!(
        disabled_message.contains("researcher") && disabled_message.contains("disabled"),
        "{disabled_message}"
    );
    let disabled_nl = disabled_runtime
        .spawn_from_natural_language("Main", 0, "Have researcher study this")
        .expect_err("disabled NL exact mention must fail");
    assert!(
        disabled_nl.to_string().contains("disabled"),
        "{disabled_nl}"
    );
    disabled_runtime.shutdown().await;
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

/// Contract (P0-A): the literal CJK delegation prompt
/// `你让researcher仔细调研pi-coding-agent` spawns the named agent without any
/// English sentinel; the other conservative Chinese constructions
/// (请/叫/派/安排/委托/交给) work too when paired with a unique exact agent
/// name and an action clause. Informational mentions, questions, and skill
/// invocations must NOT spawn.
#[tokio::test]
async fn cjk_delegation_intent_is_unicode_aware_and_negatives_do_not_spawn() -> Result<()> {
    let root = tempfile::tempdir()?;
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
    config.parent_model = model();
    let runtime = OrchestrationRuntime::new(config, recording_factory(contexts))?;

    // The literal prompt needs no English sentinel.
    let spawns = runtime
        .spawn_from_natural_language("Main", 0, "你让researcher仔细调研pi-coding-agent")?
        .expect("the literal CJK delegation prompt must spawn");
    assert_eq!(spawns[0].agent, "researcher");
    assert_eq!(spawns[0].agent_id, "researcher");
    wait_for_spawn(&runtime, &spawns[0]).await?;

    // Every conservative CJK delegation construction spawns when paired with
    // the exact agent name and an action clause.
    for prompt in [
        "请你让researcher去调查这个项目",
        "请researcher写一份调研报告",
        "把这项调研交给researcher完成",
        "安排researcher仔细调研这个仓库",
        "委托researcher调研这个仓库",
        "叫researcher去研究这个bug",
        "派researcher去处理这个任务",
    ] {
        let spawned = runtime
            .spawn_from_natural_language("Main", 0, prompt)?
            .unwrap_or_else(|| panic!("{prompt:?} must spawn"));
        assert_eq!(spawned[0].agent, "researcher", "{prompt}");
        wait_for_spawn(&runtime, &spawned[0]).await?;
    }

    // Informational / question / skill-only mentions must stay
    // recommendations, even though they name the same agents.
    for negative in [
        "researcher 是做什么的？",
        "我在文档里看到researcher",
        "请使用research技能",
        "请 review the security patch",
        "让test跑起来",
    ] {
        assert!(
            runtime
                .spawn_from_natural_language("Main", 0, negative)?
                .is_none(),
            "{negative:?} must not auto-spawn"
        );
    }
    runtime.shutdown().await;
    Ok(())
}

/// Contract (P0-A/P0-C): normalized duplicate agent names fail actionably in
/// the CJK path too, and a disabled agent named through a CJK delegation
/// fails with the disabled diagnostic instead of spawning.
#[tokio::test]
async fn cjk_ambiguity_and_disabled_agents_fail_like_english() -> Result<()> {
    let root = tempfile::tempdir()?;
    let ambiguous_runtime = runtime_with_agents(
        root.path(),
        vec![
            agent("Research-Agent", "First researcher", "FIRST_PROMPT", &[]),
            agent("research-agent", "Second researcher", "SECOND_PROMPT", &[]),
        ],
    );
    let error = ambiguous_runtime
        .spawn_from_natural_language("Main", 0, "你让Research-Agent调研这个")
        .expect_err("ambiguous CJK delegation must fail");
    let message = error.to_string();
    assert!(message.contains("ambiguous"), "{message}");
    assert!(message.contains("Research-Agent"), "{message}");
    assert!(message.contains("research-agent"), "{message}");
    ambiguous_runtime.shutdown().await;

    let mut disabled_settings = BTreeMap::new();
    disabled_settings.insert(
        "researcher".to_owned(),
        AgentRuntimeSettings {
            enabled: Some(false),
            model: None,
            tools: None,
        },
    );
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let mut disabled_config = OrchestrationConfig::new(
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
    .with_selector_settings(selector_settings())
    .with_agent_settings(disabled_settings);
    disabled_config.default_agent = "writer".to_owned();
    disabled_config.parent_model = model();
    let disabled_runtime =
        OrchestrationRuntime::new(disabled_config, recording_factory(contexts))?;
    let error = disabled_runtime
        .spawn_from_natural_language("Main", 0, "你让researcher仔细调研")
        .expect_err("disabled CJK delegation must fail");
    let message = error.to_string();
    assert!(message.contains("researcher") && message.contains("disabled"), "{message}");
    disabled_runtime.shutdown().await;
    Ok(())
}

/// Contract (P0-A multi-target): plural natural-language delegation fans out
/// to EVERY explicitly named trusted agent in one spawn batch, in mention
/// order, for CJK conjunctions without whitespace AND English lists — never a
/// silent default fallback — while single-name prompts keep spawning exactly
/// one agent and partial-word mentions never match.
#[tokio::test]
async fn plural_natural_language_delegation_spawns_every_named_agent_once() -> Result<()> {
    let root = tempfile::tempdir()?;
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![
            agent("glm", "General language model agent", "GLM_PROMPT", &[]),
            agent("grok", "Research assistant agent", "GROK_PROMPT", &[]),
        ]),
        root.path(),
    )
    .with_selector_settings(selector_settings());
    config.default_agent = "glm".to_owned();
    config.parent_model = model();
    let runtime = OrchestrationRuntime::new(config, recording_factory(contexts.clone()))?;

    // CJK conjunction without whitespace: both names spawn once, in mention
    // order, each carrying the full request as its assignment.
    let spawns = runtime
        .spawn_from_natural_language("Main", 0, "你让glm和grok一起调研这个仓库")?
        .expect("the plural CJK delegation must spawn");
    assert_eq!(spawns.len(), 2, "{spawns:?}");
    assert_eq!(spawns[0].agent, "glm", "{spawns:?}");
    assert_eq!(spawns[0].agent_id, "glm", "{spawns:?}");
    assert_eq!(spawns[1].agent, "grok", "{spawns:?}");
    assert_eq!(spawns[1].agent_id, "grok", "{spawns:?}");
    assert_ne!(spawns[0].job_id, spawns[1].job_id, "one job per agent");
    for spawn in &spawns {
        wait_for_spawn(&runtime, spawn).await?;
    }
    {
        let captured = contexts.lock();
        assert_eq!(captured.len(), 2, "each named agent must run exactly once");
        let prompts = captured
            .iter()
            .map(|context| context.system_prompt.as_str())
            .collect::<Vec<_>>();
        assert!(
            prompts.iter().any(|prompt| prompt.starts_with("GLM_PROMPT")),
            "{prompts:?}"
        );
        assert!(
            prompts
                .iter()
                .any(|prompt| prompt.starts_with("GROK_PROMPT")),
            "{prompts:?}"
        );
        assert!(
            prompts
                .iter()
                .all(|prompt| prompt.contains("你让glm和grok一起调研这个仓库")),
            "every spawn must carry the full request as its assignment: {prompts:?}"
        );
    }
    drop(contexts.lock());

    // English plural list: both names spawn in mention order.
    let english = runtime
        .spawn_from_natural_language("Main", 0, "Have glm and grok study this")?
        .expect("the English plural delegation must spawn");
    assert_eq!(english.len(), 2, "{english:?}");
    assert_eq!(english[0].agent, "glm", "{english:?}");
    assert_eq!(english[1].agent, "grok", "{english:?}");
    for spawn in &english {
        wait_for_spawn(&runtime, spawn).await?;
    }

    // Mention order follows the request, not the catalog.
    let reversed = runtime
        .spawn_from_natural_language("Main", 0, "Have grok and glm study this")?
        .expect("mention-order delegation must spawn");
    assert_eq!(reversed.len(), 2, "{reversed:?}");
    assert_eq!(reversed[0].agent, "grok", "{reversed:?}");
    assert_eq!(reversed[1].agent, "glm", "{reversed:?}");

    // Repeated mentions dedupe to one spawn per agent.
    let deduped = runtime
        .spawn_from_natural_language("Main", 0, "Have glm, grok and glm study this")?
        .expect("dedup delegation must spawn");
    assert_eq!(deduped.len(), 2, "{deduped:?}");
    assert_eq!(deduped[0].agent, "glm", "{deduped:?}");
    assert_eq!(deduped[1].agent, "grok", "{deduped:?}");

    // Single-name control: exactly one spawn, unchanged.
    let single = runtime
        .spawn_from_natural_language("Main", 0, "你让grok调研这个仓库")?
        .expect("the single-name CJK delegation must spawn");
    assert_eq!(single.len(), 1, "{single:?}");
    assert_eq!(single[0].agent, "grok", "{single:?}");

    // Partial-word negatives: `glmx` is not a mention, only grok spawns.
    let partial = runtime
        .spawn_from_natural_language("Main", 0, "Have glmx and grok study this")?
        .expect("the valid half of a partial-word prompt must spawn");
    assert_eq!(partial.len(), 1, "{partial:?}");
    assert_eq!(partial[0].agent, "grok", "{partial:?}");

    // No valid mention at all: no spawn (plain parent turn, no default spawn).
    assert!(
        runtime
            .spawn_from_natural_language("Main", 0, "Have glmx and grokx study this")?
            .is_none(),
        "a delegation with no valid mention must not spawn anything"
    );

    // Informational plural mentions stay recommendations.
    assert!(
        runtime
            .spawn_from_natural_language("Main", 0, "glm和grok哪个好？")?
            .is_none(),
        "informational plural mention must not spawn"
    );
    // CJK scope: only mentions inside an explicit delegation clause spawn —
    // a mention in a separate informational clause never rides along.
    let scoped_first = runtime
        .spawn_from_natural_language("Main", 0, "让glm调研；grok是做什么的？")?
        .expect("the delegated mention must spawn");
    assert_eq!(scoped_first.len(), 1, "{scoped_first:?}");
    assert_eq!(scoped_first[0].agent, "glm", "{scoped_first:?}");
    let scoped_second = runtime
        .spawn_from_natural_language("Main", 0, "glm是做什么的？让grok调研这个仓库")?
        .expect("the later delegated mention must spawn");
    assert_eq!(scoped_second.len(), 1, "{scoped_second:?}");
    assert_eq!(scoped_second[0].agent, "grok", "{scoped_second:?}");
    // CJK: an action clause is required AFTER the whole conjunction chain —
    // `你让glm和grok` names the pair but delegates nothing.
    assert!(
        runtime
            .spawn_from_natural_language("Main", 0, "你让glm和grok")?
            .is_none(),
        "a CJK chain without an action clause must not spawn"
    );
    // Negation: instructions NOT to delegate never spawn.
    for negative in [
        "Do not have glm study this",
        "Don't ask glm to review this",
        "不要让glm调研这个仓库",
        "别让glm调研这个仓库",
        "你别让glm调研这个仓库",
        "请别让glm调研这个仓库",
    ] {
        assert!(
            runtime
                .spawn_from_natural_language("Main", 0, negative)?
                .is_none(),
            "{negative:?} must not spawn"
        );
    }
    // `别` inside a word (`特别让…` = "especially let …", `分别让…` =
    // "respectively let …") is not a negation: the delegation still spawns.
    let especially = runtime
        .spawn_from_natural_language("Main", 0, "特别让glm调研这个仓库")?
        .expect("特别 is not a negation marker");
    assert_eq!(especially.len(), 1, "{especially:?}");
    assert_eq!(especially[0].agent, "glm", "{especially:?}");
    let respectively = runtime
        .spawn_from_natural_language("Main", 0, "分别让glm和grok调研这个仓库")?
        .expect("分别 is not a negation marker");
    assert_eq!(respectively.len(), 2, "{respectively:?}");
    assert_eq!(respectively[0].agent, "glm", "{respectively:?}");
    assert_eq!(respectively[1].agent, "grok", "{respectively:?}");
    // NFKC parity: a fullwidth CJK delegation spawns exactly like ASCII.
    let fullwidth = runtime
        .spawn_from_natural_language("Main", 0, "让ｇｌｍ调研这个仓库")?
        .expect("a fullwidth delegation must spawn");
    assert_eq!(fullwidth.len(), 1, "{fullwidth:?}");
    assert_eq!(fullwidth[0].agent, "glm", "{fullwidth:?}");
    runtime.shutdown().await;
    Ok(())
}

/// Contract (P5 authorization): prompt-time auto-spawn passes the SAME
/// before_tool_call gate as the `task` tool. A hook that BLOCKS the
/// synthetic task call suppresses the spawn (the delegation stays a plain
/// parent turn); a hook that REWRITES the task arguments also suppresses it
/// (the modified call belongs to the normal model/tool path). A hook-less
/// session keeps the convenience auto-spawn.
#[tokio::test]
async fn host_before_tool_hook_gates_natural_language_spawn() -> Result<()> {
    let root = tempfile::tempdir()?;
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let config = || {
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![
                agent("glm", "General language model agent", "GLM_PROMPT", &[]),
                agent("grok", "Research assistant agent", "GROK_PROMPT", &[]),
            ]),
            root.path(),
        )
        .with_selector_settings(selector_settings());
        config.default_agent = "glm".to_owned();
        config.parent_model = model();
        config
    };
    let parent_stream: pi_agent::StreamFn = Arc::new(|model, _context, _options| {
        use pi_ai::{AssistantMessage, AssistantMessageEvent, StopReason};
        async move {
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
    let parent_session = |hook: Option<pi_agent::BeforeToolCallFn>| {
        Session::new(SessionOptions {
            model: model(),
            cwd: root.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: pi_agent::ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: hook,
            after_tool_call: None,
            stream_fn: Some(parent_stream.clone()),
            auth_resolver: None,
        })
        .expect("parent session")
    };
    let delegation = "你让glm和grok一起调研这个仓库";

    // A hook that BLOCKS the task tool suppresses the auto-spawn.
    let blocking: pi_agent::BeforeToolCallFn = Arc::new(|context| {
        Box::pin(async move {
            Ok(pi_agent::BeforeToolCallResult {
                block: context.tool_call.name == "task",
                reason: Some("task blocked for test".to_owned()),
                arguments: None,
            })
        })
    });
    let blocking_runtime =
        OrchestrationRuntime::new(config(), recording_factory(contexts.clone()))?;
    let blocked_app =
        Application::new_with_orchestration(parent_session(Some(blocking)), blocking_runtime.clone()).await;
    blocked_app
        .prompt(delegation.to_owned(), Vec::new(), None)
        .await?;
    assert!(
        blocking_runtime.jobs(None).is_empty(),
        "a blocking task hook must suppress the auto-spawn"
    );
    blocked_app.cleanup().await;

    // A hook that REWRITES the task arguments suppresses the auto-spawn too:
    // the modified call is honored exactly once by the normal tool path.
    let rewriting: pi_agent::BeforeToolCallFn = Arc::new(|_context| {
        Box::pin(async move {
            Ok(pi_agent::BeforeToolCallResult {
                block: false,
                reason: None,
                arguments: Some(json!({ "task": "rewritten assignment" })),
            })
        })
    });
    let rewriting_runtime =
        OrchestrationRuntime::new(config(), recording_factory(contexts.clone()))?;
    let rewritten_app = Application::new_with_orchestration(
        parent_session(Some(rewriting)),
        rewriting_runtime.clone(),
    )
    .await;
    rewritten_app
        .prompt(delegation.to_owned(), Vec::new(), None)
        .await?;
    assert!(
        rewriting_runtime.jobs(None).is_empty(),
        "rewritten task arguments must not auto-spawn"
    );
    rewritten_app.cleanup().await;

    // Control: a hook-less session keeps the convenience auto-spawn.
    let plain_runtime = OrchestrationRuntime::new(config(), recording_factory(contexts.clone()))?;
    let plain_app =
        Application::new_with_orchestration(parent_session(None), plain_runtime.clone()).await;
    plain_app
        .prompt(delegation.to_owned(), Vec::new(), None)
        .await?;
    assert_eq!(
        plain_runtime.jobs(None).len(),
        2,
        "a hook-less session must keep the convenience auto-spawn"
    );
    plain_app.cleanup().await;
    Ok(())
}

/// Contract (P5 boundary fidelity): the synthetic authorization call presents
/// the REAL `task` tool boundary — the parent task definition's capability in
/// the hook context and its prepare_arguments' canonical null-filled
/// `{task, name, agent, todoTaskId, context, tasks, outputSchema,
/// schemaMode}` shape — so a hook that keys on that exact shape denies the
/// auto-spawn exactly like a real task call.
#[tokio::test]
async fn synthetic_task_call_presents_canonical_real_tool_boundary() -> Result<()> {
    let root = tempfile::tempdir()?;
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let config = || {
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![
                agent("glm", "General language model agent", "GLM_PROMPT", &[]),
                agent("grok", "Research assistant agent", "GROK_PROMPT", &[]),
            ]),
            root.path(),
        )
        .with_selector_settings(selector_settings());
        config.default_agent = "glm".to_owned();
        config.parent_model = model();
        config
    };
    let stream_fn: pi_agent::StreamFn = Arc::new(|model, _context, _options| {
        use pi_ai::{AssistantMessage, AssistantMessageEvent, StopReason};
        async move {
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
    // Denies ONLY the canonical null-filled task shape with the real Exec
    // capability — anything less (raw `{"task": ...}` or no tool metadata)
    // would not match and the spawn would slip through.
    let canonical_deny: pi_agent::BeforeToolCallFn = Arc::new(|context| {
        let arguments = context.arguments.clone();
        let capability = context
            .context
            .tools
            .iter()
            .find(|tool| tool.name == "task")
            .map(|tool| tool.capability);
        Box::pin(async move {
            let canonical = arguments.get("context").is_some_and(Value::is_null)
                && arguments.get("tasks").is_some_and(Value::is_null)
                && arguments
                    .get("task")
                    .and_then(Value::as_str)
                    .is_some_and(|task| task.contains("glm"))
                && capability == Some(pi_agent::ToolCapability::Exec);
            Ok(pi_agent::BeforeToolCallResult {
                block: canonical,
                reason: Some("canonical task call denied for test".to_owned()),
                arguments: None,
            })
        })
    });
    let session = Session::new(SessionOptions {
        model: model(),
        cwd: root.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: pi_agent::ThinkingLevel::Off,
        api_key: String::new(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: Some(canonical_deny),
        after_tool_call: None,
        stream_fn: Some(stream_fn),
        auth_resolver: None,
    })?;
    let runtime = OrchestrationRuntime::new(config(), recording_factory(contexts))?;
    let application = Application::new_with_orchestration(session, runtime.clone()).await;
    application
        .prompt("Have glm and grok study this".to_owned(), Vec::new(), None)
        .await?;
    assert!(
        runtime.jobs(None).is_empty(),
        "the canonical task shape must be denied by the hook"
    );
    application.cleanup().await;
    Ok(())
}

/// Contract (P4+P5 interaction): when the task authorization gate DENIES a
/// recognized delegation, the auto-todo classifier is suppressed too — the
/// todo DAG path must not spawn jobs that bypass the same gate. With the
/// gate open, the delegation spawns and auto-todo is skipped (no duplicate
/// orchestration), so exactly the named agents' jobs exist.
#[tokio::test]
async fn denied_delegation_also_suppresses_auto_todo() -> Result<()> {
    let root = tempfile::tempdir()?;
    let agent_dir = root.path().join("agent");
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"orchestration":{"tasks":true},"selector":{"autoMode":"auto"}}"#,
    )?;
    let mut options = ResourceManagerOptions::new(root.path().to_path_buf());
    options.agent_dir = agent_dir;
    let resources = ResourceManager::new(options)?;
    let blocking: pi_agent::BeforeToolCallFn = Arc::new(|context| {
        Box::pin(async move {
            Ok(pi_agent::BeforeToolCallResult {
                block: context.tool_call.name == "task",
                reason: Some("task blocked for test".to_owned()),
                arguments: None,
            })
        })
    });
    let session = || {
        let stream_fn: pi_agent::StreamFn = Arc::new(|model, _context, _options| {
            use pi_ai::{AssistantMessage, AssistantMessageEvent, StopReason};
            async move {
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
            model: model(),
            cwd: root.path().to_path_buf(),
            system_prompt: String::new(),
            thinking_level: pi_agent::ThinkingLevel::Off,
            api_key: String::new(),
            compaction: None,
            stream_options: Default::default(),
            tools: Some(Vec::new()),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: Some(stream_fn),
            auth_resolver: None,
        })
        .expect("parent session")
    };
    let parent = session();
    parent.attach_resources(resources.clone()).await?;
    parent.set_before_tool_call(Some(blocking.clone()));
    let config = || {
        let mut config = OrchestrationConfig::new(
            AgentCatalog::from_agents(vec![
                agent("glm", "General language model agent", "GLM_PROMPT", &[]),
                agent("grok", "Research assistant agent", "GROK_PROMPT", &[]),
            ]),
            root.path(),
        )
        .with_selector_settings(selector_settings());
        config.default_agent = "glm".to_owned();
        config.parent_model = model();
        config
    };
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let prompt = "Have glm and grok implement a parser in src/lib.rs";

    // Gate denied (blocking hook): nothing spawns — neither the delegation
    // nor any auto-todo DAG job — and no todo phases are created.
    let denied_runtime =
        OrchestrationRuntime::new(config(), recording_factory(contexts.clone()))?;
    let denied_app = Application::new_with_orchestration(parent, denied_runtime.clone()).await;
    denied_app
        .prompt(prompt.to_owned(), Vec::new(), None)
        .await?;
    assert!(
        denied_runtime.jobs(None).is_empty(),
        "a denied delegation must spawn nothing, including todo DAG jobs"
    );
    assert!(
        denied_app.session().todo_state().phases.is_empty(),
        "a denied delegation must not create an auto-todo DAG"
    );
    denied_app.cleanup().await;

    // Negated delegations never spawn and never reach the auto-todo
    // classifier — the user said NOT to run the clause, so no alternate
    // owner may execute it either. Fresh app/runtime per prompt.
    for negated in [
        "你别让glm修改src/lib.rs并运行测试",
        "Don't have glm implement a parser in src/lib.rs",
    ] {
        let negated_parent = session();
        negated_parent.attach_resources(resources.clone()).await?;
        let negated_runtime =
            OrchestrationRuntime::new(config(), recording_factory(contexts.clone()))?;
        let negated_app =
            Application::new_with_orchestration(negated_parent, negated_runtime.clone()).await;
        negated_app
            .prompt(negated.to_owned(), Vec::new(), None)
            .await?;
        assert!(
            negated_runtime.jobs(None).is_empty(),
            "{negated:?} must not spawn anything, including todo DAG jobs"
        );
        assert!(
            negated_app.session().todo_state().phases.is_empty(),
            "{negated:?} must not create an auto-todo DAG"
        );
        negated_app.cleanup().await;
    }
    // Mixed polarity: a negated clause suppresses auto-todo while a positive
    // clause in the same prompt still spawns exactly its own mentions. Both
    // fullwidth and ASCII semicolons are covered (NFKC maps `；` to `;`).
    for (mixed, expected) in [
        (
            "Don't have glm implement a parser in src/lib.rs; have grok review it",
            "grok",
        ),
        ("别让glm调研；让grok审查代码", "grok"),
        ("别让glm调研; 让grok审查代码", "grok"),
        ("别让glm调研;让grok审查代码", "grok"),
    ] {
        let mixed_parent = session();
        mixed_parent.attach_resources(resources.clone()).await?;
        let mixed_runtime =
            OrchestrationRuntime::new(config(), recording_factory(contexts.clone()))?;
        let mixed_app =
            Application::new_with_orchestration(mixed_parent, mixed_runtime.clone()).await;
        mixed_app
            .prompt(mixed.to_owned(), Vec::new(), None)
            .await?;
        let jobs = mixed_runtime.jobs(None);
        assert_eq!(jobs.len(), 1, "{mixed:?} must spawn exactly one agent: {jobs:?}");
        assert_eq!(jobs[0].agent, expected, "{jobs:?}");
        assert!(
            mixed_app.session().todo_state().phases.is_empty(),
            "{mixed:?} must not create an auto-todo DAG"
        );
        mixed_app.cleanup().await;
    }
    // An informational mention keeps the normal classifier behavior: no
    // delegation clause (positive or negated), so auto-todo runs as usual.
    let informational_parent = session();
    informational_parent.attach_resources(resources.clone()).await?;
    let informational_runtime =
        OrchestrationRuntime::new(config(), recording_factory(contexts.clone()))?;
    let informational_app =
        Application::new_with_orchestration(informational_parent, informational_runtime.clone())
            .await;
    informational_app
        .prompt(
            "glm should implement the parser in src/lib.rs".to_owned(),
            Vec::new(),
            None,
        )
        .await?;
    assert!(
        !informational_app
            .session()
            .todo_state()
            .phases
            .is_empty(),
        "an informational mention must keep the normal auto-todo classifier behavior"
    );
    informational_app.cleanup().await;

    // Gate open: the delegation spawns exactly the named agents and the
    // auto-todo classifier is skipped (no duplicate orchestration).
    let open_parent = session();
    open_parent.attach_resources(resources).await?;
    let open_runtime = OrchestrationRuntime::new(config(), recording_factory(contexts))?;
    let open_app = Application::new_with_orchestration(open_parent, open_runtime.clone()).await;
    open_app
        .prompt(prompt.to_owned(), Vec::new(), None)
        .await?;
    assert_eq!(
        open_runtime.jobs(None).len(),
        2,
        "the delegated agents must spawn exactly once each"
    );
    open_app.cleanup().await;
    Ok(())
}

/// Contract (P0-C): the workflow catalog diagnostics fail actionably when an
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
        ]),
        root.path(),
    );
    config.default_agent = "writer".to_owned();
    config.skills = vec![research];
    config.parent_model = model();
    let runtime = OrchestrationRuntime::new(config, Arc::new(|_| Box::pin(async { unreachable!() })))?;

    // Present agent + action clause: valid.
    runtime
        .validate_delegation_agents("你让researcher仔细调研pi-coding-agent")
        .expect("a present exact agent must validate");
    // English delegation to a present agent: valid.
    runtime
        .validate_delegation_agents("Have researcher study this")
        .expect("an English delegation to a present agent must validate");
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

    // Missing agent named by a CJK delegation: actionable failure.
    let error = runtime
        .validate_delegation_agents("你让ghost-agent仔细调研pi-coding-agent")
        .expect_err("a missing explicit agent must fail actionably")
        .to_string();
    assert!(error.contains("ghost-agent"), "{error}");
    assert!(error.contains("not defined"), "{error}");
    assert!(error.contains("~/.pi/agents"), "{error}");
    // Missing agent named by an English delegation: actionable failure.
    let error = runtime
        .validate_delegation_agents("Have ghost-agent study this")
        .expect_err("a missing explicit English agent must fail actionably")
        .to_string();
    assert!(error.contains("ghost-agent") && error.contains("not defined"), "{error}");

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
    assert!(error.contains("~/.pi/agents"), "{error}");

    // Plural English delegation: the conjunction chain reports EVERY named
    // agent too, and stops before the action clause.
    let error = runtime
        .validate_delegation_agents("Have ghost-one and ghost-two study this")
        .expect_err("every missing explicit English agent must be reported")
        .to_string();
    assert!(error.contains("ghost-one"), "{error}");
    assert!(error.contains("ghost-two"), "{error}");
    assert!(error.contains("not defined"), "{error}");

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
    assert!(error.contains("researcher") && error.contains("disabled"), "{error}");
    // Mixed disabled/absent delegation: BOTH conditions are reported.
    let error = disabled_runtime
        .validate_delegation_agents("Have researcher and ghost-agent study this")
        .expect_err("a mixed disabled/absent delegation must report both")
        .to_string();
    assert!(error.contains("researcher") && error.contains("disabled"), "{error}");
    assert!(
        error.contains("ghost-agent") && error.contains("not defined"),
        "{error}"
    );
    runtime.shutdown().await;
    disabled_runtime.shutdown().await;
    Ok(())
}
