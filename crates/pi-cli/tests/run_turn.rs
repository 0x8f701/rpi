//! In-process test for the line REPL's shared `Application` turn path.
//!
//! A scripted faux provider supplies deterministic output so no real network
//! or signal injection is required.

use std::io::Write;

use pi_agent::ThinkingLevel;
use pi_ai::Model;
use pi_ai::providers::{FauxProviderOptions, FauxResponse, register_faux_provider};
use pi_coding::{Application, Session, SessionOptions};

/// A completed run streams the assistant text to the writer and appends a
/// trailing newline, returning `Ok` — proving `run_turn_to` awaits the run
/// future instead of detaching it on the non-cancel branch.
#[tokio::test]
async fn run_turn_to_completed_run_writes_stream_and_trailing_newline() {
    let mut model = Model::default();
    model.id = "faux-run-turn-1".into();
    model.name = "Faux Run Turn".into();
    model.api = "faux-run-turn-test".into();
    model.provider = "faux".into();
    model.base_url = "http://localhost:0".into();

    let reg = register_faux_provider(FauxProviderOptions {
        api: "faux-run-turn-test".into(),
        provider: "faux".into(),
        models: vec![model.clone()],
        chunk_size: 4,
    });
    reg.set_responses(vec![FauxResponse::text("turn output")]);

    let cwd = tempfile::tempdir().expect("tempdir");
    let session = Session::new(SessionOptions {
        model,
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: "faux".into(),
        compaction: None,
        stream_options: Default::default(),
        tools: None,
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
    })
    .expect("build session");
    let application = Application::new(session).await;

    let mut out = Vec::new();
    pi_cli::repl::run_turn_to(&application, "go", &mut out)
        .await
        .expect("completed run returns Ok");
    reg.unregister();

    let streamed = String::from_utf8(out).expect("utf8");
    assert!(
        streamed.contains("turn output"),
        "writer received the streamed assistant text: {streamed}"
    );
    assert!(
        streamed.ends_with('\n'),
        "run_turn_to appends a trailing newline after a completed run: {streamed:?}"
    );
    let _ = Write::flush(&mut std::io::stdout());
}
