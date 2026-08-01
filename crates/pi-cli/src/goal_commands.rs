use anyhow::{Result, anyhow, bail};
use pi_coding::{Application, GoalLifecycle, GoalState};

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
}

pub fn parse_interactive_goal_command(argument: Option<&str>) -> Result<InteractiveGoalCommand> {
    let argument = argument.unwrap_or_default().trim();
    if argument.is_empty() || argument == "show" || argument == "get" {
        return Ok(InteractiveGoalCommand::Show);
    }
    let mut parts = argument.split_whitespace();
    let operation = parts.next().unwrap_or_default();
    match operation {
        "pause" => no_trailing(parts, InteractiveGoalCommand::Pause),
        "resume" => no_trailing(parts, InteractiveGoalCommand::Resume),
        "complete" => no_trailing(parts, InteractiveGoalCommand::Complete),
        "drop" => no_trailing(parts, InteractiveGoalCommand::Drop),
        "create" | "set" => parse_create(argument[operation.len()..].trim()),
        _ => parse_create(argument),
    }
}

fn no_trailing<'a>(mut parts: impl Iterator<Item = &'a str>, command: InteractiveGoalCommand) -> Result<InteractiveGoalCommand> {
    if parts.next().is_some() {
        bail!("usage: /goal [show|create [--tokens N] <objective>|pause|resume|complete|drop]");
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

pub fn execute_interactive_goal_command(
    application: &Application,
    command: InteractiveGoalCommand,
) -> Result<String> {
    match command {
        InteractiveGoalCommand::Show => Ok(format_goal_state(&application.goal_state())),
        InteractiveGoalCommand::Create {
            objective,
            token_budget,
        } => {
            application.goal_create(objective, token_budget)?;
            Ok(format_goal_state(&application.goal_state()))
        }
        InteractiveGoalCommand::Pause => {
            application.goal_pause()?;
            Ok(format_goal_state(&application.goal_state()))
        }
        InteractiveGoalCommand::Resume => {
            application.goal_resume()?;
            Ok(format_goal_state(&application.goal_state()))
        }
        InteractiveGoalCommand::Complete => {
            application.goal_complete()?;
            Ok(format_goal_state(&application.goal_state()))
        }
        InteractiveGoalCommand::Drop => {
            application.goal_drop()?;
            Ok(format_goal_state(&application.goal_state()))
        }
    }
}

#[must_use]
pub fn format_goal_state(state: &GoalState) -> String {
    let Some(goal) = &state.current else {
        return "no goal".to_owned();
    };
    let lifecycle = match goal.lifecycle {
        GoalLifecycle::Active => "active",
        GoalLifecycle::Paused => "paused",
        GoalLifecycle::Completed => "completed",
        GoalLifecycle::Dropped => "dropped",
    };
    let budget = goal.token_budget.map_or_else(
        || format!("{} tokens used", goal.usage.tokens_used),
        |budget| format!("{}/{} tokens", goal.usage.tokens_used, budget),
    );
    format!("{lifecycle} · {budget} · {}", goal.objective)
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
        assert!(parse_interactive_goal_command(Some("create --tokens 0 no")).is_err());
    }
}
