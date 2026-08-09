//! End-to-end coverage for the live delegation bridge (L2): transcribed
//! speech → delegation candidate → agent turn → reply, at the pi-coding
//! boundary.
//!
//! The in-crate unit tests prove the Ptt state machine, the STT client
//! against a mock endpoint, and `is_delegation_candidate` detection in
//! isolation; these tests wire the FULL chain together with a REAL HTTP STT
//! round trip:
//!
//! 1. `run_ptt_session` captures scripted speech chunks through the public
//!    [`CaptureBackend`] trait and transcribes them through a real
//!    `SttClient` HTTP call to a local mock OpenAI-compatible endpoint.
//! 2. The terminal `Transcript` event carries the recognized text.
//! 3. `is_delegation_candidate` flags a coding task ("fix the bug in
//!    parser.rs") and leaves plain chat alone.
//! 4. The delegation is submitted through the standard `Application::prompt`
//!    path (the delegation IS an ordinary agent turn) and the faux provider
//!    reply lands in the session transcript.
//!
//! No microphone is needed: capture is scripted (`live-capture` feature is
//! not required), and the STT endpoint is a local loopback server.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use pi_ai::providers::{FauxProviderOptions, FauxResponse, register_faux_provider};
use pi_ai::{ContentBlock, Model};
use pi_coding::live::{
    CaptureBackend, PttControl, PttSessionEvent, SttClient, is_delegation_candidate,
    run_ptt_session,
};
use pi_coding::{Application, LiveRuntimeSettings, Session, SessionOptions};
use tempfile::TempDir;

/// A capture backend that yields scripted speech chunks (amplitude above the
/// speech peak) then blocks, mirroring a live mic that keeps streaming.
struct ScriptedCapture {
    chunks: VecDeque<Vec<i16>>,
}

#[async_trait::async_trait]
impl CaptureBackend for ScriptedCapture {
    async fn next_chunk(&mut self) -> anyhow::Result<Option<Vec<i16>>> {
        if let Some(chunk) = self.chunks.pop_front() {
            return Ok(Some(chunk));
        }
        std::future::pending().await
    }
}

/// 4800 samples @ 16 kHz = 0.3 s, above `MIN_UTTERANCE` on its own and
/// carrying speech amplitude.
fn speech_chunk() -> Vec<i16> {
    std::iter::repeat(2_000i16).take(4_800).collect::<Vec<i16>>()
}

/// Serve one STT request on a dedicated blocking thread and reply with the
/// given JSON body. Returns the `http://127.0.0.1:<port>` base URL.
fn serve_stt_once(transcript: &'static str) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock STT");
    let address = listener.local_addr().expect("address");
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept");
        let mut request: Vec<u8> = Vec::new();
        let mut buffer = [0u8; 4096];
        let header_end = loop {
            if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
            let read = socket.read(&mut buffer).expect("read");
            if read == 0 {
                break 0;
            }
            request.extend_from_slice(&buffer[..read]);
        };
        let head = String::from_utf8_lossy(&request[..header_end]).into_owned();
        let content_length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("content length"))
            })
            .unwrap_or(0);
        while request.len() < header_end.saturating_add(content_length) {
            let read = socket.read(&mut buffer).expect("read body");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
        }
        let body = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            format!(r#"{{"text":"{transcript}"}}"#).len(),
            format!(r#"{{"text":"{transcript}"}}"#)
        );
        socket.write_all(body.as_bytes()).expect("respond");
    });
    format!("http://{address}")
}

fn live_settings(base: &str) -> LiveRuntimeSettings {
    LiveRuntimeSettings {
        enabled: true,
        stt_base_url: base.to_owned(),
        stt_api_key: "e2e-live-key".to_owned(),
        stt_model: "whisper-1".to_owned(),
        language: None,
        allow_insecure: true,
    }
}

/// Build a session bound to a faux provider that answers the delegated turn.
fn session_with_reply(reply: &str) -> (Session, pi_ai::providers::FauxProviderRegistration) {
    let suffix = uuid::Uuid::now_v7().to_string();
    let api = format!("live-delegation-api-{suffix}");
    let provider = format!("live-delegation-provider-{suffix}");
    let model = Model {
        id: "live-delegation-model".to_owned(),
        name: "Live Delegation".to_owned(),
        api: api.clone(),
        provider: provider.clone(),
        ..Model::default()
    };
    let registration = register_faux_provider(FauxProviderOptions {
        api,
        provider,
        models: vec![model.clone()],
        chunk_size: 1,
    });
    registration.set_responses(vec![FauxResponse::text(reply)]);
    let session = Session::new(SessionOptions {
        model,
        cwd: std::env::current_dir().expect("current directory"),
        system_prompt: String::new(),
        thinking_level: pi_agent::ThinkingLevel::Off,
        api_key: String::new(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
    })
    .expect("build session");
    session.start_new_recording().expect("start recording");
    (session, registration)
}

/// Contract: the full live delegation chain — scripted speech capture,
/// real STT HTTP round trip producing a transcript, delegation-candidate
/// detection on the recognized text, and the delegated turn running through
/// the ordinary `Application::prompt` path with the reply landing in the
/// session transcript. Plain chat never reads as a delegation candidate.
#[tokio::test]
async fn transcribed_coding_task_delegates_through_application_and_replies() {
    let base = serve_stt_once("fix the bug in parser.rs");
    let settings = live_settings(&base);
    let stt = SttClient::new().expect("stt client");
    let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut chunks = VecDeque::new();
    chunks.push_back(speech_chunk());
    chunks.push_back(speech_chunk());
    let backend: Box<dyn CaptureBackend> = Box::new(ScriptedCapture { chunks });
    let session_task = tokio::spawn(run_ptt_session(
        settings,
        stt,
        backend,
        control_rx,
        event_tx,
        pi_coding::live::NO_SPEECH_TIMEOUT,
    ));

    let first = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
        .await
        .expect("first event")
        .expect("channel alive");
    assert_eq!(first, PttSessionEvent::Started);

    control_tx.send(PttControl::Release).expect("release");
    let terminal = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
        .await
        .expect("terminal event")
        .expect("channel alive");
    let transcript = match terminal {
        PttSessionEvent::Transcript { text } => text,
        other => panic!("expected Transcript event, got {other:?}"),
    };
    session_task.await.expect("session task finished");
    assert_eq!(transcript, "fix the bug in parser.rs");

    // The recognized coding task is a delegation candidate; plain chat is not.
    assert!(
        is_delegation_candidate(&transcript),
        "coding task transcript must be a delegation candidate"
    );
    assert!(
        !is_delegation_candidate("hello world"),
        "plain chat must not be a delegation candidate"
    );

    // The delegation runs as an ordinary agent turn through the standard
    // prompt path and the reply lands in the session transcript.
    let (session, registration) = session_with_reply("delegated work done");
    let application = Application::new(session).await;
    application
        .prompt(transcript.clone(), Vec::new(), None)
        .await
        .expect("delegated prompt");
    application.wait_for_idle().await;

    let messages = application.messages();
    let all_text = messages
        .iter()
        .flat_map(|message| match message {
            pi_ai::Message::User(user) => user
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            pi_ai::Message::Assistant(assistant) => assistant
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        all_text.contains("fix the bug in parser.rs"),
        "transcribed prompt must be in the transcript: {all_text}"
    );
    assert!(
        all_text.contains("delegated work done"),
        "agent reply must land in the transcript: {all_text}"
    );

    application.cleanup().await;
    registration.unregister();
}

/// Contract: a plain-chat transcript still transcribes through the same STT
/// pipeline, but is NOT a delegation candidate (the bridge only hints the UI;
/// plain chat never tracks a delegation).
#[tokio::test]
async fn plain_chat_transcript_is_not_a_delegation_candidate() {
    let base = serve_stt_once("what is the weather like");
    let settings = live_settings(&base);
    let stt = SttClient::new().expect("stt client");
    let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut chunks = VecDeque::new();
    chunks.push_back(speech_chunk());
    chunks.push_back(speech_chunk());
    let backend: Box<dyn CaptureBackend> = Box::new(ScriptedCapture { chunks });
    let session_task = tokio::spawn(run_ptt_session(
        settings,
        stt,
        backend,
        control_rx,
        event_tx,
        pi_coding::live::NO_SPEECH_TIMEOUT,
    ));
    let first = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
        .await
        .expect("first event")
        .expect("channel alive");
    assert_eq!(first, PttSessionEvent::Started);
    control_tx.send(PttControl::Release).expect("release");
    let terminal = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
        .await
        .expect("terminal event")
        .expect("channel alive");
    let transcript = match terminal {
        PttSessionEvent::Transcript { text } => text,
        other => panic!("expected Transcript event, got {other:?}"),
    };
    session_task.await.expect("session task finished");
    assert_eq!(transcript, "what is the weather like");
    assert!(
        !is_delegation_candidate(&transcript),
        "plain chat must not be a delegation candidate"
    );
}
