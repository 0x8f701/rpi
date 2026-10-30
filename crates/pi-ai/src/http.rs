use crate::{
    HttpMethod, HttpRequest, HttpResponse, ResolvedHttpAuthority, ResolvedHttpHost,
    ResolvedHttpScheme, StreamOptions,
};
use anyhow::{Result, anyhow, bail};
use futures_util::StreamExt;
use http::{HeaderMap, HeaderName, HeaderValue};
use std::net::IpAddr;
use std::sync::Arc;

/// Default time-to-headers attempt timeout (10 minutes, matching the
/// OpenAI/Anthropic SDK default). It caps how long a request may take to
/// reach response headers; the response body streams independently and is
/// only bounded by cancellation and the response size cap, so a long SSE
/// body is never severed by the default deadline.
pub const DEFAULT_HTTP_ATTEMPT_TIMEOUT_MS: u64 = 600_000;

pub fn resolve_http_endpoint(url: &str) -> Result<(ResolvedHttpAuthority, String)> {
    let parsed = reqwest::Url::parse(url).map_err(|_| anyhow!("invalid HTTP endpoint"))?;
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
        bail!("invalid HTTP endpoint");
    }
    let scheme = match parsed.scheme() {
        "http" => ResolvedHttpScheme::Http,
        "https" => ResolvedHttpScheme::Https,
        _ => bail!("unsupported HTTP endpoint scheme"),
    };
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("HTTP endpoint is missing a host"))?;
    let host = match host.parse::<IpAddr>() {
        Ok(address) => ResolvedHttpHost::Ip(address),
        Err(_) => ResolvedHttpHost::Dns(host.to_ascii_lowercase()),
    };
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| anyhow!("HTTP endpoint has no effective port"))?;
    let authority = ResolvedHttpAuthority::new(scheme, host, port)?;
    let mut target = parsed.path().to_owned();
    if target.is_empty() {
        target.push('/');
    }
    if let Some(query) = parsed.query() {
        target.push('?');
        target.push_str(query);
    }
    Ok((authority, target))
}

pub fn request_from_url(
    method: HttpMethod,
    url: &str,
    headers: &HeaderMap,
    body: Vec<u8>,
    options: &StreamOptions,
    max_response_bytes: u64,
) -> Result<HttpRequest> {
    let (authority, path_and_query) = resolve_http_endpoint(url)?;
    HttpRequest::new(
        method,
        authority,
        path_and_query,
        header_pairs(headers),
        body,
        options
            .timeout_ms
            .filter(|timeout| *timeout > 0)
            .unwrap_or(DEFAULT_HTTP_ATTEMPT_TIMEOUT_MS),
        max_response_bytes,
        options.abort_signal.clone(),
    )
}

pub fn header_pairs(headers: &HeaderMap) -> Vec<(HeaderName, HeaderValue)> {
    headers
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

pub async fn collect_response_body(response: HttpResponse) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut stream = response.into_body();
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk?);
    }
    Ok(body)
}

#[cfg(test)]
pub(crate) struct TestHttpRequest {
    headers: HeaderMap,
}

#[cfg(test)]
impl TestHttpRequest {
    pub(crate) fn headers(&self) -> &HeaderMap {
        &self.headers
    }
}

#[cfg(test)]
pub(crate) fn test_http_request(
    method: HttpMethod,
    url: &str,
    headers: &HeaderMap,
) -> TestHttpRequest {
    let request = request_from_url(
        method,
        url,
        headers,
        Vec::new(),
        &StreamOptions::default(),
        crate::MAX_HTTP_STREAM_RESPONSE_BYTES,
    )
    .expect("transport-neutral test request");
    TestHttpRequest {
        headers: request.into_parts().headers.into_iter().collect(),
    }
}

/// The production net transport shared by every provider HTTP stream.
///
/// This executor is what `providers::common::client` falls back to when a
/// caller does not inject `StreamOptions::http_executor`, so pi-ai's own
/// provider streams work without any external transport (in particular, they
/// never depend on a sandbox). It never follows redirects and never trusts
/// ambient proxy environment variables, so an authorization header cannot be
/// silently replayed to a redirected or proxied authority. Every per-request
/// deadline, response-size bound, and cancellation comes from the
/// already-validated `HttpRequest`; nothing here re-derives or widens those
/// limits.
pub struct DefaultHttpExecutor {
    client: reqwest::Client,
}

impl DefaultHttpExecutor {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| anyhow!("failed to build the default HTTP client: {error}"))?;
        Ok(Self { client })
    }
}

/// The process-wide default executor, built at most once and shared across
/// every stream that does not inject its own transport.
pub fn default_http_executor() -> crate::SharedHttpExecutor {
    use std::sync::LazyLock;

    static DEFAULT: LazyLock<Result<crate::SharedHttpExecutor>> = LazyLock::new(|| {
        DefaultHttpExecutor::new()
            .map(Arc::new)
            .map(|executor| executor as crate::SharedHttpExecutor)
    });
    DEFAULT
        .as_ref()
        .expect("the default HTTP executor must be constructible")
        .clone()
}

#[cfg(test)]
pub(crate) fn test_http_executor() -> crate::SharedHttpExecutor {
    Arc::new(DefaultHttpExecutor::new().expect("test HTTP client"))
}

impl crate::HttpExecutor for DefaultHttpExecutor {
    fn execute<'a>(
        &'a self,
        request: HttpRequest,
    ) -> futures_util::future::BoxFuture<'a, Result<HttpResponse>> {
        use futures_util::FutureExt as _;

        async move {
            let parts = request.into_parts();
            let host = match parts.authority.host() {
                ResolvedHttpHost::Dns(host) => host.clone(),
                ResolvedHttpHost::Ip(address) if address.is_ipv6() => format!("[{address}]"),
                ResolvedHttpHost::Ip(address) => address.to_string(),
            };
            let scheme = match parts.authority.scheme() {
                ResolvedHttpScheme::Http => "http",
                ResolvedHttpScheme::Https => "https",
            };
            let url = format!(
                "{scheme}://{host}:{}{}",
                parts.authority.port(),
                parts.path_and_query
            );
            let method = match parts.method {
                HttpMethod::Get => reqwest::Method::GET,
                HttpMethod::Post => reqwest::Method::POST,
                HttpMethod::Put => reqwest::Method::PUT,
                HttpMethod::Patch => reqwest::Method::PATCH,
                HttpMethod::Delete => reqwest::Method::DELETE,
            };
            let mut builder = self
                .client
                .request(method, url)
                .headers(parts.headers.into_iter().collect::<HeaderMap>());
            if !parts.body.is_empty() {
                builder = builder.body(parts.body);
            }
            let timeout = std::time::Duration::from_millis(parts.timeout_ms);
            let deadline = tokio::time::Instant::now() + timeout;
            let cancellation = parts.abort_signal.unwrap_or_default();
            let response = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(anyhow!("Request was aborted")),
                response = tokio::time::timeout_at(deadline, builder.send()) => response,
            }
            .map_err(|_| anyhow!("HTTP request timed out"))??;
            let status = response.status();
            let headers = response.headers().clone();
            let bound = parts.max_response_bytes.get();
            let body = response.bytes_stream();
            let body = futures_util::stream::try_unfold(
                (Box::pin(body), 0_u64, cancellation, deadline),
                move |(mut body, received, cancellation, mut idle_deadline)| async move {
                    // The attempt timeout caps time-to-headers (above) and acts
                    // as a per-chunk body idle bound: the deadline resets on
                    // every chunk, so an actively streaming SSE body is never
                    // severed by a total deadline while a stalled body still
                    // fails instead of hanging forever.
                    let next = tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => {
                            return Err(anyhow!("Request was aborted"));
                        }
                        _ = tokio::time::sleep_until(idle_deadline) => {
                            return Err(anyhow!("HTTP request timed out"));
                        }
                        next = body.next() => next,
                    };
                    match next {
                        None => Ok(None),
                        Some(chunk) => {
                            let chunk = chunk?.to_vec();
                            let received = received
                                .checked_add(chunk.len() as u64)
                                .ok_or_else(|| anyhow!("HTTP response exceeded {bound} bytes"))?;
                            if received > bound {
                                return Err(anyhow!("HTTP response exceeded {bound} bytes"));
                            }
                            idle_deadline = tokio::time::Instant::now() + timeout;
                            Ok(Some((chunk, (body, received, cancellation, idle_deadline))))
                        }
                    }
                },
            );
            Ok(HttpResponse::new(status, headers, Box::pin(body)))
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn default_attempt_timeout_resolves_to_600_seconds() {
        // The default is a 10-minute time-to-headers cap (the OpenAI/Anthropic
        // SDK default), not a 2-minute total deadline. `None` and an explicit
        // `0` both mean "no override" and must resolve to the same default,
        // preserving the documented disabled semantics for the timeout setting.
        for timeout in [None, Some(0)] {
            let options = StreamOptions {
                timeout_ms: timeout,
                ..StreamOptions::default()
            };
            let request = request_from_url(
                HttpMethod::Post,
                "https://api.example.com/v1/chat/completions",
                &HeaderMap::new(),
                Vec::new(),
                &options,
                crate::MAX_HTTP_STREAM_RESPONSE_BYTES,
            )
            .expect("default request");
            assert_eq!(
                request.into_parts().timeout_ms,
                DEFAULT_HTTP_ATTEMPT_TIMEOUT_MS,
                "{timeout:?} must resolve to the default attempt timeout"
            );
        }
    }

    #[test]
    fn explicit_timeout_above_two_minutes_remains_valid() {
        // Regression: explicit configured timeouts above 120s (e.g. a 5-minute
        // provider override) must stay accepted and reach the request intact.
        let options = StreamOptions {
            timeout_ms: Some(300_000),
            ..StreamOptions::default()
        };
        let request = request_from_url(
            HttpMethod::Post,
            "https://api.example.com/v1/chat/completions",
            &HeaderMap::new(),
            Vec::new(),
            &options,
            crate::MAX_HTTP_STREAM_RESPONSE_BYTES,
        )
        .expect("explicit timeout above 120s accepted");
        assert_eq!(request.into_parts().timeout_ms, 300_000);
    }

    #[test]
    fn attempt_timeout_above_the_documented_maximum_is_rejected() {
        // The 600s documented ceiling is enforced at construction so absurd
        // millisecond values cannot wrap Duration/Instant arithmetic.
        let options = StreamOptions {
            timeout_ms: Some(crate::MAX_HTTP_ATTEMPT_TIMEOUT_MS + 1),
            ..StreamOptions::default()
        };
        let error = request_from_url(
            HttpMethod::Post,
            "https://api.example.com/v1/chat/completions",
            &HeaderMap::new(),
            Vec::new(),
            &options,
            crate::MAX_HTTP_STREAM_RESPONSE_BYTES,
        )
        .expect_err("timeout above the documented maximum must fail");
        assert!(error.to_string().contains("attempt timeout"), "{error}");
    }

    #[tokio::test]
    async fn attempt_deadline_caps_headers_but_never_severs_sse_body() {
        // The attempt timeout is a time-to-headers cap plus a per-chunk body
        // idle bound: headers arriving within the deadline admit a body that
        // then streams for longer than the deadline (each chunk resets the
        // idle clock), mirroring the pre-migration ResponseHeaderTimeout
        // contract for long SSE streams.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buffer = [0_u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let body = "data: {\"id\":\"sse-1\"}\n\ndata: {\"id\":\"sse-2\"}\n\ndata: [DONE]\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("headers");
            // Each chunk arrives after the 200ms header deadline has already
            // passed but within the same 200ms per-chunk idle window.
            for chunk in body.as_bytes().chunks(16) {
                tokio::time::sleep(Duration::from_millis(120)).await;
                socket.write_all(chunk).await.expect("body chunk");
            }
        });
        let options = StreamOptions {
            // Headers must arrive quickly; the total stream far outlives this.
            timeout_ms: Some(200),
            http_executor: Some(test_http_executor()),
            ..StreamOptions::default()
        };
        let request = request_from_url(
            HttpMethod::Post,
            &format!("http://{address}/v1/chat/completions"),
            &HeaderMap::new(),
            Vec::new(),
            &options,
            crate::MAX_HTTP_STREAM_RESPONSE_BYTES,
        )
        .expect("request");
        let response = test_http_executor()
            .execute(request)
            .await
            .expect("headers within the deadline");
        let body = collect_response_body(response)
            .await
            .expect("full SSE body must stream past the attempt deadline");
        let text = String::from_utf8(body).expect("utf8 body");
        assert!(
            text.contains("sse-2"),
            "dripped body must arrive intact: {text}"
        );
        assert!(
            text.contains("[DONE]"),
            "SSE terminator must arrive intact: {text}"
        );
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn stalled_body_fails_after_the_idle_timeout() {
        // The per-chunk idle bound must not leave a stalled body hanging
        // forever: headers arrive within the deadline, then the body makes no
        // progress, so the collection fails once the idle window elapses.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buffer = [0_u8; 4096];
            let _ = socket.read(&mut buffer).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n")
                .await
                .expect("headers");
            // Never write any body bytes; hold the connection open.
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        let options = StreamOptions {
            timeout_ms: Some(150),
            http_executor: Some(test_http_executor()),
            ..StreamOptions::default()
        };
        let request = request_from_url(
            HttpMethod::Post,
            &format!("http://{address}/v1/chat/completions"),
            &HeaderMap::new(),
            Vec::new(),
            &options,
            crate::MAX_HTTP_STREAM_RESPONSE_BYTES,
        )
        .expect("request");
        let response = test_http_executor()
            .execute(request)
            .await
            .expect("headers within the deadline");
        let error = collect_response_body(response)
            .await
            .expect_err("stalled body must fail after the idle window");
        assert!(
            error.to_string().contains("timed out"),
            "expected idle timeout error, got {error}"
        );
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn default_http_executor_streams_a_live_sse_response() {
        // The production default executor must deliver a live stream end to
        // end with no injected transport, mirroring the real provider path.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buffer = [0_u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let body = "data: {\"id\":\"default-1\"}\n\ndata: [DONE]\n\n";
            let heads = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(heads.as_bytes()).await.expect("headers");
            socket.write_all(body.as_bytes()).await.expect("body");
            // Hold the connection briefly so the client drains the framed body
            // before the socket closes.
            tokio::time::sleep(Duration::from_millis(50)).await;
        });
        let options = StreamOptions::default();
        let request = request_from_url(
            HttpMethod::Post,
            &format!("http://{address}/v1/chat/completions"),
            &HeaderMap::new(),
            Vec::new(),
            &options,
            crate::MAX_HTTP_STREAM_RESPONSE_BYTES,
        )
        .expect("request");
        let response = default_http_executor()
            .execute(request)
            .await
            .expect("default executor streams headers and body");
        let body = collect_response_body(response)
            .await
            .expect("default executor delivers the body");
        let text = String::from_utf8(body).expect("utf8 body");
        assert!(
            text.contains("default-1") && text.contains("[DONE]"),
            "{text}"
        );
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn default_http_executor_does_not_follow_redirects() {
        // Auth-bearing provider requests must not be replayed to a redirected
        // authority: the default executor surfaces the 3xx itself, never
        // following it to an arbitrary Location.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buffer = [0_u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let response = "HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:1/elsewhere\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
            socket
                .write_all(response.as_bytes())
                .await
                .expect("response");
        });
        let options = StreamOptions::default();
        let request = request_from_url(
            HttpMethod::Post,
            &format!("http://{address}/v1/chat/completions"),
            &HeaderMap::new(),
            Vec::new(),
            &options,
            crate::MAX_HTTP_STREAM_RESPONSE_BYTES,
        )
        .expect("request");
        let response = default_http_executor()
            .execute(request)
            .await
            .expect("302 must be surfaced, not followed");
        assert_eq!(
            response.status(),
            http::StatusCode::FOUND,
            "redirect must be returned unchanged"
        );
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn default_http_executor_enforces_response_body_bound() {
        // The validated max_response_bytes bound must be enforced by the
        // default executor, never widened beyond the request's own limit.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buffer = [0_u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let body = "x".repeat(4096);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("response");
        });
        let options = StreamOptions::default();
        let request = request_from_url(
            HttpMethod::Post,
            &format!("http://{address}/v1/chat/completions"),
            &HeaderMap::new(),
            Vec::new(),
            &options,
            2048,
        )
        .expect("request with a 2048-byte bound");
        let response = default_http_executor()
            .execute(request)
            .await
            .expect("headers within the bound");
        let error = collect_response_body(response)
            .await
            .expect_err("oversized body must fail");
        assert!(
            error.to_string().contains("exceeded"),
            "expected a bound violation, got {error}"
        );
        server.await.expect("server task");
    }
}
