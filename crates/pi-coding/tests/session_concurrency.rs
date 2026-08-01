//! Deterministic integration tests for the Session atomic-claim and
//! cancellation-safe reuse contracts.
//!
//! These tests do NOT touch real network, real API keys, or the user's
//! `~/.pi/agent` state. A scripted provider is registered through the public
//! `pi_ai` provider registry; it blocks an accepted run on a `Notify` gate and
//! resolves the in-flight run with an `Aborted` terminal assistant when the
//! abort signal fires, so `Session::run_print` / `Session::abort` /
//! `Session::wait_for_idle` can be exercised deterministically.
//!
//! Contracts defended:
//! * Exactly one concurrent `Session::run_print` is accepted; the second is
//!   rejected with "session is already processing a prompt" (fails on a
//!   check-then-set race that drops the active lock between the guard and the
//!   assignment).
//! * `Session::abort` settles the in-flight run into a single terminal
//!   `Aborted` assistant message that persists in history, the session returns
//!   to idle, and a subsequent prompt succeeds (fails on a cancel-detached run
//!   that leaves the session stuck "processing" or drops the terminal history).
//! * `Session::record` + `pi_coding::resume_session` persist the terminal
//!   transcript to an explicit temp session file.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::{Notify, oneshot};

use pi_agent::ThinkingLevel;
use pi_ai::{
    ApiProvider, AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream,
    ContentBlock, Model, SimpleStreamFn, SimpleStreamOptions, StopReason, StreamFn,
    new_assistant_message_event_stream, register_api_provider, register_model,
    unregister_api_providers,
};
use pi_coding::{Session, SessionOptions};

/// Boxed, sendable future yielding a scripted assistant event stream. This is
/// structurally identical to `futures_util::future::BoxFuture<'static,
/// AssistantMessageEventStream>` (the registry's `SimpleStreamFn`/`StreamFn`
/// return type), so the closures below coerce without naming `BoxFuture`.
type BoxStreamFut = Pin<Box<dyn Future<Output = AssistantMessageEventStream> + Send + 'static>>;

/// Monotonic id so each registration gets a unique `api`/`source_id` and tests
/// never collide on the global provider registry when run in parallel.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
struct ScriptedResponse {
    text: String,
    block: bool,
}

impl ScriptedResponse {
    fn blocking(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            block: true,
        }
    }
    fn immediate(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            block: false,
        }
    }
}

/// A registered scripted provider. The accepted run blocks on `release` until
/// released or aborted; the first accepted call signals `started` so a test can
/// synchronize before issuing an abort.
struct ScriptedProvider {
    release: Arc<Notify>,
    started: Option<oneshot::Receiver<()>>,
    scripts: Arc<StdMutex<VecDeque<ScriptedResponse>>>,
    invoke_count: Arc<AtomicU64>,
    source_id: String,
    model: Model,
}

impl ScriptedProvider {
    fn queue(&self, responses: Vec<ScriptedResponse>) {
        *self.scripts.lock().expect("scripts lock") = responses.into();
    }
    fn release_one(&self) {
        self.release.notify_one();
    }
    fn take_started(&mut self) -> oneshot::Receiver<()> {
        self.started
            .take()
            .expect("started signal already consumed")
    }
    fn invoke_count(&self) -> u64 {
        self.invoke_count.load(Ordering::Relaxed)
    }
    fn model(&self) -> Model {
        self.model.clone()
    }
    fn unregister(&self) {
        unregister_api_providers(&self.source_id);
    }
}

fn register_scripted_provider(label: &str) -> ScriptedProvider {
    let suffix = NEXT_ID.fetch_add(1, Ordering::Relaxed).to_string();
    let api = format!("scripted-{label}-{suffix}");
    let provider_name = format!("scripted-{label}");
    let mut model = Model::default();
    model.id = format!("scripted-model-{suffix}");
    model.name = "Scripted".into();
    model.api = api.clone();
    model.provider = provider_name.clone();
    model.base_url = "http://localhost:0".into();

    let scripts: Arc<StdMutex<VecDeque<ScriptedResponse>>> =
        Arc::new(StdMutex::new(VecDeque::new()));
    let release = Arc::new(Notify::new());
    let invoke_count = Arc::new(AtomicU64::new(0));
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let started = Arc::new(StdMutex::new(Some(started_tx)));

    register_model(model.clone());

    let scripts_simple = scripts.clone();
    let release_simple = release.clone();
    let started_simple = started.clone();
    let count_simple = invoke_count.clone();
    let simple: SimpleStreamFn = Arc::new(move |model, _ctx, options| -> BoxStreamFut {
        let scripts = scripts_simple.clone();
        let release = release_simple.clone();
        let started = started_simple.clone();
        let count = count_simple.clone();
        Box::pin(async move {
            let stream = new_assistant_message_event_stream();
            let stream_clone = stream.clone();
            // The abort signal type is inferred from `StreamOptions`; we never
            // name `CancellationToken` (not a direct test dependency) and only
            // call its inherent `is_cancelled` / `cancelled` methods.
            let abort_signal = options.stream.abort_signal;
            let script = scripts
                .lock()
                .expect("scripts lock")
                .pop_front()
                .unwrap_or_else(|| ScriptedResponse {
                    text: "fallback".into(),
                    block: false,
                });
            let _ = count.fetch_add(1, Ordering::Relaxed);
            tokio::spawn(async move {
                let mut out = AssistantMessage::pending(&model);
                if abort_signal.as_ref().is_some_and(|t| t.is_cancelled()) {
                    out.stop_reason = StopReason::Aborted;
                    out.error_message = Some("aborted before start".to_string());
                    stream_clone
                        .push(AssistantMessageEvent::Start {
                            partial: out.clone(),
                        })
                        .await;
                    stream_clone
                        .push(AssistantMessageEvent::Error {
                            reason: StopReason::Aborted,
                            error: out,
                        })
                        .await;
                    return;
                }
                stream_clone
                    .push(AssistantMessageEvent::Start {
                        partial: out.clone(),
                    })
                    .await;
                if let Some(tx) = started.lock().expect("started lock").take() {
                    let _ = tx.send(());
                }
                if script.block {
                    let released = release.notified();
                    tokio::pin!(released);
                    let aborted = async {
                        match &abort_signal {
                            Some(token) => token.cancelled().await,
                            None => std::future::pending::<()>().await,
                        }
                    };
                    tokio::pin!(aborted);
                    tokio::select! {
                        _ = &mut released => {}
                        _ = &mut aborted => {
                            out.stop_reason = StopReason::Aborted;
                            out.error_message =
                                Some("aborted while waiting for release".to_string());
                            stream_clone
                                .push(AssistantMessageEvent::Error {
                                    reason: StopReason::Aborted,
                                    error: out,
                                })
                                .await;
                            return;
                        }
                    }
                }
                let text = script.text;
                out.content.push(ContentBlock::text(text.clone()));
                stream_clone
                    .push(AssistantMessageEvent::TextDelta {
                        content_index: 0,
                        delta: text,
                        partial: out.clone(),
                    })
                    .await;
                out.stop_reason = StopReason::Stop;
                stream_clone
                    .push(AssistantMessageEvent::Done {
                        reason: StopReason::Stop,
                        message: out,
                    })
                    .await;
            });
            stream
        })
    });

    let simple_for_stream = simple.clone();
    let stream_fn: StreamFn = Arc::new(move |model, context, options| -> BoxStreamFut {
        let simple = simple_for_stream.clone();
        Box::pin(async move { simple(model, context, SimpleStreamOptions::from(options)).await })
    });

    let source_id = format!("scripted:{suffix}");
    register_api_provider(
        ApiProvider {
            api: api.clone(),
            stream: stream_fn,
            stream_simple: simple,
        },
        Some(source_id.clone()),
    );

    ScriptedProvider {
        release,
        started: Some(started_rx),
        scripts,
        invoke_count,
        source_id,
        model,
    }
}

/// Two `run_print` calls launched concurrently against one `Session`: exactly
/// one is accepted (and blocks on the scripted provider) while the other is
/// rejected with the "already processing" guard. Releasing the gate lets the
/// accepted run complete, and the session stays reusable.
#[tokio::test]
async fn concurrent_run_print_rejects_second_with_already_processing() {
    let provider = register_scripted_provider("concurrency");
    let model = provider.model();
    let cwd = tempfile::tempdir().expect("tempdir");
    let session = Session::new(SessionOptions {
        model,
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "scripted".into(),
        compaction: None,
        stream_options: Default::default(),
        tools: None,
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
    })
    .expect("build session");

    provider.queue(vec![ScriptedResponse::blocking("concurrent answer")]);

    let mut writer_a = Vec::new();
    let mut writer_b = Vec::new();
    // Pin both run futures directly (no `tokio::spawn`, so no `Send` requirement
    // and no `JoinHandle` move semantics). `Pin<Box<F>>` is `Unpin`, so `&mut`
    // is a valid `select!` branch future and neither future is moved into the
    // macro; the still-pending (accepted) future is moved out afterward.
    let mut f1 = Box::pin(session.run_print(&mut writer_a, "race prompt"));
    let mut f2 = Box::pin(session.run_print(&mut writer_b, "race prompt"));

    // The rejected run returns the guard error synchronously; the accepted run
    // is still blocked on the scripted provider, so it is the "loser" of the
    // race. `select!` keeps the still-pending future.
    let (first, winner_is_f1) = tokio::select! {
        r = &mut f1 => (r, true),
        r = &mut f2 => (r, false),
    };
    let rejected = first;
    assert!(
        rejected.is_err(),
        "exactly one concurrent run must be rejected: {rejected:?}"
    );
    let msg = rejected.unwrap_err().to_string();
    assert!(
        msg.contains("session is already processing a prompt"),
        "rejection carries the already-processing reason: {msg}"
    );

    // Release the accepted (blocking) run and assert it completes with the
    // scripted text — proving the accepted run was not dropped or detached.
    provider.release_one();
    let accepted = if winner_is_f1 { f2 } else { f1 };
    let accepted_text = accepted
        .await
        .expect("accepted run completes with the scripted text");
    assert_eq!(accepted_text, "concurrent answer");

    // The session settles to idle and remains reusable.
    session.wait_for_idle().await;
    provider.queue(vec![ScriptedResponse::immediate("after race")]);
    let mut out = Vec::new();
    let follow = session
        .run_print(&mut out, "follow up")
        .await
        .expect("follow up succeeds after the race");
    assert_eq!(follow, "after race");

    provider.unregister();
}

/// Aborting an in-flight run settles it into a single terminal `Aborted`
/// assistant message that persists in history, the session returns to idle, and
/// a subsequent scripted prompt succeeds.
#[tokio::test]
async fn abort_settles_terminal_assistant_then_reuse() {
    let mut provider = register_scripted_provider("abort");
    let model = provider.model();
    let cwd = tempfile::tempdir().expect("tempdir");
    let session = Session::new(SessionOptions {
        model,
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "scripted".into(),
        compaction: None,
        stream_options: Default::default(),
        tools: None,
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
    })
    .expect("build session");

    provider.queue(vec![ScriptedResponse::blocking("never reached")]);

    let mut writer = Vec::new();
    let mut run = Box::pin(session.run_print(&mut writer, "blocking prompt"));
    // Advance the run until the scripted provider is in-flight and blocked at
    // the release/abort gate (signaled via `started`), without completing it.
    let started = provider.take_started();
    tokio::select! {
        result = &mut run => panic!("blocking run must not complete before abort: {result:?}"),
        _ = started => {}
    }
    assert_eq!(
        provider.invoke_count(),
        1,
        "the blocking run reached the provider"
    );

    // Abort the in-flight run and await the same run future to completion.
    session.abort().await;
    let result = run.await;
    let err = result.expect_err("aborted run returns Err").to_string();
    assert!(
        err.to_lowercase().contains("abort"),
        "aborted run error mentions abort: {err}"
    );

    // Exactly one terminal assistant persists, marked Aborted with an error.
    let history = session.history();
    let assistants: Vec<_> = history.iter().filter_map(|m| m.as_assistant()).collect();
    assert_eq!(
        assistants.len(),
        1,
        "exactly one terminal assistant persists after abort: {history:?}"
    );
    let aborted = &assistants[0];
    assert_eq!(
        aborted.stop_reason,
        StopReason::Aborted,
        "terminal assistant is Aborted"
    );
    assert!(
        aborted
            .error_message
            .as_ref()
            .is_some_and(|m| !m.is_empty()),
        "aborted assistant carries an error message"
    );

    // The session settles to idle (no stuck "processing" state).
    session.wait_for_idle().await;

    // A subsequent scripted successful prompt completes — the session is reused.
    provider.queue(vec![ScriptedResponse::immediate("reused answer")]);
    let mut second_writer = Vec::new();
    let second = session
        .run_print(&mut second_writer, "second prompt")
        .await
        .expect("second prompt after abort succeeds");
    assert_eq!(second, "reused answer", "session is reusable after abort");

    provider.unregister();
}

/// `Session::record` with a `resume_session` recorder persists the terminal
/// transcript (user + assistant) to an explicit temp session file, exercising
/// the existing store API without touching `~/.pi/agent` state.
#[tokio::test]
async fn recorder_persists_messages_to_resumed_temp_session() {
    let provider = register_scripted_provider("recorder");
    let model = provider.model();
    let cwd = tempfile::tempdir().expect("tempdir");
    let session = Session::new(SessionOptions {
        model,
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "scripted".into(),
        compaction: None,
        stream_options: Default::default(),
        tools: None,
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
    })
    .expect("build session");

    // Plant a minimal Pi v3 session header in a temp file, then resume it so
    // the recorder appends to an explicit temp path.
    let session_dir = tempfile::tempdir().expect("session dir");
    let session_path = session_dir.path().join("recorded.jsonl");
    let header = serde_json::json!({
        "type": "session",
        "version": 3,
        "id": "rec-session-1",
        "timestamp": "2026-01-01T00:00:00.000Z",
        "cwd": cwd.path().to_string_lossy().into_owned(),
    });
    std::fs::write(&session_path, format!("{}\n", header)).expect("write session header");

    let recorder = pi_coding::resume_session(&session_path).expect("resume session recorder");
    assert_eq!(
        recorder.path(),
        session_path,
        "recorder targets the temp path"
    );
    session.record(recorder).expect("attach recorder");

    provider.queue(vec![ScriptedResponse::immediate("recorded answer")]);
    let mut out = Vec::new();
    let text = session
        .run_print(&mut out, "recorded prompt")
        .await
        .expect("run with recorder attached");
    assert_eq!(text, "recorded answer");

    let content = std::fs::read_to_string(&session_path).expect("read recorded session");
    let mut records: Vec<serde_json::Value> = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        records.push(serde_json::from_str(line).expect("session line is JSON"));
    }
    assert!(!records.is_empty(), "recorder wrote entries");
    assert_eq!(records[0]["type"], "session", "header preserved");
    let assistant = records
        .iter()
        .find(|v| v["type"] == "message" && v["message"]["role"] == "assistant")
        .expect("an assistant message entry was recorded");
    let recorded_text = assistant["message"]["content"][0]["text"]
        .as_str()
        .expect("assistant text recorded");
    assert_eq!(
        recorded_text, "recorded answer",
        "recorder persisted the assistant text"
    );

    provider.unregister();
}

#[tokio::test]
async fn foreground_bash_finishing_during_run_appends_after_assistant() {
    let mut provider = register_scripted_provider("foreground-bash-order");
    let model = provider.model();
    let cwd = tempfile::tempdir().expect("tempdir");
    let session = Session::new(SessionOptions {
        model,
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "scripted".into(),
        compaction: None,
        stream_options: Default::default(),
        tools: None,
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
    })
    .expect("build session");

    provider.queue(vec![ScriptedResponse::blocking("assistant answer")]);
    let mut run = Box::pin(session.run("question", vec![]));
    tokio::select! {
        result = &mut run => panic!("run completed before provider gate: {result:?}"),
        started = provider.take_started() => started.expect("provider started"),
    }
    let bash = session.execute_bash("printf foreground", false).await.expect("foreground bash");
    assert_eq!(bash.output, "foreground");
    assert!(session.history().iter().all(|message| !matches!(message, pi_ai::Message::BashExecution(_))));
    provider.release_one();
    let run = run.await.expect("run result");
    assert_eq!(run.text, "assistant answer");
    let history = session.history();
    assert!(matches!(history.last(), Some(pi_ai::Message::BashExecution(message)) if message.output == "foreground"));
    assert!(matches!(history.get(history.len() - 2), Some(pi_ai::Message::Assistant(message)) if message.text() == "assistant answer"));
    provider.unregister();
}
