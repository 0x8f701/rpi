use std::sync::{Arc, Barrier, Mutex};
use std::thread;

use chrono::{TimeZone, Utc};
use pi_ai::{AssistantMessage, ContentBlock, Message, StopReason};
use pi_coding::{
    GOAL_SESSION_CUSTOM_TYPE, Goal, GoalContinuationDecision, GoalError, GoalEvent, GoalEventKind,
    GoalLifecycle, GoalPauseReason, GoalRuntime, GoalSessionEntry, GoalState, GoalUsage,
    GoalUsageDelta, MAX_GOAL_OBJECTIVE_BYTES, MAX_GOAL_PIN_CHARS, MAX_GOAL_PINS, SessionRecorder,
    goal_events_from_session_tree, load_session_tree, resume_session, start_session_in,
};
use serde_json::{Value, json};

fn at(second: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(second, 0).single().expect("valid timestamp")
}

fn clock_at(second: i64) -> pi_coding::GoalClockFn {
    Arc::new(move || at(second))
}

fn recorder_in(directory: &tempfile::TempDir) -> SessionRecorder {
    start_session_in(
        directory.path(),
        None,
        None,
        Some(directory.path()),
        None,
        None,
    )
    .expect("create session recorder")
}

fn persist_session(recorder: &SessionRecorder) {
    recorder.persist_now().expect("materialize session");
    recorder.close().expect("close session");
}

#[test]
fn no_goal_is_the_default_and_continuation_is_pure() {
    let runtime = GoalRuntime::memory();
    assert_eq!(runtime.get().current, None);
    assert_eq!(runtime.get().revision, 0);
    assert_eq!(
        runtime.continuation_decision(),
        GoalContinuationDecision::NoGoal
    );
    assert_eq!(runtime.get().revision, 0);
}

#[test]
fn create_validates_input_and_rejects_replacement() {
    let runtime = GoalRuntime::memory();
    assert_eq!(runtime.create("   ", None), Err(GoalError::EmptyObjective));
    assert_eq!(
        runtime.create("compile securely", Some(0)),
        Err(GoalError::InvalidTokenBudget)
    );

    let created = runtime
        .create("  compile securely  ", Some(100))
        .expect("create goal");
    assert_eq!(created.objective, "compile securely");
    assert_eq!(created.token_budget, Some(100));
    assert_eq!(created.lifecycle, GoalLifecycle::Active);
    assert_eq!(created.pause_reason, None);
    assert_eq!(created.usage, GoalUsage::default());
    assert_eq!(created.origin_goal_id, None);
    assert_eq!(runtime.get().revision, 1);
    assert_eq!(
        runtime.create("replacement", None),
        Err(GoalError::GoalAlreadyExists)
    );

    runtime.complete().expect("complete goal");
    assert_eq!(
        runtime.create("replacement after terminal", None),
        Err(GoalError::GoalAlreadyExists)
    );
}

#[test]
fn pause_resume_complete_transitions_preserve_identity_and_usage() {
    let runtime = GoalRuntime::with_clock_and_persistence(clock_at(10), Arc::new(|_| Ok(())));
    let created = runtime.create("ship core", None).expect("create");
    let paused = runtime.pause().expect("pause");
    assert_eq!(paused.id, created.id);
    assert_eq!(paused.lifecycle, GoalLifecycle::Paused);
    assert_eq!(paused.pause_reason, Some(GoalPauseReason::Manual));
    assert_eq!(runtime.get().revision, 2);

    let same_pause = runtime.pause().expect("idempotent pause");
    assert_eq!(same_pause, paused);
    assert_eq!(runtime.get().revision, 2);
    assert_eq!(
        runtime.continuation_decision(),
        GoalContinuationDecision::Paused {
            goal_id: created.id.clone(),
            reason: GoalPauseReason::Manual,
            revision: 2,
        }
    );

    let resumed = runtime.resume().expect("resume");
    assert_eq!(resumed.lifecycle, GoalLifecycle::Active);
    assert_eq!(resumed.pause_reason, None);
    assert_eq!(runtime.get().revision, 3);
    let same_resume = runtime.resume().expect("idempotent resume");
    assert_eq!(same_resume, resumed);
    assert_eq!(runtime.get().revision, 3);

    let completed = runtime.complete().expect("complete");
    assert_eq!(completed.lifecycle, GoalLifecycle::Completed);
    assert_eq!(runtime.get().revision, 4);
    assert_eq!(runtime.complete().expect("idempotent complete"), completed);
    assert_eq!(runtime.get().revision, 4);
    assert_eq!(
        runtime.continuation_decision(),
        GoalContinuationDecision::Terminal {
            goal_id: created.id,
            lifecycle: GoalLifecycle::Completed,
            revision: 4,
        }
    );
}

#[test]
fn drop_is_terminal_and_illegal_transitions_do_not_mutate() {
    let runtime = GoalRuntime::memory();
    assert_eq!(runtime.pause(), Err(GoalError::NoCurrentGoal));
    assert_eq!(runtime.resume(), Err(GoalError::NoCurrentGoal));
    assert_eq!(runtime.complete(), Err(GoalError::NoCurrentGoal));
    assert_eq!(runtime.drop(), Err(GoalError::NoCurrentGoal));
    assert_eq!(
        runtime.update_usage(GoalUsageDelta::new(1, 0)),
        Err(GoalError::NoCurrentGoal)
    );

    runtime.create("discard safely", None).expect("create");
    let dropped = runtime.drop().expect("drop");
    assert_eq!(dropped.lifecycle, GoalLifecycle::Dropped);
    let terminal = runtime.get();
    assert_eq!(runtime.drop().expect("idempotent drop"), dropped);
    assert_eq!(runtime.get(), terminal);
    assert_eq!(
        runtime.pause(),
        Err(GoalError::InvalidTransition {
            operation: "pause",
            lifecycle: GoalLifecycle::Dropped,
        })
    );
    assert_eq!(
        runtime.resume(),
        Err(GoalError::InvalidTransition {
            operation: "resume",
            lifecycle: GoalLifecycle::Dropped,
        })
    );
    assert_eq!(
        runtime.complete(),
        Err(GoalError::InvalidTransition {
            operation: "complete",
            lifecycle: GoalLifecycle::Dropped,
        })
    );
    let charged = runtime
        .update_usage(GoalUsageDelta::new(1, 1))
        .expect("charge late usage after drop");
    assert_eq!(charged.lifecycle, GoalLifecycle::Dropped);
    assert_eq!(charged.pause_reason, None);
    assert_eq!(charged.usage, GoalUsage { tokens_used: 1, active_time_seconds: 1 });
    assert_eq!(runtime.get().revision, terminal.revision + 1);
}

#[test]
fn completed_goal_rejects_drop_pause_and_resume_but_accepts_late_usage() {
    let events = Arc::new(Mutex::new(Vec::<GoalEvent>::new()));
    let captured = events.clone();
    let runtime = GoalRuntime::with_persistence(Arc::new(move |event| {
        captured.lock().expect("event lock").push(event.clone());
        Ok(())
    }));
    runtime.create("finish safely", None).expect("create");
    runtime.complete().expect("complete");
    for result in [runtime.pause(), runtime.resume(), runtime.drop()] {
        assert!(matches!(result, Err(GoalError::InvalidTransition { .. })));
    }
    let charged = runtime
        .update_usage(GoalUsageDelta::new(1, 1))
        .expect("charge late usage after completion");
    assert_eq!(charged.lifecycle, GoalLifecycle::Completed);
    assert_eq!(charged.pause_reason, None);
    assert_eq!(charged.usage, GoalUsage { tokens_used: 1, active_time_seconds: 1 });
    let replayed = GoalRuntime::from_events(&events.lock().expect("event lock")).expect("replay");
    assert_eq!(replayed.get(), runtime.get());
}

#[test]
fn usage_accumulates_and_budget_exhaustion_pauses_not_completes() {
    let runtime = GoalRuntime::memory();
    let goal = runtime.create("work to budget", Some(10)).expect("create");
    let first = runtime
        .update_usage(GoalUsageDelta::new(4, 3))
        .expect("charge usage");
    assert_eq!(first.usage.tokens_used, 4);
    assert_eq!(first.usage.active_time_seconds, 3);
    assert_eq!(first.lifecycle, GoalLifecycle::Active);
    assert_eq!(first.remaining_tokens(), Some(6));
    assert_eq!(
        runtime.continuation_decision(),
        GoalContinuationDecision::Continue {
            goal_id: goal.id.clone(),
            objective: "work to budget".to_owned(),
            remaining_tokens: Some(6),
            revision: 2,
        }
    );

    let exhausted = runtime
        .update_usage(GoalUsageDelta::new(6, 2))
        .expect("exhaust budget");
    assert_eq!(exhausted.usage.tokens_used, 10);
    assert_eq!(exhausted.usage.active_time_seconds, 5);
    assert_eq!(exhausted.lifecycle, GoalLifecycle::Paused);
    assert_eq!(
        exhausted.pause_reason,
        Some(GoalPauseReason::BudgetExhausted)
    );
    assert_ne!(exhausted.lifecycle, GoalLifecycle::Completed);
    assert_eq!(exhausted.remaining_tokens(), Some(0));
    assert_eq!(runtime.resume(), Err(GoalError::BudgetExhausted));
    assert_eq!(
        runtime.continuation_decision(),
        GoalContinuationDecision::Paused {
            goal_id: goal.id,
            reason: GoalPauseReason::BudgetExhausted,
            revision: 3,
        }
    );
    assert_eq!(
        runtime.update_usage(GoalUsageDelta::default()),
        Err(GoalError::EmptyUsageUpdate)
    );
    assert_eq!(
        runtime.complete().expect("explicit completion").lifecycle,
        GoalLifecycle::Completed
    );
}

#[test]
fn persistence_failure_rolls_back_the_transition() {
    let writes = Arc::new(Mutex::new(Vec::<GoalEvent>::new()));
    let captured = writes.clone();
    let fail_after_create = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let should_fail = fail_after_create.clone();
    let runtime = GoalRuntime::with_persistence(Arc::new(move |event| {
        if should_fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(GoalError::Persistence("disk full".to_owned()));
        }
        captured.lock().expect("event lock").push(event.clone());
        Ok(())
    }));
    runtime.create("persist atomically", None).expect("create");
    let before = runtime.get();
    fail_after_create.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        runtime.pause(),
        Err(GoalError::Persistence("disk full".to_owned()))
    );
    assert_eq!(runtime.get(), before);
    assert_eq!(writes.lock().expect("event lock").len(), 1);
}

#[test]
fn replay_restores_exact_state_and_rejects_corrupt_history() {
    let events = Arc::new(Mutex::new(Vec::<GoalEvent>::new()));
    let captured = events.clone();
    let runtime = GoalRuntime::with_persistence(Arc::new(move |event| {
        captured.lock().expect("event lock").push(event.clone());
        Ok(())
    }));
    runtime.create("resume exactly", Some(50)).expect("create");
    runtime
        .update_usage(GoalUsageDelta::new(7, 2))
        .expect("usage");
    runtime.pause().expect("pause");
    let expected = runtime.get();
    let events = events.lock().expect("event lock").clone();
    let replayed = GoalRuntime::from_events(&events).expect("replay");
    assert_eq!(replayed.get(), expected);
    assert_eq!(replayed.continuation_decision(), runtime.continuation_decision());

    let mut corrupt = events;
    corrupt[1].revision = 9;
    assert!(matches!(
        GoalRuntime::from_events(&corrupt),
        Err(GoalError::InvalidJournal(message)) if message.contains("expected revision 2")
    ));
}

#[test]
fn session_custom_events_replay_across_resume() {
    let directory = tempfile::tempdir().expect("tempdir");
    let recorder = recorder_in(&directory);
    let path = recorder.path();
    let runtime = GoalRuntime::from_session_recorder(recorder.clone()).expect("goal runtime");
    runtime.create("survive resume", Some(25)).expect("create");
    runtime
        .update_usage(GoalUsageDelta::new(5, 4))
        .expect("update usage");
    runtime.pause().expect("pause");
    let expected = runtime.get();
    persist_session(&recorder);

    let resumed = resume_session(&path).expect("resume session");
    let restored = GoalRuntime::from_session_recorder(resumed.clone()).expect("restore goal");
    assert_eq!(restored.get(), expected);
    restored.resume().expect("append after resume");
    resumed.close().expect("close resumed session");

    let reloaded = load_session_tree(&path).expect("load tree");
    let events = goal_events_from_session_tree(&reloaded).expect("decode goal events");
    assert_eq!(events.len(), 4);
    assert_eq!(events.last().expect("last event").kind, GoalEventKind::Resumed);
    assert_eq!(
        GoalRuntime::from_events(&events)
            .expect("replay resumed events")
            .get()
            .current
            .expect("goal")
            .lifecycle,
        GoalLifecycle::Active
    );
}

#[test]
fn goal_events_survive_session_compaction_boundaries() {
    let directory = tempfile::tempdir().expect("tempdir");
    let recorder = recorder_in(&directory);
    let path = recorder.path();
    let runtime = GoalRuntime::from_session_recorder(recorder.clone()).expect("goal runtime");
    runtime
        .create("remain durable through compaction", Some(100))
        .expect("create");
    recorder
        .record_compaction("checkpoint", None, 900, &[])
        .expect("record compaction");
    runtime
        .update_usage(GoalUsageDelta::new(20, 8))
        .expect("update after compaction");
    persist_session(&recorder);

    let tree = load_session_tree(&path).expect("load compacted tree");
    assert!(tree.entries.iter().any(|entry| entry.entry_type == "compaction"));
    let goal_entries = tree
        .entries
        .iter()
        .filter(|entry| entry.custom_type.as_deref() == Some(GOAL_SESSION_CUSTOM_TYPE))
        .count();
    assert_eq!(goal_entries, 2);
    let restored = GoalRuntime::from_events(
        &goal_events_from_session_tree(&tree).expect("events after compaction"),
    )
    .expect("replay after compaction")
    .get();
    let goal = restored.current.expect("current goal");
    assert_eq!(goal.objective, "remain durable through compaction");
    assert_eq!(goal.usage.tokens_used, 20);
    assert_eq!(goal.lifecycle, GoalLifecycle::Active);
}

#[test]
fn session_branch_replay_uses_only_the_active_goal_history() {
    let directory = tempfile::tempdir().expect("tempdir");
    let recorder = recorder_in(&directory);
    let runtime = GoalRuntime::from_session_recorder(recorder.clone()).expect("goal runtime");
    runtime.create("branch root", None).expect("create");
    let root_id = recorder.last_entry_id().expect("root goal entry");
    runtime.pause().expect("first branch pause");
    recorder.branch(&root_id).expect("switch branch");
    let branch_runtime = GoalRuntime::from_session_recorder(recorder.clone()).expect("branch goal");
    branch_runtime.drop().expect("drop on second branch");

    let active_events = goal_events_from_session_tree(&recorder.tree().expect("tree"))
        .expect("active branch events");
    assert_eq!(active_events.len(), 2);
    assert_eq!(active_events[1].kind, GoalEventKind::Dropped);
    assert_eq!(
        GoalRuntime::from_events(&active_events)
            .expect("replay branch")
            .get()
            .current
            .expect("goal")
            .lifecycle,
        GoalLifecycle::Dropped
    );
}

#[test]
fn serialized_goal_state_has_a_narrow_secret_free_schema() {
    let runtime = GoalRuntime::memory();
    runtime
        .create("serialize public goal state", Some(33))
        .expect("create");
    runtime
        .update_usage(GoalUsageDelta::new(3, 1))
        .expect("usage");
    let value = serde_json::to_value(runtime.get()).expect("serialize state");
    let object = value.as_object().expect("state object");
    assert_eq!(
        object.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["current", "revision"]
    );
    let goal = object["current"].as_object().expect("goal object");
    assert_eq!(
        goal.keys().map(String::as_str).collect::<Vec<_>>(),
        vec![
            "createdAt",
            "id",
            "lifecycle",
            "objective",
            "tokenBudget",
            "updatedAt",
            "usage",
        ]
    );
    let serialized = serde_json::to_string(&value).expect("serialize JSON");
    for forbidden in ["apiKey", "authorization", "credential", "secret", "bearer"] {
        assert!(!serialized.to_ascii_lowercase().contains(&forbidden.to_ascii_lowercase()));
    }
}

#[test]
fn session_entry_wire_shape_is_typed_and_versioned() {
    let runtime = GoalRuntime::memory();
    let goal = runtime.create("typed entry", None).expect("create");
    let event = GoalEvent {
        revision: 1,
        timestamp: goal.updated_at,
        kind: GoalEventKind::Created,
        goal,
    };
    assert_eq!(
        serde_json::to_value(GoalSessionEntry { version: 1, event }).expect("serialize entry")
            ["version"],
        json!(1)
    );
}

#[test]
fn concurrent_mutations_are_linearized_without_lost_usage() {
    const WORKERS: usize = 64;
    let events = Arc::new(Mutex::new(Vec::<GoalEvent>::new()));
    let captured = events.clone();
    let runtime = GoalRuntime::with_persistence(Arc::new(move |event| {
        captured.lock().expect("event lock").push(event.clone());
        Ok(())
    }));
    runtime.create("linearize accounting", None).expect("create");
    let barrier = Arc::new(Barrier::new(WORKERS));
    let mut workers = Vec::new();
    for _ in 0..WORKERS {
        let runtime = runtime.clone();
        let barrier = barrier.clone();
        workers.push(thread::spawn(move || {
            barrier.wait();
            runtime
                .update_usage(GoalUsageDelta::new(1, 2))
                .expect("concurrent usage update");
        }));
    }
    for worker in workers {
        worker.join().expect("join worker");
    }

    let state = runtime.get();
    let goal = state.current.expect("goal");
    assert_eq!(goal.usage.tokens_used, WORKERS as u64);
    assert_eq!(goal.usage.active_time_seconds, (WORKERS * 2) as u64);
    assert_eq!(state.revision, WORKERS as u64 + 1);
    let events = events.lock().expect("event lock");
    assert_eq!(events.len(), WORKERS + 1);
    assert!(
        events
            .iter()
            .enumerate()
            .all(|(index, event)| event.revision == index as u64 + 1)
    );
}

#[test]
fn concurrent_create_allows_exactly_one_goal() {
    const WORKERS: usize = 24;
    let runtime = GoalRuntime::memory();
    let barrier = Arc::new(Barrier::new(WORKERS));
    let mut workers = Vec::new();
    for index in 0..WORKERS {
        let runtime = runtime.clone();
        let barrier = barrier.clone();
        workers.push(thread::spawn(move || {
            barrier.wait();
            runtime.create(format!("candidate {index}"), None)
        }));
    }
    let results = workers
        .into_iter()
        .map(|worker| worker.join().expect("join creator"))
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| **result == Err(GoalError::GoalAlreadyExists))
            .count(),
        WORKERS - 1
    );
    assert_eq!(runtime.get().revision, 1);
}

#[test]
fn malformed_and_future_session_goal_entries_fail_closed() {
    let directory = tempfile::tempdir().expect("tempdir");
    let recorder = recorder_in(&directory);
    recorder
        .record_custom_entry(GOAL_SESSION_CUSTOM_TYPE, Some(json!({"version": 99})))
        .expect("record future entry");
    let error = goal_events_from_session_tree(&recorder.tree().expect("tree"))
        .expect_err("future goal version must fail");
    assert!(matches!(error, GoalError::InvalidJournal(_)));

    let value: Value = json!({"version": 1, "event": {"revision": "not-a-number"}});
    let another = recorder_in(&tempfile::tempdir().expect("another tempdir"));
    another
        .record_custom_entry(GOAL_SESSION_CUSTOM_TYPE, Some(value))
        .expect("record malformed entry");
    assert!(matches!(
        goal_events_from_session_tree(&another.tree().expect("tree")),
        Err(GoalError::InvalidJournal(_))
    ));
}

/// Forged journal snapshots must fail closed so resume cannot invent identity,
/// lifecycle, usage, or budget state that never happened.
#[test]
fn replay_rejects_forged_snapshot_fields_and_illegal_transitions() {
    let runtime = GoalRuntime::with_clock_and_persistence(clock_at(100), Arc::new(|_| Ok(())));
    let created = runtime.create("authentic objective", Some(20)).expect("create");
    let base = GoalEvent {
        revision: 1,
        timestamp: created.updated_at,
        kind: GoalEventKind::Created,
        goal: created.clone(),
    };

    let mut first_not_created = base.clone();
    first_not_created.kind = GoalEventKind::Resumed;
    first_not_created.goal.lifecycle = GoalLifecycle::Active;
    assert!(matches!(
        GoalRuntime::from_events(&[first_not_created]),
        Err(GoalError::InvalidJournal(message)) if message.contains("first event is not created")
    ));

    let mut created_with_usage = base.clone();
    created_with_usage.goal.usage.tokens_used = 1;
    assert!(matches!(
        GoalRuntime::from_events(&[created_with_usage]),
        Err(GoalError::InvalidJournal(message)) if message.contains("invalid created snapshot")
    ));

    let mut created_with_pause_reason = base.clone();
    created_with_pause_reason.goal.pause_reason = Some(GoalPauseReason::Manual);
    assert!(matches!(
        GoalRuntime::from_events(&[created_with_pause_reason]),
        Err(GoalError::InvalidJournal(message)) if message.contains("pause reason while active")
            || message.contains("invalid created snapshot")
    ));

    let mut created_time_skew = base.clone();
    created_time_skew.goal.updated_at = at(101);
    created_time_skew.timestamp = at(101);
    assert!(matches!(
        GoalRuntime::from_events(&[created_time_skew]),
        Err(GoalError::InvalidJournal(message)) if message.contains("invalid created snapshot")
    ));

    let mut timestamp_mismatch = base.clone();
    timestamp_mismatch.timestamp = at(99);
    assert!(matches!(
        GoalRuntime::from_events(&[timestamp_mismatch]),
        Err(GoalError::InvalidJournal(message))
            if message.contains("timestamp does not match its goal snapshot")
    ));

    let mut empty_id = base.clone();
    empty_id.goal.id.clear();
    assert!(matches!(
        GoalRuntime::from_events(&[empty_id]),
        Err(GoalError::InvalidJournal(message)) if message.contains("empty goal id")
    ));

    let mut padded_objective = base.clone();
    padded_objective.goal.objective = "  padded  ".to_owned();
    assert!(matches!(
        GoalRuntime::from_events(&[padded_objective]),
        Err(GoalError::InvalidJournal(message)) if message.contains("invalid objective")
    ));

    let mut zero_budget = base.clone();
    zero_budget.goal.token_budget = Some(0);
    assert!(matches!(
        GoalRuntime::from_events(&[zero_budget]),
        Err(GoalError::InvalidJournal(message)) if message.contains("zero token budget")
    ));

    // Legal create, then forge an identity-changing pause.
    let mut pause = GoalEvent {
        revision: 2,
        timestamp: at(110),
        kind: GoalEventKind::Paused {
            reason: GoalPauseReason::Manual,
        },
        goal: {
            let mut goal = created.clone();
            goal.lifecycle = GoalLifecycle::Paused;
            goal.pause_reason = Some(GoalPauseReason::Manual);
            goal.updated_at = at(110);
            goal
        },
    };
    let good_pause = pause.clone();
    GoalRuntime::from_events(&[base.clone(), good_pause.clone()]).expect("legal pause replay");

    pause.goal.id = "forged-id".to_owned();
    assert!(matches!(
        GoalRuntime::from_events(&[base.clone(), pause.clone()]),
        Err(GoalError::InvalidJournal(message)) if message.contains("immutable goal fields changed")
    ));

    let mut objective_changed = good_pause.clone();
    objective_changed.goal.objective = "rewritten objective".to_owned();
    assert!(matches!(
        GoalRuntime::from_events(&[base.clone(), objective_changed]),
        Err(GoalError::InvalidJournal(message)) if message.contains("immutable goal fields changed")
    ));

    let mut budget_changed = good_pause.clone();
    budget_changed.goal.token_budget = Some(999);
    assert!(matches!(
        GoalRuntime::from_events(&[base.clone(), budget_changed]),
        Err(GoalError::InvalidJournal(message)) if message.contains("immutable goal fields changed")
    ));

    let mut created_at_changed = good_pause.clone();
    created_at_changed.goal.created_at = at(1);
    assert!(matches!(
        GoalRuntime::from_events(&[base.clone(), created_at_changed]),
        Err(GoalError::InvalidJournal(message)) if message.contains("immutable goal fields changed")
    ));

    let mut time_regression = good_pause.clone();
    time_regression.goal.updated_at = at(50);
    time_regression.timestamp = at(50);
    assert!(matches!(
        GoalRuntime::from_events(&[base.clone(), time_regression]),
        Err(GoalError::InvalidJournal(message))
            if message.contains("immutable goal fields changed")
                || message.contains("moves time before goal creation")
    ));

    // Resume after budget exhaustion is illegal even if the snapshot looks active.
    let exhausted = {
        let mut goal = created.clone();
        goal.usage.tokens_used = 20;
        goal.lifecycle = GoalLifecycle::Paused;
        goal.pause_reason = Some(GoalPauseReason::BudgetExhausted);
        goal.updated_at = at(120);
        GoalEvent {
            revision: 2,
            timestamp: at(120),
            kind: GoalEventKind::UsageUpdated {
                delta: GoalUsageDelta::new(20, 0),
            },
            goal,
        }
    };
    let mut forged_resume = GoalEvent {
        revision: 3,
        timestamp: at(130),
        kind: GoalEventKind::Resumed,
        goal: {
            let mut goal = exhausted.goal.clone();
            goal.lifecycle = GoalLifecycle::Active;
            goal.pause_reason = None;
            goal.updated_at = at(130);
            goal
        },
    };
    assert!(matches!(
        GoalRuntime::from_events(&[base.clone(), exhausted.clone(), forged_resume.clone()]),
        Err(GoalError::InvalidJournal(message))
            if message.contains("invalid resume transition")
                || message.contains("active after exhausting its budget")
    ));

    // Usage that crosses budget but stays Active must fail closed.
    let mut active_after_budget = exhausted.clone();
    active_after_budget.goal.lifecycle = GoalLifecycle::Active;
    active_after_budget.goal.pause_reason = None;
    assert!(matches!(
        GoalRuntime::from_events(&[base.clone(), active_after_budget]),
        Err(GoalError::InvalidJournal(message))
            if message.contains("active after exhausting its budget")
                || message.contains("exhausted usage did not pause")
    ));

    // Claiming budget exhaustion before the budget is crossed.
    let mut false_exhaustion = GoalEvent {
        revision: 2,
        timestamp: at(120),
        kind: GoalEventKind::Paused {
            reason: GoalPauseReason::BudgetExhausted,
        },
        goal: {
            let mut goal = created.clone();
            goal.lifecycle = GoalLifecycle::Paused;
            goal.pause_reason = Some(GoalPauseReason::BudgetExhausted);
            goal.usage.tokens_used = 5;
            goal.updated_at = at(120);
            goal
        },
    };
    assert!(matches!(
        GoalRuntime::from_events(&[base.clone(), false_exhaustion]),
        Err(GoalError::InvalidJournal(message))
            if message.contains("budget exhaustion before the budget")
                || message.contains("invalid pause transition")
    ));

    // Empty usage delta in the journal is corrupt history.
    let mut empty_delta = GoalEvent {
        revision: 2,
        timestamp: at(120),
        kind: GoalEventKind::UsageUpdated {
            delta: GoalUsageDelta::default(),
        },
        goal: {
            let mut goal = created.clone();
            goal.updated_at = at(120);
            goal
        },
    };
    assert!(matches!(
        GoalRuntime::from_events(&[base.clone(), empty_delta]),
        Err(GoalError::InvalidJournal(message)) if message.contains("invalid usage transition")
    ));

    // Terminal goal followed by any event is corrupt.
    let completed = GoalEvent {
        revision: 2,
        timestamp: at(120),
        kind: GoalEventKind::Completed,
        goal: {
            let mut goal = created.clone();
            goal.lifecycle = GoalLifecycle::Completed;
            goal.updated_at = at(120);
            goal
        },
    };
    let after_terminal = GoalEvent {
        revision: 3,
        timestamp: at(130),
        kind: GoalEventKind::Dropped,
        goal: {
            let mut goal = completed.goal.clone();
            goal.lifecycle = GoalLifecycle::Dropped;
            goal.updated_at = at(130);
            goal
        },
    };
    assert!(matches!(
        GoalRuntime::from_events(&[base.clone(), completed, after_terminal]),
        Err(GoalError::InvalidJournal(message)) if message.contains("event follows a terminal lifecycle")
    ));

    // Second Created replaces an existing goal — must fail.
    let mut second_create = base.clone();
    second_create.revision = 2;
    second_create.timestamp = at(140);
    second_create.goal.updated_at = at(140);
    second_create.goal.created_at = at(140);
    second_create.goal.id = "another".to_owned();
    assert!(matches!(
        GoalRuntime::from_events(&[base, second_create]),
        Err(GoalError::InvalidJournal(message)) if message.contains("created event replaces an existing goal")
    ));

    // Silence unused mut warning paths that were reassigned intentionally.
    let _ = forged_resume.kind;
}

/// Replay must enforce the same objective byte cap as create so a forged
/// Created snapshot cannot bloat resume state past MAX_GOAL_OBJECTIVE_BYTES.
#[test]
fn replay_rejects_created_objective_over_byte_cap() {
    let timestamp = at(50);
    let event = GoalEvent {
        revision: 1,
        timestamp,
        kind: GoalEventKind::Created,
        goal: Goal {
            id: "forged-oversize".to_owned(),
            origin_goal_id: None,
            objective: "G".repeat(MAX_GOAL_OBJECTIVE_BYTES + 1),
            token_budget: Some(10),
            pins: Vec::new(),
            lifecycle: GoalLifecycle::Active,
            pause_reason: None,
            created_at: timestamp,
            updated_at: timestamp,
            usage: GoalUsage::default(),
        },
    };
    assert!(matches!(
        GoalRuntime::from_events(&[event]),
        Err(GoalError::InvalidJournal(message))
            if message.contains("objective exceeds")
                || message.contains(&MAX_GOAL_OBJECTIVE_BYTES.to_string())
    ));
}

/// Usage may accumulate while manually paused; only budget exhaustion changes
/// lifecycle, and overflow must not mutate revision or cumulative counters.
#[test]
fn paused_usage_accumulates_without_auto_resume_and_overflow_is_atomic() {
    let runtime = GoalRuntime::memory();
    let created = runtime
        .create("paused accounting", Some(100))
        .expect("create");
    runtime.pause().expect("manual pause");
    let paused_revision = runtime.get().revision;

    let charged = runtime
        .update_usage(GoalUsageDelta::new(9, 4))
        .expect("usage while paused");
    assert_eq!(charged.lifecycle, GoalLifecycle::Paused);
    assert_eq!(charged.pause_reason, Some(GoalPauseReason::Manual));
    assert_eq!(charged.usage.tokens_used, 9);
    assert_eq!(charged.usage.active_time_seconds, 4);
    assert_eq!(runtime.get().revision, paused_revision + 1);
    assert_eq!(
        runtime.continuation_decision(),
        GoalContinuationDecision::Paused {
            goal_id: created.id.clone(),
            reason: GoalPauseReason::Manual,
            revision: paused_revision + 1,
        }
    );

    // Crossing the budget from a manual pause becomes budget-exhausted pause,
    // never Active and never Completed.
    let exhausted = runtime
        .update_usage(GoalUsageDelta::new(91, 1))
        .expect("exhaust while paused");
    assert_eq!(exhausted.lifecycle, GoalLifecycle::Paused);
    assert_eq!(
        exhausted.pause_reason,
        Some(GoalPauseReason::BudgetExhausted)
    );
    assert_eq!(exhausted.usage.tokens_used, 100);
    assert_eq!(runtime.resume(), Err(GoalError::BudgetExhausted));

    // Overflow must leave counters and revision untouched.
    let before_overflow = runtime.get();
    assert_eq!(
        runtime.update_usage(GoalUsageDelta::new(u64::MAX, 0)),
        Err(GoalError::UsageOverflow)
    );
    assert_eq!(
        runtime.update_usage(GoalUsageDelta::new(0, u64::MAX)),
        Err(GoalError::UsageOverflow)
    );
    assert_eq!(runtime.get(), before_overflow);

    // Exact budget boundary from Active also pauses, not completes.
    let boundary = GoalRuntime::memory();
    boundary.create("exact boundary", Some(5)).expect("create");
    let hit = boundary
        .update_usage(GoalUsageDelta::new(5, 0))
        .expect("exact budget");
    assert_eq!(hit.usage.tokens_used, 5);
    assert_eq!(hit.lifecycle, GoalLifecycle::Paused);
    assert_eq!(hit.pause_reason, Some(GoalPauseReason::BudgetExhausted));
    assert_eq!(hit.remaining_tokens(), Some(0));
    assert_ne!(hit.lifecycle, GoalLifecycle::Completed);
}

/// Wall-clock regression must not reverse goal.updated_at; commit timestamps
/// stay monotonic so replay ordering cannot be forged by a skewed clock.
#[test]
fn clock_regression_keeps_updated_at_monotonic() {
    let seconds = Arc::new(Mutex::new(50_i64));
    let clock_seconds = seconds.clone();
    let clock: pi_coding::GoalClockFn = Arc::new(move || {
        let second = *clock_seconds.lock().expect("clock lock");
        at(second)
    });
    let runtime = GoalRuntime::with_clock_and_persistence(clock, Arc::new(|_| Ok(())));
    let created = runtime.create("monotonic time", None).expect("create");
    assert_eq!(created.updated_at, at(50));

    *seconds.lock().expect("clock lock") = 10;
    let paused = runtime.pause().expect("pause after clock skew");
    assert_eq!(paused.updated_at, at(50));
    assert!(paused.updated_at >= created.updated_at);

    *seconds.lock().expect("clock lock") = 60;
    let resumed = runtime.resume().expect("resume after recovery");
    assert_eq!(resumed.updated_at, at(60));
}

/// Unrelated custom session entries must be ignored; missing goal data fails closed.
#[test]
fn session_tree_ignores_unrelated_custom_and_rejects_missing_goal_data() {
    let directory = tempfile::tempdir().expect("tempdir");
    let recorder = recorder_in(&directory);
    recorder
        .record_custom_entry("pi.unrelated.marker", Some(json!({"ok": true})))
        .expect("unrelated custom");
    recorder
        .record_custom_entry(GOAL_SESSION_CUSTOM_TYPE, None)
        .expect("goal entry without data");

    let error = goal_events_from_session_tree(&recorder.tree().expect("tree"))
        .expect_err("missing goal data must fail closed");
    assert!(matches!(
        error,
        GoalError::InvalidJournal(message) if message.contains("has no data")
    ));

    let clean = recorder_in(&tempfile::tempdir().expect("clean tempdir"));
    clean
        .record_custom_entry("pi.other", Some(json!({"x": 1})))
        .expect("noise");
    let events = goal_events_from_session_tree(&clean.tree().expect("tree")).expect("ignore noise");
    assert!(events.is_empty());
}

/// Separate GoalRuntime facades over one recorder each keep private revision
/// counters. A stale facade must be rejected with Persistence and must leave the
/// durable journal linear (rev 1 create, rev 2 primary usage only).
#[test]
fn dual_runtime_shared_recorder_rejects_stale_revision_append() {
    let directory = tempfile::tempdir().expect("tempdir");
    let recorder = recorder_in(&directory);
    let path = recorder.path();
    let primary = GoalRuntime::from_session_recorder(recorder.clone()).expect("primary");
    primary
        .create("single writer goal", Some(500))
        .expect("create");

    let stale = GoalRuntime::from_session_recorder(recorder.clone()).expect("stale sibling");
    assert_eq!(stale.get().revision, 1);

    primary
        .update_usage(GoalUsageDelta::new(3, 1))
        .expect("primary usage");
    assert_eq!(primary.get().revision, 2);

    let stale_err = stale
        .update_usage(GoalUsageDelta::new(4, 0))
        .expect_err("stale sibling must not append a duplicate revision");
    assert!(
        matches!(stale_err, GoalError::Persistence(_)),
        "stale append must surface persistence CAS failure, got {stale_err:?}"
    );
    assert_eq!(stale.get().revision, 1, "failed stale write must not advance local revision");

    persist_session(&recorder);
    let events = goal_events_from_session_tree(&load_session_tree(&path).expect("load tree"))
        .expect("linear journal decodes");
    assert_eq!(events.len(), 2);
    assert!(
        events
            .iter()
            .enumerate()
            .all(|(index, event)| event.revision == index as u64 + 1)
    );
    let restored = GoalRuntime::from_events(&events).expect("replay linear journal");
    assert_eq!(restored.get().revision, 2);
    assert_eq!(restored.get().current.expect("goal").usage.tokens_used, 3);
}

/// After branching away from later goal events, a stale runtime must not append
/// onto the new branch tip. Branch selection is in-memory until the next append,
/// so durable reload may still see the physical create+pause chain; the contract
/// is CAS rejection plus create-only active history on the live recorder tree.
#[test]
fn stale_runtime_after_branch_cannot_corrupt_active_history() {
    let directory = tempfile::tempdir().expect("tempdir");
    let recorder = recorder_in(&directory);
    let path = recorder.path();
    let runtime = GoalRuntime::from_session_recorder(recorder.clone()).expect("runtime");
    runtime.create("branch root goal", None).expect("create");
    let create_entry_id = recorder.last_entry_id().expect("create entry id");
    runtime.pause().expect("pause becomes rev2 on first branch");
    assert_eq!(runtime.get().revision, 2);

    recorder.branch(&create_entry_id).expect("branch to create");

    let live_before = goal_events_from_session_tree(&recorder.tree().expect("live tree after branch"))
        .expect("active branch decodes");
    assert_eq!(live_before.len(), 1, "live tree must be create-only after branch");
    assert_eq!(live_before[0].kind, GoalEventKind::Created);

    let stale_err = runtime
        .drop()
        .expect_err("stale post-branch writer must be rejected");
    assert!(
        matches!(stale_err, GoalError::Persistence(_)),
        "stale branch append must surface persistence CAS failure, got {stale_err:?}"
    );
    assert_eq!(runtime.get().revision, 2, "rejected drop must not mutate runtime");

    let live_after = goal_events_from_session_tree(&recorder.tree().expect("live tree after reject"))
        .expect("active branch still decodes");
    assert_eq!(live_after.len(), 1);
    assert_eq!(live_after[0].kind, GoalEventKind::Created);
    GoalRuntime::from_events(&live_after).expect("create-only branch replays");

    // Disk reload without a post-branch append may still surface the physical
    // create+pause chain; that journal must remain linear and replayable.
    persist_session(&recorder);
    let durable = goal_events_from_session_tree(&load_session_tree(&path).expect("durable tree"))
        .expect("durable journal decodes");
    assert!(
        durable.len() == 1 || durable.len() == 2,
        "durable history must stay create-only or create+pause, got {durable:?}"
    );
    let restored = GoalRuntime::from_events(&durable).expect("durable history replays");
    assert_eq!(restored.get().revision, durable.len() as u64);
    assert!(matches!(
        restored.get().current.expect("goal").lifecycle,
        GoalLifecycle::Active | GoalLifecycle::Paused
    ));
    assert_ne!(
        restored.get().current.expect("goal").lifecycle,
        GoalLifecycle::Dropped,
        "rejected stale drop must never land on durable history"
    );
}

/// Persistence failure during create must leave an empty runtime and an empty
/// journal so callers never observe a half-created goal.
#[test]
fn create_persistence_failure_leaves_runtime_empty() {
    let runtime = GoalRuntime::with_persistence(Arc::new(|_| {
        Err(GoalError::Persistence("create refused".to_owned()))
    }));
    assert_eq!(
        runtime.create("never lands", Some(10)),
        Err(GoalError::Persistence("create refused".to_owned()))
    );
    assert_eq!(runtime.get().current, None);
    assert_eq!(runtime.get().revision, 0);
    assert_eq!(
        runtime.continuation_decision(),
        GoalContinuationDecision::NoGoal
    );
}

/// Objectives above the UTF-8 byte cap must be rejected with ObjectiveTooLong
/// and leave both the runtime and durable journal empty.
#[test]
fn oversized_objective_does_not_create_unreplayable_session() {
    let directory = tempfile::tempdir().expect("tempdir");
    let recorder = recorder_in(&directory);
    let path = recorder.path();
    let runtime = GoalRuntime::from_session_recorder(recorder.clone()).expect("runtime");
    let objective = "G".repeat(MAX_GOAL_OBJECTIVE_BYTES + 1);

    assert_eq!(
        runtime.create(objective, Some(10)),
        Err(GoalError::ObjectiveTooLong {
            max_bytes: MAX_GOAL_OBJECTIVE_BYTES,
        })
    );
    assert_eq!(runtime.get().current, None);
    assert_eq!(runtime.get().revision, 0);
    assert_eq!(
        runtime.continuation_decision(),
        GoalContinuationDecision::NoGoal
    );

    recorder.persist_now().ok();
    recorder.close().ok();
    if path.exists() {
        if let Ok(tree) = load_session_tree(&path) {
            let events = goal_events_from_session_tree(&tree).expect("no corrupt goal entries");
            assert!(events.is_empty(), "rejected create must leave no goal events");
        }
    }
}

/// Early `persist_now` before any assistant message must not make later goal
/// events invisible to `tree()` / restore, and must not drop prior goal events
/// when the first assistant message finally flushes the session.
#[test]
fn early_persist_now_preserves_pending_goal_events_across_restore() {
    let directory = tempfile::tempdir().expect("tempdir");
    let recorder = recorder_in(&directory);
    let path = recorder.path();
    let runtime = GoalRuntime::from_session_recorder(recorder.clone()).expect("runtime");
    runtime
        .create("survive early flush", Some(40))
        .expect("create");

    // Force materialization while has_assistant is still false after return.
    recorder.persist_now().expect("early persist_now");

    runtime
        .update_usage(GoalUsageDelta::new(5, 2))
        .expect("usage after early flush");
    assert_eq!(runtime.get().revision, 2);

    // Live tree and a second facade restored from the same recorder must both
    // observe create + usage — disk-only views after flushed=true are a bug.
    let live_events = goal_events_from_session_tree(&recorder.tree().expect("live tree"))
        .expect("live goal events");
    assert_eq!(
        live_events.len(),
        2,
        "tree() must include pending goal events after early persist_now; got {live_events:?}"
    );
    let restored = GoalRuntime::from_session_recorder(recorder.clone()).expect("restore facade");
    assert_eq!(restored.get().revision, 2);
    assert_eq!(
        restored.get().current.expect("goal").usage.tokens_used,
        5
    );

    // First assistant append must keep the full goal journal, not only the
    // latest pending record.
    let mut assistant = AssistantMessage::pending(&dummy_model());
    assistant.content = vec![ContentBlock::text("ack")];
    assistant.stop_reason = StopReason::Stop;
    recorder
        .record_message(&Message::Assistant(assistant))
        .expect("assistant flush");
    recorder.close().expect("close after assistant");

    let durable = goal_events_from_session_tree(&load_session_tree(&path).expect("load durable"))
        .expect("durable goal events");
    assert_eq!(durable.len(), 2, "assistant flush must retain create+usage; got {durable:?}");
    let final_runtime = GoalRuntime::from_events(&durable).expect("replay durable");
    assert_eq!(final_runtime.get().revision, 2);
    assert_eq!(
        final_runtime.get().current.expect("goal").usage.tokens_used,
        5
    );
}

#[test]
fn goal_create_and_update_before_first_assistant_survive_close_and_reopen() {
    let directory = tempfile::tempdir().expect("tempdir");
    let recorder = recorder_in(&directory);
    let path = recorder.path();
    let runtime = GoalRuntime::from_session_recorder(recorder.clone()).expect("runtime");

    runtime.create("durable before assistant", Some(20)).expect("create");
    runtime
        .update_usage(GoalUsageDelta::new(4, 2))
        .expect("update");
    recorder.close().expect("close without persist_now");

    let resumed = resume_session(&path).expect("resume materialized goal session");
    let restored = GoalRuntime::from_session_recorder(resumed.clone()).expect("restore runtime");
    assert_eq!(restored.get(), runtime.get());
    resumed.close().expect("close resumed session");
}

#[test]
fn stale_topology_token_cannot_redirect_validated_goal_append() {
    let directory = tempfile::tempdir().expect("tempdir");
    let recorder = recorder_in(&directory);
    let runtime = GoalRuntime::from_session_recorder(recorder.clone()).expect("runtime");
    runtime.create("topology guarded", None).expect("create");
    let create_entry = recorder.last_entry_id().expect("create entry");
    let token = recorder.append_token();

    recorder.reset_leaf();
    recorder.branch(&create_entry).expect("return to original leaf");
    let before = recorder.tree().expect("tree before stale append");
    let error = recorder
        .record_custom_entry_durable_checked(
            &token,
            GOAL_SESSION_CUSTOM_TYPE,
            &json!({"not": "used"}),
            |_| Ok(()),
        )
        .expect_err("ABA navigation must invalidate the topology token");
    assert!(error.to_string().contains("session changed"));
    let after = recorder.tree().expect("tree after stale append");
    assert_eq!(after.active_leaf_id, before.active_leaf_id);
    assert_eq!(
        serde_json::to_value(after.entries).expect("serialize after entries"),
        serde_json::to_value(before.entries).expect("serialize before entries")
    );
    assert_eq!(runtime.get().revision, 1);
}

#[test]
fn checked_serialization_failure_leaves_goal_runtime_tree_and_file_unchanged() {
    #[derive(serde::Serialize)]
    struct FailsSerialization {
        #[serde(serialize_with = "fail_serialization")]
        value: (),
    }

    fn fail_serialization<S>(_: &(), _: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        Err(serde::ser::Error::custom("injected serialization failure"))
    }

    let directory = tempfile::tempdir().expect("tempdir");
    let recorder = recorder_in(&directory);
    let path = recorder.path();
    let runtime = GoalRuntime::from_session_recorder(recorder.clone()).expect("runtime");
    runtime.create("stable serialization", None).expect("create");
    let before_state = runtime.get();
    let before_tree = recorder.tree().expect("tree before failure");
    let before_file = std::fs::read(&path).expect("file before failure");
    let token = recorder.append_token();

    let error = recorder
        .record_custom_entry_durable_checked(
            &token,
            GOAL_SESSION_CUSTOM_TYPE,
            &FailsSerialization { value: () },
            |_| Ok(()),
        )
        .expect_err("serialization must fail");
    assert!(error.to_string().contains("injected serialization failure"));
    assert_eq!(runtime.get(), before_state);
    let after_tree = recorder.tree().expect("tree after failure");
    assert_eq!(after_tree.active_leaf_id, before_tree.active_leaf_id);
    assert_eq!(
        serde_json::to_value(after_tree.entries).expect("serialize after entries"),
        serde_json::to_value(before_tree.entries).expect("serialize before entries")
    );
    assert_eq!(std::fs::read(&path).expect("file after failure"), before_file);
}

#[test]
fn durable_goal_write_failure_leaves_runtime_tree_and_file_unchanged() {
    let directory = tempfile::tempdir().expect("tempdir");
    let recorder = recorder_in(&directory);
    let path = recorder.path();
    let runtime = GoalRuntime::from_session_recorder(recorder.clone()).expect("runtime");
    let before_tree = recorder.tree().expect("tree before write failure");
    std::fs::write(&path, b"occupied").expect("occupy recorder path");
    let before_file = std::fs::read(&path).expect("file before write failure");

    let error = runtime
        .create("cannot reach disk", None)
        .expect_err("create_new conflict must fail");
    assert!(matches!(error, GoalError::Persistence(_)));
    assert_eq!(runtime.get().revision, 0);
    assert_eq!(runtime.get().current, None);
    let after_tree = recorder.tree().expect("tree after write failure");
    assert_eq!(after_tree.active_leaf_id, before_tree.active_leaf_id);
    assert_eq!(
        serde_json::to_value(after_tree.entries).expect("serialize after entries"),
        serde_json::to_value(before_tree.entries).expect("serialize before entries")
    );
    assert_eq!(std::fs::read(&path).expect("file after write failure"), before_file);
}



fn dummy_model() -> pi_ai::Model {
    pi_ai::Model {
        id: "goal-test".into(),
        name: "goal-test".into(),
        api: "openai-completions".into(),
        provider: "test".into(),
        base_url: "http://127.0.0.1".into(),
        context_window: 8_192,
        max_tokens: 1_024,
        ..pi_ai::Model::default()
    }
}

#[test]
fn pause_on_resume_safety_pauses_only_active_goals_and_replays() {
    let events = Arc::new(Mutex::new(Vec::<GoalEvent>::new()));
    let captured = events.clone();
    let runtime = GoalRuntime::with_persistence(Arc::new(move |event| {
        captured.lock().expect("event lock").push(event.clone());
        Ok(())
    }));
    let created = runtime.create("resume deliberately", Some(100)).expect("create");
    runtime
        .update_usage(GoalUsageDelta::new(7, 3))
        .expect("charge usage");

    let paused = runtime.pause_on_resume().expect("safety pause");
    assert_eq!(paused.id, created.id);
    assert_eq!(paused.lifecycle, GoalLifecycle::Paused);
    assert_eq!(paused.pause_reason, Some(GoalPauseReason::ResumeSafety));
    assert_eq!(paused.usage.tokens_used, 7);
    assert_eq!(runtime.get().revision, 3);
    assert_eq!(
        runtime.pause_on_resume().expect("idempotent safety pause"),
        paused
    );
    assert_eq!(runtime.get().revision, 3);

    let replayed = GoalRuntime::from_events(&events.lock().expect("events")).expect("replay");
    assert_eq!(replayed.get(), runtime.get());
    assert_eq!(
        runtime.resume().expect("explicitly resume safety pause").lifecycle,
        GoalLifecycle::Active
    );

    let terminal = GoalRuntime::memory();
    terminal.create("already finished", None).expect("create terminal");
    terminal.complete().expect("complete");
    let before = terminal.get();
    assert_eq!(terminal.pause_on_resume().expect("leave terminal"), before.current.clone().expect("goal"));
    assert_eq!(terminal.get(), before);
}

#[test]
fn fork_clone_creates_lineage_and_copies_state_with_safe_lifecycle() {
    let source_runtime = GoalRuntime::memory();
    source_runtime
        .create("continue in fork", Some(200))
        .expect("create source");
    let source = source_runtime
        .update_usage(GoalUsageDelta::new(31, 9))
        .expect("source usage");

    let events = Arc::new(Mutex::new(Vec::<GoalEvent>::new()));
    let captured = events.clone();
    let fork_runtime = GoalRuntime::with_persistence(Arc::new(move |event| {
        captured.lock().expect("events").push(event.clone());
        Ok(())
    }));
    let cloned = fork_runtime.fork_clone(&source).expect("fork clone");

    assert_ne!(cloned.id, source.id);
    assert_eq!(cloned.origin_goal_id.as_deref(), Some(source.id.as_str()));
    assert_eq!(cloned.objective, source.objective);
    assert_eq!(cloned.token_budget, source.token_budget);
    assert_eq!(cloned.usage, source.usage);
    assert_eq!(cloned.lifecycle, GoalLifecycle::Paused);
    assert_eq!(cloned.pause_reason, Some(GoalPauseReason::ResumeSafety));
    assert_eq!(fork_runtime.get().revision, 1);
    assert!(matches!(
        &events.lock().expect("events")[0].kind,
        GoalEventKind::ForkCloned { source: recorded } if recorded == &source
    ));
    assert_eq!(
        GoalRuntime::from_events(&events.lock().expect("events"))
            .expect("replay fork")
            .get(),
        fork_runtime.get()
    );
}

#[test]
fn fork_clone_preserves_paused_and_terminal_state() {
    let paused_source = GoalRuntime::memory();
    paused_source.create("paused source", None).expect("create");
    let paused = paused_source.pause().expect("pause source");
    let paused_clone = GoalRuntime::memory().fork_clone(&paused).expect("clone paused");
    assert_eq!(paused_clone.lifecycle, GoalLifecycle::Paused);
    assert_eq!(paused_clone.pause_reason, Some(GoalPauseReason::Manual));
    assert_eq!(paused_clone.origin_goal_id.as_deref(), Some(paused.id.as_str()));

    let completed_source = GoalRuntime::memory();
    completed_source.create("completed source", None).expect("create");
    let completed = completed_source.complete().expect("complete source");
    let completed_clone = GoalRuntime::memory()
        .fork_clone(&completed)
        .expect("clone completed");
    assert_eq!(completed_clone.lifecycle, GoalLifecycle::Completed);
    assert_eq!(completed_clone.pause_reason, None);

    let dropped_source = GoalRuntime::memory();
    dropped_source.create("dropped source", None).expect("create");
    let dropped = dropped_source.drop().expect("drop source");
    let dropped_clone = GoalRuntime::memory().fork_clone(&dropped).expect("clone dropped");
    assert_eq!(dropped_clone.lifecycle, GoalLifecycle::Dropped);
    assert_eq!(dropped_clone.pause_reason, None);
}

#[test]
fn fork_clone_is_durable_and_forged_lineage_fails_closed() {
    let source_runtime = GoalRuntime::memory();
    source_runtime.create("durable fork", Some(80)).expect("create source");
    let source = source_runtime.pause().expect("pause source");

    let directory = tempfile::tempdir().expect("tempdir");
    let recorder = recorder_in(&directory);
    let path = recorder.path();
    let fork_runtime = GoalRuntime::from_session_recorder(recorder.clone()).expect("fork runtime");
    let cloned = fork_runtime.fork_clone(&source).expect("persist fork clone");
    recorder.close().expect("close fork recorder");

    let resumed = resume_session(&path).expect("resume fork session");
    let restored = GoalRuntime::from_session_recorder(resumed.clone()).expect("restore fork goal");
    assert_eq!(restored.get(), fork_runtime.get());
    assert_eq!(restored.get().current.expect("goal").origin_goal_id, Some(source.id.clone()));
    resumed.close().expect("close resumed");

    let mut event = GoalEvent {
        revision: 1,
        timestamp: cloned.updated_at,
        kind: GoalEventKind::ForkCloned {
            source: source.clone(),
        },
        goal: cloned,
    };
    event.goal.origin_goal_id = Some("forged-origin".to_owned());
    assert!(matches!(
        GoalRuntime::from_events(&[event]),
        Err(GoalError::InvalidJournal(message)) if message.contains("invalid fork clone snapshot")
    ));
}

#[test]
fn fork_clone_persistence_failure_is_atomic() {
    let source_runtime = GoalRuntime::memory();
    let source = source_runtime.create("source", None).expect("source");
    let fork_runtime = GoalRuntime::with_persistence(Arc::new(|_| {
        Err(GoalError::Persistence("fork write refused".to_owned()))
    }));
    assert_eq!(
        fork_runtime.fork_clone(&source),
        Err(GoalError::Persistence("fork write refused".to_owned()))
    );
    assert_eq!(fork_runtime.get().revision, 0);
    assert_eq!(fork_runtime.get().current, None);
}

#[test]
fn fork_clone_transitions_a_copied_source_journal_to_new_lineage() {
    let directory = tempfile::tempdir().expect("tempdir");
    let recorder = recorder_in(&directory);
    let path = recorder.path();
    let source_runtime = GoalRuntime::from_session_recorder(recorder.clone()).expect("source runtime");
    source_runtime
        .create("copied journal fork", Some(120))
        .expect("create source");
    source_runtime
        .update_usage(GoalUsageDelta::new(11, 5))
        .expect("source usage");

    let copied = GoalRuntime::from_session_recorder(recorder.clone())
        .expect("rebuild from copied source journal");
    let source = copied.get().current.expect("copied source goal");
    let cloned = copied
        .fork_clone(&source)
        .expect("fork clone copied journal");
    assert_ne!(cloned.id, source.id);
    assert_eq!(cloned.origin_goal_id.as_deref(), Some(source.id.as_str()));
    assert_eq!(cloned.lifecycle, GoalLifecycle::Paused);
    assert_eq!(cloned.pause_reason, Some(GoalPauseReason::ResumeSafety));
    assert_eq!(cloned.objective, source.objective);
    assert_eq!(cloned.token_budget, source.token_budget);
    assert_eq!(cloned.usage, source.usage);

    recorder.close().expect("close copied fork session");
    let resumed = resume_session(&path).expect("resume copied fork session");
    let restored = GoalRuntime::from_session_recorder(resumed.clone()).expect("restore cloned goal");
    assert_eq!(restored.get(), copied.get());
    assert_eq!(restored.get().revision, 3);
    resumed.close().expect("close restored fork session");
}

#[test]
fn pins_lifecycle_pin_list_unpin_and_replay() {
    let events = Arc::new(Mutex::new(Vec::<GoalEvent>::new()));
    let captured = events.clone();
    let runtime = GoalRuntime::with_persistence(Arc::new(move |event| {
        captured.lock().expect("event lock").push(event.clone());
        Ok(())
    }));
    runtime.create("pin the workflow", Some(100)).expect("create");
    assert!(runtime.pins().is_empty());
    assert_eq!(runtime.get().current.expect("goal").pins, Vec::<String>::new());

    let pinned = runtime.pin("keep the release checklist in scope").expect("pin");
    assert_eq!(pinned.pins, vec!["keep the release checklist in scope".to_owned()]);
    assert_eq!(runtime.pins(), vec!["keep the release checklist in scope".to_owned()]);
    assert_eq!(runtime.get().revision, 2);
    assert!(matches!(
        &events.lock().expect("events")[1].kind,
        GoalEventKind::PinsUpdated { pins } if pins == &vec!["keep the release checklist in scope".to_owned()]
    ));

    runtime.pin("reference the omp skill-card style").expect("second pin");
    assert_eq!(runtime.pins().len(), 2);

    let unpinned = runtime.unpin(0).expect("unpin first");
    assert_eq!(unpinned.pins, vec!["reference the omp skill-card style".to_owned()]);
    assert_eq!(runtime.pins(), vec!["reference the omp skill-card style".to_owned()]);
    assert_eq!(runtime.get().revision, 4);
    assert!(matches!(
        &events.lock().expect("events")[3].kind,
        GoalEventKind::PinsUpdated { pins } if pins == &vec!["reference the omp skill-card style".to_owned()]
    ));

    let replayed = GoalRuntime::from_events(&events.lock().expect("events")).expect("replay");
    assert_eq!(replayed.get(), runtime.get());
    assert_eq!(replayed.pins(), runtime.pins());
    assert_eq!(
        replayed.continuation_decision(),
        runtime.continuation_decision()
    );
}

#[test]
fn pins_validate_empty_length_limit_and_index_bounds() {
    let runtime = GoalRuntime::memory();
    assert_eq!(runtime.pin("no goal yet"), Err(GoalError::NoCurrentGoal));
    runtime.create("bounded pins", Some(50)).expect("create");
    assert_eq!(runtime.pin("   "), Err(GoalError::EmptyPin));
    assert_eq!(
        runtime.pin("G".repeat(MAX_GOAL_PIN_CHARS + 1)),
        Err(GoalError::PinTooLong {
            max_chars: MAX_GOAL_PIN_CHARS,
        })
    );
    for index in 0..MAX_GOAL_PINS {
        runtime.pin(format!("pin {index}")).expect("pin within limit");
    }
    assert_eq!(runtime.pins().len(), MAX_GOAL_PINS);
    assert_eq!(
        runtime.pin("overflow"),
        Err(GoalError::PinLimitReached {
            max_pins: MAX_GOAL_PINS,
        })
    );
    assert_eq!(
        runtime.unpin(MAX_GOAL_PINS),
        Err(GoalError::PinIndexOutOfRange {
            index: MAX_GOAL_PINS,
            len: MAX_GOAL_PINS,
        })
    );
    let after_unpin = runtime.unpin(0).expect("unpin");
    assert_eq!(after_unpin.pins.len(), MAX_GOAL_PINS - 1);
    assert_eq!(
        runtime.unpin(MAX_GOAL_PINS - 1),
        Err(GoalError::PinIndexOutOfRange {
            index: MAX_GOAL_PINS - 1,
            len: MAX_GOAL_PINS - 1,
        })
    );
    // Rejected mutations must not advance the journal.
    assert_eq!(runtime.get().revision, 2 + MAX_GOAL_PINS as u64);
}

#[test]
fn pins_are_terminal_gated_and_forged_pin_events_fail_closed() {
    let terminal = GoalRuntime::memory();
    terminal.create("terminal pins", None).expect("create");
    terminal.complete().expect("complete");
    assert!(matches!(
        terminal.pin("late pin"),
        Err(GoalError::InvalidTransition {
            operation: "pin",
            lifecycle: GoalLifecycle::Completed,
        })
    ));
    assert!(matches!(
        terminal.unpin(0),
        Err(GoalError::InvalidTransition {
            operation: "unpin",
            lifecycle: GoalLifecycle::Completed,
        })
    ));
    assert_eq!(terminal.get().revision, 2, "rejected pin must not advance the journal");

    let runtime = GoalRuntime::with_clock_and_persistence(clock_at(100), Arc::new(|_| Ok(())));
    let created = runtime.create("forge pins", None).expect("create");
    let base = GoalEvent {
        revision: 1,
        timestamp: created.updated_at,
        kind: GoalEventKind::Created,
        goal: created.clone(),
    };

    // A PinsUpdated event whose snapshot did not change the pins is a no-op
    // journal write and must fail closed.
    let noop_pins = GoalEvent {
        revision: 2,
        timestamp: at(110),
        kind: GoalEventKind::PinsUpdated { pins: Vec::new() },
        goal: {
            let mut goal = created.clone();
            goal.updated_at = at(110);
            goal
        },
    };
    assert!(matches!(
        GoalRuntime::from_events(&[base.clone(), noop_pins]),
        Err(GoalError::InvalidJournal(message)) if message.contains("invalid pins transition")
    ));

    // A Paused event may not smuggle pins in alongside the lifecycle change.
    let mut pause_with_pins = GoalEvent {
        revision: 2,
        timestamp: at(110),
        kind: GoalEventKind::Paused {
            reason: GoalPauseReason::Manual,
        },
        goal: {
            let mut goal = created.clone();
            goal.lifecycle = GoalLifecycle::Paused;
            goal.pause_reason = Some(GoalPauseReason::Manual);
            goal.updated_at = at(110);
            goal
        },
    };
    pause_with_pins.goal.pins = vec!["forged".to_owned()];
    assert!(matches!(
        GoalRuntime::from_events(&[base.clone(), pause_with_pins]),
        Err(GoalError::InvalidJournal(message)) if message.contains("invalid pause transition")
    ));

    // A legal pins update replays, and its snapshot list must match the event.
    let legal_pins = GoalEvent {
        revision: 2,
        timestamp: at(110),
        kind: GoalEventKind::PinsUpdated {
            pins: vec!["legal pin".to_owned()],
        },
        goal: {
            let mut goal = created.clone();
            goal.pins = vec!["legal pin".to_owned()];
            goal.updated_at = at(110);
            goal
        },
    };
    let replayed = GoalRuntime::from_events(&[base, legal_pins]).expect("legal pins replay");
    assert_eq!(replayed.pins(), vec!["legal pin".to_owned()]);
}

#[test]
fn pins_survive_session_resume_and_fork_clone_copies_them() {
    let directory = tempfile::tempdir().expect("tempdir");
    let recorder = recorder_in(&directory);
    let path = recorder.path();
    let runtime = GoalRuntime::from_session_recorder(recorder.clone()).expect("goal runtime");
    runtime.create("durable pins", Some(40)).expect("create");
    runtime.pin("role model example").expect("pin");
    runtime.pin("second example").expect("second pin");
    let expected = runtime.get();
    persist_session(&recorder);

    let resumed = resume_session(&path).expect("resume session");
    let restored = GoalRuntime::from_session_recorder(resumed.clone()).expect("restore goal");
    assert_eq!(restored.get(), expected);
    assert_eq!(restored.pins(), vec!["role model example".to_owned(), "second example".to_owned()]);

    // Fork clones carry the pinned role-model context into the new lineage.
    let source = restored.get().current.expect("restored goal");
    let cloned = GoalRuntime::memory().fork_clone(&source).expect("fork clone");
    assert_eq!(cloned.pins, source.pins);
    resumed.close().expect("close resumed session");
}

#[test]
fn pins_serialize_under_camel_case_and_skip_when_empty() {
    let runtime = GoalRuntime::memory();
    runtime.create("pin schema", None).expect("create");
    let empty_state = runtime.get();
    let empty = serde_json::to_value(&empty_state).expect("serialize empty");
    assert!(
        empty["current"].get("pins").is_none(),
        "empty pins must be omitted from the wire schema"
    );
    assert_eq!(
        serde_json::from_value::<GoalState>(empty).expect("empty round trip"),
        empty_state,
        "empty-pins goals must round trip through the wire schema"
    );
    runtime.pin("role model example").expect("pin");
    let with_pins = serde_json::to_value(runtime.get()).expect("serialize with pins");
    assert_eq!(with_pins["current"]["pins"], json!(["role model example"]));
    let round_tripped: GoalState =
        serde_json::from_value(with_pins).expect("with-pins round trip");
    assert_eq!(round_tripped, runtime.get());
    assert_eq!(
        round_tripped.current.expect("goal").pins,
        vec!["role model example".to_owned()]
    );
}

#[test]
fn pin_and_unpin_enforce_all_boundaries_and_reject_illegal_transitions() {
    let runtime = GoalRuntime::memory();
    // No goal yet: pin/unpin fail with the same typed error as every mutation.
    assert_eq!(runtime.pin("nope"), Err(GoalError::NoCurrentGoal));
    assert_eq!(runtime.unpin(0), Err(GoalError::NoCurrentGoal));

    runtime.create("bounded pins", None).expect("create");

    // Empty and over-long pins are rejected at the exact boundary.
    assert_eq!(runtime.pin("   "), Err(GoalError::EmptyPin));
    assert!(matches!(
        runtime.pin("x".repeat(MAX_GOAL_PIN_CHARS + 1)),
        Err(GoalError::PinTooLong { max_chars }) if max_chars == MAX_GOAL_PIN_CHARS
    ));
    runtime
        .pin("x".repeat(MAX_GOAL_PIN_CHARS))
        .expect("boundary-length pin accepted");

    // Unpin out of range is a typed error, not a silent no-op.
    assert!(matches!(
        runtime.unpin(99),
        Err(GoalError::PinIndexOutOfRange { index: 99, len: 1 })
    ));

    // The pin cap is enforced at exactly MAX_GOAL_PINS (one over fails).
    for i in 0..MAX_GOAL_PINS - 1 {
        runtime.pin(format!("pin {i}")).expect("pin under the cap");
    }
    assert!(matches!(
        runtime.pin("one over the cap"),
        Err(GoalError::PinLimitReached { max_pins }) if max_pins == MAX_GOAL_PINS
    ));

    // Unpin restores capacity and removes exactly the indexed pin (list shifts).
    runtime.unpin(0).expect("unpin first");
    runtime.pin("freed slot").expect("pin after unpin");
    let pins = runtime.pins();
    assert_eq!(pins.len(), MAX_GOAL_PINS, "unpin must restore one slot");
    assert_eq!(
        pins.last().map(String::as_str),
        Some("freed slot"),
        "unpinned slot must be refilled by the new pin"
    );
    assert!(!pins.iter().any(|pin| pin.len() == MAX_GOAL_PIN_CHARS && pin.chars().all(|c| c == 'x')));

    // Pinning a completed goal is an illegal transition.
    let done = GoalRuntime::memory();
    done.create("finished", None).expect("create");
    done.complete().expect("complete");
    assert!(matches!(
        done.pin("late pin"),
        Err(GoalError::InvalidTransition { operation, .. }) if operation == "pin"
    ));
    assert!(matches!(
        done.unpin(0),
        Err(GoalError::InvalidTransition { operation, .. }) if operation == "unpin"
    ));
}

#[test]
fn pins_updated_events_replay_through_the_journal() {
    let events = Arc::new(Mutex::new(Vec::<GoalEvent>::new()));
    let captured = events.clone();
    let runtime = GoalRuntime::with_persistence(Arc::new(move |event| {
        captured.lock().expect("event lock").push(event.clone());
        Ok(())
    }));
    runtime.create("journal pins", None).expect("create");
    runtime.pin("first pin").expect("pin one");
    runtime.pin("second pin").expect("pin two");
    runtime.unpin(0).expect("unpin one");
    let expected = runtime.get();
    let events = events.lock().expect("event lock").clone();

    // Exactly one PinsUpdated per mutation, each carrying the resulting list.
    let pins_updates: Vec<&GoalEvent> = events
        .iter()
        .filter(|event| matches!(event.kind, GoalEventKind::PinsUpdated { .. }))
        .collect();
    assert_eq!(pins_updates.len(), 3, "every pin/unpin must journal a PinsUpdated");
    for event in &pins_updates {
        let GoalEventKind::PinsUpdated { pins } = &event.kind else {
            unreachable!("filtered");
        };
        assert_eq!(pins, &event.goal.pins, "journal pins must match the resulting goal");
    }

    // Replay restores the exact pinned state (the resume path relies on it).
    let replayed = GoalRuntime::from_events(&events).expect("replay");
    assert_eq!(replayed.get(), expected);
    assert_eq!(replayed.pins(), runtime.pins());
}
