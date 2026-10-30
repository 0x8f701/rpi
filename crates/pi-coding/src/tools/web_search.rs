//! `web_search` tool: one-call Exa web search.
//!
//! POSTs `https://api.exa.ai/search` once per call with the query and a
//! bounded summary request, then renders ranked title/url/snippet blocks.
//! The tool never launches a browser, never fetches result URLs, and never
//! fetches page bodies — result URLs are meant for a later explicit
//! read/browser call. `EXA_API_KEY` is resolved lazily at execution:
//! absent/empty keys fail explicitly (Exa is the only backend; there is no
//! silent fallback), and the key value is never logged, echoed, or included
//! in results. Disabled wholesale when `RPI_OFFLINE` is set to a truthy
//! value, matching the CLI truthy semantics (`1`/`true`/`yes`).

use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::Value;

use pi_agent::{AbortSignal, AgentTool, AgentToolResult, ToolCapability};

use crate::redact::redact_secrets;
use crate::truncate::truncate_head;

use super::{arg_int, arg_str, check_aborted, s_number, s_object, s_string, text_result};

/// Number of results requested from Exa (`numResults`) and the hard cap on
/// `max_results` (everything beyond this is noise for the agent context).
const MAX_RESULTS_CAP: i64 = 10;
const DEFAULT_MAX_RESULTS: i64 = 5;
/// Exa search timeout (covers connect + full body).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Output byte budget — keeps agent context lean.
const OUTPUT_MAX_BYTES: usize = 4 * 1024;
/// Whole response-body cap for the search JSON (bounded independent of the
/// rendered output so a hostile payload cannot stream unbounded bytes).
const RESPONSE_MAX_BYTES: u64 = 256 * 1024;
/// Snippet length cap (chars, pre-rendered summaries/highlights).
const SNIPPET_MAX_CHARS: usize = 500;
const EXA_SEARCH_URL: &str = "https://api.exa.ai/search";
const EXA_API_KEY_ENV: &str = "EXA_API_KEY";
const USER_AGENT: &str = concat!("rpi/", env!("CARGO_PKG_VERSION"));
const RPI_OFFLINE_ENV: &str = "RPI_OFFLINE";
const OFFLINE_MESSAGE: &str = "web_search is disabled while RPI_OFFLINE is set";
const NO_RESULTS_MESSAGE: &str =
    "web_search upstream (Exa) returned no usable results; try a different query or check the provider";

/// One rendered search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Builds a schema-compatible standalone tool that fails closed without a
/// captured application generation.
pub(crate) fn web_search_tool() -> AgentTool {
    let params = s_object(
        vec![
            ("query", s_string("Search query (plain text)")),
            (
                "max_results",
                s_number(&format!(
                    "Maximum results to return (default {DEFAULT_MAX_RESULTS}, max {MAX_RESULTS_CAP})"
                )),
            ),
        ],
        vec!["query"],
    );
    let description = format!(
        "Search the web via the Exa API and return ranked plain-text result blocks (title / url / snippet). \
Output is bounded to {}KB. Disabled while RPI_OFFLINE is set.",
        OUTPUT_MAX_BYTES / 1024
    );
    AgentTool::new("web_search", description, params, move |ctx| async move {
        run_web_search(ctx.arguments, ctx.abort).await
    })
    .with_capability(ToolCapability::Read)
}

pub(crate) fn web_search_tool_with_generation(
    capture: crate::tools::GenerationCaptureFn,
) -> AgentTool {
    let mut tool = web_search_tool();
    tool.execute = std::sync::Arc::new(move |ctx| {
        let capture = capture.clone();
        Box::pin(async move {
            let offline = offline_disabled();
            if offline {
                return Err(anyhow!(OFFLINE_MESSAGE));
            }
            let generation = capture()?;
            let api_key = exa_api_key();
            run_web_search_with_executor(
                ctx.arguments,
                ctx.abort,
                offline,
                api_key.as_deref(),
                generation
                    .sandbox()
                    .http()
                    .executor(pi_sandbox::HostHttpClass::WebSearch),
            )
            .await
        })
    });
    tool
}

/// Truthy semantics for `RPI_OFFLINE`, matching the CLI: any of
/// `1`/`true`/`yes` (case-insensitive, trimmed) is "on".
fn offline_disabled_from(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

/// Reads the live `RPI_OFFLINE` env var. Kept as a tiny seam so the truthy
/// parsing is unit-testable without mutating the process environment.
fn offline_disabled() -> bool {
    offline_disabled_from(std::env::var(RPI_OFFLINE_ENV).ok().as_deref())
}

/// Reads the live `EXA_API_KEY` env var. Presence-only seam: the value is
/// passed to the executor path but never logged, echoed, or embedded in
/// results, and tests inject a dummy key instead of consulting the process
/// environment.
fn exa_api_key() -> Option<String> {
    exa_api_key_from(std::env::var(EXA_API_KEY_ENV).ok().as_deref())
}

fn exa_api_key_from(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
}

pub(crate) async fn run_web_search(args: Value, abort: AbortSignal) -> Result<AgentToolResult> {
    run_web_search_with(args, abort, offline_disabled()).await
}

/// Core entry with an injectable offline flag so the disabled path is testable
/// without mutating the process environment (the workspace forbids `unsafe`,
/// which `std::env::set_var` requires in edition 2024). Fails closed: no
/// generation, no network authority, no search.
pub(crate) async fn run_web_search_with(
    args: Value,
    abort: AbortSignal,
    offline: bool,
) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    if offline {
        return Err(anyhow!(OFFLINE_MESSAGE));
    }
    if arg_str(&args, "query").trim().is_empty() {
        return Err(anyhow!("query must not be empty"));
    }
    let _ = arg_int(&args, "max_results")?;
    Err(anyhow!("web_search requires an application generation"))
}

async fn run_web_search_with_executor(
    args: Value,
    abort: AbortSignal,
    offline: bool,
    api_key: Option<&str>,
    executor: pi_ai::SharedHttpExecutor,
) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    if offline {
        return Err(anyhow!(OFFLINE_MESSAGE));
    }
    let query = arg_str(&args, "query");
    if query.trim().is_empty() {
        return Err(anyhow!("query must not be empty"));
    }
    let max_results = arg_int(&args, "max_results")?
        .unwrap_or(DEFAULT_MAX_RESULTS)
        .clamp(1, MAX_RESULTS_CAP) as usize;
    let Some(api_key) = exa_api_key_from(api_key) else {
        return Err(anyhow!(
            "web_search is not configured: Exa is the only search backend and EXA_API_KEY is not set"
        ));
    };

    let body = serde_json::to_vec(&serde_json::json!({
        "query": query,
        "numResults": max_results,
        "type": "auto",
        "contents": { "summary": { "query": query } }
    }))?;

    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static(USER_AGENT),
    );
    headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        http::HeaderName::from_static("x-api-key"),
        http::HeaderValue::from_str(&api_key)
            .map_err(|_| anyhow!("EXA_API_KEY contains invalid header characters"))?,
    );

    let request = pi_ai::request_from_url(
        pi_ai::HttpMethod::Post,
        EXA_SEARCH_URL,
        &headers,
        body,
        &pi_ai::StreamOptions {
            timeout_ms: Some(REQUEST_TIMEOUT.as_millis() as u64),
            abort_signal: Some(abort.cancellation_token()),
            ..Default::default()
        },
        RESPONSE_MAX_BYTES,
    )?;
    // Exactly one network request per call; result URLs are never fetched.
    let response = executor.execute(request).await.map_err(|error| {
        anyhow!(
            "web_search request failed: {}",
            redact_secrets(&error.to_string())
        )
    })?;
    let status = response.status();
    let bytes = pi_ai::collect_response_body(response).await.map_err(|error| {
        anyhow!(
            "web_search response failed: {}",
            redact_secrets(&error.to_string())
        )
    })?;
    check_aborted(&abort)?;
    if !status.is_success() {
        return Err(anyhow!("{}", status_error_message(status)));
    }
    let parsed: Value = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow!("web_search upstream returned an invalid response (malformed JSON)"))?;
    let results = parse_exa_results(&parsed, max_results)?;
    if results.is_empty() {
        return Err(anyhow!(NO_RESULTS_MESSAGE));
    }
    let output = format_results(&results);
    let tr = truncate_head(&output, usize::MAX, OUTPUT_MAX_BYTES);
    Ok(text_result(tr.content))
}

/// Stable status categories without echoing any upstream body content.
fn status_error_message(status: http::StatusCode) -> String {
    match status.as_u16() {
        400 | 422 => format!("web_search request rejected as invalid by upstream (HTTP {status})"),
        401 => "web_search upstream rejected the credential (HTTP 401): check EXA_API_KEY".into(),
        429 => "web_search quota exceeded upstream (HTTP 429)".into(),
        code if (500..600).contains(&code) => {
            format!("web_search upstream unavailable (HTTP {status})")
        }
        _ => format!("web_search upstream rejected the request (HTTP {status})"),
    }
}

/// Parses an Exa search JSON body into bounded result blocks.
///
/// The root must be a JSON object carrying a `results` array; anything else
/// (non-object root, array root, missing/non-array `results`) is an invalid
/// upstream response, not an empty result set. Rows must carry an absolute
/// http/https `url` to be usable and are otherwise skipped: rank order is
/// preserved, the title falls back to the URL, and the snippet prefers
/// `summary`, then joined `highlights`, capped at [`SNIPPET_MAX_CHARS`] with
/// whitespace normalized.
pub(crate) fn parse_exa_results(body: &Value, max_results: usize) -> Result<Vec<SearchResult>> {
    let Some(rows) = body.get("results").and_then(Value::as_array) else {
        return Err(anyhow!(
            "web_search upstream returned an invalid response (missing results array)"
        ));
    };
    let max_results = max_results.max(1);
    let mut out: Vec<SearchResult> = Vec::with_capacity(max_results);
    for row in rows {
        if out.len() >= max_results {
            break;
        }
        let Some(url) = absolute_http_url(row.get("url").and_then(Value::as_str)) else {
            continue;
        };
        let title = row
            .get("title")
            .and_then(Value::as_str)
            .map(bounded_snippet)
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| url.clone());
        let snippet = row
            .get("summary")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| {
                row.get("highlights")
                    .and_then(Value::as_array)
                    .map(|highlights| {
                        highlights
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
            })
            .map(|snippet| bounded_snippet(&snippet))
            .unwrap_or_default();
        out.push(SearchResult {
            title,
            url,
            snippet,
        });
    }
    out.truncate(max_results);
    Ok(out)
}

/// Accepts only absolute http/https URLs with a host.
fn absolute_http_url(raw: Option<&str>) -> Option<String> {
    let raw = raw?.trim();
    let parsed = url::Url::parse(raw).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    if parsed.host_str().is_none() || raw != parsed.as_str() {
        return None;
    }
    Some(parsed.as_str().to_owned())
}

/// Collapses whitespace runs and caps the text at [`SNIPPET_MAX_CHARS`] chars.
fn bounded_snippet(text: &str) -> String {
    let normalized: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized.chars().take(SNIPPET_MAX_CHARS).collect()
}

/// Renders results as `title\nurl\nsnippet\n---` blocks separated by blank
/// lines. Empty titles/titles equal to the URL are not duplicated: the title
/// line is omitted when it matches the URL, and empty snippets are omitted.
pub(crate) fn format_results(results: &[SearchResult]) -> String {
    let mut blocks = Vec::with_capacity(results.len());
    for r in results {
        let mut block = String::new();
        if !r.title.is_empty() && r.title != r.url {
            block.push_str(&r.title);
            block.push('\n');
        }
        block.push_str(&r.url);
        if !r.snippet.is_empty() {
            block.push('\n');
            block.push_str(&r.snippet);
        }
        blocks.push(block);
    }
    blocks.join("\n---\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    const DUMMY_KEY: &str = "exa-test-key-dummy-not-secret-000";

    fn tool() -> AgentTool {
        web_search_tool()
    }

    struct RecordingExecutor {
        requests: std::sync::Mutex<Vec<pi_ai::HttpRequestParts>>,
        status: http::StatusCode,
        body: Vec<u8>,
    }

    impl RecordingExecutor {
        fn new(status: http::StatusCode, body: Value) -> std::sync::Arc<Self> {
            Arc::new(Self {
                requests: std::sync::Mutex::new(Vec::new()),
                status,
                body: serde_json::to_vec(&body).expect("response JSON"),
            })
        }

        fn raw(status: http::StatusCode, body: &'static [u8]) -> std::sync::Arc<Self> {
            Arc::new(Self {
                requests: std::sync::Mutex::new(Vec::new()),
                status,
                body: body.to_vec(),
            })
        }

        fn request_count(&self) -> usize {
            self.requests.lock().expect("web_search request mutex").len()
        }

        fn take_request(&self) -> pi_ai::HttpRequestParts {
            self.requests
                .lock()
                .expect("web_search request mutex")
                .pop()
                .expect("web_search request was captured")
        }
    }

    impl pi_ai::HttpExecutor for RecordingExecutor {
        fn execute<'a>(
            &'a self,
            request: pi_ai::HttpRequest,
        ) -> futures_util::future::BoxFuture<'a, anyhow::Result<pi_ai::HttpResponse>> {
            use futures_util::FutureExt as _;

            async move {
                self.requests
                    .lock()
                    .expect("web_search request mutex")
                    .push(request.into_parts());
                Ok(pi_ai::HttpResponse::new(
                    self.status,
                    http::HeaderMap::new(),
                    Box::pin(futures_util::stream::iter([Ok(self.body.clone())])),
                ))
            }
            .boxed()
        }
    }

    fn failing_executor(message: &'static str) -> std::sync::Arc<CountingFailingExecutor> {
        Arc::new(CountingFailingExecutor {
            calls: std::sync::atomic::AtomicUsize::new(0),
            message,
        })
    }

    struct CountingFailingExecutor {
        calls: std::sync::atomic::AtomicUsize,
        message: &'static str,
    }

    impl pi_ai::HttpExecutor for CountingFailingExecutor {
        fn execute<'a>(
            &'a self,
            _request: pi_ai::HttpRequest,
        ) -> futures_util::future::BoxFuture<'a, anyhow::Result<pi_ai::HttpResponse>> {
            use futures_util::FutureExt as _;

            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move { Err(anyhow!(self.message)) }.boxed()
        }
    }

    fn run(
        executor: pi_ai::SharedHttpExecutor,
        args: Value,
    ) -> impl Future<Output = Result<AgentToolResult>> {
        run_web_search_with_executor(args, AbortSignal::none(), false, Some(DUMMY_KEY), executor)
    }

    #[test]
    fn schema_requires_query_and_makes_max_results_optional() {
        let t = tool();
        assert!(t.parameters.required.contains(&"query".to_string()));
        assert!(!t.parameters.required.contains(&"max_results".to_string()));
    }

    #[test]
    fn offline_disabled_from_parses_rpi_truthy_values() {
        for v in ["1", "true", "TRUE", "Yes", " yes "] {
            assert!(offline_disabled_from(Some(v)), "expected {v:?} to be on");
        }
        for v in [
            None,
            Some(""),
            Some("0"),
            Some("false"),
            Some("no"),
            Some("yep"),
            Some("2"),
        ] {
            assert!(!offline_disabled_from(v), "expected {v:?} to be off");
        }
    }

    #[tokio::test]
    async fn offline_flag_short_circuits_before_network() {
        let executor = RecordingExecutor::new(http::StatusCode::OK, json!({ "results": [] }));
        let err = run_web_search_with_executor(
            json!({ "query": "test" }),
            AbortSignal::none(),
            true,
            Some(DUMMY_KEY),
            executor.clone(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(err, OFFLINE_MESSAGE);
        assert_eq!(executor.request_count(), 0);
    }

    #[tokio::test]
    async fn empty_query_is_rejected_without_network() {
        let executor = RecordingExecutor::new(http::StatusCode::OK, json!({ "results": [] }));
        let err = run(
            executor.clone(),
            json!({ "query": "   " }),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("query must not be empty"), "{err}");
        assert_eq!(executor.request_count(), 0);
    }

    #[tokio::test]
    async fn absent_api_key_fails_explicitly_without_network() {
        let executor = RecordingExecutor::new(
            http::StatusCode::OK,
            json!({ "results": [{ "url": "https://example.com/" }] }),
        );
        let err = run_web_search_with_executor(
            json!({ "query": "test" }),
            AbortSignal::none(),
            false,
            None,
            executor.clone(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("EXA_API_KEY"), "{err}");
        assert!(err.contains("only search backend"), "{err}");
        assert_eq!(executor.request_count(), 0);
    }

    #[tokio::test]
    async fn empty_api_key_fails_explicitly_without_network() {
        let executor = RecordingExecutor::new(http::StatusCode::OK, json!({ "results": [] }));
        let err = run_web_search_with_executor(
            json!({ "query": "test" }),
            AbortSignal::none(),
            false,
            Some("   "),
            executor.clone(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("EXA_API_KEY"), "{err}");
        assert_eq!(executor.request_count(), 0);
    }

    #[tokio::test]
    async fn injected_executor_receives_exact_single_exa_request() {
        let executor = RecordingExecutor::new(
            http::StatusCode::OK,
            json!({ "results": [{ "url": "https://example.com/", "title": "Ex" }] }),
        );
        run(
            executor.clone(),
            json!({ "query": "rust async await", "max_results": 3 }),
        )
        .await
        .expect("injected executor succeeds");

        assert_eq!(executor.request_count(), 1);
        let captured = executor.take_request();
        assert_eq!(captured.method, pi_ai::HttpMethod::Post);
        assert_eq!(
            captured.authority.scheme(),
            pi_ai::ResolvedHttpScheme::Https
        );
        assert_eq!(
            captured.authority.host(),
            &pi_ai::ResolvedHttpHost::Dns("api.exa.ai".to_owned())
        );
        assert_eq!(captured.authority.port(), 443);
        assert_eq!(captured.path_and_query, "/search");
        assert_eq!(
            serde_json::from_slice::<Value>(&captured.body).expect("request JSON"),
            json!({
                "query": "rust async await",
                "numResults": 3,
                "type": "auto",
                "contents": { "summary": { "query": "rust async await" } }
            })
        );
        let headers = captured.headers.into_iter().collect::<http::HeaderMap>();
        assert_eq!(
            headers
                .get(http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
        assert_eq!(
            headers
                .get(http::header::ACCEPT)
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
        assert_eq!(
            headers
                .get("x-api-key")
                .and_then(|value| value.to_str().ok()),
            Some(DUMMY_KEY)
        );
        assert_eq!(captured.timeout_ms, REQUEST_TIMEOUT.as_millis() as u64);
        assert_eq!(captured.max_response_bytes.get(), RESPONSE_MAX_BYTES);
        assert!(captured.abort_signal.is_some());
    }


    #[tokio::test]
    async fn max_results_absent_defaults_to_five() {
        let executor = RecordingExecutor::new(
            http::StatusCode::OK,
            json!({ "results": [{ "url": "https://example.com/" }] }),
        );
        run(executor.clone(), json!({ "query": "q" }))
            .await
            .expect("default call succeeds");
        let captured = executor.take_request();
        let body = serde_json::from_slice::<Value>(&captured.body).expect("request JSON");
        assert_eq!(body["numResults"], json!(DEFAULT_MAX_RESULTS));
    }

    #[tokio::test]
    async fn num_results_beyond_cap_is_clamped_in_body() {
        let executor = RecordingExecutor::new(
            http::StatusCode::OK,
            json!({ "results": [{ "url": "https://example.com/" }] }),
        );
        run(executor.clone(), json!({ "query": "q", "max_results": 500 }))
            .await
            .expect("clamped call succeeds");
        let captured = executor.take_request();
        let body = serde_json::from_slice::<Value>(&captured.body).expect("request JSON");
        assert_eq!(body["numResults"], json!(MAX_RESULTS_CAP));
    }

    #[tokio::test]
    async fn num_results_below_one_is_clamped_in_body() {
        let executor = RecordingExecutor::new(
            http::StatusCode::OK,
            json!({ "results": [{ "url": "https://example.com/" }] }),
        );
        run(executor.clone(), json!({ "query": "q", "max_results": 0 }))
            .await
            .expect("clamped call succeeds");
        let captured = executor.take_request();
        let body = serde_json::from_slice::<Value>(&captured.body).expect("request JSON");
        assert_eq!(body["numResults"], json!(1));
    }

    #[tokio::test]
    async fn max_results_null_is_rejected_without_network() {
        let executor = RecordingExecutor::new(
            http::StatusCode::OK,
            json!({ "results": [{ "url": "https://example.com/" }] }),
        );
        let err = run(
            executor.clone(),
            json!({ "query": "q", "max_results": Value::Null }),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("max_results"), "{err}");
        assert_eq!(executor.request_count(), 0);
    }

    #[test]
    fn parse_keeps_rank_and_falls_back_title_to_url() {
        let body = json!({
            "results": [
                { "title": "First", "url": "https://a.example/", "summary": "alpha summary" },
                { "url": "https://b.example/", "highlights": ["h1", "h2"] },
                { "title": "", "url": "https://c.example/" }
            ]
        });
        let results = parse_exa_results(&body, 10).expect("valid results");
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].title, "First");
        assert_eq!(results[0].url, "https://a.example/");
        assert_eq!(results[0].snippet, "alpha summary");
        assert_eq!(results[1].title, "https://b.example/");
        assert_eq!(results[1].snippet, "h1 h2");
        assert_eq!(results[2].title, "https://c.example/");
        assert_eq!(results[2].snippet, "");
    }

    #[test]
    fn parse_filters_non_http_and_relative_urls() {
        let body = json!({
            "results": [
                { "url": "ftp://example.com/file" },
                { "url": "//cdn.example/relative" },
                { "url": "not a url" },
                { "url": "" },
                { "title": "kept" }
            ]
        });
        let results = parse_exa_results(&body, 10).expect("array is present");
        assert!(results.is_empty(), "non-http rows must be skipped");
    }

    #[test]
    fn parse_caps_results_and_snippets_with_normalized_whitespace() {
        let long_summary = "x".repeat(1200);
        let spaced = "a \t\n b   c";
        let body = json!({
            "results": [
                { "url": "https://a.example/", "summary": long_summary },
                { "url": "https://b.example/", "summary": spaced }
            ]
        });
        let results = parse_exa_results(&body, 1).expect("valid results");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].snippet.len(), SNIPPET_MAX_CHARS);
        let second = parse_exa_results(&body, 10).expect("valid results");
        assert_eq!(second[0].snippet.len(), SNIPPET_MAX_CHARS);
        assert_eq!(second[1].snippet.chars().count(), "a b c".chars().count());
        assert_eq!(second[1].snippet, "a b c");
    }

    #[test]
    fn missing_results_array_is_an_invalid_response() {
        let err = parse_exa_results(&json!({}), 5).unwrap_err().to_string();
        assert!(err.contains("invalid response"), "{err}");
        assert!(err.contains("missing results array"), "{err}");
        let err = parse_exa_results(&json!([]), 5).unwrap_err().to_string();
        assert!(err.contains("invalid response"), "{err}");
        let err = parse_exa_results(&json!("nope"), 5).unwrap_err().to_string();
        assert!(err.contains("invalid response"), "{err}");
    }

    #[tokio::test]
    async fn empty_results_fail_explicitly_naming_upstream() {
        let executor = RecordingExecutor::new(http::StatusCode::OK, json!({ "results": [] }));
        let err = run(executor, json!({ "query": "q" }))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no usable results"), "{err}");
        assert!(err.contains("Exa"), "{err}");
        assert!(!err.contains("No results"), "{err}");
    }

    #[tokio::test]
    async fn all_invalid_rows_fail_like_empty_results() {
        let executor = RecordingExecutor::new(
            http::StatusCode::OK,
            json!({ "results": [ { "url": "ftp://x/" }, { "title": "no url" } ] }),
        );
        let err = run(executor, json!({ "query": "q" }))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no usable results"), "{err}");
    }

    #[tokio::test]
    async fn malformed_json_is_an_invalid_response_not_no_results() {
        for body in [b"{not-json".as_slice(), b"<html>gateway</html>".as_slice()] {
            let executor = RecordingExecutor::raw(http::StatusCode::OK, body);
            let err = run(executor, json!({ "query": "q" }))
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("invalid response (malformed JSON)"), "{err}");
        }
    }

    #[tokio::test]
    async fn status_categories_are_stable_without_body_echo() {
        let cases = [
            (http::StatusCode::BAD_REQUEST, "invalid by upstream"),
            (http::StatusCode::UNPROCESSABLE_ENTITY, "invalid by upstream"),
            (http::StatusCode::UNAUTHORIZED, "EXA_API_KEY"),
            (http::StatusCode::FORBIDDEN, "rejected the request"),
            (http::StatusCode::TOO_MANY_REQUESTS, "quota exceeded"),
            (http::StatusCode::BAD_GATEWAY, "upstream unavailable"),
        ];
        for (status, needle) in cases {
            let executor = RecordingExecutor::new(status, json!({ "message": "secret body" }));
            let err = run(executor, json!({ "query": "q" }))
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains(needle), "{status}: {err}");
            assert!(!err.contains("secret body"), "{err}");
        }
        assert_eq!(
            status_error_message(http::StatusCode::SERVICE_UNAVAILABLE),
            "web_search upstream unavailable (HTTP 503 Service Unavailable)"
        );
    }

    #[tokio::test]
    async fn transport_failures_redact_error_text() {
        let executor = failing_executor(
            "transport failed: api_key=exa-test-key-dummy-not-secret-000",
        );
        let err = run(executor, json!({ "query": "q" }))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("web_search request failed"), "{err}");
        assert!(
            !err.contains("exa-test-key-dummy-not-secret-000"),
            "{err}"
        );
        assert!(err.contains("[REDACTED]"), "{err}");
    }

    #[tokio::test]
    async fn exactly_one_request_and_no_result_url_fetch() {
        let executor = RecordingExecutor::new(
            http::StatusCode::OK,
            json!({ "results": [
                { "url": "https://one.example/", "title": "One" },
                { "url": "https://two.example/never-fetch", "title": "Two" }
            ]}),
        );
        let result = run(executor.clone(), json!({ "query": "q" }))
            .await
            .expect("search succeeds");
        assert_eq!(executor.request_count(), 1);
        let text = match result.content.first() {
            Some(pi_ai::ContentBlock::Text { text, .. }) => text.clone(),
            _ => String::new(),
        };
        // The result URL must be rendered, not fetched: the executor never saw
        // a second request to either host.
        assert!(text.contains("https://two.example/never-fetch"), "{text}");
    }

    #[test]
    fn format_results_renders_blocks_and_omits_duplicate_titles() {
        let results = vec![
            SearchResult {
                title: "T".into(),
                url: "https://u/".into(),
                snippet: "S".into(),
            },
            SearchResult {
                title: "https://v/".into(),
                url: "https://v/".into(),
                snippet: "no title".into(),
            },
        ];
        let out = format_results(&results);
        assert_eq!(out, "T\nhttps://u/\nS\n---\nhttps://v/\nno title");
    }

    #[test]
    fn absolute_http_url_rejects_edge_cases() {
        assert_eq!(
            absolute_http_url(Some("https://example.com/path")),
            Some("https://example.com/path".to_owned())
        );
        assert_eq!(absolute_http_url(None), None);
        assert_eq!(absolute_http_url(Some(" https://example.com ")), None);
        assert_eq!(absolute_http_url(Some("https://")), None);
    }
}