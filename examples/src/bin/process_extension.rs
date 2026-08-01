use std::{
    collections::BTreeSet,
    env,
    io::{BufRead, BufReader, Write},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, ensure};
use pi_agent::{AbortController, AgentToolResult, ToolExecutionMode};
use pi_ai::{ContentBlock, Schema};
use pi_cli::extension_ui::{ExtensionUiAdapter, ExtensionUiEvent};
use pi_coding::{
    ExtensionCapability, ExtensionCapabilityManifest, ExtensionCommandDescriptor,
    ExtensionEventHookDescriptor, ExtensionFrame, ExtensionHostFrame, ExtensionHostRequest,
    ExtensionInvocation, ExtensionMode, ExtensionOrigin, ExtensionPermissionSet,
    ExtensionRegistration, ExtensionRuntime, ExtensionRuntimeEvent, ExtensionRuntimeOptions,
    ExtensionSpec, ExtensionToolDescriptor, ExtensionUiCapability, ExtensionUiRequest,
    ExtensionUiResponse, ProtocolResult, RuntimeUiRequest, UiNotificationLevel,
    EXTENSION_PROTOCOL_VERSION,
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::broadcast;

const CHILD_ARGUMENT: &str = "--extension-child";
const MALFORMED_ARGUMENT: &str = "--malformed-child";
const SAMPLE_ID: &str = "sample.process";

#[tokio::main]
async fn main() -> Result<()> {
    match env::args().nth(1).as_deref() {
        Some(CHILD_ARGUMENT) => run_extension_child(),
        Some(MALFORMED_ARGUMENT) => {
            println!("this is deliberately not JSON");
            Ok(())
        }
        _ => run_host_sample().await,
    }
}

async fn run_host_sample() -> Result<()> {
    let executable = env::current_exe().context("resolving sample executable")?;
    let cwd = env::current_dir().context("resolving sample working directory")?;
    let ui = Arc::new(ExtensionUiAdapter::new());
    let mut ui_events = ui.subscribe();
    let responder = {
        let ui = ui.clone();
        tokio::spawn(async move {
            loop {
                match ui_events.recv().await {
                    Ok(ExtensionUiEvent::InteractionRequested { interaction }) => {
                        let response = match interaction.request {
                            ExtensionUiRequest::Select { options, .. } => {
                                ExtensionUiResponse::Selected {
                                    value: options.first().map(|option| option.value.clone()),
                                }
                            }
                            ExtensionUiRequest::Confirm { .. } => {
                                ExtensionUiResponse::Confirmed { confirmed: true }
                            }
                            ExtensionUiRequest::Input { value, .. } => {
                                ExtensionUiResponse::Input { value }
                            }
                            ExtensionUiRequest::Editor { prefill, .. } => {
                                ExtensionUiResponse::Edited { value: prefill }
                            }
                            ExtensionUiRequest::Notify { .. }
                            | ExtensionUiRequest::Status { .. }
                            | ExtensionUiRequest::Widget { .. }
                            | ExtensionUiRequest::Title { .. }
                            | ExtensionUiRequest::SetEditorText { .. } => {
                                ExtensionUiResponse::Acknowledged
                            }
                        };
                        let _ = ui.respond(&interaction.id, response);
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    };

    let runtime = ExtensionRuntime::process(
        Some(ui),
        ExtensionRuntimeOptions {
            mode: ExtensionMode::Tui,
            handshake_timeout: Duration::from_secs(2),
            load_timeout: Duration::from_secs(2),
            initialize_timeout: Duration::from_secs(2),
            invocation_timeout: Duration::from_secs(2),
            hook_timeout: Duration::from_secs(2),
            shutdown_timeout: Duration::from_secs(1),
            ..ExtensionRuntimeOptions::default()
        },
    );
    let mut runtime_events = runtime.subscribe();

    let valid = extension_spec(
        SAMPLE_ID,
        &executable,
        &cwd,
        ExtensionOrigin::User,
        true,
        CHILD_ARGUMENT,
    );
    let malformed = extension_spec(
        "sample.malformed",
        &executable,
        &cwd,
        ExtensionOrigin::User,
        true,
        MALFORMED_ARGUMENT,
    );
    let untrusted = extension_spec(
        "sample.untrusted",
        &executable,
        &cwd,
        ExtensionOrigin::Project,
        false,
        CHILD_ARGUMENT,
    );
    let report = runtime.load(vec![valid, malformed, untrusted]).await;
    ensure!(report.loaded.len() == 1, "valid extension did not load");
    ensure!(
        report.failures.len() == 2,
        "malformed and untrusted extensions were not isolated"
    );
    ensure!(
        report
            .failures
            .iter()
            .any(|failure| failure.extension_id == "sample.untrusted"
                && failure.message.contains("untrusted project extension")),
        "project trust gating did not reject the extension"
    );

    let command = runtime
        .invoke_command("sample", "from-host".to_owned(), None, None)
        .await?;
    ensure!(command["selection"] == "alpha", "UI selection was not returned");
    ensure!(
        command["events"]
            .as_array()
            .is_some_and(|events| events.iter().any(|event| event == "session_start")),
        "extension did not receive session_start"
    );

    let (_, abort) = AbortController::new();
    let tool = runtime
        .invoke_tool(
            "sample_echo",
            "sample-call".to_owned(),
            json!({ "text": "hello subprocess" }),
            abort,
            None,
        )
        .await?;
    ensure!(
        tool.content.iter().any(|block| {
            matches!(block, ContentBlock::Text { text, .. } if text == "hello subprocess")
        }),
        "subprocess tool did not return its result"
    );

    let reload = runtime.reload(Vec::new()).await;
    ensure!(reload.loaded.is_empty(), "reload retained an extension");
    ensure!(runtime.commands().is_empty(), "stale command survived reload");
    wait_for_invalidation(&mut runtime_events).await?;

    runtime.shutdown().await;
    responder.abort();
    println!(
        "sample loaded a command/tool, received lifecycle events, requested UI, isolated failures, enforced trust, and invalidated on reload"
    );
    Ok(())
}

fn extension_spec(
    id: &str,
    executable: &std::path::Path,
    cwd: &std::path::Path,
    origin: ExtensionOrigin,
    project_trusted: bool,
    argument: &str,
) -> ExtensionSpec {
    let mut spec = ExtensionSpec::new(
        id,
        executable,
        cwd,
        origin,
        project_trusted,
        ExtensionPermissionSet::allow_all(),
    );
    spec.arguments.push(argument.to_owned());
    spec
}

async fn wait_for_invalidation(
    events: &mut broadcast::Receiver<ExtensionRuntimeEvent>,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match events.recv().await {
                Ok(ExtensionRuntimeEvent::Invalidated { instance, .. })
                    if instance.extension_id == SAMPLE_ID =>
                {
                    return Ok(());
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(anyhow!("runtime event stream closed"));
                }
            }
        }
    })
    .await
    .map_err(|_| anyhow!("timed out waiting for extension invalidation"))?
}

fn run_extension_child() -> Result<()> {
    let stdin = std::io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let mut line = String::new();

    read_line(&mut input, &mut line)?;
    let hello: ExtensionHostFrame = serde_json::from_str(&line).context("reading host hello")?;
    let extension_id = match hello {
        ExtensionHostFrame::Hello {
            protocol_version,
            instance,
            ..
        } => {
            ensure!(
                protocol_version == EXTENSION_PROTOCOL_VERSION,
                "unsupported host protocol version"
            );
            instance.extension_id
        }
        frame => return Err(anyhow!("expected host hello, received {frame:?}")),
    };

    write_frame(
        &mut output,
        &ExtensionFrame::Hello {
            protocol_version: EXTENSION_PROTOCOL_VERSION,
            manifest: ExtensionCapabilityManifest {
                id: extension_id,
                name: "Process extension sample".to_owned(),
                version: "1.0.0".to_owned(),
                capabilities: [
                    ExtensionCapability::Commands,
                    ExtensionCapability::Tools,
                    ExtensionCapability::EventHooks,
                    ExtensionCapability::Ui,
                ]
                .into_iter()
                .collect::<BTreeSet<_>>(),
                ui_capabilities: [
                    ExtensionUiCapability::Select,
                    ExtensionUiCapability::Notify,
                ]
                .into_iter()
                .collect::<BTreeSet<_>>(),
            },
        },
    )?;

    let mut received_events = Vec::new();
    let mut next_ui_request = 0_u64;
    loop {
        read_line(&mut input, &mut line)?;
        let frame: ExtensionHostFrame =
            serde_json::from_str(&line).context("reading host frame")?;
        match frame {
            ExtensionHostFrame::Request { id, request, .. } => match request {
                ExtensionHostRequest::Load => {
                    register_capabilities(&mut output)?;
                    write_success(&mut output, id, Value::Null)?;
                }
                ExtensionHostRequest::Initialize => {
                    let ui_id = next_request_id(&mut next_ui_request);
                    write_frame(
                        &mut output,
                        &ExtensionFrame::Request {
                            id: ui_id.clone(),
                            request: pi_coding::ExtensionRuntimeRequest::Ui {
                                ui: RuntimeUiRequest {
                                    request: ExtensionUiRequest::Notify {
                                        message: "sample initialized".to_owned(),
                                        level: UiNotificationLevel::Info,
                                    },
                                    timeout_ms: Some(1_000),
                                },
                            },
                        },
                    )?;
                    let _ = await_ui_response(&mut input, &mut line, &ui_id)?;
                    write_success(&mut output, id, Value::Null)?;
                }
                ExtensionHostRequest::Invoke { invocation } => match invocation {
                    ExtensionInvocation::Event { event } => {
                        received_events.push(event.name.clone());
                        write_success(&mut output, id, json!({ "received": event.name }))?;
                    }
                    ExtensionInvocation::Command { arguments, .. } => {
                        let ui_id = next_request_id(&mut next_ui_request);
                        write_frame(
                            &mut output,
                            &ExtensionFrame::Request {
                                id: ui_id.clone(),
                                request: pi_coding::ExtensionRuntimeRequest::Ui {
                                    ui: RuntimeUiRequest {
                                        request: ExtensionUiRequest::Select {
                                            title: "Choose a sample value".to_owned(),
                                            options: vec![pi_coding::UiSelectOption {
                                                value: "alpha".to_owned(),
                                                label: "Alpha".to_owned(),
                                                description: None,
                                            }],
                                        },
                                        timeout_ms: Some(1_000),
                                    },
                                },
                            },
                        )?;
                        let response = await_ui_response(&mut input, &mut line, &ui_id)?;
                        let selection = match response {
                            ExtensionUiResponse::Selected { value } => value,
                            ExtensionUiResponse::Cancelled => None,
                            response => {
                                return Err(anyhow!("unexpected select response {response:?}"));
                            }
                        };
                        write_success(
                            &mut output,
                            id,
                            json!({
                                "arguments": arguments,
                                "selection": selection,
                                "events": received_events,
                            }),
                        )?;
                    }
                    ExtensionInvocation::Tool { arguments, .. } => {
                        let text = arguments
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        write_success(
                            &mut output,
                            id,
                            serde_json::to_value(AgentToolResult::text(text))
                                .context("encoding tool result")?,
                        )?;
                    }
                    ExtensionInvocation::RenderMessage { .. } => write_failure(
                        &mut output,
                        id,
                        "not_registered",
                        "sample has no renderer",
                    )?,
                },
            },
            ExtensionHostFrame::Cancel { .. } => {}
            ExtensionHostFrame::Shutdown { .. } => return Ok(()),
            ExtensionHostFrame::Hello { .. } => {
                return Err(anyhow!("received duplicate host hello"));
            }
            ExtensionHostFrame::Response { .. } => {
                return Err(anyhow!("received unsolicited host response"));
            }
        }
    }
}

fn register_capabilities(output: &mut impl Write) -> Result<()> {
    write_frame(
        output,
        &ExtensionFrame::Register {
            registration: ExtensionRegistration::Command {
                command: ExtensionCommandDescriptor {
                    name: "sample".to_owned(),
                    description: Some("Exercise the process extension UI bridge".to_owned()),
                },
            },
        },
    )?;
    write_frame(
        output,
        &ExtensionFrame::Register {
            registration: ExtensionRegistration::Tool {
                tool: ExtensionToolDescriptor {
                    name: "sample_echo".to_owned(),
                    label: "Sample echo".to_owned(),
                    description: "Echo text through the subprocess protocol".to_owned(),
                    parameters: Schema::object_ordered(vec![(
                        "text".to_owned(),
                        Schema::string(),
                        true,
                    )]),
                    execution_mode: ToolExecutionMode::Default,
                    prompt_guidelines: Vec::new(),
                },
            },
        },
    )?;
    for event in ["session_start", "session_shutdown"] {
        write_frame(
            output,
            &ExtensionFrame::Register {
                registration: ExtensionRegistration::EventHook {
                    hook: ExtensionEventHookDescriptor {
                        event: event.to_owned(),
                    },
                },
            },
        )?;
    }
    Ok(())
}

fn await_ui_response(
    input: &mut impl BufRead,
    line: &mut String,
    expected_id: &str,
) -> Result<ExtensionUiResponse> {
    loop {
        read_line(input, line)?;
        let frame: ExtensionHostFrame =
            serde_json::from_str(line).context("reading UI response")?;
        match frame {
            ExtensionHostFrame::Response { id, result } if id == expected_id => {
                let value = protocol_value(result)?;
                return serde_json::from_value(value).context("decoding UI response");
            }
            ExtensionHostFrame::Cancel { id } if id == expected_id => {
                return Ok(ExtensionUiResponse::Cancelled);
            }
            ExtensionHostFrame::Shutdown { .. } => {
                return Err(anyhow!("host shut down during UI request"));
            }
            frame => return Err(anyhow!("unexpected UI response frame: {frame:?}")),
        }
    }
}

fn protocol_value(result: ProtocolResult) -> Result<Value> {
    match result {
        ProtocolResult::Success { value } => Ok(value),
        ProtocolResult::Failure { error } => Err(anyhow!("{}: {}", error.code, error.message)),
    }
}

fn next_request_id(next: &mut u64) -> String {
    *next = next.saturating_add(1);
    format!("sample-ui-{next}")
}

fn write_success(output: &mut impl Write, id: String, value: Value) -> Result<()> {
    write_frame(
        output,
        &ExtensionFrame::Response {
            id,
            result: ProtocolResult::Success { value },
        },
    )
}

fn write_failure(
    output: &mut impl Write,
    id: String,
    code: &str,
    message: &str,
) -> Result<()> {
    write_frame(
        output,
        &ExtensionFrame::Response {
            id,
            result: ProtocolResult::Failure {
                error: pi_coding::ProtocolError {
                    code: code.to_owned(),
                    message: message.to_owned(),
                },
            },
        },
    )
}

fn read_line(input: &mut impl BufRead, line: &mut String) -> Result<()> {
    line.clear();
    let count = input.read_line(line).context("reading extension JSONL")?;
    ensure!(count != 0, "extension protocol input closed");
    ensure!(line.ends_with('\n'), "protocol frame was not LF terminated");
    ensure!(!line.ends_with("\r\n"), "protocol rejected CRLF");
    line.pop();
    Ok(())
}

fn write_frame(output: &mut impl Write, frame: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *output, frame).context("encoding extension JSONL")?;
    output.write_all(b"\n").context("writing extension JSONL")?;
    output.flush().context("flushing extension JSONL")
}
