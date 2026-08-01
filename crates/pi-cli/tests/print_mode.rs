//! In-process print-mode tests through the shared `Application` lifecycle.
//! Scripted faux providers keep output deterministic without API keys or
//! network access.

use std::io::Write;

use pi_agent::ThinkingLevel;
use pi_ai::Model;
use pi_ai::providers::{FauxProviderOptions, FauxResponse, register_faux_provider};
use pi_coding::{Application, Session, SessionOptions};

/// A print-mode run must stream the faux assistant text to the writer.
#[tokio::test]
async fn print_mode_streams_faux_text() {
    let mut model = Model::default();
    model.id = "faux-print-1".into();
    model.name = "Faux Print".into();
    model.api = "faux-print-test".into();
    model.provider = "faux".into();
    model.base_url = "http://localhost:0".into();

    let reg = register_faux_provider(FauxProviderOptions {
        api: "faux-print-test".into(),
        provider: "faux".into(),
        models: vec![model.clone()],
        chunk_size: 4,
    });
    reg.set_responses(vec![FauxResponse::text("hello streaming world")]);

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
    let text = pi_cli::session_run::run_print_to(&application, "go", &mut out, false)
        .await
        .expect("run_print completes");
    reg.unregister();

    let streamed = String::from_utf8(out).expect("utf8");
    assert!(
        streamed.contains("hello streaming world"),
        "streamed output contains the faux text: {streamed}"
    );
    assert!(
        text.contains("hello streaming world"),
        "returned final text contains the faux text: {text}"
    );
    assert!(streamed.ends_with('\n'), "trailing newline appended");
    let _ = Write::flush(&mut std::io::stdout());
}

#[tokio::test]
async fn print_mode_expands_text_and_image_arguments() {
    let mut model = Model::default();
    model.id = "faux-print-files".into();
    model.name = "Faux Print Files".into();
    model.api = "faux-print-files-test".into();
    model.provider = "faux".into();
    model.base_url = "http://localhost:0".into();

    let reg = register_faux_provider(FauxProviderOptions {
        api: "faux-print-files-test".into(),
        provider: "faux".into(),
        models: vec![model.clone()],
        chunk_size: 4,
    });
    reg.set_responses(vec![FauxResponse::text("files accepted")]);

    let cwd = tempfile::tempdir().expect("tempdir");
    std::fs::write(cwd.path().join("notes.txt"), "embedded text").expect("write text");
    let image = image::DynamicImage::new_rgb8(2, 2);
    let mut image_bytes = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut image_bytes),
            image::ImageFormat::Png,
        )
        .expect("encode png");
    std::fs::write(cwd.path().join("shot.png"), image_bytes).expect("write image");
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
    let application = Application::new(session.clone()).await;

    let mut out = Vec::new();
    pi_cli::session_run::run_print_to(
        &application,
        "inspect @notes.txt and @shot.png",
        &mut out,
        false,
    )
    .await
    .expect("file prompt completes");
    reg.unregister();

    let user = session
        .history()
        .into_iter()
        .find_map(|message| match message {
            pi_ai::Message::User(user) => Some(user),
            _ => None,
        })
        .expect("user message");
    assert!(user.content.iter().any(|block| matches!(
        block,
        pi_ai::ContentBlock::Text { text, .. }
            if text.contains("<file name=\"notes.txt\">\nembedded text\n</file>")
    )));
    assert!(user.content.iter().any(|block| matches!(
        block,
        pi_ai::ContentBlock::Image { mime_type, .. } if mime_type == "image/png"
    )));
}

#[tokio::test]
async fn print_mode_executes_goal_command_without_agent_turn() {
    let cwd = tempfile::tempdir().expect("tempdir");
    let session = Session::new(SessionOptions {
        model: Model::default(),
        cwd: cwd.path().to_path_buf(),
        system_prompt: String::new(),
        thinking_level: ThinkingLevel::Off,
        api_key: String::new(),
        compaction: None,
        stream_options: Default::default(),
        tools: Some(Vec::new()),
        before_tool_call: None,
        after_tool_call: None,
        stream_fn: None,
        auth_resolver: None,
    })
    .expect("session");
    session
        .record(
            pi_coding::start_session_in(
                cwd.path(),
                session.model().as_ref(),
                Some("off"),
                Some(cwd.path()),
                Some("print-goal"),
                None,
            )
            .expect("recorder"),
        )
        .expect("attach recorder");
    let application = Application::new(session.clone()).await;
    let mut output = Vec::new();
    let result = pi_cli::session_run::run_print_to(
        &application,
        "/goal create --tokens 20 ship cleanly",
        &mut output,
        false,
    )
    .await
    .expect("goal command");
    assert!(result.contains("active · 0/20 tokens · ship cleanly"));
    assert_eq!(session.history().len(), 0, "goal command must not run the agent");
    assert_eq!(String::from_utf8(output).unwrap(), format!("{result}\n"));
}
