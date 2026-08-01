use crate::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Model, ProviderHeaders,
    ProviderResponse, StopReason, StreamOptions,
};
use anyhow::{Result, anyhow};
use chrono::{DateTime, NaiveDateTime, Utc};
use futures_util::StreamExt;
use reqwest::{
    Client, Response, StatusCode,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashMap, time::Duration};

// Retry/timeout defaults, mirroring pi's provider retry helpers
// (ai/providers/retry.go). The server-delay cap defaults to 60s; setting
// max_retry_delay_ms to 0 disables it. The header timeout defaults to 10min,
// matching the OpenAI/Anthropic SDK default; it caps time-to-headers only so a
// long SSE body is never severed.
const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;
const DEFAULT_TIMEOUT_MS: u64 = 600_000;
const RETRY_BASE_DELAY_MS: f64 = 500.0;
const RETRY_BACKOFF_CAP_MS: f64 = 8_000.0;
const MAX_PROVIDER_ERROR_BODY_CHARS: usize = 4_000;
// Largest millisecond delay representable as a Duration (mirrors Go's
// maxServerDelayMs = math.MaxInt64 / time.Millisecond); keeps an absurd
// server-requested delay from wrapping int64 nanoseconds into a negative
// Duration.
const MAX_SERVER_DELAY_MS: f64 = (i64::MAX as f64) / 1_000_000.0;

pub fn client(_options: &StreamOptions) -> Result<Client> {
    // No client-level total timeout: a total cap would sever a long SSE body.
    // The time-to-headers cap is applied per-send in send_cancellable, mirroring
    // pi's ResponseHeaderTimeout; the body stream stays uncapped and is only
    // aborted by the cancellation token.
    Ok(Client::builder().build()?)
}

pub fn is_aborted(options: &StreamOptions) -> bool {
    options
        .abort_signal
        .as_ref()
        .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
}

pub async fn fail(
    stream: &AssistantMessageEventStream,
    mut output: AssistantMessage,
    message: impl Into<String>,
    aborted: bool,
) {
    output.stop_reason = if aborted {
        StopReason::Aborted
    } else {
        StopReason::Error
    };
    output.error_message = Some(message.into());
    stream
        .push(AssistantMessageEvent::Error {
            reason: output.stop_reason,
            error: output.clone(),
        })
        .await;
    stream.end(Some(output)).await;
}

pub fn headers_map(headers: &HeaderMap) -> HashMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().into(), value.into()))
        })
        .collect()
}

fn is_public_response_header(name: &HeaderName) -> bool {
    !matches!(
        name.as_str(),
        "authorization"
            | "proxy-authorization"
            | "cookie"
            | "set-cookie"
            | "set-cookie2"
            | "x-api-key"
            | "api-key"
            | "cf-aig-authorization"
            | "x-auth-token"
    )
}

pub fn public_headers_map(headers: &HeaderMap) -> HashMap<String, String> {
    headers
        .iter()
        .filter(|(name, _)| is_public_response_header(name))
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().into(), value.into()))
        })
        .collect()
}

async fn await_provider_hook<T>(
    options: &StreamOptions,
    future: crate::ProviderHookFuture<T>,
) -> Result<T> {
    if is_aborted(options) {
        return Err(anyhow!("Request was aborted"));
    }
    match &options.abort_signal {
        Some(token) => tokio::select! {
            biased;
            _ = token.clone().cancelled_owned() => Err(anyhow!("Request was aborted")),
            result = future => result,
        },
        None => future.await,
    }
}

pub async fn notify_response_headers(
    options: &StreamOptions,
    status: u16,
    headers: &HeaderMap,
    model: &Model,
) -> Result<()> {
    if let Some(hook) = &options.on_response {
        hook(
            ProviderResponse {
                status,
                headers: headers_map(headers),
            },
            model,
        )?;
    }
    if let Some(hook) = &options.after_provider_response {
        await_provider_hook(
            options,
            hook(
                ProviderResponse {
                    status,
                    headers: public_headers_map(headers),
                },
                model.clone(),
            ),
        )
        .await?;
    }
    Ok(())
}

pub async fn notify_response(
    options: &StreamOptions,
    response: &Response,
    model: &Model,
) -> Result<()> {
    notify_response_headers(
        options,
        response.status().as_u16(),
        response.headers(),
        model,
    )
    .await
}

pub async fn error_body(
    label: &str,
    response: Response,
    options: &StreamOptions,
) -> Result<String> {
    let status = response.status();
    let body = match &options.abort_signal {
        Some(token) => tokio::select! {
            biased;
            _ = token.clone().cancelled_owned() => return Err(anyhow!("Request was aborted")),
            body = response.text() => body.unwrap_or_default(),
        },
        None => response.text().await.unwrap_or_default(),
    };
    Ok(format_provider_error(label, status.as_u16(), &body))
}

// format_provider_error mirrors pi's formatProviderError (error-body.ts): the
// provider's structured error.message wins, with error.code appended as
// " (code)"; otherwise the trimmed raw body is used. The surfaced message is
// capped at MAX_PROVIDER_ERROR_BODY_CHARS UTF-16 code units, matching JS
// String.length semantics, with a "... [truncated N chars]" suffix.
fn format_provider_error(label: &str, status: u16, body: &str) -> String {
    let mut msg = body.trim().to_owned();
    if let Ok(parsed) = serde_json::from_str::<ProviderErrorShape>(body) {
        if !parsed.error.message.is_empty() {
            msg = if parsed.error.code.is_empty() {
                parsed.error.message
            } else {
                format!("{} ({})", parsed.error.message, parsed.error.code)
            };
        }
    }
    let msg = truncate_error_text(&msg, MAX_PROVIDER_ERROR_BODY_CHARS);
    format!("{label} API error {status}: {msg}")
}

#[derive(Deserialize)]
struct ProviderErrorShape {
    #[serde(default)]
    error: ProviderErrorDetail,
}

#[derive(Deserialize, Default)]
struct ProviderErrorDetail {
    #[serde(default)]
    message: String,
    #[serde(default)]
    code: String,
}

// truncate_error_text ports pi's truncateErrorText: JS measures with
// String.length / String.slice (UTF-16 code units), so the cap and the
// "[truncated N chars]" count are UTF-16-unit based, not byte- or rune-based.
fn truncate_error_text(text: &str, max_chars: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= max_chars {
        return text.to_owned();
    }
    let head = String::from_utf16_lossy(&units[..max_chars]);
    format!("{head}... [truncated {} chars]", units.len() - max_chars)
}

// retryable mirrors pi's shouldRetryResponse: 2xx is never retried (the
// x-should-retry override only applies to non-2xx); otherwise the header wins,
// then 408, 409, 429, and any >=500.
fn retryable(status: StatusCode, headers: &HeaderMap) -> bool {
    if status.is_success() {
        return false;
    }
    match headers
        .get("x-should-retry")
        .and_then(|value| value.to_str().ok())
    {
        Some("true") => return true,
        Some("false") => return false,
        _ => {}
    }
    matches!(status.as_u16(), 408 | 409 | 429) || status.is_server_error()
}

// parse_float_prefix mirrors JS Number.parseFloat, which pi uses to read the
// Retry-After headers: leading whitespace is skipped, the longest valid numeric
// prefix is consumed, and trailing junk is ignored ("3600s" -> 3600). A prefix
// that overflows f64 yields +/-Inf, matching JS ("1e400" is Infinity, not NaN);
// the caller clamps before building a Duration. The "Infinity" literal is
// accepted case-sensitively as a prefix. None stands for JS NaN.
fn parse_float_prefix(input: &str) -> Option<f64> {
    let start = input
        .char_indices()
        .take_while(|(_, c)| c.is_whitespace() || *c == '\u{feff}')
        .last()
        .map_or(0, |(i, c)| i + c.len_utf8());
    let s = &input[start..];
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut sign = 1.0_f64;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        if bytes[i] == b'-' {
            sign = -1.0;
        }
        i += 1;
    }
    if s[i..].starts_with("Infinity") {
        return Some(sign * f64::INFINITY);
    }
    let mut digits = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    let mut end = i;
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        let mut j = i + 1;
        if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        let mut k = j;
        while k < bytes.len() && bytes[k].is_ascii_digit() {
            k += 1;
        }
        if k > j {
            end = k;
        }
    }
    let prefix = &s[..end];
    match prefix.parse::<f64>() {
        Ok(f) => Some(f),
        Err(_) => {
            // The grammar matched, so a parse failure is overflow (Rust reports
            // it as an error rather than +/-Inf). Normalize lone dots for
            // parsers that reject them, then fall back to +/-Inf like JS.
            let normalized = normalize_float_prefix(prefix);
            match normalized.parse::<f64>() {
                Ok(f) => Some(f),
                Err(_) => Some(sign * f64::INFINITY),
            }
        }
    }
}

fn normalize_float_prefix(prefix: &str) -> String {
    let mut out = String::with_capacity(prefix.len() + 1);
    let bytes = prefix.as_bytes();
    if bytes.first() == Some(&b'.') {
        out.push('0');
    }
    for (k, &b) in bytes.iter().enumerate() {
        out.push(b as char);
        if b == b'.' {
            let next = bytes.get(k + 1);
            let needs_zero = next.is_none() || matches!(next, Some(b'e') | Some(b'E'));
            if needs_zero {
                out.push('0');
            }
        }
    }
    out
}

// server_retry_delay_ms extracts a server-requested retry delay in
// milliseconds, mirroring pi's getRetryDelayMs header handling. retry-after-ms
// wins when it parses; otherwise Retry-After is read as seconds, falling back to
// an HTTP date. A present-but-unparseable Retry-After still counts as
// server-dictated (Some(0.0) = immediate retry), matching pi's NaN ->
// Math.max(0, ms) clamp, rather than falling back to backoff. None means no
// header dictated the delay.
fn server_retry_delay_ms(headers: &HeaderMap) -> Option<f64> {
    if let Some(v) = headers.get("retry-after-ms").and_then(|v| v.to_str().ok()) {
        if let Some(ms) = parse_float_prefix(v) {
            return Some(ms);
        }
    }
    if let Some(ra) = headers.get("retry-after").and_then(|v| v.to_str().ok()) {
        if let Some(secs) = parse_float_prefix(ra) {
            return Some(secs * 1000.0);
        }
        if let Some(t) = parse_http_date(ra) {
            return Some((t - Utc::now()).num_milliseconds() as f64);
        }
        return Some(0.0);
    }
    None
}

// parse_http_date accepts the three HTTP-date forms Go's http.ParseTime
// handles: the RFC 7231 IMF-fixdate (RFC 2822, what servers actually send),
// RFC 850, and ANSI C asctime. All are interpreted as UTC, matching HTTP.
fn parse_http_date(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc2822(s) {
        return Some(dt.with_timezone(&Utc));
    }
    // RFC 850 legacy: "Sunday, 06-Nov-94 08:49:37 GMT".
    if let Ok(nd) = NaiveDateTime::parse_from_str(s, "%A, %d-%b-%y %H:%M:%S GMT") {
        return Some(nd.and_utc());
    }
    // ANSI C asctime: "Sun Nov  6 08:49:37 1994" (day is space-padded).
    let normalized = s.replace("  ", " ");
    NaiveDateTime::parse_from_str(&normalized, "%a %b %d %H:%M:%S %Y")
        .ok()
        .map(|nd| nd.and_utc())
}

// ceil_seconds renders the numeric part of pi's `${Math.ceil(ms / 1000)}s`,
// including JS's "Infinity" spelling for a header value that overflowed f64.
fn ceil_seconds(ms: f64) -> String {
    let secs = (ms / 1000.0).ceil();
    if secs.is_infinite() && secs.is_sign_positive() {
        return "Infinity".to_owned();
    }
    if secs.fract() == 0.0 {
        format!("{}", secs as i64)
    } else {
        format!("{secs}")
    }
}

// server_delay_duration converts a validated server-requested delay. Negative
// values (a Retry-After date in the past) retry immediately, matching pi's
// `Math.max(0, ms)`, and the upper clamp keeps an absurd delay from wrapping
// int64 nanoseconds into a negative Duration. JS setTimeout truncates its delay
// to an integer millisecond count.
fn server_delay_duration(ms: f64) -> Duration {
    let ms = match ms {
        m if m < 0.0 => 0.0,
        m if m > MAX_SERVER_DELAY_MS => MAX_SERVER_DELAY_MS,
        m => m,
    };
    Duration::from_millis(ms as u64)
}

// backoff_delay is pi's computed fallback: min(0.5s * 2^attempt, 8s) with up to
// 25% downward jitter. max_retry_delay_ms bounds only a server-requested delay,
// never this. The jitter samples wall-clock entropy plus a per-thread counter
// (no `rand` dependency, which is not in the workspace).
fn backoff_delay(attempt: usize) -> Duration {
    let backoff = (RETRY_BASE_DELAY_MS * 2.0_f64.powi(attempt as i32)).min(RETRY_BACKOFF_CAP_MS);
    let jitter = 1.0 - 0.25 * jitter_unit();
    Duration::from_millis((backoff * jitter) as u64)
}

fn jitter_unit() -> f64 {
    use std::cell::Cell;
    use std::time::{SystemTime, UNIX_EPOCH};
    thread_local! { static CALLS: Cell<u64> = const { Cell::new(0x9E3779B97F4A7C15) }; }
    let counter = CALLS.with(|c| {
        let v = c.get();
        c.set(v.wrapping_add(0x9E3779B97F4A7C15));
        v
    });
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut z = counter ^ nanos.wrapping_mul(0xD1B54A32D192ED03);
    z = z.wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^= z >> 31;
    (z >> 40) as f64 / (1u64 << 24) as f64
}

// retry_delay computes the wait before the next attempt, mirroring pi's
// getRetryDelayMs: a server-dictated delay wins once it passes the
// max_retry_delay_ms cap (defaulting to DEFAULT_MAX_RETRY_DELAY_MS; 0 disables),
// otherwise the jittered backoff applies. A server delay above the cap fails
// the request immediately instead of being clamped, so the agent-level retry
// policy can surface it.
fn retry_delay(headers: &HeaderMap, attempt: usize, options: &StreamOptions) -> Result<Duration> {
    let limit = match options.max_retry_delay_ms {
        Some(0) => 0,
        Some(m) => m,
        None => DEFAULT_MAX_RETRY_DELAY_MS,
    };
    match server_retry_delay_ms(headers) {
        Some(ms) => {
            if limit > 0 && ms > limit as f64 {
                return Err(anyhow!(
                    "Server requested {}s retry delay (max: {}s)",
                    ceil_seconds(ms),
                    ceil_seconds(limit as f64)
                ));
            }
            Ok(server_delay_duration(ms))
        }
        None => Ok(backoff_delay(attempt)),
    }
}

async fn send_cancellable(
    options: &StreamOptions,
    request: reqwest::RequestBuilder,
) -> Result<Response> {
    // Cap time-to-headers only (mirrors pi's ResponseHeaderTimeout): the send()
    // future resolves once response headers arrive, so a timeout here never
    // touches the streaming body, which is read later in consume_sse without a
    // deadline. The body is aborted only by the cancellation token.
    let timeout = match options.timeout_ms {
        Some(0) | None => Duration::from_millis(DEFAULT_TIMEOUT_MS),
        Some(ms) => Duration::from_millis(ms),
    };
    match &options.abort_signal {
        Some(token) => tokio::select! {
            biased;
            _ = token.clone().cancelled_owned() => Err(anyhow!("Request was aborted")),
            response = request.send() => Ok(response?),
            _ = tokio::time::sleep(timeout) => Err(anyhow!("request timed out after {}ms", timeout.as_millis())),
        },
        None => match tokio::time::timeout(timeout, request.send()).await {
            Ok(response) => Ok(response?),
            Err(_) => Err(anyhow!("request timed out after {}ms", timeout.as_millis())),
        },
    }
}

async fn abortable_sleep(options: &StreamOptions, delay: Duration) -> Result<()> {
    match &options.abort_signal {
        Some(token) => tokio::select! {
            biased;
            _ = token.clone().cancelled_owned() => Err(anyhow!("Request was aborted")),
            _ = tokio::time::sleep(delay) => Ok(()),
        },
        None => {
            tokio::time::sleep(delay).await;
            Ok(())
        }
    }
}

pub async fn send_with_retry<F>(options: &StreamOptions, mut build: F) -> Result<Response>
where
    F: FnMut() -> reqwest::RequestBuilder,
{
    for attempt in 0..=options.max_retries {
        if is_aborted(options) {
            return Err(anyhow!("Request was aborted"));
        }
        match send_cancellable(options, build()).await {
            Ok(response)
                if retryable(response.status(), response.headers())
                    && attempt < options.max_retries =>
            {
                let delay = retry_delay(response.headers(), attempt, options)?;
                drop(response);
                abortable_sleep(options, delay).await?;
            }
            Ok(response) => return Ok(response),
            Err(_) if is_aborted(options) => return Err(anyhow!("Request was aborted")),
            Err(error) if attempt == options.max_retries => return Err(error),
            Err(_) => abortable_sleep(options, backoff_delay(attempt)).await?,
        }
    }
    Err(anyhow!("request retry loop ended unexpectedly"))
}

pub async fn consume_sse<F>(
    response: Response,
    options: &StreamOptions,
    mut handle: F,
) -> Result<()>
where
    F: FnMut(Option<&str>, &str) -> Result<()>,
{
    let mut bytes = response.bytes_stream();
    let mut pending = String::new();
    loop {
        let chunk = match &options.abort_signal {
            Some(token) => {
                tokio::select! {biased;_=token.clone().cancelled_owned()=>return Err(anyhow!("Request was aborted")),chunk=bytes.next()=>chunk,}
            }
            None => bytes.next().await,
        };
        let Some(chunk) = chunk else { break };
        if is_aborted(options) {
            return Err(anyhow!("Request was aborted"));
        }
        pending.push_str(&String::from_utf8_lossy(&chunk?));
        drain_sse_events(&mut pending, &mut handle)?;
    }
    if is_aborted(options) {
        return Err(anyhow!("Request was aborted"));
    }
    let final_event = pending.trim_matches(['\r', '\n']);
    if !final_event.is_empty() {
        dispatch_sse_event(final_event, &mut handle)?;
    }
    if is_aborted(options) {
        return Err(anyhow!("Request was aborted"));
    }
    Ok(())
}
fn drain_sse_events<F>(pending: &mut String, handle: &mut F) -> Result<()>
where
    F: FnMut(Option<&str>, &str) -> Result<()>,
{
    loop {
        let separator = pending
            .find("\r\n\r\n")
            .map(|position| (position, 4))
            .or_else(|| pending.find("\n\n").map(|position| (position, 2)));
        let Some((position, len)) = separator else {
            return Ok(());
        };
        let event = pending[..position].to_owned();
        pending.drain(..position + len);
        dispatch_sse_event(&event, handle)?;
    }
}
fn dispatch_sse_event<F>(event: &str, handle: &mut F) -> Result<()>
where
    F: FnMut(Option<&str>, &str) -> Result<()>,
{
    let mut name = None;
    let mut data = Vec::new();
    for line in event.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(value) = line.strip_prefix("event:") {
            name = Some(value.trim())
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.trim_start())
        }
    }
    if !data.is_empty() {
        handle(name, &data.join("\n"))?;
    }
    Ok(())
}

fn is_sensitive_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "authorization"
            | "proxy-authorization"
            | "x-api-key"
            | "api-key"
            | "cf-aig-authorization"
            | "x-auth-token"
            | "cookie"
            | "set-cookie"
            | "set-cookie2"
    )
}

pub fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<()> {
    let name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|error| anyhow!("Invalid HTTP header name {name:?}: {error}"))?;
    let mut value = HeaderValue::try_from(value)
        .map_err(|error| anyhow!("Invalid HTTP header value for {}: {error}", name.as_str()))?;
    if is_sensitive_header(&name) {
        value.set_sensitive(true);
    }
    headers.insert(name, value);
    Ok(())
}

pub fn insert_header_map(headers: &mut HeaderMap, source: &HashMap<String, String>) -> Result<()> {
    for (name, value) in source {
        insert_header(headers, name, value)?;
    }
    Ok(())
}

pub async fn apply_provider_headers(
    headers: HeaderMap,
    model: &Model,
    options: &StreamOptions,
) -> Result<HeaderMap> {
    let Some(hook) = &options.before_provider_headers else {
        return Ok(headers);
    };
    let input: ProviderHeaders = headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), Some(value.to_owned())))
        })
        .collect();
    let transformed = await_provider_hook(options, hook(input, model.clone())).await?;
    let mut canonical = ProviderHeaders::new();
    for (name, value) in transformed {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| anyhow!("Invalid HTTP header name {name:?}: {error}"))?;
        let key = name.as_str().to_owned();
        match canonical.entry(key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(value);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                if value.is_none() || entry.get().is_some() {
                    entry.insert(value);
                }
            }
        }
    }
    let mut output = HeaderMap::with_capacity(canonical.len());
    for (name, value) in canonical {
        if let Some(value) = value {
            insert_header(&mut output, &name, &value)?;
        }
    }
    Ok(output)
}

pub fn apply_payload(value: Value, model: &Model, options: &StreamOptions) -> Result<Value> {
    if let Some(hook) = &options.on_payload {
        hook(value, model)
    } else {
        Ok(value)
    }
}

pub async fn apply_provider_request(
    value: Value,
    model: &Model,
    options: &StreamOptions,
) -> Result<Value> {
    let value = apply_payload(value, model, options)?;
    match &options.before_provider_request {
        Some(hook) => await_provider_hook(options, hook(value, model.clone())).await,
        None => Ok(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use tokio::sync::oneshot;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn parses_crlf_and_final_unterminated_events() {
        let mut pending = "event: delta\r\ndata: one\r\n\r\ndata: two".to_owned();
        let mut received = Vec::new();
        drain_sse_events(&mut pending, &mut |event, data| {
            received.push((event.map(str::to_owned), data.to_owned()));
            Ok(())
        })
        .expect("drain CRLF event");
        dispatch_sse_event(&pending, &mut |event, data| {
            received.push((event.map(str::to_owned), data.to_owned()));
            Ok(())
        })
        .expect("dispatch final event");
        assert_eq!(
            received,
            vec![(Some("delta".into()), "one".into()), (None, "two".into())]
        );
    }

    // Accepts one HTTP/1.1 connection, reads the request headers, then either
    // writes `response` and holds the connection open (so a body never arrives)
    // or stays silent, so the client's send/SSE wait can never complete on its
    // own. `request_seen` fires once the request is in flight; `responded` fires
    // after `response` bytes (if any) are written.
    fn spawn_stalled_server(
        response: Option<&'static [u8]>,
    ) -> (String, oneshot::Receiver<()>, oneshot::Receiver<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (request_seen_tx, request_seen_rx) = oneshot::channel();
        let (responded_tx, responded_rx) = oneshot::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = Vec::new();
            let mut tmp = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let n = stream.read(&mut tmp).expect("read request headers");
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&tmp[..n]);
            }
            let _ = request_seen_tx.send(());
            if let Some(body) = response {
                stream.write_all(body).expect("write response");
                let _ = stream.flush();
                let _ = responded_tx.send(());
            }
            // Hold the connection open until the client closes it: the wait
            // must be interrupted by cancellation, never by server progress.
            loop {
                match stream.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });
        (format!("http://{addr}"), request_seen_rx, responded_rx)
    }

    // Like `spawn_stalled_server`, but after writing `response` it holds the
    // connection until `close` is signaled, then drops the stream so the client
    // observes EOF at a controlled moment. Used to drive the SSE EOF race: the
    // server has already sent a complete event, and EOF is armed together with
    // cancellation so both are ready when `consume_sse` next polls.
    fn spawn_eof_server(
        response: &'static [u8],
    ) -> (String, oneshot::Receiver<()>, oneshot::Sender<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (responded_tx, responded_rx) = oneshot::channel();
        let (close_tx, close_rx) = oneshot::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = Vec::new();
            let mut tmp = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let n = stream.read(&mut tmp).expect("read request headers");
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&tmp[..n]);
            }
            stream.write_all(response).expect("write response");
            let _ = stream.flush();
            let _ = responded_tx.send(());
            // Hold until told to close, then drop so the client sees EOF.
            let _ = close_rx.blocking_recv();
            drop(stream);
        });
        (format!("http://{addr}"), responded_rx, close_tx)
    }

    #[tokio::test]
    async fn request_send_is_interrupted_by_cancellation() {
        let (url, request_seen, _responded) = spawn_stalled_server(None);
        let token = CancellationToken::new();
        let mut options = StreamOptions::default();
        options.max_retries = 0;
        options.abort_signal = Some(token.clone());
        let http = client(&options).expect("client");
        let task = tokio::spawn(async move { send_with_retry(&options, || http.get(&url)).await });
        // The server only fires `request_seen` once it has read the request
        // headers, i.e. the client's `request.send()` future has been polled
        // and is now pending on the response that never arrives.
        request_seen.await.expect("request in flight");
        assert!(
            !task.is_finished(),
            "send must be parked on the stalled response, not completed"
        );
        token.cancel();
        let result = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("in-flight send must return promptly after cancellation");
        assert_eq!(
            result
                .expect("send task joins")
                .expect_err("cancellation must fail the send")
                .to_string(),
            "Request was aborted"
        );
    }

    #[tokio::test]
    async fn retry_backoff_sleep_is_interrupted_by_cancellation() {
        const TOO_MANY: &[u8] =
            b"HTTP/1.1 429 Too Many Requests\r\nretry-after-ms: 60000\r\ncontent-length: 0\r\n\r\n";
        let (url, _request_seen, responded) = spawn_stalled_server(Some(TOO_MANY));
        let token = CancellationToken::new();
        let mut options = StreamOptions::default();
        options.max_retries = 1;
        options.abort_signal = Some(token.clone());
        let http = client(&options).expect("client");
        let task = tokio::spawn(async move { send_with_retry(&options, || http.get(&url)).await });
        responded.await.expect("retryable response delivered");
        // Let the retry loop receive the response, drop it, and enter the 60s
        // backoff. A pre-cancelled token would have returned instantly; the
        // task staying pending proves the sleep is genuinely in progress.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !task.is_finished(),
            "retry task should be sleeping in the 60s backoff, not finished"
        );
        token.cancel();
        let result = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("60s retry backoff must return promptly after cancellation");
        assert_eq!(
            result
                .expect("retry task joins")
                .expect_err("cancellation must interrupt the backoff")
                .to_string(),
            "Request was aborted"
        );
    }

    #[tokio::test]
    async fn backoff_sleep_primitive_is_interrupted_by_cancellation() {
        let token = CancellationToken::new();
        let mut options = StreamOptions::default();
        options.abort_signal = Some(token.clone());
        let task =
            tokio::spawn(async move { abortable_sleep(&options, Duration::from_secs(3600)).await });
        // Let the task enter the 3600s sleep, then prove it is genuinely
        // pending (a pre-cancelled token would have returned instantly).
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !task.is_finished(),
            "sleep task should be pending in the 3600s sleep, not finished"
        );
        token.cancel();
        let result = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("3600s sleep must return promptly after cancellation");
        assert_eq!(
            result
                .expect("sleep task joins")
                .expect_err("cancellation must interrupt the sleep")
                .to_string(),
            "Request was aborted"
        );
    }

    #[tokio::test]
    async fn sse_body_wait_is_interrupted_by_cancellation() {
        const SSE_HEADERS: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
        let (url, _request_seen, responded) = spawn_stalled_server(Some(SSE_HEADERS));
        let token = CancellationToken::new();
        let mut options = StreamOptions::default();
        options.abort_signal = Some(token.clone());
        let http = client(&options).expect("client");
        let response_task = tokio::spawn(async move { http.get(&url).send().await });
        responded.await.expect("SSE headers delivered");
        let response = response_task
            .await
            .expect("response task joins")
            .expect("headers resolve the response");
        let task =
            tokio::spawn(async move { consume_sse(response, &options, |_, _| Ok(())).await });
        // Let consume_sse enter the body wait (server holds the stream open),
        // then prove it is genuinely pending rather than pre-cancelled.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !task.is_finished(),
            "SSE task should be waiting on the body, not finished"
        );
        token.cancel();
        let result = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("SSE body wait must return promptly after cancellation");
        assert_eq!(
            result
                .expect("SSE task joins")
                .expect_err("cancellation must interrupt the SSE wait")
                .to_string(),
            "Request was aborted"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sse_eof_race_does_not_become_success() {
        // The server sends a complete event then holds the connection. We wait
        // until the event has been dispatched (so consume_sse is parked on the
        // next bytes_stream poll), then arm EOF and cancellation together. On
        // the current_thread runtime the spawned task is not polled until the
        // final await, so both the EOF (None) and the cancellation are ready at
        // the next select poll; biased cancellation priority must make the
        // abort win — EOF cannot become success.
        const SSE: &[u8] =
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\ndata: hello\n\n";
        let (url, responded, close_tx) = spawn_eof_server(SSE);
        let token = CancellationToken::new();
        let mut options = StreamOptions::default();
        options.abort_signal = Some(token.clone());
        let http = client(&options).expect("client");
        let response_task = tokio::spawn(async move { http.get(&url).send().await });
        responded.await.expect("SSE event delivered");
        let response = response_task
            .await
            .expect("response task joins")
            .expect("headers resolve the response");
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let task = tokio::spawn(async move {
            consume_sse(response, &options, |_, _| {
                event_tx.send(()).expect("event delivered");
                Ok(())
            })
            .await
        });
        // The handler firing proves consume_sse is past the first chunk and
        // parked on the next bytes_stream poll (server still holds).
        event_rx.recv().await.expect("event dispatched");
        assert!(
            !task.is_finished(),
            "SSE task should be parked awaiting the next chunk, not finished"
        );
        // Arm EOF and cancellation without yielding in between, so both are
        // ready when the spawned task is next polled.
        let _ = close_tx.send(());
        token.cancel();
        let result = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("SSE EOF race must resolve promptly");
        assert_eq!(
            result
                .expect("SSE task joins")
                .expect_err("cancellation at EOF must not become success")
                .to_string(),
            "Request was aborted"
        );
    }

    #[tokio::test]
    async fn error_body_read_is_interrupted_by_cancellation() {
        // A non-retryable error response whose body never arrives: error_body's
        // text() read must abort promptly on cancellation, not hang on the
        // stalled body.
        const BAD: &[u8] = b"HTTP/1.1 400 Bad Request\r\ncontent-length: 100\r\n\r\n";
        let (url, _request_seen, responded) = spawn_stalled_server(Some(BAD));
        let token = CancellationToken::new();
        let mut options = StreamOptions::default();
        options.abort_signal = Some(token.clone());
        let http = client(&options).expect("client");
        let response_task = tokio::spawn(async move { http.get(&url).send().await });
        responded.await.expect("error headers delivered");
        let response = response_task
            .await
            .expect("response task joins")
            .expect("headers resolve the response");
        let task = tokio::spawn(async move { error_body("Test", response, &options).await });
        // Let the task enter the stalled body read, then prove it is genuinely
        // pending (a pre-cancelled token would have returned instantly).
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !task.is_finished(),
            "error_body should be waiting on the stalled body, not finished"
        );
        token.cancel();
        let result = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("error_body read must return promptly after cancellation");
        assert_eq!(
            result
                .expect("error_body task joins")
                .expect_err("cancellation must abort the body read")
                .to_string(),
            "Request was aborted"
        );
    }

    #[tokio::test]
    async fn error_body_without_token_keeps_message_format() {
        // No abort_signal: error_body must read the full body and produce the
        // same formatted message as before (JSON error/message extraction).
        const BAD: &[u8] =
            b"HTTP/1.1 400 Bad Request\r\ncontent-length: 28\r\n\r\n{\"error\":{\"message\":\"boom\"}}";
        let (url, _request_seen, responded) = spawn_stalled_server(Some(BAD));
        let options = StreamOptions::default();
        let http = client(&options).expect("client");
        let response_task = tokio::spawn(async move { http.get(&url).send().await });
        responded.await.expect("error headers delivered");
        let response = response_task
            .await
            .expect("response task joins")
            .expect("headers resolve the response");
        let message = error_body("Acme", response, &options)
            .await
            .expect("no-token reads the body");
        assert_eq!(message, "Acme API error 400: boom");
    }

    // ---- Retry-after header parsing (numeric prefix, seconds, HTTP date) ----

    #[test]
    fn parse_float_prefix_matches_js_parse_float() {
        // Longest numeric prefix, leading whitespace skipped, trailing junk
        // ignored, "Infinity" accepted case-sensitively; overflow -> +/-Inf.
        assert_eq!(parse_float_prefix("3600s"), Some(3600.0));
        assert_eq!(parse_float_prefix("  120  "), Some(120.0));
        assert_eq!(parse_float_prefix("0"), Some(0.0));
        assert_eq!(parse_float_prefix("60.5"), Some(60.5));
        assert_eq!(parse_float_prefix(".5"), Some(0.5));
        assert_eq!(parse_float_prefix("1e6"), Some(1_000_000.0));
        assert_eq!(parse_float_prefix("+500"), Some(500.0));
        assert_eq!(parse_float_prefix("Infinity"), Some(f64::INFINITY));
        assert_eq!(parse_float_prefix("-Infinity"), Some(f64::NEG_INFINITY));
        assert_eq!(parse_float_prefix("1e400"), Some(f64::INFINITY));
        assert_eq!(parse_float_prefix("not-a-number"), None);
        assert_eq!(parse_float_prefix("abc123"), None);
    }

    #[test]
    fn server_retry_delay_ms_prefers_retry_after_ms() {
        let mut h = HeaderMap::new();
        h.insert("retry-after-ms", HeaderValue::from_static("3000"));
        h.insert("retry-after", HeaderValue::from_static("100"));
        assert_eq!(server_retry_delay_ms(&h), Some(3000.0));
    }

    #[test]
    fn server_retry_delay_ms_parses_retry_after_seconds_with_numeric_prefix() {
        let mut h = HeaderMap::new();
        h.insert("retry-after", HeaderValue::from_static("3600s"));
        assert_eq!(server_retry_delay_ms(&h), Some(3_600_000.0));
    }

    #[test]
    fn server_retry_delay_ms_parses_http_date() {
        // A date ~10s in the future should yield ~10000ms (allow slack for the
        // tiny elapsed between sampling `future` and reading the clock).
        let future = chrono::Utc::now() + chrono::Duration::seconds(10);
        let datestr = future.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        let mut h = HeaderMap::new();
        h.insert(
            "retry-after",
            HeaderValue::from_str(datestr.as_str()).unwrap(),
        );
        let ms = server_retry_delay_ms(&h).expect("http date yields a delay");
        assert!(ms > 5_000.0 && ms < 15_000.0, "expected ~10000ms, got {ms}");
    }

    #[test]
    fn server_retry_delay_ms_present_but_unparseable_is_zero_server_delay() {
        // A present-but-unparseable Retry-After is still server-dictated: an
        // immediate retry (0ms), NOT a fallback to None/backoff (mirrors pi's
        // NaN -> Math.max(0, ms) clamp).
        let mut h = HeaderMap::new();
        h.insert("retry-after", HeaderValue::from_static("not-a-date"));
        assert_eq!(server_retry_delay_ms(&h), Some(0.0));
    }

    #[test]
    fn server_retry_delay_ms_none_when_headers_absent() {
        let h = HeaderMap::new();
        assert_eq!(server_retry_delay_ms(&h), None);
    }

    // ---- retryable: status + x-should-retry override table ----

    #[test]
    fn retryable_status_and_header_override_table() {
        // 2xx is never retried, even with an explicit x-should-retry: true; pi
        // checks the status range before honoring the override.
        let mut h = HeaderMap::new();
        h.insert("x-should-retry", HeaderValue::from_static("true"));
        assert!(!retryable(StatusCode::OK, &h));
        assert!(!retryable(StatusCode::NO_CONTENT, &h));

        // Override applies to non-2xx: true forces a retry on 400 (otherwise
        // not retried), false suppresses a retry on 429 (otherwise retried).
        let mut h = HeaderMap::new();
        h.insert("x-should-retry", HeaderValue::from_static("true"));
        assert!(retryable(StatusCode::BAD_REQUEST, &h));
        let mut h = HeaderMap::new();
        h.insert("x-should-retry", HeaderValue::from_static("false"));
        assert!(!retryable(StatusCode::TOO_MANY_REQUESTS, &h));

        // No override: only 408/409/429 and >=500 are retryable.
        let h = HeaderMap::new();
        assert!(!retryable(StatusCode::OK, &h));
        assert!(!retryable(StatusCode::BAD_REQUEST, &h));
        assert!(!retryable(StatusCode::NOT_FOUND, &h));
        assert!(retryable(StatusCode::REQUEST_TIMEOUT, &h)); // 408
        assert!(retryable(StatusCode::CONFLICT, &h)); // 409
        assert!(retryable(StatusCode::TOO_MANY_REQUESTS, &h)); // 429
        assert!(retryable(StatusCode::INTERNAL_SERVER_ERROR, &h)); // 500
        assert!(retryable(StatusCode::BAD_GATEWAY, &h)); // 502
        assert!(retryable(StatusCode::SERVICE_UNAVAILABLE, &h)); // 503
        assert!(retryable(StatusCode::GATEWAY_TIMEOUT, &h)); // 504
    }

    // ---- retry_delay: server-delay cap (fail-fast) vs jittered backoff ----

    #[test]
    fn retry_delay_server_delay_within_default_limit_is_honored() {
        let mut h = HeaderMap::new();
        h.insert("retry-after-ms", HeaderValue::from_static("5000"));
        let options = StreamOptions::default();
        assert_eq!(
            retry_delay(&h, 0, &options).unwrap(),
            Duration::from_millis(5000)
        );
    }

    #[test]
    fn retry_delay_server_delay_above_default_limit_fails_fast() {
        let mut h = HeaderMap::new();
        h.insert("retry-after-ms", HeaderValue::from_static("120000"));
        let options = StreamOptions::default(); // max_retry_delay_ms None -> 60s default
        let err = retry_delay(&h, 0, &options).unwrap_err().to_string();
        assert_eq!(err, "Server requested 120s retry delay (max: 60s)");
    }

    #[test]
    fn retry_delay_explicit_limit_enforced() {
        let mut h = HeaderMap::new();
        h.insert("retry-after-ms", HeaderValue::from_static("7000"));
        let mut options = StreamOptions::default();
        options.max_retry_delay_ms = Some(5000);
        let err = retry_delay(&h, 0, &options).unwrap_err().to_string();
        assert_eq!(err, "Server requested 7s retry delay (max: 5s)");
    }

    #[test]
    fn retry_delay_max_retry_delay_zero_disables_limit() {
        let mut h = HeaderMap::new();
        h.insert("retry-after-ms", HeaderValue::from_static("120000"));
        let mut options = StreamOptions::default();
        options.max_retry_delay_ms = Some(0);
        assert_eq!(
            retry_delay(&h, 0, &options).unwrap(),
            Duration::from_secs(120)
        );
    }

    #[test]
    fn retry_delay_falls_back_to_jittered_backoff_without_server_header() {
        let h = HeaderMap::new();
        let options = StreamOptions::default();
        let d = retry_delay(&h, 0, &options).unwrap();
        let base = RETRY_BASE_DELAY_MS.min(RETRY_BACKOFF_CAP_MS);
        let ms = d.as_millis() as f64;
        assert!(
            ms >= (base * 0.75).floor() && ms <= base,
            "backoff {ms} not in [{}, {}]",
            (base * 0.75).floor(),
            base
        );
    }

    // ---- backoff jitter bounds ----

    #[test]
    fn backoff_delay_is_jittered_and_bounded() {
        for attempt in 0..=8 {
            let base =
                (RETRY_BASE_DELAY_MS * 2.0_f64.powi(attempt as i32)).min(RETRY_BACKOFF_CAP_MS);
            let lower = (base * 0.75).floor() as i64;
            let upper = base.ceil() as i64;
            let mut seen = std::collections::HashSet::new();
            for _ in 0..200 {
                let ms = backoff_delay(attempt).as_millis() as i64;
                assert!(
                    ms >= lower && ms <= upper,
                    "attempt {attempt}: {ms} not in [{lower}, {upper}]"
                );
                seen.insert(ms);
            }
            // 25% jitter must actually vary the delay, not collapse to one value.
            assert!(
                seen.len() > 1,
                "attempt {attempt}: jitter produced no variation"
            );
        }
    }

    #[test]
    fn ceil_seconds_renders_like_js_math_ceil() {
        assert_eq!(ceil_seconds(0.0), "0");
        assert_eq!(ceil_seconds(60_000.0), "60");
        assert_eq!(ceil_seconds(60_500.0), "61");
        assert_eq!(ceil_seconds(61_000.0), "61");
        assert_eq!(ceil_seconds(f64::INFINITY), "Infinity");
    }

    // ---- provider error formatting: code + UTF-16 truncation ----

    #[test]
    fn format_provider_error_extracts_message_and_code() {
        let body = r#"{"error":{"message":"boom","code":"rate_limited"}}"#;
        assert_eq!(
            format_provider_error("Acme", 400, body),
            "Acme API error 400: boom (rate_limited)"
        );
    }

    #[test]
    fn format_provider_error_message_without_code_has_no_parens() {
        let body = r#"{"error":{"message":"boom"}}"#;
        assert_eq!(
            format_provider_error("Acme", 400, body),
            "Acme API error 400: boom"
        );
    }

    #[test]
    fn format_provider_error_falls_back_to_trimmed_body() {
        assert_eq!(
            format_provider_error("Acme", 500, "  plain text error  "),
            "Acme API error 500: plain text error"
        );
    }

    #[test]
    fn format_provider_error_truncates_utf16_units() {
        // Supplementary-plane chars are 2 UTF-16 units each. 2001 of them =
        // 4002 units, exceeding the 4000-unit cap by 2 -> head is 2000 chars
        // (4000 units) and the suffix reports 2 truncated units.
        let msg = "\u{10000}".repeat(2001);
        let body = format!(
            r#"{{"error":{{"message":{}}}}}"#,
            serde_json::to_string(&msg).unwrap()
        );
        let out = format_provider_error("Acme", 400, &body);
        let expected_head = "\u{10000}".repeat(2000);
        assert_eq!(
            out,
            format!("Acme API error 400: {expected_head}... [truncated 2 chars]")
        );
    }

    #[test]
    fn format_provider_error_truncates_ascii_units() {
        let msg = "x".repeat(4005);
        let body = format!(
            r#"{{"error":{{"message":{}}}}}"#,
            serde_json::to_string(&msg).unwrap()
        );
        let out = format_provider_error("Acme", 400, &body);
        let expected_head = "x".repeat(4000);
        assert_eq!(
            out,
            format!("Acme API error 400: {expected_head}... [truncated 5 chars]")
        );
    }

    // ---- send_with_retry: last-attempt boundary + network-error retry ----

    #[tokio::test]
    async fn send_with_retry_returns_retryable_response_on_last_attempt() {
        // max_retries = 0 -> a single attempt. A retryable 429 is returned as-is
        // rather than retried or converted to an error: at the last attempt the
        // retryable&&attempt<max_retries guard is false.
        const TOO_MANY: &[u8] =
            b"HTTP/1.1 429 Too Many Requests\r\nretry-after-ms: 1\r\ncontent-length: 0\r\n\r\n";
        let (url, _request_seen, responded) = spawn_stalled_server(Some(TOO_MANY));
        let mut options = StreamOptions::default();
        options.max_retries = 0;
        let http = client(&options).expect("client");
        let task = tokio::spawn(async move { send_with_retry(&options, || http.get(&url)).await });
        responded.await.expect("retryable response delivered");
        let response = task
            .await
            .expect("task joins")
            .expect("last attempt returns the response, not an error");
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn send_with_retry_retries_network_errors() {
        // A freshly-closed port gives an immediate connection-refused network
        // error on every attempt. With max_retries = 1 the loop must call build()
        // twice (initial attempt + one retry, separated by a jittered backoff)
        // before surfacing the error.
        let closed_port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr").port()
        };
        let url = format!("http://127.0.0.1:{closed_port}");
        let mut options = StreamOptions::default();
        options.max_retries = 1;
        let http = client(&options).expect("client");
        let mut builds = 0usize;
        let result = send_with_retry(&options, || {
            builds += 1;
            http.get(&url)
        })
        .await;
        assert!(result.is_err(), "network errors must surface as an error");
        assert_eq!(
            builds, 2,
            "build() must run once per attempt (initial + 1 retry)"
        );
    }

    // ---- header timeout fires without cancellation (time-to-headers cap) ----

    #[tokio::test]
    async fn send_times_out_on_stalled_headers_without_cancellation() {
        // No abort_signal: the time-to-headers cap must fire on its own (mirrors
        // pi's ResponseHeaderTimeout) instead of hanging forever on the stalled
        // response. The body is never reached, so a long SSE stream is not what
        // trips this.
        let (url, _request_seen, _responded) = spawn_stalled_server(None);
        let mut options = StreamOptions::default();
        options.max_retries = 0;
        options.timeout_ms = Some(200);
        let http = client(&options).expect("client");
        let err = send_with_retry(&options, || http.get(&url))
            .await
            .expect_err("header timeout must fire");
        assert!(
            err.to_string().contains("timed out"),
            "expected timeout error, got {err}"
        );
    }
}
