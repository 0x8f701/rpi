//! CLI adapters for llama.cpp router management and Hugging Face GGUF installs.

use std::io::Write as _;

use anyhow::{Context, Result, anyhow, bail};
use pi_ai::{HuggingFaceClient, LlamaRouterSettings, find_hugging_face_token};
use pi_coding::{CatalogSource, GgufDownloadProgress, LlamaManager};
use tokio_util::sync::CancellationToken;

use crate::args::LlamaCommand;

pub async fn run(command: LlamaCommand) -> Result<()> {
    let manager = LlamaManager::default();
    match command {
        LlamaCommand::Configure { base_url, api_key } => {
            let models = manager
                .configure(LlamaRouterSettings::validated(&base_url, api_key)?)
                .await?;
            let stored = manager
                .settings()?
                .ok_or_else(|| anyhow!("validated llama.cpp settings were not persisted"))?;
            println!("Configured llama.cpp router at {}", stored.base_url);
            println!("Discovered {} live model(s)", models.len());
        }
        LlamaCommand::Status { reload } => {
            let settings = manager.effective_settings()?.ok_or_else(|| {
                anyhow!("llama.cpp router is not configured; run `pi llama configure URL` or set LLAMA_BASE_URL")
            })?;
            println!("router {}", settings.base_url);
            let models = manager.router_models(reload).await?;
            if models.is_empty() {
                println!("No router models reported.");
            }
            for model in models {
                println!("{}\t{}", model.status.value.as_str(), model.id);
            }
        }
        LlamaCommand::Refresh => {
            let refreshed = manager.refresh_catalog().await?;
            match refreshed.source {
                CatalogSource::Live => println!(
                    "Refreshed llama.cpp catalog from router: {} live model(s)",
                    refreshed.models.len()
                ),
                CatalogSource::Cache => {
                    eprintln!(
                        "pi: llama.cpp router unavailable; using cached catalog{}",
                        refreshed
                            .warning
                            .as_deref()
                            .map_or_else(String::new, |warning| format!(": {warning}"))
                    );
                    println!("Loaded {} cached model(s)", refreshed.models.len());
                }
            }
        }
        LlamaCommand::Load { model } => {
            let models = manager.load_model(&model).await?;
            println!("Requested load for {model}");
            println!("Router now exposes {} live model(s)", models.len());
        }
        LlamaCommand::Unload { model } => {
            let models = manager.unload_model(&model).await?;
            println!("Requested unload for {model}");
            println!("Router now exposes {} live model(s)", models.len());
        }
        LlamaCommand::Search { query } => {
            let client = hugging_face_client().await?;
            let models = client.search(&query).await?;
            for model in models {
                println!("{}\t{} downloads", model.id, model.downloads);
            }
        }
        LlamaCommand::Details { repository } => {
            let client = hugging_face_client().await?;
            let details = client.details(&repository).await?;
            if let Some(gated) = &details.gated {
                println!("{}\tgated ({gated})", details.id);
            } else {
                println!("{}", details.id);
            }
            for quantization in details.quantizations {
                println!(
                    "  {}\t{}\t{} file(s)",
                    quantization.name,
                    quantization
                        .size
                        .map_or_else(|| "unknown size".to_owned(), format_bytes),
                    quantization.files.len()
                );
                for file in quantization.files {
                    let checksum = file
                        .sha256
                        .as_deref()
                        .map_or("", |value| if value.is_empty() { "" } else { " sha256" });
                    println!(
                        "    {}\t{}{}",
                        file.name,
                        file.size
                            .map_or_else(|| "unknown size".to_owned(), format_bytes),
                        checksum
                    );
                }
            }
        }
        LlamaCommand::Download {
            repository,
            quantization,
        } => {
            let client = hugging_face_client().await?;
            let details = client.details(&repository).await?;
            let cancel = CancellationToken::new();
            let operation_cancel = cancel.clone();
            let operation = manager.install_from_hugging_face(
                &client,
                &details,
                quantization.as_deref(),
                &operation_cancel,
                print_progress,
            );
            tokio::pin!(operation);
            let installed = tokio::select! {
                result = &mut operation => result?,
                signal = tokio::signal::ctrl_c() => {
                    signal.context("installing Ctrl-C handler")?;
                    cancel.cancel();
                    operation.await.context("cancelling GGUF download")?
                }
            };
            eprintln!();
            println!(
                "Installed {}:{} ({} file(s)) under {}",
                installed.repository,
                installed.quantization,
                installed.files.len(),
                manager.models_dir().display()
            );
        }
        LlamaCommand::Installed => {
            let installed = manager.installed()?;
            for model in installed.models {
                let bytes = model.files.iter().map(|file| file.size).sum::<u64>();
                println!(
                    "{}:{}\t{}\t{} file(s)",
                    model.repository,
                    model.quantization,
                    format_bytes(bytes),
                    model.files.len()
                );
            }
        }
    }
    Ok(())
}

pub async fn run_slash(line: &str) -> Result<String> {
    let mut parts = line.split_whitespace();
    let action = parts.next().unwrap_or("status");
    let manager = LlamaManager::default();
    match action {
        "status" => {
            let settings = manager.effective_settings()?.ok_or_else(|| {
                anyhow!("llama.cpp router is not configured; use `pi llama configure URL` or set LLAMA_BASE_URL")
            })?;
            let models = manager.router_models(false).await?;
            let lines = models
                .into_iter()
                .map(|model| format!("{} {}", model.status.value.as_str(), model.id))
                .collect::<Vec<_>>();
            Ok(if lines.is_empty() {
                format!("llama.cpp {} · no models", settings.base_url)
            } else {
                format!("llama.cpp {}\n{}", settings.base_url, lines.join("\n"))
            })
        }
        "refresh" => {
            let result = manager.refresh_catalog().await?;
            Ok(format!(
                "{} {} llama.cpp model(s)",
                if result.source == CatalogSource::Live {
                    "Refreshed"
                } else {
                    "Loaded cached"
                },
                result.models.len()
            ))
        }
        "load" => {
            let model = required_rest(line, action, "/llama load <model>")?;
            manager.load_model(model).await?;
            Ok(format!("Requested load for {model}"))
        }
        "unload" => {
            let model = required_rest(line, action, "/llama unload <model>")?;
            manager.unload_model(model).await?;
            Ok(format!("Requested unload for {model}"))
        }
        "configure" => {
            let url = parts
                .next()
                .ok_or_else(|| anyhow!("usage: /llama configure <url> [api-key]"))?;
            let key = parts.next().map(ToOwned::to_owned);
            let models = manager
                .configure(LlamaRouterSettings::validated(url, key)?)
                .await?;
            Ok(format!(
                "Configured llama.cpp · {} live model(s)",
                models.len()
            ))
        }
        other => {
            bail!("unknown /llama subcommand {other:?}; use status|configure|refresh|load|unload")
        }
    }
}

async fn hugging_face_client() -> Result<HuggingFaceClient> {
    HuggingFaceClient::new(
        find_hugging_face_token().await,
        std::env::var("HF_ENDPOINT").ok().as_deref(),
    )
}

fn required_rest<'a>(line: &'a str, action: &str, usage: &str) -> Result<&'a str> {
    let value = line[action.len()..].trim();
    if value.is_empty() {
        bail!("usage: {usage}");
    }
    Ok(value)
}

fn print_progress(progress: GgufDownloadProgress) {
    let total = progress.total.map_or_else(|| "?".to_owned(), format_bytes);
    let resumed = if progress.resumed { " resumed" } else { "" };
    eprint!(
        "\r{}: {} / {}{}",
        progress.file,
        format_bytes(progress.downloaded),
        total,
        resumed
    );
    let _ = std::io::stderr().flush();
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64 / 1024.0;
    let mut unit = UNITS[0];
    for next in &UNITS[1..] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    if value >= 10.0 {
        format!("{value:.1} {unit}")
    } else {
        format!("{value:.2} {unit}")
    }
}
