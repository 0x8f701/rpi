//! `/team` interactive command surface over the Agent Team Core.

use anyhow::{Result, bail};
use pi_coding::agent_dir_path;

use crate::team::{Team, TeamRegistry, TeamRunnerCommand, probe_runner, run_runner_command};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TeamCliCommand {
    List,
    Status {
        team: Option<String>,
    },
    Create {
        name: String,
        project_label: String,
        project_cwd: String,
    },
    Remove {
        team: String,
    },
    Join {
        team: String,
        origin: String,
        label: Option<String>,
        role: Option<String>,
    },
    Leave {
        team: String,
        runner: String,
    },
    Run {
        team: String,
        runner: String,
        command: TeamRunnerCommand,
    },
    Broadcast {
        team: String,
        command: TeamRunnerCommand,
    },
}

/// Runner token source for `/team join`: read from the environment so the
/// credential never appears in shell history or process listings.
const TEAM_TOKEN_ENV: &str = "RPI_TEAM_TOKEN";

const TEAM_USAGE: &str = "Usage: /team [list|status [name]|create <name> [project-label [cwd]]|join <name> <origin> [label]|leave <team> <runner>|run <team> <runner> <json>|broadcast <team> <json>|remove <name>] \
     (join reads the runner token from $RPI_TEAM_TOKEN; run/broadcast commands are JSONL RPC like {\"type\":\"get_state\"})";

pub fn parse_team_command(arg: &str) -> Result<TeamCliCommand, String> {
    let mut parts = arg.split_whitespace();
    let sub = parts.next().unwrap_or("list");
    match sub {
        "list" => Ok(TeamCliCommand::List),
        "status" => Ok(TeamCliCommand::Status {
            team: parts.next().map(str::to_owned),
        }),
        "create" => {
            let values = parts.collect::<Vec<_>>();
            if values.is_empty() {
                return Err("usage: /team create <name> [project-label [cwd]]".to_owned());
            }
            let name = values[0].to_owned();
            let project_label = values
                .get(1)
                .map_or_else(|| name.clone(), |value| (*value).to_owned());
            let project_cwd = values.get(2).map_or_else(
                || {
                    std::env::current_dir()
                        .map_or_else(|_| String::new(), |p| p.to_string_lossy().into_owned())
                },
                |value| (*value).to_owned(),
            );
            Ok(TeamCliCommand::Create {
                name,
                project_label,
                project_cwd,
            })
        }
        "remove" => {
            let team = parts.next().ok_or("usage: /team remove <name>")?.to_owned();
            Ok(TeamCliCommand::Remove { team })
        }
        "join" => {
            let team = parts
                .next()
                .ok_or("usage: /team join <name> <origin> [label]")?
                .to_owned();
            let origin = parts
                .next()
                .ok_or("usage: /team join <name> <origin> [label]")?
                .to_owned();
            let mut label = parts.next().map(str::to_owned);
            let mut role = None;
            if let Some(rest) = label.as_deref() {
                let mut fields = rest.split('=');
                if fields.next() == Some("role") {
                    role = fields.next().map(str::to_owned);
                    label = None;
                }
            }
            Ok(TeamCliCommand::Join {
                team,
                origin,
                label,
                role,
            })
        }
        "leave" => {
            let team = parts
                .next()
                .ok_or("usage: /team leave <team> <runner>")?
                .to_owned();
            let runner = parts
                .next()
                .ok_or("usage: /team leave <team> <runner>")?
                .to_owned();
            Ok(TeamCliCommand::Leave { team, runner })
        }
        "run" | "broadcast" => {
            let team = parts
                .next()
                .ok_or("usage: /team run|broadcast <team> <runner> <json> (broadcast omits the runner)")?
                .to_owned();
            let runner = if sub == "run" {
                parts.next().map(str::to_owned)
            } else {
                None
            };
            let json = parts.collect::<Vec<_>>().join(" ");
            let command: TeamRunnerCommand = serde_json::from_str(&json)
                .map_err(|error| format!("invalid runner command: {error}"))?;
            if sub == "run" {
                Ok(TeamCliCommand::Run {
                    team,
                    runner: runner.ok_or("usage: /team run <team> <runner> <json>")?,
                    command,
                })
            } else {
                Ok(TeamCliCommand::Broadcast { team, command })
            }
        }
        _ => Err(format!("unknown /team subcommand: {sub}\n{TEAM_USAGE}")),
    }
}

pub async fn execute_team_command(command: TeamCliCommand) -> Result<String> {
    let registry = TeamRegistry::open(&agent_dir_path())?;
    match command {
        TeamCliCommand::List => {
            let teams = registry.list();
            if teams.is_empty() {
                return Ok("No teams.".to_owned());
            }
            Ok(teams
                .iter()
                .map(|team| {
                    format!(
                        "{}  {}  {}  {} member(s)",
                        team.id,
                        team.name,
                        team.project.label,
                        team.members.len()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        TeamCliCommand::Status { team } => {
            let teams = resolve_team(&registry, team.as_deref())?;
            let mut lines = Vec::new();
            for team in teams {
                lines.push(format!(
                    "{} ({}) — {}",
                    team.name, team.project.label, team.project.cwd
                ));
                for (member, probe) in crate::team::team_status(&registry, &team).await {
                    match probe {
                        Ok(state) => lines.push(format!(
                            "  {}  {}  {}{}  online · session {} · {} session(s) in {}",
                            member.runner_id,
                            member.origin,
                            member.label,
                            member
                                .role
                                .as_deref()
                                .map_or(String::new(), |role| format!(" ({role})")),
                            state.session_id.as_deref().unwrap_or("(none)"),
                            state.session_count,
                            state.cwd,
                        )),
                        Err(error) => lines.push(format!(
                            "  {}  {}  offline: {error:#}",
                            member.runner_id, member.origin
                        )),
                    }
                }
            }
            Ok(lines.join("\n"))
        }
        TeamCliCommand::Create {
            name,
            project_label,
            project_cwd,
        } => {
            let team = registry.create_team(&name, &project_label, &project_cwd)?;
            Ok(format!(
                "created team {} ({} members)",
                team.name,
                team.members.len()
            ))
        }
        TeamCliCommand::Remove { team } => {
            let team = find_team_by_name(&registry, &team)?;
            registry.remove_team(&team.id)?;
            Ok(format!("removed team {}", team.name))
        }
        TeamCliCommand::Join {
            team,
            origin,
            label,
            role,
        } => {
            let token = std::env::var(TEAM_TOKEN_ENV).map_err(|_| {
                anyhow::anyhow!(
                    "{TEAM_TOKEN_ENV} is not set; the runner token must be passed through the environment, never the command line"
                )
            })?;
            let team = find_team_by_name(&registry, &team)?;
            let member = registry.join(
                &team.id,
                &origin,
                &token,
                label.as_deref().unwrap_or("runner"),
                role.as_deref(),
            )?;
            Ok(format!(
                "joined {} to team {} as {}",
                member.origin, team.name, member.runner_id
            ))
        }
        TeamCliCommand::Leave { team, runner } => {
            let team = find_team_by_name(&registry, &team)?;
            registry.leave(&team.id, &runner)?;
            Ok(format!("left runner {runner} from team {}", team.name))
        }
        TeamCliCommand::Run {
            team,
            runner,
            command,
        } => {
            let team = find_team_by_name(&registry, &team)?;
            let (member, token) = registry.member_with_token(&team.id, &runner)?;
            let id = format!("team-{}", uuid::Uuid::now_v7());
            let data = run_runner_command(&member.origin, &token, &command, &id).await?;
            Ok(serde_json::to_string_pretty(&data).unwrap_or_else(|_| data.to_string()))
        }
        TeamCliCommand::Broadcast { team, command } => {
            let team = find_team_by_name(&registry, &team)?;
            if team.members.is_empty() {
                return Ok("team has no members".to_owned());
            }
            let mut lines = Vec::new();
            for member in &team.members {
                let token = registry.runner_token(&team.id, &member.runner_id)?;
                let id = format!("team-{}", uuid::Uuid::now_v7());
                match run_runner_command(&member.origin, &token, &command, &id).await {
                    Ok(data) => {
                        lines.push(format!("{}  {}  {}", member.runner_id, member.origin, data))
                    }
                    Err(error) => lines.push(format!(
                        "{}  {}  ERROR: {error:#}",
                        member.runner_id, member.origin
                    )),
                }
            }
            Ok(lines.join("\n"))
        }
    }
}

fn resolve_team(registry: &TeamRegistry, name: Option<&str>) -> Result<Vec<Team>> {
    match name {
        Some(name) => Ok(vec![find_team_by_name(registry, name)?]),
        None => Ok(registry.list()),
    }
}

fn find_team_by_name(registry: &TeamRegistry, name: &str) -> Result<Team> {
    let mut matches = registry
        .list()
        .into_iter()
        .filter(|team| team.name == name)
        .collect::<Vec<_>>();
    match matches.as_mut_slice() {
        [team] => Ok(team.clone()),
        [] => bail!("team '{name}' was not found"),
        _ => bail!("team name '{name}' is ambiguous"),
    }
}
