use std::sync::Arc;

use pi_agent::{AbortController, ThinkingLevel};
use pi_ai::{Model, Usage};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, ChildSessionFactory, OrchestrationConfig,
    OrchestrationRuntime, Session, SessionOptions, TaskItem,
};

fn definition() -> AgentDefinition {
    AgentDefinition { name: "task".to_owned(),
    description: "usage test agent".to_owned(),
    system_prompt: "Return the requested result.".to_owned(),
    tools: Some(Vec::new()),
    autoload_skills: Vec::new(),
    model: None,
    thinking_level: Some(ThinkingLevel::Off),
    max_turns: None,
    max_tool_calls: None,
    timeout_secs: None,
    disallowed_tools: Vec::new(),
    capability_ceiling: None,
    source: AgentDefinitionSource::Bundled,
    path: None, trusted: true, kind: pi_coding::AgentDefinitionKind::Agent, personality: None, soft_budget: None }
}

#[tokio::test]
async fn completed_child_usage_is_preserved_in_result_and_job_snapshot() {
    let artifacts = tempfile::tempdir().expect("artifacts");
    let model = Model {
        id: "usage-model".to_owned(),
        name: "Usage Model".to_owned(),
        api: "usage-api".to_owned(),
        provider: "usage-provider".to_owned(),
        ..Model::default()
    };
    let expected = Usage {
        input: 17,
        output: 5,
        cache_read: 3,
        cache_write: 2,
        cache_write_1h: 1,
        reasoning: 4,
        total_tokens: 32,
        ..Usage::default()
    };
    let stream_usage = expected.clone();
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, _options| {
        let usage = stream_usage.clone();
        Box::pin(async move {
            let stream = pi_ai::new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                let mut message = pi_ai::AssistantMessage::pending(&model);
                message.content.push(pi_ai::ContentBlock::text("complete"));
                message.usage = usage;
                message.stop_reason = pi_ai::StopReason::Stop;
                producer.end(Some(message)).await;
            });
            stream
        })
    });
    let factory: ChildSessionFactory = Arc::new(move |request| {
        let model = model.clone();
        let stream_fn = stream_fn.clone();
        Box::pin(async move {
            Session::new(SessionOptions {
                model,
                cwd: std::env::current_dir().expect("cwd"),
                system_prompt: request.system_prompt,
                thinking_level: ThinkingLevel::Off,
                api_key: String::new(),
                compaction: None,
                stream_options: Default::default(),
                tools: Some(request.orchestration_tools),
                before_tool_call: None,
                after_tool_call: None,
                stream_fn: Some(stream_fn),
                auth_resolver: None,
            })
        })
    });
    let runtime = OrchestrationRuntime::new(
        OrchestrationConfig::new(AgentCatalog::from_agents(vec![definition()]), artifacts.path()),
        factory,
    )
    .expect("runtime");
    let (_, abort) = AbortController::new();
    let results = runtime
        .run_tasks(
            "Main",
            0,
            vec![TaskItem {
                index: 0,
                id: "UsageChild".to_owned(),
                agent: "task".to_owned(),
                assignment: "report usage".to_owned(),
                todo_task_id: None,
                ..Default::default()
            }],
            abort,
        )
        .await
        .expect("child result");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].usage, expected);
    let jobs = runtime.jobs(None);
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].result.as_ref().expect("settled job result").usage,
        expected
    );

    runtime.shutdown().await;
}

#[test]
fn missing_usage_deserializes_as_zero_for_compatibility() {
    let result: pi_coding::TaskResult = serde_json::from_value(serde_json::json!({
        "index": 0,
        "id": "Legacy",
        "agent": "task",
        "status": "idle",
        "output": "done",
        "artifactRef": "agent://Legacy",
        "historyRef": "history://Legacy",
        "artifactUri": "artifact://Legacy"
    }))
    .expect("legacy task result");
    assert_eq!(result.usage, Usage::default());
}
