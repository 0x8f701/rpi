use std::io::{self, IsTerminal, Write};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use pi_coding::{
    AuthEvent, AuthInteraction, AuthManager, AuthPrompt, AuthPromptOption, AuthType, CredentialInfo,
};

use crate::models_config::auth_json_path;

pub struct ConsoleAuthInteraction {
    interactive: bool,
}

impl ConsoleAuthInteraction {
    #[must_use]
    pub fn interactive() -> Self {
        Self { interactive: true }
    }

    #[must_use]
    pub fn explicit_only() -> Self {
        Self { interactive: false }
    }
}

#[async_trait]
impl AuthInteraction for ConsoleAuthInteraction {
    async fn prompt(&self, prompt: AuthPrompt) -> Result<String> {
        if !self.interactive || !io::stdin().is_terminal() {
            bail!("authentication requires an interactive terminal for this prompt")
        }
        match prompt {
            AuthPrompt::Select { message, options } => prompt_select(&message, &options),
            AuthPrompt::Secret { message, .. } => prompt_secret(&message),
            AuthPrompt::Text { message, .. } | AuthPrompt::ManualCode { message, .. } => {
                prompt_line(&message)
            }
        }
    }

    fn notify(&self, event: AuthEvent) {
        match event {
            AuthEvent::Info { message, links } => {
                eprintln!("{message}");
                for link in links {
                    match link.label {
                        Some(label) => eprintln!("{label}: {}", link.url),
                        None => eprintln!("{}", link.url),
                    }
                }
            }
            AuthEvent::AuthUrl { url, instructions } => {
                if let Some(instructions) = instructions {
                    eprintln!("{instructions}");
                }
                eprintln!("{url}");
            }
            AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                interval_seconds: _,
                expires_in_seconds: _,
            } => {
                eprintln!("Open {verification_uri} and enter code {user_code}");
            }
            AuthEvent::Progress { message } => eprintln!("{message}"),
        }
    }
}

pub async fn login(provider: Option<&str>, interactive: bool) -> Result<CredentialInfo> {
    let provider = require_explicit_provider_when_noninteractive(provider, interactive)?;
    let manager = manager()?;
    let interaction = if interactive {
        ConsoleAuthInteraction::interactive()
    } else {
        ConsoleAuthInteraction::explicit_only()
    };
    manager.login(provider, None, &interaction).await
}

pub async fn logout(provider: Option<&str>, interactive: bool) -> Result<CredentialInfo> {
    let provider = require_explicit_provider_when_noninteractive(provider, interactive)?;
    let manager = manager()?;
    let interaction = if interactive {
        ConsoleAuthInteraction::interactive()
    } else {
        ConsoleAuthInteraction::explicit_only()
    };
    manager.logout(provider, &interaction).await
}

pub async fn login_cli(provider: Option<&str>) -> Result<()> {
    let info = login(provider, true).await?;
    println!(
        "Logged in to {} using {}",
        info.provider_id,
        info.credential_type.label()
    );
    Ok(())
}

pub async fn logout_cli(provider: Option<&str>) -> Result<()> {
    let info = logout(provider, true).await?;
    println!("Logged out of {}", info.provider_id);
    Ok(())
}

fn manager() -> Result<AuthManager> {
    let path = auth_json_path().ok_or_else(|| anyhow::anyhow!("auth.json path is unavailable"))?;
    AuthManager::new(path)
}

fn require_explicit_provider_when_noninteractive(
    provider: Option<&str>,
    interactive: bool,
) -> Result<Option<&str>> {
    if !interactive && provider.is_none_or(|provider| provider.trim().is_empty()) {
        bail!("provider is required outside an interactive terminal")
    }
    Ok(provider)
}

fn prompt_select(message: &str, options: &[AuthPromptOption]) -> Result<String> {
    if options.is_empty() {
        bail!("authentication prompt has no choices")
    }
    eprintln!("{message}");
    for (index, option) in options.iter().enumerate() {
        match &option.description {
            Some(description) => eprintln!("  {}. {} — {description}", index + 1, option.label),
            None => eprintln!("  {}. {}", index + 1, option.label),
        }
    }
    loop {
        let value = prompt_line("Selection")?;
        if let Ok(index) = value.parse::<usize>()
            && let Some(option) = index.checked_sub(1).and_then(|index| options.get(index))
        {
            return Ok(option.id.clone());
        }
        if let Some(option) = options.iter().find(|option| option.id == value) {
            return Ok(option.id.clone());
        }
        eprintln!("Enter a number from 1 to {}", options.len());
    }
}

fn prompt_line(message: &str) -> Result<String> {
    eprint!("{message}: ");
    io::stderr()
        .flush()
        .context("flushing authentication prompt")?;
    let mut value = String::new();
    io::stdin()
        .read_line(&mut value)
        .context("reading authentication prompt")?;
    Ok(value.trim().to_owned())
}

fn prompt_secret(message: &str) -> Result<String> {
    eprint!("{message}: ");
    io::stderr()
        .flush()
        .context("flushing authentication prompt")?;
    let secret = read_secret_line();
    eprintln!();
    secret
}

fn read_secret_line() -> Result<String> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, read};
    use crossterm::terminal::enable_raw_mode;

    enable_raw_mode().context("enabling private terminal input")?;
    let result = (|| -> Result<String> {
        let mut secret = String::new();
        loop {
            let event = read().context("reading private terminal input")?;
            let Event::Key(key) = event else { continue };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match key.code {
                KeyCode::Enter => return Ok(secret),
                KeyCode::Backspace => {
                    secret.pop();
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    secret.push(character);
                }
                KeyCode::Esc => bail!("authentication cancelled"),
                _ => {}
            }
        }
    })();
    let _ = crossterm::terminal::disable_raw_mode();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noninteractive_commands_require_explicit_provider() {
        let error = require_explicit_provider_when_noninteractive(None, false)
            .expect_err("missing provider must fail outside a terminal");
        assert!(error.to_string().contains("provider is required"));
        assert_eq!(
            require_explicit_provider_when_noninteractive(Some("anthropic"), false)
                .expect("explicit provider"),
            Some("anthropic")
        );
    }
}
