//! D8 host_network proof contracts (§10.4, §17 of the sandbox design):
//! the session HTTP transport's DNS bounding/deduplication, shared-lookup
//! cancellation races, debug redaction, closed operator request variants,
//! DNS pinning with revision reuse, redirect reauthorization with secret
//! stripping, typed missing-destination rejection, and typed unsupported
//! initial auth. All fixtures are loopback listeners or injected fakes —
//! no real host network and no ambient reqwest construction.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use pi_ai::{
    API_OPENAI_COMPLETIONS, HttpMethod, HttpRequest, Model, ResolvedHttpAuthority,
    ResolvedHttpHost, ResolvedHttpScheme, StreamOptions, collect_response_body,
};
use pi_coding::sandbox::http::{
    DnsResolution, DnsResolver, SYSTEM_DNS_MAX_ADDRESSES, SYSTEM_DNS_SYNTHETIC_TTL,
    SessionHttpError, SessionHttpRequest, SessionHttpRequirements, SessionHttpTransport,
    SharedDnsCompletion, SharedDnsLookup, SharedDnsResult, SystemDnsResolver,
    drain_dns_completions, resolve_session_http_policy,
};
use pi_coding::settings::{HostHttpDestinationWire, HostHttpHostWire, HostHttpPolicySettings};
use pi_sandbox::{
    CredentialPolicy, DnsName, FileSystemPolicy, HostHttpClass, HostHttpDestination, HostHttpHost,
    HostHttpPolicy, HostHttpScheme, HostPlatform, IpScope, NetworkPolicy, PolicyRevision,
    PolicySnapshot, ResourceLimits, SandboxPolicy, SharedPolicySnapshot,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn dns(name: &str) -> DnsName {
    DnsName::parse(name.to_owned()).expect("valid DNS name")
}

fn destination(
    class: HostHttpClass,
    host: HostHttpHost,
    port: u16,
    scopes: Vec<IpScope>,
) -> HostHttpDestination {
    HostHttpDestination::new(class, HostHttpScheme::Http, host, port, scopes).expect("destination")
}

fn snapshot(revision: u64, destinations: Vec<HostHttpDestination>) -> SharedPolicySnapshot {
    let policy = SandboxPolicy::workspace_write(
        FileSystemPolicy::new(Vec::new(), Vec::new(), Vec::new()).expect("filesystem policy"),
        NetworkPolicy::None,
        HostHttpPolicy::new(destinations).expect("http policy"),
        CredentialPolicy::new(Vec::new()).expect("credential policy"),
        ResourceLimits::confined_defaults(),
    )
    .expect("sandbox policy");
    Arc::new(
        PolicySnapshot::new(
            PolicyRevision::new(revision).expect("revision"),
            HostPlatform::Linux,
            policy,
        )
        .expect("policy snapshot"),
    )
}

/// DNS resolver whose answers and (revision, host) call log are injected,
/// so every transport execute is hermetic.
#[derive(Clone)]
struct MapDnsResolver {
    map: Arc<Mutex<HashMap<String, Vec<IpAddr>>>>,
    calls: Arc<Mutex<Vec<(PolicyRevision, String)>>>,
}

impl MapDnsResolver {
    fn new(entries: Vec<(String, Vec<IpAddr>)>) -> Self {
        Self {
            map: Arc::new(Mutex::new(entries.into_iter().collect())),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn calls(&self) -> Vec<(PolicyRevision, String)> {
        self.calls.lock().clone()
    }
}

impl DnsResolver for MapDnsResolver {
    fn resolve<'a>(
        &'a self,
        revision: PolicyRevision,
        host: &'a DnsName,
        _cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<DnsResolution, SessionHttpError>> {
        let host = host.as_str().to_owned();
        let addresses = self.map.lock().get(&host).cloned();
        self.calls.lock().push((revision, host.clone()));
        let result = match addresses {
            Some(addresses) => DnsResolution::new(addresses, SYSTEM_DNS_SYNTHETIC_TTL),
            None => Err(SessionHttpError::DnsLookupFailed),
        };
        Box::pin(async move { result })
    }
}

fn request(
    port: u16,
    path: &str,
    headers: Vec<(http::HeaderName, http::HeaderValue)>,
) -> HttpRequest {
    let authority = ResolvedHttpAuthority::new(
        ResolvedHttpScheme::Http,
        ResolvedHttpHost::Dns("api.test".to_owned()),
        port,
    )
    .expect("authority");
    HttpRequest::new(
        HttpMethod::Get,
        authority,
        path.to_owned(),
        headers,
        Vec::new(),
        5000,
        64 * 1024,
        None,
    )
    .expect("request")
}

fn session_request(
    port: u16,
    path: &str,
    headers: Vec<(http::HeaderName, http::HeaderValue)>,
) -> SessionHttpRequest {
    SessionHttpRequest::from_pi_ai(HostHttpClass::ProviderApi, request(port, path, headers))
        .expect("session request")
}

async fn read_request_head(socket: &mut TcpStream) -> HashMap<String, String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        let n = socket.read(&mut chunk).await.expect("read request head");
        if n == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..n]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buffer);
    let mut headers = HashMap::new();
    for line in head.lines().skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    headers
}

async fn write_response(
    socket: &mut TcpStream,
    status: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) {
    let mut response = format!(
        "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str("\r\n");
    socket
        .write_all(response.as_bytes())
        .await
        .expect("write response head");
    socket.write_all(body).await.expect("write response body");
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if condition() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("condition not satisfied within the wait budget");
}

// ---------------------------------------------------------------------------
// 1. DNS bounding and deduplication
// ---------------------------------------------------------------------------

#[tokio::test]
async fn system_dns_resolution_is_bounded_and_deduplicated() {
    // Bounding: an empty answer and an answer above the address ceiling are
    // typed rejections, never silent truncation.
    let empty = DnsResolution::new(Vec::new(), Duration::from_secs(60));
    assert!(
        matches!(empty, Err(SessionHttpError::DnsEmpty)),
        "{empty:?}"
    );
    let overflow = DnsResolution::new(
        (0..=SYSTEM_DNS_MAX_ADDRESSES)
            .map(|octet| IpAddr::from(Ipv4Addr::new(192, 0, 2, octet as u8)))
            .collect(),
        Duration::from_secs(60),
    );
    assert!(
        matches!(overflow, Err(SessionHttpError::DnsOverflow)),
        "{overflow:?}"
    );
    // Deduplication: duplicate addresses collapse and the answer is ordered.
    let a = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10));
    let b = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
    let resolution = DnsResolution::new(vec![a, b, a], Duration::from_secs(60))
        .expect("deduplicated resolution");
    assert_eq!(resolution.addresses(), &[b, a][..]);
    assert_eq!(resolution.ttl(), Duration::from_secs(60));
    // The shared lookup state itself is exercised below with deterministic
    // completion and cancellation outcomes. Do not assert public DNS behavior:
    // some test environments synthesize reserved addresses for nonexistent names.
}

// ---------------------------------------------------------------------------
// 2. Last-waiter cancellation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shared_dns_last_waiter_cancels_once_without_cache() {
    let (finished_tx, mut finished_rx) = mpsc::unbounded_channel();
    let host = dns("nonexistent.invalid");
    let revision = PolicyRevision::new(3).expect("revision");
    let lookup = SharedDnsLookup::new(revision, host, finished_tx);
    let first = lookup.subscribe().expect("first waiter");
    let second = lookup.subscribe().expect("second waiter");
    // Both waiters drop before the shared lookup ever runs: dropping the
    // LAST waiter cancels the shared lookup exactly once.
    drop(first);
    drop(second);
    assert_eq!(
        lookup.state.completion(),
        SharedDnsCompletion::Cancelled,
        "last-waiter drop must cancel the pending shared lookup"
    );
    let delivered = finished_rx.try_recv();
    assert!(
        matches!(delivered, Ok(_)),
        "cancellation is delivered to the drain channel"
    );
    assert!(
        matches!(
            finished_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ),
        "cancellation must be delivered exactly once"
    );
}

// ---------------------------------------------------------------------------
// 3. Completion/drop race releases once
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shared_dns_completion_drop_race_releases_once() {
    let (finished_tx, mut finished_rx) = mpsc::unbounded_channel();
    let lookup = SharedDnsLookup::new(
        PolicyRevision::new(4).expect("revision"),
        dns("nonexistent.invalid"),
        finished_tx,
    );
    let waiter = lookup.subscribe().expect("waiter");
    // Completion first: the single transition wins exactly once.
    let outcome: SharedDnsResult = Err(SessionHttpError::DnsLookupFailed);
    assert!(
        lookup.state.complete_once(&outcome),
        "first completion transitions the lookup"
    );
    assert!(
        !lookup.state.complete_once(&outcome),
        "completion is a single transition"
    );
    assert_eq!(lookup.state.completion(), SharedDnsCompletion::Failed);
    // The waiter's drop racing a completed lookup must not cancel or notify:
    // the waiter count is released once and no second drain delivery occurs.
    drop(waiter);
    assert_eq!(
        lookup.state.completion(),
        SharedDnsCompletion::Failed,
        "drop after completion must not cancel a completed lookup"
    );
    let delivered = finished_rx.try_recv();
    assert!(
        matches!(delivered, Ok(_)),
        "completion notifies the drain channel"
    );
    assert!(
        matches!(
            finished_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ),
        "drop must not deliver a second notification"
    );
}

// ---------------------------------------------------------------------------
// 4. Pointer-identical row removal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shared_dns_removes_only_pointer_identical_row() {
    let resolver = SystemDnsResolver::new().expect("system resolver");
    let host = dns("nonexistent.invalid");
    let revision = PolicyRevision::new(5).expect("revision");
    let key = (revision, host.clone());
    let (finished_tx, finished_rx) = mpsc::unbounded_channel();
    // Two DISTINCT lookups share the same (revision, host) key; only the
    // first is the tracked in-flight row.
    let tracked = SharedDnsLookup::new(revision, host.clone(), finished_tx.clone());
    let lookalike = SharedDnsLookup::new(revision, host, finished_tx);
    resolver
        .in_flight
        .lock()
        .insert(key.clone(), tracked.clone());
    let drain = tokio::spawn(drain_dns_completions(
        Arc::downgrade(&resolver),
        finished_rx,
    ));
    let outcome: SharedDnsResult = Ok(DnsResolution::new(
        vec![IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))],
        Duration::from_secs(60),
    )
    .expect("resolution"));
    // Completing the lookalike must NOT remove the pointer-different row:
    // give the drain loop every chance to misbehave, then assert the tracked
    // row is still in place.
    lookalike.state.complete_once(&outcome);
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    let row = resolver
        .in_flight
        .lock()
        .get(&key)
        .expect("tracked row remains after a lookalike completion")
        .clone();
    assert!(
        Arc::ptr_eq(&row.state, &tracked.state),
        "a pointer-different completion must not remove the in-flight row"
    );
    // Completing the tracked lookup removes exactly that row.
    tracked.state.complete_once(&outcome);
    wait_until(|| resolver.in_flight.lock().is_empty()).await;
    assert!(
        resolver.in_flight.lock().is_empty(),
        "the pointer-identical row is removed"
    );
    drain.abort();
}

// ---------------------------------------------------------------------------
// 5. Debug redaction
// ---------------------------------------------------------------------------

#[test]
fn session_request_debug_redacts_query_body_and_header_values() {
    let authority = ResolvedHttpAuthority::new(
        ResolvedHttpScheme::Https,
        ResolvedHttpHost::Dns("api.example.com".to_owned()),
        443,
    )
    .expect("authority");
    let headers = vec![
        (
            http::HeaderName::from_static("authorization"),
            http::HeaderValue::from_str("Bearer redaction-fixture-value").expect("header value"),
        ),
        (
            http::HeaderName::from_static("x-api-key"),
            http::HeaderValue::from_str("header-secret-value").expect("header value"),
        ),
    ];
    let request = HttpRequest::new(
        HttpMethod::Post,
        authority,
        "/v1/chat/completions?access_token=query-secret-value".to_owned(),
        headers,
        b"body-secret-payload".to_vec(),
        5000,
        1024 * 1024,
        None,
    )
    .expect("request");
    let session = SessionHttpRequest::from_pi_ai(HostHttpClass::ProviderApi, request)
        .expect("session request");
    let debug = format!("{session:?}");
    assert!(debug.contains("path_and_query: \"[REDACTED]\""), "{debug}");
    assert!(debug.contains("header_count: 2"), "{debug}");
    assert!(debug.contains("body_bytes: 19"), "{debug}");
    for secret in [
        "redaction-fixture-value",
        "header-secret-value",
        "query-secret-value",
        "body-secret-payload",
        "Bearer",
    ] {
        assert!(
            !debug.contains(secret),
            "Debug must not leak {secret:?}: {debug}"
        );
    }
}

// ---------------------------------------------------------------------------
// 6. Closed operator request variants
// ---------------------------------------------------------------------------

#[test]
fn operator_request_variants_are_closed() {
    use pi_coding::operator_http::{
        AnthropicTokenBody, CodexDevicePollBody, CodexTokenBody, GoogleCodeAssistLoadBody,
        GoogleCodeAssistOnboardBody, GoogleOperationName, GoogleProjectId, GoogleTierId,
        GoogleTokenBody, InitialAuthError, KimiDeviceCodeBody, KimiRefreshBody, KimiTokenBody,
        OpenRouterKeyExchangeBody, OperatorHttpError, OperatorHttpRequest, OperatorOperation,
        ReleaseChannel, ReleaseDownloadUrl, SecretValue, XaiDeviceCodeBody, XaiRefreshBody,
        XaiTokenBody,
    };
    let secret = |value: &str| SecretValue::new(value.to_owned()).expect("secret value");
    let requests: Vec<(OperatorHttpRequest, OperatorOperation, Vec<&str>)> = vec![
        (
            OperatorHttpRequest::AnthropicToken {
                body: AnthropicTokenBody::refresh(secret("anthropic-refresh-secret"))
                    .expect("body"),
            },
            OperatorOperation::AnthropicToken,
            vec!["anthropic-refresh-secret"],
        ),
        (OperatorHttpRequest::CodexDeviceCode, OperatorOperation::CodexDeviceCode, vec![]),
        (
            OperatorHttpRequest::CodexDevicePoll {
                body: CodexDevicePollBody::new(secret("device-auth-id"), secret("user-code"))
                    .expect("body"),
            },
            OperatorOperation::CodexDevicePoll,
            vec!["device-auth-id", "user-code"],
        ),
        (
            OperatorHttpRequest::CodexToken {
                body: CodexTokenBody::refresh(secret("codex-refresh-secret")).expect("body"),
            },
            OperatorOperation::CodexToken,
            vec!["codex-refresh-secret"],
        ),
        (
            OperatorHttpRequest::GoogleToken {
                body: GoogleTokenBody::refresh(secret("google-refresh-secret")).expect("body"),
            },
            OperatorOperation::GoogleToken,
            vec!["google-refresh-secret"],
        ),
        (
            OperatorHttpRequest::GoogleCodeAssistLoad {
                body: GoogleCodeAssistLoadBody::new(None),
                bearer: secret("google-bearer-secret"),
            },
            OperatorOperation::GoogleCodeAssistLoad,
            vec!["google-bearer-secret"],
        ),
        (
            OperatorHttpRequest::GoogleCodeAssistOnboard {
                body: GoogleCodeAssistOnboardBody::new(
                    GoogleTierId::parse("tier-1".to_owned()).expect("tier"),
                    None,
                ),
                bearer: secret("google-bearer-secret"),
            },
            OperatorOperation::GoogleCodeAssistOnboard,
            vec!["google-bearer-secret"],
        ),
        (
            OperatorHttpRequest::GoogleCodeAssistPoll {
                operation: GoogleOperationName::parse("operations/abc-123".to_owned())
                    .expect("operation name"),
                bearer: secret("google-bearer-secret"),
            },
            OperatorOperation::GoogleCodeAssistPoll,
            vec!["google-bearer-secret"],
        ),
        (OperatorHttpRequest::XaiDeviceCode { body: XaiDeviceCodeBody }, OperatorOperation::XaiDeviceCode, vec![]),
        (
            OperatorHttpRequest::XaiToken {
                body: XaiTokenBody::new(secret("xai-device-secret")).expect("body"),
            },
            OperatorOperation::XaiToken,
            vec!["xai-device-secret"],
        ),
        (
            OperatorHttpRequest::XaiRefresh {
                body: XaiRefreshBody::new(secret("xai-refresh-secret")).expect("body"),
            },
            OperatorOperation::XaiRefresh,
            vec!["xai-refresh-secret"],
        ),
        (OperatorHttpRequest::KimiDeviceCode { body: KimiDeviceCodeBody }, OperatorOperation::KimiDeviceCode, vec![]),
        (
            OperatorHttpRequest::KimiToken {
                body: KimiTokenBody::new(secret("kimi-device-secret")).expect("body"),
            },
            OperatorOperation::KimiToken,
            vec!["kimi-device-secret"],
        ),
        (
            OperatorHttpRequest::KimiRefresh {
                body: KimiRefreshBody::new(secret("kimi-refresh-secret")).expect("body"),
            },
            OperatorOperation::KimiRefresh,
            vec!["kimi-refresh-secret"],
        ),
        (
            OperatorHttpRequest::OpenRouterKeyExchange {
                body: OpenRouterKeyExchangeBody::new(
                    secret("openrouter-code-secret"),
                    secret("openrouter-verifier-secret"),
                )
                .expect("body"),
            },
            OperatorOperation::OpenRouterKeyExchange,
            vec!["openrouter-code-secret", "openrouter-verifier-secret"],
        ),
        (
            OperatorHttpRequest::RadiusCatalog {
                bearer: secret("radius-bearer-secret"),
            },
            OperatorOperation::RadiusCatalog,
            vec!["radius-bearer-secret"],
        ),
        (
            OperatorHttpRequest::SelfUpdateMetadata {
                channel: ReleaseChannel::Stable,
            },
            OperatorOperation::SelfUpdateMetadata,
            vec![],
        ),
        (
            OperatorHttpRequest::SelfUpdateAsset {
                url: ReleaseDownloadUrl::parse(
                    "https://github.com/0x8f701/rpi/releases/download/v1.2.3/rpi-1.2.3-x86_64-unknown-linux-gnu.tar.gz"
                        .to_owned(),
                )
                .expect("asset download url"),
            },
            OperatorOperation::SelfUpdateAsset,
            vec![],
        ),
    ];
    // The variant set is closed: exactly one request per operation, every
    // operation reachable, and the SelfUpdateAsset URL is structurally
    // validated (the transport pins it against the active override or the
    // fixed GitHub download host, so it is never an arbitrary authority).
    let mut operations: Vec<OperatorOperation> = Vec::new();
    for (request, expected, secrets) in &requests {
        assert!(
            !operations.contains(expected),
            "duplicate operation {expected:?}"
        );
        operations.push(*expected);
        assert_eq!(request.operation(), *expected);
        let debug = format!("{request:?}");
        assert!(debug.contains("OperatorHttpRequest"), "{debug}");
        for secret in secrets {
            assert!(
                !debug.contains(secret),
                "operator Debug must redact {secret:?}: {debug}"
            );
        }
    }
    assert_eq!(
        operations.len(),
        18,
        "all operator operations are reachable"
    );
    // Request grammar is closed and validated: malformed download URLs,
    // operation names, identifiers, and secret values are typed rejections
    // (never coerced or silently truncated).
    assert!(
        ReleaseDownloadUrl::parse("ftp://example/asset".to_owned()).is_err(),
        "non-http scheme"
    );
    assert!(
        ReleaseDownloadUrl::parse("https:///no-host".to_owned()).is_err(),
        "missing host"
    );
    assert!(
        ReleaseDownloadUrl::parse("not a url".to_owned()).is_err(),
        "unparseable url"
    );
    assert!(
        GoogleOperationName::parse("bogus".to_owned()).is_err(),
        "missing operations/ prefix"
    );
    assert!(
        GoogleProjectId::parse(String::new()).is_err(),
        "empty project id"
    );
    assert!(SecretValue::new(String::new()).is_err(), "empty secret");
    assert!(SecretValue::new("a\0b".to_owned()).is_err(), "NUL secret");
    // Typed operational errors display bounded, actionable text.
    assert!(
        OperatorHttpError::RequestTooLarge { limit: 1024 }
            .to_string()
            .contains("1024")
    );
    assert!(
        InitialAuthError::TransportUnsupported
            .to_string()
            .contains("unsupported")
    );
}

// ---------------------------------------------------------------------------
// 7. DNS pinning: scope mismatch rejection + revision reuse
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dns_pin_rejects_scope_mismatch_and_reuses_revision() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let resolver = Arc::new(MapDnsResolver::new(vec![(
        "api.test".to_owned(),
        vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
    )]));
    // Scope mismatch: the DNS pin resolves to loopback but the destination
    // authorizes only global addresses — rejected BEFORE any connection.
    let denied = SessionHttpTransport::new(
        snapshot(
            1,
            vec![destination(
                HostHttpClass::ProviderApi,
                HostHttpHost::Dns {
                    name: dns("api.test"),
                },
                port,
                vec![IpScope::Global],
            )],
        ),
        resolver.clone(),
    )
    .expect("denied transport");
    let outcome = denied
        .execute(session_request(port, "/ping", Vec::new()))
        .await;
    assert!(
        matches!(outcome, Err(SessionHttpError::IpScopeDenied)),
        "{outcome:?}"
    );
    // Matching scope: the pinned loopback address is used (the URL host is a
    // DNS name only the pin can resolve) and the same revision is reused
    // across executes.
    let pinned = SessionHttpTransport::new(
        snapshot(
            2,
            vec![destination(
                HostHttpClass::ProviderApi,
                HostHttpHost::Dns {
                    name: dns("api.test"),
                },
                port,
                vec![IpScope::Loopback],
            )],
        ),
        resolver.clone(),
    )
    .expect("pinned transport");
    let serve = tokio::spawn(serve_ok(listener));
    let first = pinned
        .execute(session_request(port, "/ping", Vec::new()))
        .await
        .expect("pinned request succeeds");
    assert_eq!(first.status(), http::StatusCode::OK);
    let second = pinned
        .execute(session_request(port, "/ping", Vec::new()))
        .await
        .expect("reused revision succeeds");
    assert_eq!(second.status(), http::StatusCode::OK);
    serve.await.expect("server task");
    let calls = resolver.calls();
    assert_eq!(
        calls
            .iter()
            .filter(|(revision, _)| revision.get() == 1)
            .count(),
        1,
        "the denied attempt resolves under revision 1: {calls:?}"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|(revision, _)| revision.get() == 2)
            .count(),
        2,
        "both successful executes resolve under revision 2: {calls:?}"
    );
}

async fn serve_ok(listener: TcpListener) {
    for _ in 0..2 {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let _ = read_request_head(&mut socket).await;
        write_response(&mut socket, "200 OK", &[], b"ok").await;
    }
}

// ---------------------------------------------------------------------------
// 8. Redirect reauthorization + secret stripping
// ---------------------------------------------------------------------------

const REDIRECT_SECRET: &str = "redirect-secret-bearer-value";

#[tokio::test]
async fn redirect_reauthorizes_authority_and_strips_secrets() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let resolver = Arc::new(MapDnsResolver::new(vec![
        ("api.test".to_owned(), vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
        (
            "other.test".to_owned(),
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
        ),
    ]));
    let destinations = vec![
        destination(
            HostHttpClass::ProviderApi,
            HostHttpHost::Dns {
                name: dns("api.test"),
            },
            port,
            vec![IpScope::Loopback],
        ),
        destination(
            HostHttpClass::ProviderApi,
            HostHttpHost::Dns {
                name: dns("other.test"),
            },
            port,
            vec![IpScope::Loopback],
        ),
    ];
    let transport =
        SessionHttpTransport::new(snapshot(9, destinations), resolver).expect("transport");
    let seen = Arc::new(Mutex::new(Vec::<HashMap<String, String>>::new()));
    let seen_task = seen.clone();
    let location_cross = format!("http://other.test:{port}/next");
    let serve = tokio::spawn(async move {
        // Leg 1: hop 0 redirects to a DIFFERENT authority (other.test).
        let (mut first, _) = listener.accept().await.expect("accept leg1 hop0");
        let _ = read_request_head(&mut first).await;
        write_response(
            &mut first,
            "302 Found",
            &[("Location", location_cross.as_str())],
            b"",
        )
        .await;
        let (mut second, _) = listener.accept().await.expect("accept leg1 hop1");
        let headers = read_request_head(&mut second).await;
        seen_task.lock().push(headers);
        write_response(
            &mut second,
            "200 OK",
            &[("Content-Type", "application/sdp")],
            b"v=0 answer one",
        )
        .await;
        // Leg 2: hop 0 redirects to the SAME authority (relative Location).
        let (mut third, _) = listener.accept().await.expect("accept leg2 hop0");
        let _ = read_request_head(&mut third).await;
        write_response(&mut third, "302 Found", &[("Location", "/same-next")], b"").await;
        let (mut fourth, _) = listener.accept().await.expect("accept leg2 hop1");
        let headers = read_request_head(&mut fourth).await;
        seen_task.lock().push(headers);
        write_response(
            &mut fourth,
            "200 OK",
            &[("Content-Type", "application/sdp")],
            b"v=0 answer two",
        )
        .await;
    });
    let auth = vec![(
        http::HeaderName::from_static("authorization"),
        http::HeaderValue::from_str(&format!("Bearer {REDIRECT_SECRET}")).expect("header value"),
    )];
    // Cross-authority redirect: the hop is reauthorized against the new
    // destination and secret headers are stripped.
    let first = transport
        .execute(session_request(port, "/start", auth.clone()))
        .await
        .expect("cross-authority redirect chain");
    assert_eq!(first.status(), http::StatusCode::OK);
    assert_eq!(
        collect_response_body(first).await.expect("body"),
        b"v=0 answer one"
    );
    // Same-authority redirect: headers (including authorization) survive.
    let second = transport
        .execute(session_request(port, "/same", auth))
        .await
        .expect("same-authority redirect chain");
    assert_eq!(second.status(), http::StatusCode::OK);
    assert_eq!(
        collect_response_body(second).await.expect("body"),
        b"v=0 answer two"
    );
    serve.await.expect("server task");
    let seen = seen.lock();
    assert_eq!(seen.len(), 2, "exactly the two redirected hops were served");
    let cross = &seen[0];
    assert!(
        !cross.contains_key("authorization"),
        "cross-authority redirect must strip authorization: {cross:?}"
    );
    assert!(
        cross.values().all(|value| !value.contains(REDIRECT_SECRET)),
        "no secret leaks on the cross hop: {cross:?}"
    );
    let same = &seen[1];
    assert!(
        same.contains_key("authorization"),
        "same-authority redirect keeps authorization: {same:?}"
    );
    assert!(
        same["authorization"].contains(REDIRECT_SECRET),
        "bearer survives same-authority redirects: {same:?}"
    );
}

// ---------------------------------------------------------------------------
// 9. Missing required destination is typed and never widens
// ---------------------------------------------------------------------------

#[tokio::test]
async fn missing_required_destination_is_typed_and_never_widens() {
    pi_ai::providers::register_builtins();
    let model = Model {
        id: "gpt-5".to_owned(),
        name: "gpt-5".to_owned(),
        api: API_OPENAI_COMPLETIONS.to_owned(),
        provider: "openai".to_owned(),
        base_url: "https://api.openai.com/v1".to_owned(),
        ..Model::default()
    };
    let requirements = SessionHttpRequirements::derive(
        &model,
        &pi_coding::RequestAuth::default(),
        &StreamOptions::default(),
    )
    .expect("requirements derive");
    // Omission derives the exact requirement set (provider destination only).
    let derived = resolve_session_http_policy(None, &requirements).expect("derived policy");
    assert_eq!(
        derived.destinations().len(),
        1,
        "one provider destination derives"
    );
    let required = derived.destinations()[0].canonical_key();
    assert_eq!(required.class(), HostHttpClass::ProviderApi);
    // A configured policy that omits the requirement is a TYPED error.
    let empty = HostHttpPolicySettings {
        destinations: Vec::new(),
    };
    let missing = resolve_session_http_policy(Some(&empty), &requirements);
    assert!(
        matches!(missing, Err(SessionHttpError::MissingHostHttpDestination)),
        "{missing:?}"
    );
    // A configured policy for a DIFFERENT destination still does not widen to
    // include the requirement: typed error again.
    let other = HostHttpPolicySettings {
        destinations: vec![HostHttpDestinationWire {
            class: HostHttpClass::ProviderApi,
            scheme: HostHttpScheme::Https,
            host: HostHttpHostWire::Dns {
                name: "anthropic.example".to_owned(),
            },
            port: 443,
            allowed_ip_scopes: vec![IpScope::Global],
        }],
    };
    let missing_other = resolve_session_http_policy(Some(&other), &requirements);
    assert!(
        matches!(
            missing_other,
            Err(SessionHttpError::MissingHostHttpDestination)
        ),
        "{missing_other:?}"
    );
    // A configured policy that CONTAINS the requirement passes and stays
    // exact: explicit configuration is never unioned with the derived set.
    let full = HostHttpPolicySettings {
        destinations: vec![
            HostHttpDestinationWire {
                class: HostHttpClass::ProviderApi,
                scheme: HostHttpScheme::Https,
                host: HostHttpHostWire::Dns {
                    name: "api.openai.com".to_owned(),
                },
                port: 443,
                allowed_ip_scopes: vec![IpScope::Global],
            },
            HostHttpDestinationWire {
                class: HostHttpClass::ImageGeneration,
                scheme: HostHttpScheme::Https,
                host: HostHttpHostWire::Dns {
                    name: "images.example".to_owned(),
                },
                port: 443,
                allowed_ip_scopes: vec![IpScope::Global],
            },
        ],
    };
    let policy = resolve_session_http_policy(Some(&full), &requirements).expect("full policy");
    assert_eq!(
        policy.destinations().len(),
        2,
        "configured policy is not widened"
    );
    assert!(
        policy
            .destinations()
            .iter()
            .any(|row| row.canonical_key() == required),
        "the required destination is satisfied by the configured row"
    );
}

// ---------------------------------------------------------------------------
// 10. Unsupported initial auth is a typed operational error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn initial_auth_unsupported_is_operational() {
    use pi_coding::oauth::refresh_initial_with_operator;
    use pi_coding::operator_http::{InitialAuthError, OperatorHttpTransport};
    // Constructing the operator transport reads endpoint-override env vars:
    // KIMI_CODE_OAUTH_HOST / KIMI_OAUTH_HOST stay fail-closed, while
    // PI_UPDATE_BASE_URL is now honored (validated https or http-loopback) by
    // the self-update operations only. CI sets none of them and the workspace
    // forbids env mutation in edition 2024, so the transport builds cleanly.
    let operator = OperatorHttpTransport::new().expect("operator transport");
    let credential = pi_coding::Credential::ApiKey {
        key: None,
        env: HashMap::new(),
        extra: serde_json::Map::new(),
    };
    // An unknown provider is a TYPED operational error — no network, no
    // configuration failure.
    let unknown = refresh_initial_with_operator("no-such-provider", &credential, &operator).await;
    assert!(
        matches!(unknown, Err(InitialAuthError::TransportUnsupported)),
        "{unknown:?}"
    );
    // A known provider without stored OAuth cannot refresh: same typed
    // operational error, and nothing is transmitted.
    let known = refresh_initial_with_operator("anthropic", &credential, &operator).await;
    assert!(
        matches!(known, Err(InitialAuthError::TransportUnsupported)),
        "{known:?}"
    );
}
