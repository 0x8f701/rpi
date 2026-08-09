use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use futures_util::FutureExt;
use reqwest::Url;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use tokio::sync::mpsc::unbounded_channel;

use crate::*;

use super::common;
use super::responses::{
    OpenAIResponsesOptions, StreamState, build_responses_params, build_simple_responses_options,
    get_responses_compat, grammar_tool_input_properties, handle_event,
};

const DEFAULT_AZURE_API_VERSION: &str = "v1";
const AZURE_API_VERSION_ENV: &str = "AZURE_OPENAI_API_VERSION";
const AZURE_BASE_URL_ENV: &str = "AZURE_OPENAI_BASE_URL";
const AZURE_RESOURCE_NAME_ENV: &str = "AZURE_OPENAI_RESOURCE_NAME";
const AZURE_DEPLOYMENT_NAME_MAP_ENV: &str = "AZURE_OPENAI_DEPLOYMENT_NAME_MAP";

/// Provider-native options for Azure OpenAI's Responses API.
#[derive(Clone, Default)]
pub struct AzureOpenAIResponsesOptions {
    pub stream: StreamOptions,
    pub reasoning_effort: Option<String>,
    pub reasoning_summary: Option<String>,
    pub service_tier: Option<String>,
    pub tool_choice: Option<Value>,
    pub azure_api_version: Option<String>,
    pub azure_resource_name: Option<String>,
    pub azure_base_url: Option<String>,
    pub azure_deployment_name: Option<String>,
}

impl From<StreamOptions> for AzureOpenAIResponsesOptions {
    fn from(stream: StreamOptions) -> Self {
        Self {
            stream,
            ..Self::default()
        }
    }
}

pub fn register_azure_openai_responses() {
    register_api_provider(
        ApiProvider {
            api: API_AZURE_OPENAI_RESPONSES.into(),
            stream: Arc::new(|model, context, options| {
                async move { stream_azure_openai_responses(model, context, options.into()) }.boxed()
            }),
            stream_simple: Arc::new(|model, context, options| {
                async move { stream_simple_azure_openai_responses(model, context, options) }.boxed()
            }),
            generate_image: None,
        },
        None,
    );
}

pub fn stream_simple_azure_openai_responses(
    model: Model,
    context: Context,
    options: SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let OpenAIResponsesOptions {
        stream,
        reasoning_effort,
        reasoning_summary,
        service_tier,
        tool_choice,
        ..
    } = build_simple_responses_options(&model, &context, options);
    stream_azure_openai_responses(
        model,
        context,
        AzureOpenAIResponsesOptions {
            stream,
            reasoning_effort,
            reasoning_summary,
            service_tier,
            tool_choice,
            ..AzureOpenAIResponsesOptions::default()
        },
    )
}

pub fn stream_azure_openai_responses(
    model: Model,
    context: Context,
    options: AzureOpenAIResponsesOptions,
) -> AssistantMessageEventStream {
    let stream = new_assistant_message_event_stream();
    let task_stream = stream.clone();
    tokio::spawn(async move {
        let mut output = AssistantMessage::pending(&model);

        let auth = match resolve_azure_auth(&model, &options.stream) {
            Ok(auth) => auth,
            Err(error) => {
                common::fail(&task_stream, output, error.to_string(), false).await;
                return;
            }
        };
        let url = match azure_responses_url(&model, &options) {
            Ok(url) => url,
            Err(error) => {
                common::fail(&task_stream, output, error.to_string(), false).await;
                return;
            }
        };
        let deployment_name = resolve_deployment_name(&model, &options);
        let shared_options = OpenAIResponsesOptions {
            stream: options.stream.clone(),
            reasoning_effort: options.reasoning_effort.clone(),
            reasoning_summary: options.reasoning_summary.clone(),
            service_tier: options.service_tier.clone(),
            tool_choice: options.tool_choice.clone(),
            responses_stateful_chain: false,
        };
        let grammar_input_properties = match grammar_tool_input_properties(
            &context.tools,
            get_responses_compat(&model).supports_openai_grammar_tools,
        ) {
            Ok(properties) => properties,
            Err(error) => {
                common::fail(&task_stream, output, error.to_string(), false).await;
                return;
            }
        };
        let mut params = match build_responses_params(&model, &context, &shared_options) {
            Ok(params) => params,
            Err(error) => {
                common::fail(&task_stream, output, error.to_string(), false).await;
                return;
            }
        };
        params["model"] = Value::String(deployment_name);
        let body = match common::apply_provider_request(params, &model, &options.stream).await {
            Ok(body) => body,
            Err(error) => {
                common::fail(&task_stream, output, error.to_string(), false).await;
                return;
            }
        };
        let headers = match azure_request_headers(&model, &options.stream, auth) {
            Ok(headers) => match common::apply_provider_headers(headers, &model, &options.stream).await {
                Ok(headers) => headers,
                Err(error) => {
                    common::fail(&task_stream, output, error.to_string(), false).await;
                    return;
                }
            },
            Err(error) => {
                common::fail(&task_stream, output, error.to_string(), false).await;
                return;
            }
        };
        let client = match common::client(&options.stream) {
            Ok(client) => client,
            Err(error) => {
                common::fail(&task_stream, output, error.to_string(), false).await;
                return;
            }
        };

        let response = match common::send_with_retry(&options.stream, || {
            client
                .post(url.clone())
                .headers(headers.clone())
                .json(&body)
        })
        .await
        {
            Ok(response) => response,
            Err(error) => {
                let aborted = common::is_aborted(&options.stream);
                common::fail(&task_stream, output, error.to_string(), aborted).await;
                return;
            }
        };
        if let Err(error) = common::notify_response(&options.stream, &response, &model).await {
            common::fail(&task_stream, output, error.to_string(), false).await;
            return;
        }
        if !response.status().is_success() {
            let (message, aborted) =
                match common::error_body("Azure OpenAI Responses", response, &options.stream).await
                {
                    Ok(message) => (message, common::is_aborted(&options.stream)),
                    Err(error) => (error.to_string(), true),
                };
            common::fail(&task_stream, output, message, aborted).await;
            return;
        }

        task_stream
            .push(AssistantMessageEvent::Start {
                partial: output.clone(),
            })
            .await;
        let (sender, mut receiver) = unbounded_channel::<AssistantMessageEvent>();
        let drain_stream = task_stream.clone();
        let drainer = tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                drain_stream.push(event).await;
            }
        });

        let mut state = StreamState::with_grammar_input_properties(grammar_input_properties);
        let stream_result = common::consume_sse(response, &options.stream, |name, data| {
            handle_event(
                name,
                data,
                &mut output,
                &mut state,
                &model,
                options.service_tier.as_deref(),
                &sender,
            )
        })
        .await;
        drop(sender);
        let _ = drainer.await;

        if let Err(error) = stream_result {
            let aborted = common::is_aborted(&options.stream);
            common::fail(&task_stream, output, error.to_string(), aborted).await;
            return;
        }
        if common::is_aborted(&options.stream) {
            common::fail(&task_stream, output, "Request was aborted", true).await;
            return;
        }
        if !state.saw_terminal {
            common::fail(
                &task_stream,
                output,
                "Azure OpenAI Responses stream ended before a terminal response event",
                false,
            )
            .await;
            return;
        }
        if output.stop_reason == StopReason::Pending {
            common::fail(
                &task_stream,
                output,
                "Azure OpenAI Responses stream ended without a stop reason",
                false,
            )
            .await;
            return;
        }
        if matches!(output.stop_reason, StopReason::Error | StopReason::Aborted) {
            common::fail(&task_stream, output, "An unknown error occurred", false).await;
            return;
        }

        state.materialize(&mut output);
        task_stream
            .push(AssistantMessageEvent::Done {
                reason: output.stop_reason,
                message: output.clone(),
            })
            .await;
        task_stream.end(Some(output)).await;
    });
    stream
}

#[derive(Clone)]
struct AzureAuth {
    name: &'static str,
    value: String,
}

impl std::fmt::Debug for AzureAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AzureAuth")
            .field("name", &self.name)
            .field("value", &"[REDACTED]")
            .finish()
    }
}

fn resolve_azure_auth(model: &Model, options: &StreamOptions) -> Result<AzureAuth> {
    if let Some(auth) = auth_from_headers(&options.headers, "request")? {
        return Ok(auth);
    }
    if let Some(model_headers) = &model.headers {
        if let Some(auth) = auth_from_headers(model_headers, "model")? {
            return Ok(auth);
        }
    }
    if let Some(api_key) = options
        .api_key
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(AzureAuth {
            name: "api-key",
            value: api_key.to_string(),
        });
    }
    Err(anyhow!(
        "No API key or Entra bearer token for provider: {}",
        model.provider
    ))
}

fn auth_from_headers(headers: &HashMap<String, String>, scope: &str) -> Result<Option<AzureAuth>> {
    let mut matches = headers.iter().filter_map(|(name, value)| {
        let canonical = if name.eq_ignore_ascii_case("authorization") {
            Some("authorization")
        } else if name.eq_ignore_ascii_case("api-key") {
            Some("api-key")
        } else {
            None
        }?;
        (!value.trim().is_empty()).then(|| AzureAuth {
            name: canonical,
            value: value.clone(),
        })
    });
    let first = matches.next();
    if matches.next().is_some() {
        return Err(anyhow!(
            "Multiple Azure OpenAI authentication headers in {scope} headers; provide exactly one non-empty Authorization or api-key header"
        ));
    }
    Ok(first)
}

fn azure_request_headers(
    model: &Model,
    options: &StreamOptions,
    auth: AzureAuth,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    if let Some(attribution) = merge_provider_attribution_headers(
        model,
        options.session_id.as_deref(),
        false,
        &HashMap::new(),
    ) {
        common::insert_header_map(&mut headers, &attribution)?;
    }
    common::insert_header(&mut headers, "content-type", "application/json")?;
    common::insert_header(&mut headers, "accept", "text/event-stream")?;
    if let Some(model_headers) = &model.headers {
        insert_non_auth_headers(&mut headers, model_headers)?;
    }
    insert_non_auth_headers(&mut headers, &options.headers)?;
    insert_sensitive_header(&mut headers, auth.name, &auth.value)?;
    Ok(headers)
}

fn insert_non_auth_headers(
    headers: &mut HeaderMap,
    source: &HashMap<String, String>,
) -> Result<()> {
    for (name, value) in source {
        if name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("api-key") {
            continue;
        }
        common::insert_header(headers, name, value)?;
    }
    Ok(())
}

fn insert_sensitive_header(headers: &mut HeaderMap, name: &'static str, value: &str) -> Result<()> {
    let mut value = HeaderValue::try_from(value)
        .map_err(|error| anyhow!("Invalid HTTP header value for {name}: {error}"))?;
    value.set_sensitive(true);
    headers.insert(HeaderName::from_static(name), value);
    Ok(())
}

fn explicit_env_value<'a>(options: &'a StreamOptions, name: &str) -> Option<&'a str> {
    options
        .env
        .get(name)
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
}

fn resolve_deployment_name(model: &Model, options: &AzureOpenAIResponsesOptions) -> String {
    if let Some(name) = options
        .azure_deployment_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
    {
        return name.to_string();
    }
    parse_deployment_name_map(explicit_env_value(
        &options.stream,
        AZURE_DEPLOYMENT_NAME_MAP_ENV,
    ))
    .remove(&model.id)
    .unwrap_or_else(|| model.id.clone())
}

fn parse_deployment_name_map(value: Option<&str>) -> HashMap<String, String> {
    let mut deployments = HashMap::new();
    let Some(value) = value else {
        return deployments;
    };
    for entry in value.split(',') {
        let Some((model_id, deployment_name)) = entry.trim().split_once('=') else {
            continue;
        };
        let model_id = model_id.trim();
        let deployment_name = deployment_name.trim();
        if !model_id.is_empty() && !deployment_name.is_empty() {
            deployments.insert(model_id.to_string(), deployment_name.to_string());
        }
    }
    deployments
}

fn azure_responses_url(model: &Model, options: &AzureOpenAIResponsesOptions) -> Result<Url> {
    let base_url = options
        .azure_base_url
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| explicit_env_value(&options.stream, AZURE_BASE_URL_ENV))
        .map(str::to_string)
        .or_else(|| {
            options
                .azure_resource_name
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| explicit_env_value(&options.stream, AZURE_RESOURCE_NAME_ENV))
                .map(|resource| format!("https://{}.openai.azure.com/openai/v1", resource.trim()))
        })
        .or_else(|| (!model.base_url.trim().is_empty()).then(|| model.base_url.clone()))
        .ok_or_else(|| {
            anyhow!(
                "Azure OpenAI base URL is required; provide azure_base_url, AZURE_OPENAI_BASE_URL, azure_resource_name, AZURE_OPENAI_RESOURCE_NAME, or model.base_url"
            )
        })?;
    let api_version = options
        .azure_api_version
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| explicit_env_value(&options.stream, AZURE_API_VERSION_ENV))
        .unwrap_or(DEFAULT_AZURE_API_VERSION);

    let mut url = normalize_azure_base_url(&base_url)?;
    let path = url.path().trim_end_matches('/');
    url.set_path(&format!("{path}/responses"));
    url.query_pairs_mut()
        .append_pair("api-version", api_version);
    Ok(url)
}

fn normalize_azure_base_url(base_url: &str) -> Result<Url> {
    let mut url =
        Url::parse(base_url.trim()).map_err(|_| anyhow!("Invalid Azure OpenAI base URL"))?;
    let host = url.host_str().unwrap_or_default();
    let is_azure_host = host.ends_with(".openai.azure.com")
        || host.ends_with(".cognitiveservices.azure.com")
        || host.ends_with(".ai.azure.com");
    let path = url.path().trim_end_matches('/');
    if is_azure_host && matches!(path, "" | "/openai" | "/openai/v1/responses") {
        url.set_path("/openai/v1");
        url.set_query(None);
    } else if url.path().ends_with('/') && url.path() != "/" {
        let trimmed = path.to_string();
        url.set_path(&trimmed);
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::json;
    use tokio::sync::oneshot;
    use tokio_util::sync::CancellationToken;

    use super::*;

    const SSE: &str = "\
data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_azure\"}}\n\
\n\
data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_azure\"}}\n\
\n\
data: {\"type\":\"response.reasoning_summary_text.delta\",\"output_index\":0,\"delta\":\"thinking\"}\n\
\n\
data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_azure\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"thinking\"}]}}\n\
\n\
data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"message\",\"id\":\"msg_azure\"}}\n\
\n\
data: {\"type\":\"response.output_text.delta\",\"output_index\":1,\"delta\":\"Azure answer\"}\n\
\n\
data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"message\",\"id\":\"msg_azure\",\"content\":[{\"type\":\"output_text\",\"text\":\"Azure answer\"}]}}\n\
\n\
data: {\"type\":\"response.output_item.added\",\"output_index\":2,\"item\":{\"type\":\"function_call\",\"id\":\"fc_azure\",\"call_id\":\"call_azure\",\"name\":\"lookup\",\"arguments\":\"\"}}\n\
\n\
data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":2,\"delta\":\"{\\\"query\\\":\\\"x\\\"}\"}\n\
\n\
data: {\"type\":\"response.output_item.done\",\"output_index\":2,\"item\":{\"type\":\"function_call\",\"id\":\"fc_azure\",\"call_id\":\"call_azure\",\"name\":\"lookup\",\"arguments\":\"{\\\"query\\\":\\\"x\\\"}\"}}\n\
\n\
data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_azure\",\"status\":\"completed\",\"output\":[{\"type\":\"reasoning\",\"id\":\"rs_azure\",\"encrypted_content\":\"ciphertext\"}],\"usage\":{\"input_tokens\":20,\"output_tokens\":8,\"total_tokens\":28,\"input_tokens_details\":{\"cached_tokens\":5},\"output_tokens_details\":{\"reasoning_tokens\":3}}}}\n\
\n";

    #[derive(Clone, Debug)]
    struct CapturedRequest {
        path: String,
        headers: HashMap<String, Vec<String>>,
        body: String,
    }

    fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn request_content_length(headers: &[u8]) -> usize {
        String::from_utf8_lossy(headers)
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|value| value.trim().parse().ok())
            })
            .unwrap_or(0)
    }

    fn read_request(socket: &mut std::net::TcpStream) -> CapturedRequest {
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            match socket.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => request.extend_from_slice(&buffer[..read]),
                Err(_) => break,
            }
            if let Some(header_end) = find_subsequence(&request, b"\r\n\r\n") {
                let content_length = request_content_length(&request[..header_end]);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
        }
        let header_end = find_subsequence(&request, b"\r\n\r\n").unwrap_or(request.len());
        let head = String::from_utf8_lossy(&request[..header_end]);
        let body_start = (header_end + 4).min(request.len());
        let mut lines = head.lines();
        let path = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or_default()
            .to_string();
        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                headers
                    .entry(name.to_ascii_lowercase())
                    .or_default()
                    .push(value.trim().to_string());
            }
        }
        CapturedRequest {
            path,
            headers,
            body: String::from_utf8_lossy(&request[body_start..]).to_string(),
        }
    }

    fn spawn_sse_server(
        captured: Arc<Mutex<Vec<CapturedRequest>>>,
        failures: usize,
    ) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture server");
        let address = listener.local_addr().expect("fixture address");
        let attempts = Arc::new(AtomicUsize::new(0));
        let task_attempts = attempts.clone();
        std::thread::spawn(move || {
            for attempt in 0..=failures {
                let (mut socket, _) = listener.accept().expect("accept fixture request");
                captured
                    .lock()
                    .expect("capture lock")
                    .push(read_request(&mut socket));
                task_attempts.fetch_add(1, Ordering::SeqCst);
                if attempt < failures {
                    socket
                        .write_all(
                            b"HTTP/1.1 429 Too Many Requests\r\nRetry-After-Ms: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .expect("write retry response");
                } else {
                    socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                        )
                        .expect("write SSE headers");
                    socket.write_all(SSE.as_bytes()).expect("write SSE body");
                }
                socket.flush().expect("flush fixture response");
            }
        });
        (format!("http://{address}"), attempts)
    }

    fn azure_model(base_url: String) -> Model {
        Model {
            id: "gpt-5.4".into(),
            name: "GPT-5.4".into(),
            api: API_AZURE_OPENAI_RESPONSES.into(),
            provider: "azure-openai-responses".into(),
            base_url,
            reasoning: true,
            max_tokens: 4096,
            cost: ModelCost {
                input: 1.25,
                output: 10.0,
                cache_read: 0.25,
                ..ModelCost::default()
            },
            ..Model::default()
        }
    }

    fn request_context() -> Context {
        let parameters = Schema::object(
            HashMap::from([(
                "query".into(),
                Schema {
                    schema_type: Some(Value::String("string".into())),
                    ..Schema::default()
                },
            )]),
            vec!["query".into()],
        );
        Context {
            system_prompt: "Be precise".into(),
            messages: vec![Message::user_text("Use the lookup tool", 1)],
            tools: vec![ToolDefinition {
                name: "lookup".into(),
                description: "Look up a value".into(),
                parameters,
                constrained_sampling: None,
            }],
        }
    }

    #[test]
    fn azure_url_normalizes_resource_hosts_and_encodes_api_version() {
        let model = azure_model(String::new());
        let options = AzureOpenAIResponsesOptions {
            azure_base_url: Some("https://example.openai.azure.com/".into()),
            azure_api_version: Some("2025-04-01 preview".into()),
            ..AzureOpenAIResponsesOptions::default()
        };
        let url = azure_responses_url(&model, &options).expect("Azure URL");
        assert_eq!(
            url.as_str(),
            "https://example.openai.azure.com/openai/v1/responses?api-version=2025-04-01+preview"
        );
    }

    #[test]
    fn azure_config_and_deployment_precedence_match_upstream() {
        let model = azure_model("https://model.example.test/base".into());
        let options = AzureOpenAIResponsesOptions {
            stream: StreamOptions {
                env: HashMap::from([
                    (
                        AZURE_BASE_URL_ENV.into(),
                        "https://env.example.test/root".into(),
                    ),
                    (AZURE_API_VERSION_ENV.into(), "env-version".into()),
                    (
                        AZURE_DEPLOYMENT_NAME_MAP_ENV.into(),
                        " ignored , gpt-5.4 = env-deployment , malformed=".into(),
                    ),
                ]),
                ..StreamOptions::default()
            },
            azure_base_url: Some("https://option.example.test/api".into()),
            azure_api_version: Some("option-version".into()),
            azure_deployment_name: Some("option-deployment".into()),
            ..AzureOpenAIResponsesOptions::default()
        };
        assert_eq!(
            azure_responses_url(&model, &options)
                .expect("Azure URL")
                .as_str(),
            "https://option.example.test/api/responses?api-version=option-version"
        );
        assert_eq!(
            resolve_deployment_name(&model, &options),
            "option-deployment"
        );

        let mapped = AzureOpenAIResponsesOptions {
            stream: options.stream,
            ..AzureOpenAIResponsesOptions::default()
        };
        assert_eq!(resolve_deployment_name(&model, &mapped), "env-deployment");
        assert_eq!(
            azure_responses_url(&model, &mapped)
                .expect("env Azure URL")
                .as_str(),
            "https://env.example.test/root/responses?api-version=env-version"
        );
    }

    #[test]
    fn caller_auth_header_wins_case_insensitively_and_preserves_kind() {
        let model = Model {
            provider: "azure-openai-responses".into(),
            headers: Some(HashMap::from([
                ("Authorization".into(), "Bearer model-token".into()),
                ("X-Shared".into(), "model".into()),
            ])),
            ..Model::default()
        };
        let options = StreamOptions {
            api_key: Some("option-api-key".into()),
            headers: HashMap::from([
                ("API-KEY".into(), "caller-api-key".into()),
                ("x-shared".into(), "caller".into()),
            ]),
            ..StreamOptions::default()
        };
        let auth = resolve_azure_auth(&model, &options).expect("caller auth");
        assert_eq!(auth.name, "api-key");
        let headers = azure_request_headers(&model, &options, auth).expect("Azure headers");
        assert_eq!(headers.get_all("api-key").iter().count(), 1);
        assert_eq!(
            headers.get("api-key").and_then(|value| value.to_str().ok()),
            Some("caller-api-key")
        );
        assert!(!headers.contains_key("authorization"));
        assert_eq!(
            headers
                .get("x-shared")
                .and_then(|value| value.to_str().ok()),
            Some("caller")
        );
    }

    #[test]
    fn model_entra_bearer_wins_over_option_key_when_caller_auth_is_empty() {
        let model = Model {
            provider: "azure-openai-responses".into(),
            headers: Some(HashMap::from([(
                "aUtHoRiZaTiOn".into(),
                "Bearer model-entra-token".into(),
            )])),
            ..Model::default()
        };
        let options = StreamOptions {
            api_key: Some("option-api-key".into()),
            headers: HashMap::from([("Api-Key".into(), "   ".into())]),
            ..StreamOptions::default()
        };
        let auth = resolve_azure_auth(&model, &options).expect("model auth");
        assert_eq!(auth.name, "authorization");
        let headers = azure_request_headers(&model, &options, auth).expect("Azure headers");
        assert_eq!(headers.get_all("authorization").iter().count(), 1);
        assert_eq!(
            headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some("Bearer model-entra-token")
        );
        assert!(!headers.contains_key("api-key"));
    }

    #[test]
    fn dual_auth_in_one_scope_fails_closed_without_secret_leakage() {
        let caller_secret = "caller-secret-must-not-leak";
        let bearer_secret = "bearer-secret-must-not-leak";
        let options = StreamOptions {
            headers: HashMap::from([
                ("Authorization".into(), format!("Bearer {bearer_secret}")),
                ("API-key".into(), caller_secret.into()),
            ]),
            ..StreamOptions::default()
        };
        let error = resolve_azure_auth(&Model::default(), &options).expect_err("ambiguous auth");
        assert!(
            error
                .to_string()
                .contains("Multiple Azure OpenAI authentication headers")
        );
        assert!(!error.to_string().contains(caller_secret));
        assert!(!error.to_string().contains(bearer_secret));
    }

    #[test]
    fn missing_auth_error_is_sanitized() {
        let model = Model {
            provider: "azure-openai-responses".into(),
            headers: Some(HashMap::from([(
                "X-Secret".into(),
                "model-secret-must-not-leak".into(),
            )])),
            ..Model::default()
        };
        let error =
            resolve_azure_auth(&model, &StreamOptions::default()).expect_err("missing Azure auth");
        assert_eq!(
            error.to_string(),
            "No API key or Entra bearer token for provider: azure-openai-responses"
        );
        assert!(!error.to_string().contains("model-secret-must-not-leak"));
    }

    #[test]
    fn malformed_selected_auth_value_is_rejected_without_secret_leakage() {
        let secret = "secret-must-not-leak";
        let model = Model {
            provider: "azure-openai-responses".into(),
            ..Model::default()
        };
        let options = StreamOptions {
            headers: HashMap::from([(
                "AUTHORIZATION".into(),
                format!("Bearer {secret}\ninjected: value"),
            )]),
            ..StreamOptions::default()
        };
        let auth = resolve_azure_auth(&model, &options).expect("selected caller auth");
        let error =
            azure_request_headers(&model, &options, auth).expect_err("malformed auth value");
        assert!(error.to_string().contains("authorization"));
        assert!(!error.to_string().contains(secret));
    }

    #[test]
    fn selected_auth_debug_is_redacted() {
        let secret = "debug-secret-must-not-leak";
        let auth = AzureAuth { name: "api-key", value: secret.into() };
        let debug = format!("{auth:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains(secret));
    }

    #[test]
    fn registers_azure_responses_api() {
        register_azure_openai_responses();
        assert!(get_api_provider(API_AZURE_OPENAI_RESPONSES).is_some());
    }

    #[tokio::test]
    async fn azure_payload_headers_and_sse_reuse_responses_semantics() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (base_url, _) = spawn_sse_server(captured.clone(), 0);
        let model = azure_model(base_url);
        let payload_seen = Arc::new(Mutex::new(None::<Value>));
        let payload_capture = payload_seen.clone();
        let responses_seen = Arc::new(Mutex::new(Vec::<ProviderResponse>::new()));
        let response_capture = responses_seen.clone();
        let options = AzureOpenAIResponsesOptions {
            stream: StreamOptions {
                api_key: Some("azure-api-key".into()),
                on_payload: Some(Arc::new(move |payload, _| {
                    *payload_capture.lock().expect("payload hook lock") = Some(payload.clone());
                    Ok(payload)
                })),
                on_response: Some(Arc::new(move |response, _| {
                    response_capture
                        .lock()
                        .expect("response hook lock")
                        .push(response);
                    Ok(())
                })),
                ..StreamOptions::default()
            },
            reasoning_effort: Some("high".into()),
            azure_api_version: Some("2025-04-01-preview".into()),
            azure_deployment_name: Some("production-deployment".into()),
            ..AzureOpenAIResponsesOptions::default()
        };
        let stream = stream_azure_openai_responses(model, request_context(), options);
        let final_message = stream.result().await.expect("final Azure message");
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }

        assert_eq!(final_message.stop_reason, StopReason::ToolUse);
        assert_eq!(final_message.response_id.as_deref(), Some("resp_azure"));
        assert_eq!(final_message.usage.input, 15);
        assert_eq!(final_message.usage.cache_read, 5);
        assert_eq!(final_message.usage.output, 8);
        assert_eq!(final_message.usage.reasoning, 3);
        assert_eq!(final_message.text(), "Azure answer");
        let thinking_signature = final_message.content.iter().find_map(|block| match block {
            ContentBlock::Thinking {
                thinking_signature, ..
            } => thinking_signature.as_deref(),
            _ => None,
        });
        assert!(thinking_signature.is_some_and(|signature| signature.contains("ciphertext")));
        assert!(final_message.content.iter().any(|block| matches!(
            block,
            ContentBlock::ToolCall(call)
                if call.id == "call_azure|fc_azure"
                    && call.name == "lookup"
                    && call.arguments == json!({"query": "x"})
        )));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AssistantMessageEvent::Start { .. }))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AssistantMessageEvent::ThinkingDelta { delta, .. } if delta == "thinking"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AssistantMessageEvent::TextDelta { delta, .. } if delta == "Azure answer"
        )));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AssistantMessageEvent::ToolCallEnd { .. }))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AssistantMessageEvent::Done { .. }))
        );
        assert_eq!(
            payload_seen
                .lock()
                .expect("payload hook lock")
                .as_ref()
                .and_then(|payload| payload.get("model"))
                .and_then(Value::as_str),
            Some("production-deployment")
        );
        let responses = responses_seen.lock().expect("response hook lock");
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].status, 200);

        let requests = captured.lock().expect("capture lock");
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.path, "/responses?api-version=2025-04-01-preview");
        assert_eq!(
            request.headers.get("api-key"),
            Some(&vec!["azure-api-key".into()])
        );
        assert!(!request.headers.contains_key("authorization"));
        assert_eq!(
            request.headers.get("accept"),
            Some(&vec!["text/event-stream".into()])
        );
        let payload: Value = serde_json::from_str(&request.body).expect("Azure payload JSON");
        assert_eq!(payload["model"], "production-deployment");
        assert_eq!(payload["stream"], true);
        assert_eq!(payload["store"], false);
        assert_eq!(payload["reasoning"]["effort"], "high");
        assert_eq!(payload["reasoning"]["summary"], "auto");
        assert_eq!(payload["include"][0], "reasoning.encrypted_content");
        assert_eq!(payload["input"][0]["role"], "developer");
        assert_eq!(payload["tools"][0]["name"], "lookup");
    }

    #[tokio::test]
    async fn azure_maps_structured_http_errors_without_secret_leakage() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind error server");
        let address = listener.local_addr().expect("error server address");
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept error request");
            let _ = read_request(&mut socket);
            let body = r#"{"error":{"message":"credential rejected","code":"invalid_api_key"}}"#;
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).expect("write error response");
            socket.flush().expect("flush error response");
        });

        let secret = "azure-secret-must-not-leak";
        let model = azure_model(format!("http://{address}"));
        let stream = stream_azure_openai_responses(
            model,
            Context::default(),
            AzureOpenAIResponsesOptions {
                stream: StreamOptions {
                    api_key: Some(secret.into()),
                    ..StreamOptions::default()
                },
                ..AzureOpenAIResponsesOptions::default()
            },
        );
        let result = stream.result().await.expect("Azure error result");
        assert_eq!(result.stop_reason, StopReason::Error);
        assert_eq!(
            result.error_message.as_deref(),
            Some("Azure OpenAI Responses API error 401: credential rejected (invalid_api_key)")
        );
        assert!(!result.error_message.as_deref().unwrap_or_default().contains(secret));
    }

    #[tokio::test]
    async fn azure_retries_retryable_status_with_identical_request() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (base_url, attempts) = spawn_sse_server(captured.clone(), 1);
        let model = azure_model(base_url);
        let options = AzureOpenAIResponsesOptions {
            stream: StreamOptions {
                api_key: Some("retry-key".into()),
                max_retries: 1,
                ..StreamOptions::default()
            },
            ..AzureOpenAIResponsesOptions::default()
        };
        let stream = stream_azure_openai_responses(model, request_context(), options);
        let final_message = stream.result().await.expect("retried Azure response");
        assert_eq!(final_message.stop_reason, StopReason::ToolUse);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        let requests = captured.lock().expect("capture lock");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].path, requests[1].path);
        assert_eq!(requests[0].body, requests[1].body);
    }

    #[tokio::test]
    async fn azure_cancellation_aborts_stalled_sse() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stalled server");
        let address = listener.local_addr().expect("stalled address");
        let (headers_sent, headers_seen) = oneshot::channel();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept stalled request");
            let _ = read_request(&mut socket);
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .expect("write stalled headers");
            socket.flush().expect("flush stalled headers");
            let _ = headers_sent.send(());
            let mut buffer = [0_u8; 1];
            let _ = socket.read(&mut buffer);
        });

        let token = CancellationToken::new();
        let model = azure_model(format!("http://{address}"));
        let options = AzureOpenAIResponsesOptions {
            stream: StreamOptions {
                api_key: Some("cancel-key".into()),
                abort_signal: Some(token.clone()),
                ..StreamOptions::default()
            },
            ..AzureOpenAIResponsesOptions::default()
        };
        let stream = stream_azure_openai_responses(model, Context::default(), options);
        headers_seen.await.expect("server sent headers");
        token.cancel();
        let result = tokio::time::timeout(Duration::from_secs(2), stream.result())
            .await
            .expect("cancellation completes")
            .expect("aborted result");
        assert_eq!(result.stop_reason, StopReason::Aborted);
        assert_eq!(result.error_message.as_deref(), Some("Request was aborted"));
    }
}
