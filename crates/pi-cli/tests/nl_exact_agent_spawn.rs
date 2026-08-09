//! Exact natural-language agent mention → orchestration spawn → visible job cards.
//!
//! Contract: `Have researcher study this` spawns the trusted researcher through
//! Application.prompt, emits AgentUpdated/JobUpdated, and fills
//! JobCardPresentationAdapter (Subagents panel source). Skill-only text does not spawn.

use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use parking_lot::Mutex;
use pi_agent::ThinkingLevel;
use pi_ai::{AssistantMessage, AssistantMessageEvent, Context, Model, StopReason};
use pi_cli::job_card_adapter::JobCardPresentationAdapter;
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, Application, ApplicationEvent,
    ChildSessionFactory, JobStatus, OrchestrationConfig, OrchestrationEvent, OrchestrationRuntime,
    OrchestrationSkill, SelectorSettings, Session, SessionOptions, SkillSource,
};

fn model() -> Model {
    Model {
        id: "nl-spawn-model".to_owned(),
        name: "NL Spawn model".to_owned(),
        api: "nl-spawn-api".to_owned(),
        provider: "nl-spawn-provider".to_owned(),
        ..Model::default()
    }
}

fn agent(name: &str, description: &str, prompt: &str) -> AgentDefinition {
    AgentDefinition { name: name.to_owned(),
    description: description.to_owned(),
    system_prompt: prompt.to_owned(),
    tools: Some(Vec::new()),
    autoload_skills: Vec::new(),
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

fn parent_session() -> Session {
    Session::new(SessionOptions {
        model: model(),
        cwd: std::env::current_dir().expect("cwd"),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "nl-spawn".to_owned(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: Some(Arc::new(|model, _context, _| {
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
        })),
        auth_resolver: None,
    })
    .expect("parent session")
}

fn child_factory(contexts: Arc<Mutex<Vec<Context>>>) -> ChildSessionFactory {
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
                cwd: std::env::current_dir().expect("cwd"),
                system_prompt: request.system_prompt,
                thinking_level: request.thinking_level.unwrap_or(ThinkingLevel::Off),
                api_key: "nl-spawn-child".to_owned(),
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

#[tokio::test]
async fn have_researcher_prompt_spawns_visible_job_and_agent_cards() {
    let root = tempfile::tempdir().expect("tempdir");
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![
            agent(
                "researcher",
                "Research and study assigned topics",
                "RESEARCHER_PROMPT",
            ),
            agent("writer", "Write assigned content", "WRITER_PROMPT"),
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
    let runtime =
        OrchestrationRuntime::new(config, child_factory(contexts.clone())).expect("runtime");

    let application = Application::new_with_orchestration(parent_session(), runtime.clone()).await;
    let mut events = application.subscribe();

    application
        .prompt(
            "Have researcher study this".to_owned(),
            Vec::new(),
            None,
        )
        .await
        .expect("prompt with exact agent mention");

    let mut adapter = JobCardPresentationAdapter::new();
    let mut saw_agent = false;
    let mut saw_job = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !(saw_agent && saw_job && !adapter.cards_in_source_order().is_empty())
        && tokio::time::Instant::now() < deadline
    {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Ok(ApplicationEvent::Orchestration(event))) => {
                match &event {
                    OrchestrationEvent::AgentUpdated { agent, .. }
                        if agent.id == "researcher" || agent.display_name == "researcher" =>
                    {
                        assert_eq!(agent.display_name, "researcher");
                        saw_agent = true;
                    }
                    OrchestrationEvent::JobUpdated { job, .. } if job.agent == "researcher" => {
                        assert_eq!(job.agent_id, "researcher");
                        assert!(matches!(
                            job.status,
                            JobStatus::Queued | JobStatus::Running | JobStatus::Completed
                        ));
                        saw_job = true;
                    }
                    _ => {}
                }
                adapter.apply_application_event(&ApplicationEvent::Orchestration(event));
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }

    assert!(saw_agent, "Application must forward AgentUpdated for researcher");
    assert!(saw_job, "Application must forward JobUpdated for researcher");
    let cards = adapter.cards_in_source_order();
    assert!(
        !cards.is_empty(),
        "Subagents job_cards must be non-empty after exact NL spawn"
    );
    assert!(
        cards.iter().any(|card| {
            card.agent == "researcher"
                && (card.display_name == "researcher" || card.agent_id == "researcher")
        }),
        "job card must carry researcher human display name: {cards:?}"
    );

    // Skill-only phrasing must not spawn another researcher job.
    let before = runtime.jobs(None).len();
    application
        .prompt("Use research for this".to_owned(), Vec::new(), None)
        .await
        .expect("skill-only prompt");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        runtime.jobs(None).len(),
        before,
        "Use research for this must not spawn a subagent"
    );

    application.cleanup().await;
    runtime.shutdown().await;
}

/// Contract: the literal CJK delegation prompt
/// `你让researcher仔细调研pi-coding-agent` spawns the named researcher
/// WITHOUT any English sentinel. The delegation gate recognizes the Chinese
/// construction `让` + exact agent name + action clause, so no `please` is
/// needed. Informational / question / skill mentions (`researcher 是做什么的？`,
/// `我在文档里看到researcher`, `请使用research技能`) must NOT auto-spawn, and
/// the spawned child's system prompt must carry the researcher definition
/// without the overlapping `research` skill body.
#[tokio::test]
async fn literal_cjk_delegation_prompt_spawns_named_agent_without_english_sentinel() {
    let root = tempfile::tempdir().expect("tempdir");
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![
            agent(
                "researcher",
                "Research and study assigned topics",
                "RESEARCHER_PROMPT",
            ),
            agent("writer", "Write assigned content", "WRITER_PROMPT"),
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
    let runtime =
        OrchestrationRuntime::new(config, child_factory(contexts.clone())).expect("runtime");

    let application = Application::new_with_orchestration(parent_session(), runtime.clone()).await;
    let mut events = application.subscribe();

    application
        .prompt(
            "你让researcher仔细调研pi-coding-agent".to_owned(),
            Vec::new(),
            None,
        )
        .await
        .expect("literal CJK delegation prompt");

    let mut saw_job = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !saw_job && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Ok(ApplicationEvent::Orchestration(event))) => {
                if let OrchestrationEvent::JobUpdated { job, .. } = &event {
                    if job.agent == "researcher" {
                        assert_eq!(job.agent_id, "researcher");
                        saw_job = true;
                    }
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }
    assert!(
        saw_job,
        "the literal CJK prompt must spawn the researcher job without an English sentinel"
    );

    // The spawned child must run the researcher definition, not the
    // overlapping `research` skill.
    let captured = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let captured = contexts.lock();
            if !captured.is_empty() {
                break captured.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("child session context captured");
    assert_eq!(captured.len(), 1);
    assert!(
        captured[0].system_prompt.starts_with("RESEARCHER_PROMPT"),
        "child must start with the researcher prompt: {}",
        captured[0].system_prompt
    );
    assert!(
        !captured[0].system_prompt.contains("RESEARCH_BODY"),
        "an exact agent spawn must not autoload the overlapping research skill body: {}",
        captured[0].system_prompt
    );

    // Negatives: informational mentions, a question, and a skill invocation
    // must not auto-spawn, even though they mention the same names.
    for negative in [
        "researcher 是做什么的？",
        "我在文档里看到researcher",
        "请使用research技能",
    ] {
        let before = runtime.jobs(None).len();
        application
            .prompt(negative.to_owned(), Vec::new(), None)
            .await
            .expect("negative prompt");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            runtime.jobs(None).len(),
            before,
            "{negative:?} must not auto-spawn a subagent"
        );
    }

    application.cleanup().await;
    runtime.shutdown().await;
}
