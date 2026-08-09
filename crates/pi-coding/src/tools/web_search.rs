//! `web_search` tool: read-only DuckDuckGo Instant Answer lookup.
//!
//! Hits `https://api.duckduckgo.com/` with `format=json&no_html=1&skip_disambig=1`
//! and renders the abstract plus `RelatedTopics` as plain-text result blocks.
//! Disabled wholesale when `PI_OFFLINE` is set to a truthy value, matching
//! `pi_cli::session_run::offline`'s truthy semantics (`1`/`true`/`yes`).

use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::Value;

use pi_agent::{AbortSignal, AgentTool, AgentToolResult, ToolCapability};

use crate::truncate::truncate_head;

use super::{arg_int, arg_str, check_aborted, s_number, s_object, s_string, text_result};

/// Hard cap on `max_results` (the IA endpoint is small; more is noise).
const MAX_RESULTS_CAP: i64 = 10;
const DEFAULT_MAX_RESULTS: i64 = 5;
/// DuckDuckGo Instant Answer timeout (covers connect + full body).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Output byte budget — keeps agent context lean.
const OUTPUT_MAX_BYTES: usize = 4 * 1024;
const USER_AGENT: &str = concat!("rpi/", env!("CARGO_PKG_VERSION"));

/// One rendered search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Builds the `web_search` tool. No workspace binding — it only reaches the
/// network.
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
        "Search the web via the DuckDuckGo Instant Answer API and return plain-text result blocks (title / url / snippet). \
Output is bounded to {}KB. Disabled while PI_OFFLINE is set.",
        OUTPUT_MAX_BYTES / 1024
    );
    AgentTool::new("web_search", description, params, move |ctx| {
        async move { run_web_search(ctx.arguments, ctx.abort).await }
    })
    .with_capability(ToolCapability::Read)
}

/// Truthy semantics for `PI_OFFLINE`, matching `pi_cli::session_run::offline`:
/// any of `1`/`true`/`yes` (case-insensitive, trimmed) is "on".
fn offline_disabled_from(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

/// Reads the live `PI_OFFLINE` env var. Kept as a tiny seam so the truthy
/// parsing is unit-testable without mutating the process environment.
fn offline_disabled() -> bool {
    offline_disabled_from(std::env::var("PI_OFFLINE").ok().as_deref())
}

pub(crate) async fn run_web_search(args: Value, abort: AbortSignal) -> Result<AgentToolResult> {
    run_web_search_with(args, abort, offline_disabled()).await
}

/// Core entry with an injectable offline flag so the disabled path is testable
/// without mutating the process environment (the workspace forbids `unsafe`,
/// which `std::env::set_var` requires in edition 2024).
pub(crate) async fn run_web_search_with(
    args: Value,
    abort: AbortSignal,
    offline: bool,
) -> Result<AgentToolResult> {
    check_aborted(&abort)?;
    if offline {
        return Err(anyhow!("web_search is disabled while PI_OFFLINE is set"));
    }
    let query = arg_str(&args, "query");
    if query.trim().is_empty() {
        return Err(anyhow!("query must not be empty"));
    }
    // Clamp: 1..=cap; missing → default. Non-positive also clamps to 1.
    let max_results = arg_int(&args, "max_results")?
        .unwrap_or(DEFAULT_MAX_RESULTS)
        .clamp(1, MAX_RESULTS_CAP) as usize;

    let url = build_url(&query);
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| anyhow!("web_search HTTP client error: {e}"))?;
    let send = async {
        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow!("web_search request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(anyhow!("web_search request failed: HTTP {}", resp.status()));
        }
        resp.json::<Value>()
            .await
            .map_err(|e| anyhow!("web_search response parse failed: {e}"))
    };
    let body = match tokio::time::timeout(REQUEST_TIMEOUT, send).await {
        Ok(inner) => inner?,
        Err(_) => {
            return Err(anyhow!(
                "web_search request timed out after {}s",
                REQUEST_TIMEOUT.as_secs()
            ))
        }
    };
    check_aborted(&abort)?;

    let results = parse_ddg_response(&body, max_results);
    let output = format_results(&results);
    let tr = truncate_head(&output, usize::MAX, OUTPUT_MAX_BYTES);
    Ok(text_result(tr.content))
}

/// Constructs the DuckDuckGo Instant Answer URL for `query`.
pub(crate) fn build_url(query: &str) -> String {
    let encoded = url_escape(query);
    format!("https://api.duckduckgo.com/?q={encoded}&format=json&no_html=1&skip_disambig=1&t=rpi")
}

/// Minimal percent-encoder for the query string (the DDG `q` parameter).
/// Encodes everything except RFC 3986 unreserved chars (`A-Za-z0-9-_.~`).
fn url_escape(input: &str) -> String {
    let needs_escape = input
        .as_bytes()
        .iter()
        .any(|b| !matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~'));
    if !needs_escape {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len() + 16);
    for &b in input.as_bytes() {
        if matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// Parses a DuckDuckGo Instant Answer JSON body into bounded result blocks.
///
/// Order: abstract first (if present), then flat `RelatedTopics` entries
/// (`Text` + `FirstURL`), recursing once into nested `Topics`. Entries whose
/// `FirstURL` starts with `//` are skipped (DDG sometimes emits protocol-relative
/// disambiguation links). Pure for unit testing — no network.
pub(crate) fn parse_ddg_response(body: &Value, max_results: usize) -> Vec<SearchResult> {
    let max_results = max_results.max(1);
    let mut out: Vec<SearchResult> = Vec::with_capacity(max_results.min(16));

    let heading = body.get("Heading").and_then(|v| v.as_str()).unwrap_or("").trim();
    let abstract_text = body.get("AbstractText").and_then(|v| v.as_str()).unwrap_or("").trim();
    let abstract_url = body.get("AbstractURL").and_then(|v| v.as_str()).unwrap_or("").trim();
    if !abstract_text.is_empty() || !abstract_url.is_empty() {
        out.push(SearchResult {
            title: if heading.is_empty() {
                abstract_url.to_string()
            } else {
                heading.to_string()
            },
            url: abstract_url.to_string(),
            snippet: abstract_text.to_string(),
        });
    }

    if let Some(topics) = body.get("RelatedTopics").and_then(|v| v.as_array()) {
        collect_topics(topics, &mut out, max_results);
    }

    out.truncate(max_results);
    out
}

fn collect_topics(topics: &[Value], out: &mut Vec<SearchResult>, max_results: usize) {
    for topic in topics {
        if out.len() >= max_results {
            return;
        }
        let text = topic.get("Text").and_then(|v| v.as_str()).unwrap_or("");
        let first_url = topic.get("FirstURL").and_then(|v| v.as_str()).unwrap_or("");
        // Nested disambiguation group: recurse once.
        if text.is_empty() && first_url.is_empty() {
            if let Some(inner) = topic.get("Topics").and_then(|v| v.as_array()) {
                collect_topics(inner, out, max_results);
            }
            continue;
        }
        if first_url.is_empty() || first_url.starts_with("//") {
            continue;
        }
        let (title, snippet) = split_text(text);
        out.push(SearchResult {
            title,
            url: first_url.to_string(),
            snippet,
        });
    }
}

/// DDG `Text` is often `"Topic Name - description"`. Split into title/snippet
/// on the first `" - "`; fall back to the whole text as the snippet.
fn split_text(text: &str) -> (String, String) {
    let text = text.trim();
    if let Some((head, tail)) = text.split_once(" - ") {
        (head.trim().to_string(), tail.trim().to_string())
    } else {
        (String::new(), text.to_string())
    }
}

/// Renders results as `title\nurl\nsnippet\n---` blocks separated by blank
/// lines. Empty titles are omitted. Empty result list → "No results".
pub(crate) fn format_results(results: &[SearchResult]) -> String {
    if results.is_empty() {
        return "No results".to_string();
    }
    let mut blocks = Vec::with_capacity(results.len());
    for r in results {
        let mut block = String::new();
        if !r.title.is_empty() {
            block.push_str(&r.title);
            block.push('\n');
        }
        if !r.url.is_empty() {
            block.push_str(&r.url);
            block.push('\n');
        }
        if !r.snippet.is_empty() {
            block.push_str(&r.snippet);
            block.push('\n');
        }
        if block.ends_with('\n') {
            block.pop();
        }
        blocks.push(block);
    }
    blocks.join("\n---\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool() -> AgentTool {
        web_search_tool()
    }

    #[test]
    fn schema_requires_query_and_makes_max_results_optional() {
        let t = tool();
        assert!(t.parameters.required.contains(&"query".to_string()));
        assert!(!t.parameters.required.contains(&"max_results".to_string()));
    }

    #[test]
    fn offline_disabled_from_parses_truthy_values() {
        for v in ["1", "true", "TRUE", "Yes", " yes "] {
            assert!(offline_disabled_from(Some(v)), "expected {v:?} to be on");
        }
        for v in [None, Some(""), Some("0"), Some("false"), Some("no"), Some("yep"), Some("2")] {
            assert!(!offline_disabled_from(v), "expected {v:?} to be off");
        }
    }

    #[tokio::test]
    async fn offline_flag_short_circuits_before_network() {
        // Inject the offline flag directly (the workspace forbids `unsafe`, so
        // `std::env::set_var` is off-limits in edition 2024). This exercises
        // the exact disabled-path the live env check routes to.
        let err = run_web_search_with(json!({ "query": "test" }), AbortSignal::none(), true)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("web_search is disabled while PI_OFFLINE is set"),
            "unexpected offline error: {err}"
        );
    }

    #[tokio::test]
    async fn empty_query_is_rejected_without_network() {
        // offline=false bypasses the live env (PI_OFFLINE may be set in the
        // test sandbox); we exercise the empty-query guard in isolation.
        let err = run_web_search_with(json!({ "query": "   " }), AbortSignal::none(), false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("query must not be empty"), "{err}");
    }

    #[test]
    fn build_url_encodes_query_and_pins_params() {
        let url = build_url("rust async await");
        assert_eq!(
            url,
            "https://api.duckduckgo.com/?q=rust%20async%20await&format=json&no_html=1&skip_disambig=1&t=rpi"
        );
        let url2 = build_url("a&b=c");
        assert!(url2.contains("a%26b%3Dc"), "{url2}");
    }

    #[test]
    fn parse_abstract_and_related_topics() {
        let body = json!({
            "Heading": "Rust (programming language)",
            "AbstractText": "Rust is a systems programming language.",
            "AbstractURL": "https://www.rust-lang.org/",
            "RelatedTopics": [
                { "Text": "Cargo - Rust's package manager", "FirstURL": "https://doc.rust-lang.org/cargo/" },
                { "Text": "Crates.io - registry", "FirstURL": "https://crates.io/" },
                // protocol-relative disambiguation link → skipped
                { "Text": "bad", "FirstURL": "//duckduckgo.com/x" },
                // nested Topics group → recursed
                { "Topics": [{ "Text": "Tokio - async runtime", "FirstURL": "https://tokio.rs/" }] }
            ]
        });
        let results = parse_ddg_response(&body, 10);
        assert_eq!(results.len(), 4);
        assert_eq!(results[0].title, "Rust (programming language)");
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
        assert_eq!(results[1].title, "Cargo");
        assert_eq!(results[1].snippet, "Rust's package manager");
        assert_eq!(results[2].title, "Crates.io");
        assert_eq!(results[3].title, "Tokio");
        assert_eq!(results[3].url, "https://tokio.rs/");
    }

    #[test]
    fn parse_falls_back_to_related_topics_when_abstract_empty() {
        let body = json!({
            "Heading": "",
            "AbstractText": "",
            "AbstractURL": "",
            "RelatedTopics": [{ "Text": "Only - one result", "FirstURL": "https://example.com/" }]
        });
        let results = parse_ddg_response(&body, 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Only");
        assert_eq!(results[0].snippet, "one result");
    }

    #[test]
    fn parse_no_results_when_empty() {
        let body = json!({ "Heading": "", "AbstractText": "", "AbstractURL": "", "RelatedTopics": [] });
        let results = parse_ddg_response(&body, 5);
        assert!(results.is_empty());
        assert_eq!(format_results(&results), "No results");
    }

    #[test]
    fn parse_respects_max_results() {
        let body = json!({
            "RelatedTopics": [
                { "Text": "a - 1", "FirstURL": "https://a/" },
                { "Text": "b - 2", "FirstURL": "https://b/" },
                { "Text": "c - 3", "FirstURL": "https://c/" }
            ]
        });
        let results = parse_ddg_response(&body, 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "a");
    }

    #[test]
    fn format_results_renders_blocks() {
        let results = vec![
            SearchResult { title: "T".into(), url: "https://u/".into(), snippet: "S".into() },
            SearchResult { title: "".into(), url: "https://v/".into(), snippet: "no title".into() },
        ];
        let out = format_results(&results);
        assert_eq!(out, "T\nhttps://u/\nS\n---\nhttps://v/\nno title");
    }
}