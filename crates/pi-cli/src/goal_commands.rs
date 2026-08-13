use anyhow::{Result, anyhow, bail};
use pi_coding::{Application, Goal, GoalActivationOutcome, GoalLifecycle, GoalPauseReason, GoalState};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InteractiveGoalCommand {
    Show,
    Create {
        objective: String,
        token_budget: Option<u64>,
    },
    Pause,
    Resume,
    Complete,
    Drop,
    Pin {
        text: String,
    },
    Pins,
    Unpin {
        index: usize,
    },
}


pub fn parse_interactive_goal_command(argument: Option<&str>) -> Result<InteractiveGoalCommand> {
    let argument = argument.unwrap_or_default().trim();
    if argument.is_empty() || argument == "show" || argument == "get" || argument == "inspect" {
        return Ok(InteractiveGoalCommand::Show);
    }
    let mut parts = argument.split_whitespace();
    let operation = parts.next().unwrap_or_default();
    match operation {
        "show" | "get" | "inspect" => no_trailing(parts, InteractiveGoalCommand::Show),
        "pause" => no_trailing(parts, InteractiveGoalCommand::Pause),
        "resume" => no_trailing(parts, InteractiveGoalCommand::Resume),
        "complete" => no_trailing(parts, InteractiveGoalCommand::Complete),
        "drop" => no_trailing(parts, InteractiveGoalCommand::Drop),
        "pin" => {
            let text = argument[operation.len()..].trim();
            if text.is_empty() {
                bail!("usage: /goal pin <text>");
            }
            Ok(InteractiveGoalCommand::Pin {
                text: text.to_owned(),
            })
        }
        "pins" => no_trailing(parts, InteractiveGoalCommand::Pins),
        "unpin" => {
            let index = parts
                .next()
                .ok_or_else(|| anyhow!("usage: /goal unpin <index>"))?;
            if parts.next().is_some() {
                bail!("usage: /goal unpin <index>");
            }
            let index = index
                .parse::<usize>()
                .map_err(|_| anyhow!("usage: /goal unpin <index>"))?;
            Ok(InteractiveGoalCommand::Unpin { index })
        }
        "create" | "set" => parse_create(argument[operation.len()..].trim()),
        _ => parse_create(argument),
    }
}

fn no_trailing<'a>(mut parts: impl Iterator<Item = &'a str>, command: InteractiveGoalCommand) -> Result<InteractiveGoalCommand> {
    if parts.next().is_some() {
        bail!("usage: /goal [show|inspect|create [--tokens N] <objective>|pause|resume|complete|drop|pin <text>|pins|unpin <index>]");
    }
    Ok(command)
}

fn parse_create(argument: &str) -> Result<InteractiveGoalCommand> {
    let mut parts = argument.split_whitespace().peekable();
    let mut token_budget = None;
    let mut objective = Vec::new();
    while let Some(part) = parts.next() {
        if part == "--tokens" {
            let value = parts
                .next()
                .ok_or_else(|| anyhow!("--tokens requires a positive integer"))?;
            let value = value
                .parse::<u64>()
                .map_err(|_| anyhow!("--tokens requires a positive integer"))?;
            if value == 0 {
                bail!("--tokens requires a positive integer");
            }
            token_budget = Some(value);
        } else {
            objective.push(part);
            objective.extend(parts);
            break;
        }
    }
    let objective = objective.join(" ");
    if objective.trim().is_empty() {
        bail!("goal objective must not be empty");
    }
    Ok(InteractiveGoalCommand::Create {
        objective,
        token_budget,
    })
}

pub async fn execute_interactive_goal_command(
    application: &Application,
    command: InteractiveGoalCommand,
) -> Result<String> {
    let activation = match command {
        InteractiveGoalCommand::Show => None,
        InteractiveGoalCommand::Create {
            objective,
            token_budget,
        } => Some(application.activate_goal(objective, token_budget).await?),
        InteractiveGoalCommand::Pause => {
            application.goal_pause()?;
            None
        }
        InteractiveGoalCommand::Resume => Some(application.resume_goal_work().await?),
        InteractiveGoalCommand::Complete => {
            application.goal_complete()?;
            None
        }
        InteractiveGoalCommand::Drop => {
            application.goal_drop()?;
            None
        }
        InteractiveGoalCommand::Pin { text } => {
            application.goal_pin(text)?;
            None
        }
        InteractiveGoalCommand::Pins => {
            return Ok(format_goal_pins(&application.goal_state()));
        }
        InteractiveGoalCommand::Unpin { index } => {
            application.goal_unpin(index)?;
            None
        }
    };
    let state = format_goal_state(&application.goal_state());
    Ok(match activation {
        Some(GoalActivationOutcome::Started) => format!("Goal work started · {state}"),
        Some(GoalActivationOutcome::Queued) => format!("Goal work queued · {state}"),
        Some(GoalActivationOutcome::AlreadyActive) => format!("Goal work already active · {state}"),
        None => state,
    })
}

/// Renders the goal's role-model pins as a numbered listing, or a short
/// "no pins"/"no goal" marker when there is nothing to list.
#[must_use]
pub fn format_goal_pins(state: &GoalState) -> String {
    let Some(goal) = &state.current else {
        return "no goal".to_owned();
    };
    if goal.pins.is_empty() {
        return "no pins".to_owned();
    }
    goal.pins
        .iter()
        .enumerate()
        .map(|(index, pin)| format!("{}. {pin}", index + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Human-readable reason a goal is paused. The manual and resume-safety
/// reasons stay recoverable with `/goal resume`; a budget-exhausted pause
/// cannot be resumed and is never presented as resumable.
#[must_use]
pub fn format_pause_reason(reason: GoalPauseReason) -> &'static str {
    match reason {
        GoalPauseReason::Manual => "manually paused",
        GoalPauseReason::BudgetExhausted => "budget exhausted; cannot resume",
        GoalPauseReason::ResumeSafety => "session resumed; run /goal resume",
    }
}

/// Lifecycle label for a goal. Paused goals carry their human-readable
/// pause reason so the user never has to guess why work is suspended.
fn lifecycle_label(goal: &Goal) -> String {
    match goal.lifecycle {
        GoalLifecycle::Active => "active".to_owned(),
        GoalLifecycle::Paused => goal.pause_reason.map_or_else(
            || "paused".to_owned(),
            |reason| format!("paused ({})", format_pause_reason(reason)),
        ),
        GoalLifecycle::Completed => "completed".to_owned(),
        GoalLifecycle::Dropped => "dropped".to_owned(),
    }
}

#[must_use]
pub fn format_goal_state(state: &GoalState) -> String {
    let Some(goal) = &state.current else {
        return "no goal".to_owned();
    };
    let budget = goal.token_budget.map_or_else(
        || format!("{} tokens used", goal.usage.tokens_used),
        |budget| format!("{}/{} tokens", goal.usage.tokens_used, budget),
    );
    format!("{} · {budget} · {}", lifecycle_label(goal), goal.objective)
}

#[must_use]
pub fn format_goal_details(state: &GoalState) -> String {
    let Some(goal) = &state.current else {
        return "No goal is active. Choose Create goal to set an objective.".to_owned();
    };
    let tokens = goal.token_budget.map_or_else(
        || format!("{} (no budget)", goal.usage.tokens_used),
        |budget| format!("{} / {budget}", goal.usage.tokens_used),
    );
    let mut details = format!(
        "Objective: {}\nStatus: {}\nTokens: {tokens}\nTime spent: {}s",
        goal.objective,
        lifecycle_label(goal),
        goal.usage.active_time_seconds
    );
    if !goal.pins.is_empty() {
        details.push_str("\nPins:");
        for (index, pin) in goal.pins.iter().enumerate() {
            details.push_str(&format!("\n  {}. {pin}", index + 1));
        }
    }
    details
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_goal_create_and_lifecycle_commands() {
        assert_eq!(
            parse_interactive_goal_command(Some("create --tokens 42 ship safely")).unwrap(),
            InteractiveGoalCommand::Create {
                objective: "ship safely".to_owned(),
                token_budget: Some(42),
            }
        );
        assert_eq!(
            parse_interactive_goal_command(Some("pause")).unwrap(),
            InteractiveGoalCommand::Pause
        );
        assert_eq!(
            parse_interactive_goal_command(Some("inspect")).unwrap(),
            InteractiveGoalCommand::Show
        );
        assert_eq!(
            parse_interactive_goal_command(Some("show")).unwrap(),
            InteractiveGoalCommand::Show
        );
        assert_eq!(
            parse_interactive_goal_command(Some("get")).unwrap(),
            InteractiveGoalCommand::Show
        );
        assert!(parse_interactive_goal_command(Some("create --tokens 0 no")).is_err());
        assert!(parse_interactive_goal_command(Some("inspect extra")).is_err());
    }

    #[test]
    fn exact_chinese_objective_is_create_shorthand() {
        assert_eq!(
            parse_interactive_goal_command(Some("制作zig版本的pi-coding-agent")).unwrap(),
            InteractiveGoalCommand::Create {
                objective: "制作zig版本的pi-coding-agent".to_owned(),
                token_budget: None,
            }
        );
    }

    #[test]
    fn parses_goal_pin_pins_and_unpin_commands() {
        assert_eq!(
            parse_interactive_goal_command(Some("pin keep the release checklist in scope")).unwrap(),
            InteractiveGoalCommand::Pin {
                text: "keep the release checklist in scope".to_owned(),
            }
        );
        assert_eq!(
            parse_interactive_goal_command(Some("pins")).unwrap(),
            InteractiveGoalCommand::Pins
        );
        assert_eq!(
            parse_interactive_goal_command(Some("unpin 0")).unwrap(),
            InteractiveGoalCommand::Unpin { index: 0 }
        );
        assert_eq!(
            parse_interactive_goal_command(Some("unpin 7")).unwrap(),
            InteractiveGoalCommand::Unpin { index: 7 }
        );
        assert!(parse_interactive_goal_command(Some("pin")).is_err(), "pin needs text");
        assert!(parse_interactive_goal_command(Some("pin   ")).is_err(), "pin needs text");
        assert!(parse_interactive_goal_command(Some("unpin")).is_err(), "unpin needs an index");
        assert!(parse_interactive_goal_command(Some("unpin nope")).is_err(), "unpin index must be a number");
        assert!(parse_interactive_goal_command(Some("unpin 0 extra")).is_err(), "unpin takes one index");
        assert!(parse_interactive_goal_command(Some("pins extra")).is_err(), "pins takes no arguments");
    }

    #[test]
    fn formats_goal_pins_and_details_with_pin_section() {
        let mut state: GoalState = serde_json::from_value(serde_json::json!({
            "current": {
                "id": "goal-1",
                "objective": "ship safely",
                "tokenBudget": 10,
                "pins": ["follow the checklist", "reference the omp style"],
                "lifecycle": "active",
                "createdAt": "2026-01-01T00:00:00Z",
                "updatedAt": "2026-01-01T00:00:00Z",
                "usage": { "tokensUsed": 0, "activeTimeSeconds": 0 }
            },
            "revision": 1
        }))
        .expect("goal state json");
        assert_eq!(
            format_goal_pins(&state),
            "1. follow the checklist\n2. reference the omp style"
        );
        let details = format_goal_details(&state);
        assert!(details.contains("Pins:"), "{details}");
        assert!(details.contains("  1. follow the checklist"), "{details}");
        assert!(details.contains("  2. reference the omp style"), "{details}");

        state.current.as_mut().expect("goal").pins.clear();
        assert_eq!(format_goal_pins(&state), "no pins");
        assert!(!format_goal_details(&state).contains("Pins:"), "empty pins must not render a Pins section");
        assert_eq!(format_goal_pins(&GoalState::default()), "no goal");
    }

    #[test]
    fn formats_empty_goal_details_for_overlay() {
        let details = format_goal_details(&GoalState::default());
        assert_eq!(details, "No goal is active. Choose Create goal to set an objective.");
    }

    #[test]
    fn formats_paused_goal_with_human_readable_reason() {
        let state = |reason: &str| -> GoalState {
            serde_json::from_value(serde_json::json!({
                "current": {
                    "id": "goal-1",
                    "objective": "ship safely",
                    "tokenBudget": 10,
                    "lifecycle": "paused",
                    "pauseReason": reason,
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "usage": { "tokensUsed": 4, "activeTimeSeconds": 12 }
                },
                "revision": 2
            }))
            .expect("goal state json")
        };

        // Manual pause: plainly recoverable, reason explained.
        let manual = state("manual");
        let manual_line = format_goal_state(&manual);
        assert!(manual_line.contains("manually paused"), "{manual_line}");
        assert!(!manual_line.contains("run /goal resume"), "{manual_line}");

        // Resume-safety pause: the exact recovery command is spelled out.
        let safety = state("resume_safety");
        let safety_line = format_goal_state(&safety);
        let safety_details = format_goal_details(&safety);
        assert!(safety_line.contains("session resumed; run /goal resume"), "{safety_line}");
        assert!(safety_details.contains("session resumed; run /goal resume"), "{safety_details}");

        // Budget-exhausted pause: clearly exhausted and not resumable.
        let exhausted = state("budget_exhausted");
        let exhausted_line = format_goal_state(&exhausted);
        let exhausted_details = format_goal_details(&exhausted);
        assert!(exhausted_line.contains("budget exhausted; cannot resume"), "{exhausted_line}");
        assert!(exhausted_details.contains("budget exhausted; cannot resume"), "{exhausted_details}");
        assert!(!exhausted_line.contains("run /goal resume"), "{exhausted_line}");

        // Active goals keep the plain lifecycle label.
        let active: GoalState = serde_json::from_value(serde_json::json!({
            "current": {
                "id": "goal-1",
                "objective": "ship safely",
                "tokenBudget": 10,
                "lifecycle": "active",
                "createdAt": "2026-01-01T00:00:00Z",
                "updatedAt": "2026-01-01T00:00:00Z",
                "usage": { "tokensUsed": 4, "activeTimeSeconds": 12 }
            },
            "revision": 1
        }))
        .expect("goal state json");
        assert!(
            format_goal_state(&active).starts_with("active · 4/10 tokens · ship safely"),
            "{}",
            format_goal_state(&active)
        );
        assert!(
            format_goal_details(&active).contains("Status: active"),
            "{}",
            format_goal_details(&active)
        );
    }
}
