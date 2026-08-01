use std::sync::Arc;

use pi_ai::{ContentBlock, Model, StopReason, Usage};
use pi_coding::{
    Application, GoalContinuationDecision, GoalLifecycle, GoalPauseReason, Session, SessionOptions,
};

fn usage_session(usage: Usage) -> Session {
    let model = Model {
        id: "application-goal-model".to_owned(),
        name: "Application Goal Model".to_owned(),
        api: "application-goal-api".to_owned(),
        provider: "application-goal-provider".to_owned(),
        ..Model::default()
    };
    let stream_fn: pi_agent::StreamFn = Arc::new(move |model, _context, _options| {
        let usage = usage.clone();
        Box::pin(async move {
            let stream = pi_ai::new_assistant_message_event_stream();
            let producer = stream.clone();
            tokio::spawn(async move {
                let mut message = pi_ai::AssistantMessage::pending(&model);
                message.content.push(ContentBlock::text("done"));
                message.usage = usage;
                message.stop_reason = StopReason::Stop;
                producer.end(Some(message)).await;
            });
            stream
        })
    });
    Session::new(SessionOptions {
        model,
        cwd: std::env::current_dir().expect("cwd"),
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
    .expect("session")
}

#[tokio::test]
async fn application_charges_post_turn_usage_once_and_pauses_at_budget() {
    let directory = tempfile::tempdir().expect("session dir");
    let session = usage_session(Usage {
        input: 7,
        output: 3,
        cache_read: 2,
        cache_write: 1,
        total_tokens: 13,
        ..Usage::default()
    });
    session
        .record(
            pi_coding::start_session_in(
                std::env::current_dir().expect("cwd"),
                session.model().as_ref(),
                Some("off"),
                Some(directory.path()),
                Some("application-goal-usage"),
                None,
            )
            .expect("recorder"),
        )
        .expect("attach recorder");
    let application = Application::new(session).await;
    application
        .goal_create("finish within budget", Some(13))
        .expect("create goal");
    application
        .prompt("work".to_owned(), Vec::new(), None)
        .await
        .expect("prompt");
    application.wait_for_idle().await;

    let state = application.goal_state();
    let goal = state.current.expect("goal");
    assert_eq!(goal.usage.tokens_used, 13);
    assert_eq!(goal.lifecycle, GoalLifecycle::Paused);
    assert_eq!(goal.pause_reason, Some(GoalPauseReason::BudgetExhausted));
    assert_eq!(
        application.goal_continuation_decision(),
        GoalContinuationDecision::Paused {
            goal_id: goal.id,
            reason: GoalPauseReason::BudgetExhausted,
            revision: state.revision,
        }
    );
}

#[tokio::test]
async fn application_resume_safety_and_fork_lineage_rebuild_goal_runtime() {
    let directory = tempfile::tempdir().expect("session dir");
    let first = usage_session(Usage::default());
    let first_recorder = pi_coding::start_session_in(
        std::env::current_dir().expect("cwd"),
        first.model().as_ref(),
        Some("off"),
        Some(directory.path()),
        Some("goal-source"),
        None,
    )
    .expect("source recorder");
    let source_path = first_recorder.path();
    first.record(first_recorder).expect("attach source recorder");
    let source = Application::new(first).await;
    let original = source.goal_create("preserve lineage", Some(50)).expect("goal");

    let resumed_session = usage_session(Usage::default());
    resumed_session
        .record(pi_coding::resume_session(&source_path).expect("resume recorder"))
        .expect("attach resumed recorder");
    let resumed = Application::new(resumed_session).await;
    resumed.prepare_resumed_goal(false).expect("resume safety");
    let resumed_goal = resumed.goal_state().current.expect("resumed goal");
    assert_eq!(resumed_goal.id, original.id);
    assert_eq!(resumed_goal.lifecycle, GoalLifecycle::Paused);
    assert_eq!(resumed_goal.pause_reason, Some(GoalPauseReason::ResumeSafety));

    let fork_recorder = pi_coding::fork_session_in(
        &source_path,
        std::env::current_dir().expect("cwd"),
        Some(directory.path()),
        Some("goal-fork"),
    )
    .expect("fork recorder");
    let fork = usage_session(Usage::default());
    fork.record(fork_recorder).expect("attach fork recorder");
    let fork = Application::new(fork).await;
    fork.prepare_resumed_goal(true).expect("fork goal");
    let forked = fork.goal_state().current.expect("forked goal");
    assert_ne!(forked.id, original.id);
    assert_eq!(forked.origin_goal_id.as_deref(), Some(original.id.as_str()));
    assert_eq!(forked.objective, original.objective);
    assert_eq!(forked.token_budget, original.token_budget);
    assert_eq!(forked.lifecycle, GoalLifecycle::Paused);
    assert_eq!(forked.pause_reason, Some(GoalPauseReason::ResumeSafety));
}
