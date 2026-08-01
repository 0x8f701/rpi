//! Full deterministic orchestration campaign.
//!
//! Contracts defended:
//! - natural-language task text selects the matching enabled agent and autoloads skills once
//! - duplicate requested names allocate deterministic suffixes
//! - task returns a job UUID distinct from the canonical agent id
//! - hub send addressed by job UUID steers the running child (canonical `to`, requested UUID)
//! - steered child mutates a workspace artifact from guidance exactly once (no dead-letter)
//! - parent monitors lifecycle, cancels a sibling, and terminal statuses stay truthful
//! - disabled agents are isolated from advertisement, auto-select, and explicit spawn
//! - read-only agents deny write tools at execution time (`Tool write not found`)

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use pi_agent::{AbortController, AgentTool, ThinkingLevel, ToolCallContext};
use pi_ai::{ContentBlock, Context, Message, Model, StopReason, ToolCall};
use pi_coding::{
    AgentCatalog, AgentDefinition, AgentDefinitionSource, AgentRuntimeSettings, AgentStatus,
    ChildSessionFactory, JobSnapshot, JobStatus, OrchestrationConfig, OrchestrationRuntime,
    OrchestrationSkill, SelectorSettings, Session, SessionOptions, SkillSource, TaskSpawn,
};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const GUIDANCE_TOKEN: &str = "GUIDANCE_ONCE_v7f3a9c2";
const GUIDANCE_BODY: &str = "steered-payload-exact-once\n";
const MARKER_REL: &str = "workspace/marker.txt";
const MARKER_INITIAL: &[u8] = b"PENDING\n";

fn model() -> Model {
    Model {
        id: "campaign-model".to_owned(),
        name: "Campaign model".to_owned(),
        api: "campaign-api".to_owned(),
        provider: "campaign-provider".to_owned(),
        ..Model::default()
    }
}

fn agent(
    name: &str,
    description: &str,
    prompt: &str,
    tools: &[&str],
    autoload_skills: &[&str],
) -> AgentDefinition {
    AgentDefinition {
        name: name.to_owned(),
        description: description.to_owned(),
        system_prompt: prompt.to_owned(),
        tools: Some(tools.iter().map(|tool| (*tool).to_owned()).collect()),
        autoload_skills: autoload_skills
            .iter()
            .map(|skill| (*skill).to_owned())
            .collect(),
        model: None,
        thinking_level: Some(ThinkingLevel::Off),
        source: AgentDefinitionSource::User,
        path: None,
        trusted: true,
    }
}

fn skill(root: &Path, name: &str, description: &str, body: &str) -> OrchestrationSkill {
    let base_dir = root.join("skills").join(name);
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

fn tool<'a>(tools: &'a [AgentTool], name: &str) -> &'a AgentTool {
    tools
        .iter()
        .find(|candidate| candidate.name == name)
        .unwrap_or_else(|| panic!("missing tool {name}"))
}

fn context(id: &str, arguments: Value) -> ToolCallContext {
    let (_, abort) = AbortController::new();
    ToolCallContext {
        tool_call_id: id.to_owned(),
        arguments,
        on_update: Arc::new(|_| {}),
        abort,
        model: None,
    }
}

fn context_texts(context: &Context) -> Vec<String> {
    context
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(
                user.content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            Message::Custom(custom) => Some(
                custom
                    .content
                    .to_blocks()
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect()
}

fn guidance_hits(context: &Context) -> usize {
    context_texts(context)
        .into_iter()
        .filter(|text| text.contains(GUIDANCE_TOKEN))
        .count()
}

struct CampaignFactory {
    workspace: PathBuf,
    steered_started: Arc<Notify>,
    steered_running: Arc<AtomicUsize>,
    hang_running: Arc<AtomicUsize>,
    hang_started: Arc<Notify>,
    release_steered_first: CancellationToken,
    guidance_write_calls: Arc<AtomicUsize>,
    captured_prompts: Arc<Mutex<Vec<String>>>,
    captured_steered_contexts: Arc<Mutex<Vec<Context>>>,
}

impl CampaignFactory {
    fn build(self) -> ChildSessionFactory {
        let workspace = self.workspace;
        let steered_started = self.steered_started;
        let steered_running = self.steered_running;
        let hang_running = self.hang_running;
        let hang_started = self.hang_started;
        let release_steered_first = self.release_steered_first;
        let guidance_write_calls = self.guidance_write_calls;
        let captured_prompts = self.captured_prompts;
        let captured_steered_contexts = self.captured_steered_contexts;

        Arc::new(move |request| {
            let workspace = workspace.clone();
            let steered_started = steered_started.clone();
            let steered_running = steered_running.clone();
            let hang_running = hang_running.clone();
            let hang_started = hang_started.clone();
            let release_steered_first = release_steered_first.clone();
            let guidance_write_calls = guidance_write_calls.clone();
            let captured_prompts = captured_prompts.clone();
            let captured_steered_contexts = captured_steered_contexts.clone();
            let child_id = request.child_id.clone();
            let system_prompt = request.system_prompt.clone();
            let orchestration_tools = request.orchestration_tools.clone();
            let requested_tool_names = request.requested_tool_names.clone();
            let thinking_level = request.thinking_level;
            let model = request.model.clone();

            Box::pin(async move {
                let cwd = workspace.to_string_lossy().into_owned();
                let mut tools = match requested_tool_names.as_deref() {
                    Some(names) => names
                        .iter()
                        .filter(|name| {
                            !matches!(
                                name.as_str(),
                                "todo" | "process" | "task" | "hub" | "goal"
                            )
                        })
                        .map(|name| {
                            pi_coding::create_tool(name, &cwd).unwrap_or_else(|error| {
                                panic!("create tool {name} for {child_id}: {error}")
                            })
                        })
                        .collect::<Vec<_>>(),
                    None => pi_coding::create_coding_tools(&cwd),
                };
                tools.extend(orchestration_tools);

                let call_index = Arc::new(AtomicUsize::new(0));
                let stream_child_id = child_id.clone();
                let stream_system_prompt = system_prompt.clone();
                let stream_fn: pi_agent::StreamFn = Arc::new(move |model, context, options| {
                    let child_id = stream_child_id.clone();
                    let steered_started = steered_started.clone();
                    let steered_running = steered_running.clone();
                    let hang_running = hang_running.clone();
                    let hang_started = hang_started.clone();
                    let release_steered_first = release_steered_first.clone();
                    let guidance_write_calls = guidance_write_calls.clone();
                    let captured_prompts = captured_prompts.clone();
                    let captured_steered_contexts = captured_steered_contexts.clone();
                    let call_index = call_index.clone();
                    let system_prompt = stream_system_prompt.clone();
                    Box::pin(async move {
                        let stream = pi_ai::new_assistant_message_event_stream();
                        let producer = stream.clone();
                        let call = call_index.fetch_add(1, Ordering::SeqCst);
                        tokio::spawn(async move {
                            let mut message = pi_ai::AssistantMessage::pending(&model);
                            match child_id.as_str() {
                                "Scribe" => {
                                    captured_steered_contexts.lock().push(context.clone());
                                    if call == 0 {
                                        captured_prompts.lock().push(system_prompt.clone());
                                        steered_running.fetch_add(1, Ordering::SeqCst);
                                        steered_started.notify_waiters();
                                        let abort = options.stream.abort_signal;
                                        tokio::select! {
                                            () = release_steered_first.cancelled() => {}
                                            () = async {
                                                match abort {
                                                    Some(signal) => signal.cancelled().await,
                                                    None => std::future::pending::<()>().await,
                                                }
                                            } => {}
                                        }
                                        steered_running.fetch_sub(1, Ordering::SeqCst);
                                        message
                                            .content
                                            .push(ContentBlock::text("awaiting guidance"));
                                        message.stop_reason = StopReason::Stop;
                                    } else if guidance_hits(&context) > 0
                                        && guidance_write_calls.load(Ordering::SeqCst) == 0
                                    {
                                        guidance_write_calls.fetch_add(1, Ordering::SeqCst);
                                        message.content.push(ContentBlock::ToolCall(ToolCall {
                                            id: "guided-write".to_owned(),
                                            name: "write".to_owned(),
                                            arguments: json!({
                                                "path": MARKER_REL,
                                                "content": GUIDANCE_BODY,
                                            }),
                                            thought_signature: None,
                                        }));
                                        message.stop_reason = StopReason::ToolUse;
                                    } else {
                                        message.content.push(ContentBlock::text(
                                            "guidance applied exactly once",
                                        ));
                                        message.stop_reason = StopReason::Stop;
                                    }
                                }
                                "Scribe_2" => {
                                    hang_running.fetch_add(1, Ordering::SeqCst);
                                    hang_started.notify_waiters();
                                    let abort = options
                                        .stream
                                        .abort_signal
                                        .expect("hanging child requires abort");
                                    abort.cancelled().await;
                                    hang_running.fetch_sub(1, Ordering::SeqCst);
                                    message.stop_reason = StopReason::Aborted;
                                }
                                "Scout" => {
                                    if call == 0 {
                                        message.content.push(ContentBlock::ToolCall(ToolCall {
                                            id: "scout-write-denied".to_owned(),
                                            name: "write".to_owned(),
                                            arguments: json!({
                                                "path": MARKER_REL,
                                                "content": "scout-must-not-write\n",
                                            }),
                                            thought_signature: None,
                                        }));
                                        message.stop_reason = StopReason::ToolUse;
                                    } else {
                                        message.content.push(ContentBlock::text(
                                            "scout finished without write",
                                        ));
                                        message.stop_reason = StopReason::Stop;
                                    }
                                }
                                _ => {
                                    message
                                        .content
                                        .push(ContentBlock::text(format!("{child_id} done")));
                                    message.stop_reason = StopReason::Stop;
                                }
                            }
                            producer.end(Some(message)).await;
                        });
                        stream
                    })
                });

                Session::new(SessionOptions {
                    model,
                    cwd: workspace,
                    system_prompt,
                    thinking_level: thinking_level.unwrap_or(ThinkingLevel::Off),
                    api_key: "campaign".to_owned(),
                    compaction: None,
                    stream_options: Default::default(),
                    tools: Some(tools),
                    before_tool_call: None,
                    after_tool_call: None,
                    stream_fn: Some(stream_fn),
                    auth_resolver: None,
                })
            })
        })
    }
}

async fn wait_for_count(
    count: &AtomicUsize,
    notify: &Notify,
    expected: usize,
    description: &str,
) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let notified = notify.notified();
            if count.load(Ordering::SeqCst) == expected {
                break;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {description}"));
}

async fn wait_for_job_status(
    runtime: &OrchestrationRuntime,
    job_id: &str,
    status: JobStatus,
    description: &str,
) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let jobs = runtime.jobs(Some(std::slice::from_ref(&job_id.to_owned())));
            if jobs
                .iter()
                .any(|job| job.id == job_id && job.status == status)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {description} ({status:?})"));
}

fn build_runtime(
    artifacts: &Path,
    skills: Vec<OrchestrationSkill>,
    factory: ChildSessionFactory,
) -> OrchestrationRuntime {
    let mut agent_settings = BTreeMap::new();
    agent_settings.insert(
        "shadow".to_owned(),
        AgentRuntimeSettings {
            enabled: Some(false),
            model: None,
            tools: None,
        },
    );

    let mut config = OrchestrationConfig::new(
        AgentCatalog::from_agents(vec![
            agent(
                "reviewer",
                "Review Rust security vulnerabilities and patches",
                "REVIEWER_PROMPT",
                &["read", "write"],
                &["rust-security"],
            ),
            agent(
                "writer",
                "Write release documentation and migration guides",
                "WRITER_PROMPT",
                &["read", "write"],
                &["docs-writing"],
            ),
            agent(
                "scout",
                "Read-only reconnaissance of repositories",
                "SCOUT_PROMPT",
                &["read"],
                &[],
            ),
            agent(
                "shadow",
                "Review Rust security vulnerabilities in dark mode",
                "SHADOW_PROMPT",
                &["read", "write"],
                &["rust-security"],
            ),
        ]),
        artifacts,
    )
    .with_selector_settings(selector_settings())
    .with_agent_settings(agent_settings)
    .with_parent_model(model());
    config.default_agent = "writer".to_owned();
    config.skills = skills;
    config.max_concurrency = 4;
    config.idle_ttl = None;
    config.mailbox_capacity = 16;
    OrchestrationRuntime::new(config, factory).expect("campaign runtime")
}

#[tokio::test]
async fn orchestration_campaign_selector_steer_cancel_and_isolation() {
    let root = tempfile::tempdir().expect("root");
    let artifacts = root.path().join("artifacts");
    let workspace = root.path().join("work");
    std::fs::create_dir_all(workspace.join("workspace")).expect("workspace");
    let marker_path = workspace.join(MARKER_REL);
    std::fs::write(&marker_path, MARKER_INITIAL).expect("seed marker");

    let skills = vec![
        skill(
            root.path(),
            "rust-security",
            "Review Rust code for memory safety and security vulnerabilities",
            "RUST_SECURITY_BODY",
        ),
        skill(
            root.path(),
            "docs-writing",
            "Write release documentation and migration guides",
            "DOCS_WRITING_BODY",
        ),
        skill(
            root.path(),
            "security-audit",
            "Audit security vulnerabilities in patches",
            "SECURITY_AUDIT_BODY",
        ),
    ];

    let steered_started = Arc::new(Notify::new());
    let hang_started = Arc::new(Notify::new());
    let steered_running = Arc::new(AtomicUsize::new(0));
    let hang_running = Arc::new(AtomicUsize::new(0));
    let release_steered_first = CancellationToken::new();
    let guidance_write_calls = Arc::new(AtomicUsize::new(0));
    let captured_prompts = Arc::new(Mutex::new(Vec::new()));
    let captured_steered_contexts = Arc::new(Mutex::new(Vec::new()));

    let factory = CampaignFactory {
        workspace: workspace.clone(),
        steered_started: steered_started.clone(),
        steered_running: steered_running.clone(),
        hang_running: hang_running.clone(),
        hang_started: hang_started.clone(),
        release_steered_first: release_steered_first.clone(),
        guidance_write_calls: guidance_write_calls.clone(),
        captured_prompts: captured_prompts.clone(),
        captured_steered_contexts: captured_steered_contexts.clone(),
    }
    .build();

    let runtime = build_runtime(&artifacts, skills, factory);
    let tools = runtime.agent_tools("Main", 0);
    let task = tool(&tools, "task");
    let hub = tool(&tools, "hub");

    assert!(
        task.description.contains("reviewer —"),
        "enabled reviewer must be advertised: {}",
        task.description
    );
    assert!(
        task.description.contains("writer —"),
        "enabled writer must be advertised: {}",
        task.description
    );
    assert!(
        task.description.contains("scout —"),
        "enabled scout must be advertised: {}",
        task.description
    );
    assert!(
        !task.description.contains("shadow —"),
        "disabled shadow must not be advertised: {}",
        task.description
    );

    assert_eq!(
        runtime.select_agent("Review Rust security vulnerabilities in this patch", None),
        "reviewer"
    );
    assert_eq!(
        runtime.select_agent(
            "Review Rust security vulnerabilities in this patch",
            Some("writer")
        ),
        "writer"
    );

    let disabled_err = (task.execute)(context(
        "spawn-disabled-shadow",
        json!({
            "name": "ShadowChild",
            "agent": "shadow",
            "task": "Review Rust security vulnerabilities in this patch"
        }),
    ))
    .await
    .expect_err("explicit disabled agent must fail");
    let disabled_msg = disabled_err.to_string();
    assert!(
        disabled_msg.contains("shadow") && disabled_msg.contains("disabled"),
        "{disabled_msg}"
    );
    assert!(
        disabled_msg.contains("/agents") || disabled_msg.contains("settings.agents"),
        "{disabled_msg}"
    );

    let unknown_err = (task.execute)(context(
        "spawn-unknown-agent",
        json!({
            "name": "Ghost",
            "agent": "no-such-agent",
            "task": "do nothing"
        }),
    ))
    .await
    .expect_err("unknown agent must fail");
    let unknown_msg = unknown_err.to_string();
    assert!(
        unknown_msg.contains("no-such-agent") || unknown_msg.contains("unknown"),
        "{unknown_msg}"
    );

    let scout_spawn = (task.execute)(context(
        "spawn-scout",
        json!({
            "name": "Scout",
            "agent": "scout",
            "task": "Read-only reconnaissance of repositories"
        }),
    ))
    .await
    .expect("scout spawn");
    let scout_spawns: Vec<TaskSpawn> =
        serde_json::from_value(scout_spawn.details).expect("scout spawn details");
    assert_eq!(scout_spawns.len(), 1);
    assert_eq!(scout_spawns[0].agent_id, "Scout");
    assert_eq!(scout_spawns[0].agent, "scout");
    let scout_job = scout_spawns[0].job_id.clone();
    assert!(!scout_job.is_empty());
    assert_ne!(scout_job, scout_spawns[0].agent_id);

    let scout_jobs = runtime
        .wait_jobs(
            std::slice::from_ref(&scout_job),
            Some(Duration::from_secs(5)),
            None,
        )
        .await
        .expect("scout settle");
    assert_eq!(scout_jobs.len(), 1);
    assert_eq!(scout_jobs[0].status, JobStatus::Completed);
    let scout_result = scout_jobs[0].result.as_ref().expect("scout result");
    assert!(scout_result.error.is_none(), "{:?}", scout_result.error);
    assert_eq!(
        std::fs::read(&marker_path).expect("marker after scout"),
        MARKER_INITIAL
    );

    let scout_history_path = runtime
        .resolve_history_reference("Scout")
        .expect("scout history");
    let scout_history: Vec<Message> =
        serde_json::from_slice(&std::fs::read(scout_history_path).expect("read scout history"))
            .expect("parse scout history");
    let write_denial = scout_history.iter().find_map(|message| match message {
        Message::ToolResult(result) if result.tool_name == "write" => Some(result),
        _ => None,
    });
    let write_denial = write_denial.expect("read-only scout must attempt write and receive denial");
    assert!(
        write_denial.is_error,
        "write denial must be flagged as error: {write_denial:?}"
    );
    let denial_text = write_denial
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(
        denial_text.contains("Tool write not found"),
        "expected write-tool denial, got: {denial_text}"
    );

    let batch = tokio::time::timeout(
        Duration::from_millis(800),
        (task.execute)(context(
            "spawn-campaign-batch",
            json!({
                "tasks": [
                    {
                        "name": "Scribe",
                        "task": "Review Rust security vulnerabilities in this patch"
                    },
                    {
                        "name": "Scribe",
                        "task": "hang until cancelled background watch"
                    }
                ]
            }),
        )),
    )
    .await
    .expect("task must return before children finish")
    .expect("batch spawn");
    let mut spawns: Vec<TaskSpawn> =
        serde_json::from_value(batch.details).expect("batch spawn details");
    spawns.sort_by_key(|spawn| spawn.index);
    assert_eq!(spawns.len(), 2);

    assert_eq!(spawns[0].agent_id, "Scribe");
    assert_eq!(spawns[1].agent_id, "Scribe_2");
    assert_eq!(spawns[0].agent, "reviewer", "NL security text selects reviewer");
    assert_eq!(spawns[1].agent, "writer");
    assert!(spawns.iter().all(|spawn| spawn.status == JobStatus::Queued));
    assert!(spawns.iter().all(|spawn| !spawn.job_id.is_empty()));
    assert_ne!(spawns[0].job_id, spawns[0].agent_id);
    assert_ne!(spawns[1].job_id, spawns[1].agent_id);
    assert_ne!(spawns[0].job_id, spawns[1].job_id);

    let steered_job = spawns[0].job_id.clone();
    let steered_agent = spawns[0].agent_id.clone();
    let hang_job = spawns[1].job_id.clone();
    let hang_agent = spawns[1].agent_id.clone();

    wait_for_count(
        &steered_running,
        &steered_started,
        1,
        "steered scribe running",
    )
    .await;
    wait_for_count(&hang_running, &hang_started, 1, "hanging sibling running").await;
    wait_for_job_status(&runtime, &steered_job, JobStatus::Running, "steered running").await;
    wait_for_job_status(&runtime, &hang_job, JobStatus::Running, "hang running").await;

    let jobs_snapshot = (hub.execute)(context("jobs-running", json!({ "op": "jobs" })))
        .await
        .expect("hub jobs");
    let running_jobs: Vec<JobSnapshot> =
        serde_json::from_value(jobs_snapshot.details["jobs"].clone()).expect("running jobs");
    assert!(
        running_jobs
            .iter()
            .any(|job| job.id == steered_job && job.status == JobStatus::Running)
    );
    assert!(
        running_jobs
            .iter()
            .any(|job| job.id == hang_job && job.status == JobStatus::Running)
    );

    let prompts = captured_prompts.lock().clone();
    assert_eq!(prompts.len(), 1, "steered child builds one system prompt");
    let prompt = &prompts[0];
    assert!(
        prompt.starts_with("REVIEWER_PROMPT"),
        "selected reviewer prompt: {prompt}"
    );
    assert!(!prompt.contains("WRITER_PROMPT"), "{prompt}");
    assert!(!prompt.contains("SHADOW_PROMPT"), "{prompt}");
    assert_eq!(prompt.matches("RUST_SECURITY_BODY").count(), 1, "{prompt}");
    assert_eq!(prompt.matches("SECURITY_AUDIT_BODY").count(), 1, "{prompt}");
    assert_eq!(prompt.matches("DOCS_WRITING_BODY").count(), 0, "{prompt}");
    assert_eq!(
        prompt
            .matches("<autoloaded_skill name=\"rust-security\"")
            .count(),
        1,
        "{prompt}"
    );
    assert_eq!(
        prompt
            .matches("<autoloaded_skill name=\"security-audit\"")
            .count(),
        1,
        "{prompt}"
    );
    assert!(prompt.contains("skill://rust-security"), "{prompt}");
    assert!(prompt.contains("skill://security-audit"), "{prompt}");

    let guidance = format!("{GUIDANCE_TOKEN}: replace marker with guided payload exactly once");
    let send = (hub.execute)(context(
        "steer-by-job-uuid",
        json!({
            "op": "send",
            "to": steered_job,
            "message": guidance,
        }),
    ))
    .await
    .expect("hub send by job uuid");
    let receipts = send.details["receipts"]
        .as_array()
        .expect("receipts array");
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0]["to"], steered_agent);
    assert_eq!(receipts[0]["requested"], steered_job);
    assert_eq!(receipts[0]["outcome"], "woken");
    assert!(receipts[0].get("error").is_none() || receipts[0]["error"].is_null());

    assert!(
        runtime.inbox(&steered_agent, true).is_empty(),
        "active steered delivery must not leave a canonical mailbox duplicate"
    );
    assert!(
        runtime.inbox(&steered_job, true).is_empty(),
        "job uuid must not be a separate mailbox"
    );

    release_steered_first.cancel();

    let steered_wait = tokio::time::timeout(
        Duration::from_secs(5),
        (hub.execute)(context(
            "wait-steered",
            json!({
                "op": "wait",
                "ids": [steered_job],
                "timeoutMs": 5000
            }),
        )),
    )
    .await
    .expect("wait steered timeout")
    .expect("wait steered result");
    let steered_done: Vec<JobSnapshot> =
        serde_json::from_value(steered_wait.details["jobs"].clone()).expect("steered jobs");
    let steered = steered_done
        .iter()
        .find(|job| job.id == steered_job)
        .expect("steered job present");
    assert_eq!(steered.status, JobStatus::Completed);
    assert_eq!(steered.agent_id, steered_agent);
    let steered_result = steered.result.as_ref().expect("steered result");
    assert!(steered_result.error.is_none(), "{:?}", steered_result.error);
    assert_eq!(steered_result.output, "guidance applied exactly once");
    assert_eq!(
        guidance_write_calls.load(Ordering::SeqCst),
        1,
        "child must issue exactly one guided write"
    );

    let marker_bytes = std::fs::read(&marker_path).expect("read guided marker");
    assert_eq!(
        marker_bytes,
        GUIDANCE_BODY.as_bytes(),
        "marker bytes must equal guidance payload"
    );

    let result_artifact = runtime
        .resolve_agent_reference(&steered_agent)
        .expect("steered artifact path");
    let artifact_bytes = std::fs::read(&result_artifact).expect("read result artifact");
    assert_eq!(
        artifact_bytes,
        b"guidance applied exactly once",
        "settled artifact body must match final output"
    );

    let steered_contexts = captured_steered_contexts.lock().clone();
    assert!(
        steered_contexts.len() >= 2,
        "expected initial + steered provider turns, got {}",
        steered_contexts.len()
    );
    // Initial hang turn has no guidance; the first post-steer provider request must
    // observe the payload exactly once. Later turns may retain it in history, but the
    // guided write fires only on first observation (guidance_write_calls == 1 above).
    assert_eq!(
        guidance_hits(&steered_contexts[0]),
        0,
        "initial provider turn must not include mid-run guidance"
    );
    assert_eq!(
        guidance_hits(&steered_contexts[1]),
        1,
        "first steered provider turn must include guidance exactly once"
    );
    let first_steered_turn = steered_contexts
        .iter()
        .position(|ctx| guidance_hits(ctx) > 0)
        .expect("guidance must reach at least one provider turn");
    assert_eq!(
        first_steered_turn, 1,
        "guidance must first appear on the immediate post-steer turn"
    );

    let hang_still = runtime.jobs(Some(std::slice::from_ref(&hang_job)));
    assert_eq!(hang_still[0].status, JobStatus::Running);
    assert_eq!(hang_still[0].agent_id, hang_agent);

    let cancel = (hub.execute)(context(
        "cancel-hang",
        json!({ "op": "cancel", "ids": [hang_job] }),
    ))
    .await
    .expect("cancel hang");
    assert_eq!(cancel.details["cancelled"], json!([hang_job]));

    let hang_wait = tokio::time::timeout(
        Duration::from_secs(3),
        (hub.execute)(context(
            "wait-hang",
            json!({
                "op": "wait",
                "ids": [hang_job],
                "timeoutMs": 3000
            }),
        )),
    )
    .await
    .expect("wait hang timeout")
    .expect("wait hang result");
    let hang_done: Vec<JobSnapshot> =
        serde_json::from_value(hang_wait.details["jobs"].clone()).expect("hang jobs");
    let hang = hang_done
        .iter()
        .find(|job| job.id == hang_job)
        .expect("hang job present");
    assert_eq!(hang.status, JobStatus::Cancelled);
    assert!(hang.finished_at.is_some());
    let hang_result = hang.result.as_ref().expect("hang result");
    assert_eq!(hang_result.status, AgentStatus::Aborted);
    assert_eq!(hang_result.error.as_deref(), Some("task cancelled"));

    let final_jobs = (hub.execute)(context("jobs-final", json!({ "op": "jobs" })))
        .await
        .expect("final jobs");
    let retained: Vec<JobSnapshot> =
        serde_json::from_value(final_jobs.details["jobs"].clone()).expect("retained");
    let by_id = |id: &str| {
        retained
            .iter()
            .find(|job| job.id == id)
            .unwrap_or_else(|| panic!("missing retained job {id}"))
    };
    assert_eq!(by_id(&scout_job).status, JobStatus::Completed);
    assert_eq!(by_id(&steered_job).status, JobStatus::Completed);
    assert_eq!(by_id(&hang_job).status, JobStatus::Cancelled);

    assert!(runtime.inbox(&steered_agent, true).is_empty());
    assert!(runtime.inbox(&steered_job, true).is_empty());

    assert_eq!(
        std::fs::read(&marker_path).expect("final marker"),
        GUIDANCE_BODY.as_bytes()
    );

    let unknown_send = (hub.execute)(context(
        "send-unknown-uuid",
        json!({
            "op": "send",
            "to": "00000000-0000-7000-8000-000000000099",
            "message": "no recipient"
        }),
    ))
    .await
    .expect("unknown send tool result");
    let unknown_receipts = unknown_send.details["receipts"]
        .as_array()
        .expect("unknown receipts");
    assert_eq!(unknown_receipts.len(), 1);
    assert_eq!(unknown_receipts[0]["outcome"], "failed");
    let unknown_error = unknown_receipts[0]["error"]
        .as_str()
        .expect("failed receipt error");
    assert!(
        unknown_error.contains("unknown orchestration agent"),
        "{unknown_error}"
    );

    runtime.shutdown().await;
    assert_eq!(runtime.active_child_count(), 0);
}
