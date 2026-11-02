use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroU64;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::{BoxFuture, FutureExt, Shared};
use parking_lot::Mutex;
use pi_sandbox::{
    DnsName, HostHttpClass, HostHttpDestination, HostHttpDestinationKey, HostHttpHost,
    HostHttpPolicy, HostHttpScheme, IpScope, PolicyRevision, SandboxMode, SharedPolicySnapshot,
};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::RequestAuth;
use crate::settings::{HostHttpHostWire, HostHttpPolicySettings};

pub const SYSTEM_DNS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
pub const SYSTEM_DNS_SYNTHETIC_TTL: Duration = Duration::from_secs(60);
pub const SYSTEM_DNS_MIN_ADDRESSES: usize = 1;
pub const SYSTEM_DNS_MAX_ADDRESSES: usize = 32;
pub const MAX_SESSION_HTTP_REDIRECTS: usize = 10;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DnsResolution {
    addresses: Box<[IpAddr]>,
    ttl: Duration,
}

impl DnsResolution {
    pub fn new(mut addresses: Vec<IpAddr>, ttl: Duration) -> Result<Self, SessionHttpError> {
        addresses.sort_unstable();
        addresses.dedup();
        if addresses.len() < SYSTEM_DNS_MIN_ADDRESSES {
            return Err(SessionHttpError::DnsEmpty);
        }
        if addresses.len() > SYSTEM_DNS_MAX_ADDRESSES {
            return Err(SessionHttpError::DnsOverflow);
        }
        Ok(Self {
            addresses: addresses.into_boxed_slice(),
            ttl,
        })
    }

    pub fn addresses(&self) -> &[IpAddr] {
        &self.addresses
    }
    pub const fn ttl(&self) -> Duration {
        self.ttl
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SessionHttpError {
    #[error("session HTTP request is invalid")]
    InvalidRequest,
    #[error("session HTTP destination is not authorized")]
    DestinationNotAuthorized,
    #[error("session HTTP DNS lookup failed")]
    DnsLookupFailed,
    #[error("session HTTP DNS lookup returned no addresses")]
    DnsEmpty,
    #[error("session HTTP DNS lookup returned too many addresses")]
    DnsOverflow,
    #[error("session HTTP policy is missing a required destination")]
    MissingHostHttpDestination,
    #[error("session HTTP destination resolves outside its allowed IP scopes")]
    IpScopeDenied,
    #[error("session HTTP redirect is invalid or unauthorized")]
    RedirectDenied,
    #[error("session HTTP redirect limit exceeded")]
    RedirectLimitExceeded,
    #[error("session HTTP request timed out")]
    DeadlineExceeded,
    #[error("session HTTP request was cancelled")]
    Cancelled,
    #[error("session HTTP response exceeded its byte limit")]
    ResponseTooLarge,
    #[error("session HTTP transport failed")]
    Transport,
    #[error("session HTTP resolver runtime is unavailable")]
    ResolverRuntimeUnavailable,
}

pub trait DnsResolver: Send + Sync {
    fn resolve<'a>(
        &'a self,
        revision: PolicyRevision,
        host: &'a DnsName,
        cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<DnsResolution, SessionHttpError>>;
}

#[derive(Clone)]
struct DnsCacheEntry {
    resolution: DnsResolution,
    expires_at: Instant,
}
pub type SharedDnsResult = Result<DnsResolution, SessionHttpError>;

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SharedDnsCompletion {
    Pending = 0,
    Succeeded = 1,
    Failed = 2,
    Cancelled = 3,
}

impl SharedDnsCompletion {
    pub fn load(state: &AtomicU8) -> Self {
        match state.load(Ordering::Acquire) {
            0 => Self::Pending,
            1 => Self::Succeeded,
            2 => Self::Failed,
            3 => Self::Cancelled,
            _ => std::process::abort(),
        }
    }

    pub fn complete(state: &AtomicU8, next: Self) -> Result<(), Self> {
        if next == Self::Pending {
            std::process::abort();
        }
        state
            .compare_exchange(
                Self::Pending as u8,
                next as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|value| match value {
                0 => Self::Pending,
                1 => Self::Succeeded,
                2 => Self::Failed,
                3 => Self::Cancelled,
                _ => std::process::abort(),
            })
    }
}

pub struct SharedDnsState {
    key: (PolicyRevision, DnsName),
    future: Shared<BoxFuture<'static, SharedDnsResult>>,
    abort: CancellationToken,
    waiters: AtomicUsize,
    completion: AtomicU8,
    finished: mpsc::UnboundedSender<Arc<SharedDnsState>>,
}

impl SharedDnsState {
    /// Current completion state; used by the D8 host_network proof contracts
    /// to observe single-transition completion and last-waiter cancellation.
    pub fn completion(&self) -> SharedDnsCompletion {
        SharedDnsCompletion::load(&self.completion)
    }

    pub fn register_waiter(self: &Arc<Self>) -> Result<(), SessionHttpError> {
        if SharedDnsCompletion::load(&self.completion) != SharedDnsCompletion::Pending {
            return Err(SessionHttpError::DnsLookupFailed);
        }
        self.waiters
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_add(1)
            })
            .map_err(|_| SessionHttpError::DnsLookupFailed)?;
        if SharedDnsCompletion::load(&self.completion) == SharedDnsCompletion::Pending {
            Ok(())
        } else {
            if self.waiters.fetch_sub(1, Ordering::AcqRel) == 0 {
                std::process::abort();
            }
            Err(SessionHttpError::DnsLookupFailed)
        }
    }

    pub fn complete_once(self: &Arc<Self>, outcome: &SharedDnsResult) -> bool {
        let next = match outcome {
            Ok(_) => SharedDnsCompletion::Succeeded,
            Err(SessionHttpError::Cancelled) => SharedDnsCompletion::Cancelled,
            Err(_) => SharedDnsCompletion::Failed,
        };
        if SharedDnsCompletion::complete(&self.completion, next).is_ok() {
            let _ = self.finished.send(self.clone());
            true
        } else {
            false
        }
    }

    pub fn release_waiter(self: &Arc<Self>, is_complete: bool) {
        let previous = self.waiters.fetch_sub(1, Ordering::AcqRel);
        if previous == 0 {
            std::process::abort();
        }
        if previous == 1
            && !is_complete
            && SharedDnsCompletion::complete(&self.completion, SharedDnsCompletion::Cancelled)
                .is_ok()
        {
            self.abort.cancel();
            let _ = self.finished.send(self.clone());
        }
    }
}

#[derive(Clone)]
pub struct SharedDnsLookup {
    pub state: Arc<SharedDnsState>,
}

pub struct SharedDnsWaiter {
    state: Arc<SharedDnsState>,
    future: Shared<BoxFuture<'static, SharedDnsResult>>,
    is_registered: bool,
    is_complete: bool,
}

impl SharedDnsLookup {
    pub fn new(
        revision: PolicyRevision,
        host: DnsName,
        finished: mpsc::UnboundedSender<Arc<SharedDnsState>>,
    ) -> Self {
        let abort = CancellationToken::new();
        let lookup_abort = abort.clone();
        let lookup_host = host.as_str().to_owned();
        let future = async move {
            let lookup = async {
                let addresses = tokio::net::lookup_host((lookup_host.as_str(), 0))
                    .await
                    .map_err(|_| SessionHttpError::DnsLookupFailed)?
                    .map(|address| address.ip())
                    .collect::<Vec<_>>();
                DnsResolution::new(addresses, SYSTEM_DNS_SYNTHETIC_TTL)
            };
            tokio::select! {
                () = lookup_abort.cancelled() => Err(SessionHttpError::Cancelled),
                result = tokio::time::timeout(SYSTEM_DNS_LOOKUP_TIMEOUT, lookup) =>
                    result.map_err(|_| SessionHttpError::DnsLookupFailed)?,
            }
        }
        .boxed()
        .shared();
        Self {
            state: Arc::new(SharedDnsState {
                key: (revision, host),
                future,
                abort,
                waiters: AtomicUsize::new(0),
                completion: AtomicU8::new(SharedDnsCompletion::Pending as u8),
                finished,
            }),
        }
    }

    pub fn subscribe(&self) -> Result<SharedDnsWaiter, SessionHttpError> {
        self.state.register_waiter()?;
        Ok(SharedDnsWaiter {
            state: self.state.clone(),
            future: self.state.future.clone(),
            is_registered: true,
            is_complete: false,
        })
    }
}

impl Future for SharedDnsWaiter {
    type Output = SharedDnsResult;
    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.future).poll(context) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.state.complete_once(&result);
                self.is_complete = true;
                if self.is_registered {
                    self.is_registered = false;
                    self.state.release_waiter(true);
                }
                Poll::Ready(result)
            }
        }
    }
}

impl Drop for SharedDnsWaiter {
    fn drop(&mut self) {
        if self.is_registered {
            self.is_registered = false;
            self.state.release_waiter(self.is_complete);
        }
    }
}

pub struct SystemDnsResolver {
    cache: Mutex<BTreeMap<(PolicyRevision, DnsName), DnsCacheEntry>>,
    /// In-flight lookups by (revision, host); public so the D8 host_network
    /// proof contracts can observe pointer-identical row removal.
    pub in_flight: Mutex<BTreeMap<(PolicyRevision, DnsName), SharedDnsLookup>>,
    finished: mpsc::UnboundedSender<Arc<SharedDnsState>>,
}

impl SystemDnsResolver {
    pub fn new() -> Result<Arc<Self>, SessionHttpError> {
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| SessionHttpError::ResolverRuntimeUnavailable)?;
        let (finished, receiver) = mpsc::unbounded_channel();
        let resolver = Arc::new(Self {
            cache: Mutex::new(BTreeMap::new()),
            in_flight: Mutex::new(BTreeMap::new()),
            finished,
        });
        handle.spawn(drain_dns_completions(Arc::downgrade(&resolver), receiver));
        Ok(resolver)
    }
}

pub async fn drain_dns_completions(
    resolver: Weak<SystemDnsResolver>,
    mut receiver: mpsc::UnboundedReceiver<Arc<SharedDnsState>>,
) {
    while let Some(state) = receiver.recv().await {
        let Some(resolver) = resolver.upgrade() else {
            break;
        };
        let mut in_flight = resolver.in_flight.lock();
        if in_flight
            .get(&state.key)
            .is_some_and(|lookup| Arc::ptr_eq(&lookup.state, &state))
        {
            in_flight.remove(&state.key);
        }
    }
}

impl DnsResolver for SystemDnsResolver {
    fn resolve<'a>(
        &'a self,
        revision: PolicyRevision,
        host: &'a DnsName,
        cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, SharedDnsResult> {
        async move {
            let key = (revision, host.clone());
            let now = Instant::now();
            {
                let mut cache = self.cache.lock();
                cache.retain(|_, entry| entry.expires_at > now);
                if let Some(entry) = cache.get(&key) {
                    return Ok(entry.resolution.clone());
                }
            }
            let waiter = {
                let mut in_flight = self.in_flight.lock();
                let lookup = in_flight.entry(key.clone()).or_insert_with(|| {
                    SharedDnsLookup::new(revision, host.clone(), self.finished.clone())
                });
                match lookup.subscribe() {
                    Ok(waiter) => waiter,
                    Err(_) => {
                        let replacement =
                            SharedDnsLookup::new(revision, host.clone(), self.finished.clone());
                        let waiter = replacement.subscribe()?;
                        *lookup = replacement;
                        waiter
                    }
                }
            };
            let resolution = tokio::select! {
                () = cancellation.cancelled() => return Err(SessionHttpError::Cancelled),
                result = waiter => result?,
            };
            self.cache.lock().insert(
                key,
                DnsCacheEntry {
                    expires_at: Instant::now() + resolution.ttl(),
                    resolution: resolution.clone(),
                },
            );
            Ok(resolution)
        }
        .boxed()
    }
}

pub struct SessionHttpRequest {
    class: HostHttpClass,
    method: pi_ai::HttpMethod,
    authority: pi_ai::ResolvedHttpAuthority,
    path_and_query: String,
    headers: Vec<(http::HeaderName, http::HeaderValue)>,
    body: Vec<u8>,
    timeout: Duration,
    max_response_bytes: NonZeroU64,
    cancellation: Option<CancellationToken>,
}

impl SessionHttpRequest {
    pub fn from_pi_ai(
        class: HostHttpClass,
        request: pi_ai::HttpRequest,
    ) -> Result<Self, SessionHttpError> {
        let parts = request.into_parts();
        let timeout = Duration::from_millis(parts.timeout_ms);
        if timeout.is_zero() {
            return Err(SessionHttpError::InvalidRequest);
        }
        Ok(Self {
            class,
            method: parts.method,
            authority: parts.authority,
            path_and_query: parts.path_and_query,
            headers: parts.headers,
            body: parts.body,
            timeout,
            max_response_bytes: parts.max_response_bytes,
            cancellation: parts.abort_signal,
        })
    }
}

impl fmt::Debug for SessionHttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionHttpRequest")
            .field("class", &self.class)
            .field("method", &self.method)
            .field("authority", &self.authority)
            .field("path_and_query", &"[REDACTED]")
            .field("header_count", &self.headers.len())
            .field("body_bytes", &self.body.len())
            .field("timeout", &self.timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

pub struct AuthorizedDestination {
    destination: HostHttpDestination,
    socket_addresses: Box<[SocketAddr]>,
}

#[derive(Clone)]
pub struct RedirectRequest {
    method: pi_ai::HttpMethod,
    authority: pi_ai::ResolvedHttpAuthority,
    path_and_query: String,
    headers: Vec<(http::HeaderName, http::HeaderValue)>,
    body: Vec<u8>,
}

#[derive(Clone)]
pub enum SessionHttpAuthority {
    Confined(Arc<SessionHttpTransport>),
    ExplicitDanger(Arc<RawSessionHttpTransport>),
}

impl SessionHttpAuthority {
    pub fn executor(&self, class: HostHttpClass) -> pi_ai::SharedHttpExecutor {
        Arc::new(SessionHttpExecutor {
            class,
            authority: self.clone(),
        })
    }
}

pub struct SessionHttpExecutor {
    class: HostHttpClass,
    authority: SessionHttpAuthority,
}

impl pi_ai::HttpExecutor for SessionHttpExecutor {
    fn execute<'a>(
        &'a self,
        request: pi_ai::HttpRequest,
    ) -> BoxFuture<'a, anyhow::Result<pi_ai::HttpResponse>> {
        async move {
            let request = SessionHttpRequest::from_pi_ai(self.class, request)?;
            match &self.authority {
                SessionHttpAuthority::Confined(transport) => transport.execute(request).await,
                SessionHttpAuthority::ExplicitDanger(transport) => transport.execute(request).await,
            }
            .map_err(anyhow::Error::from)
        }
        .boxed()
    }
}

pub struct SessionHttpTransport {
    snapshot: SharedPolicySnapshot,
    destinations: BTreeMap<HostHttpDestinationKey, HostHttpDestination>,
    resolver: Arc<dyn DnsResolver>,
}

pub struct RawSessionHttpTransport {
    client: reqwest::Client,
}

impl SessionHttpTransport {
    pub fn new(
        snapshot: SharedPolicySnapshot,
        resolver: Arc<dyn DnsResolver>,
    ) -> Result<Arc<Self>, SessionHttpError> {
        if snapshot.policy().mode() == SandboxMode::DangerFullAccess {
            return Err(SessionHttpError::InvalidRequest);
        }
        snapshot
            .validate_digest()
            .map_err(|_| SessionHttpError::InvalidRequest)?;
        let policy = snapshot
            .policy()
            .host_http()
            .ok_or(SessionHttpError::InvalidRequest)?;
        let mut destinations = BTreeMap::new();
        for destination in policy.destinations() {
            if destinations
                .insert(destination.canonical_key(), destination.clone())
                .is_some()
            {
                return Err(SessionHttpError::InvalidRequest);
            }
        }
        Ok(Arc::new(Self {
            snapshot,
            destinations,
            resolver,
        }))
    }

    pub async fn execute(
        &self,
        request: SessionHttpRequest,
    ) -> Result<pi_ai::HttpResponse, SessionHttpError> {
        let deadline = Instant::now() + request.timeout;
        let cancellation = request.cancellation.unwrap_or_default();
        let mut current = RedirectRequest {
            method: request.method,
            authority: request.authority,
            path_and_query: request.path_and_query,
            headers: request.headers,
            body: request.body,
        };
        let addresses = resolve_authority(
            self.snapshot.revision(),
            self.resolver.as_ref(),
            &current.authority,
            deadline,
            &cancellation,
        )
        .await?;
        let mut authorized = authorize_destination(
            &self.destinations,
            request.class,
            &current.authority,
            &addresses,
        )?;
        for hop in 0..=MAX_SESSION_HTTP_REDIRECTS {
            let response =
                connect_pinned(current.clone(), &authorized, deadline, &cancellation).await?;
            if !is_redirect_status(response.status()) {
                let status = response.status();
                let headers = response.headers().clone();
                return Ok(pi_ai::HttpResponse::new(
                    status,
                    headers,
                    bounded_response_body(
                        response,
                        request.timeout,
                        cancellation,
                        request.max_response_bytes,
                    ),
                ));
            }
            (current, authorized) = reauthorize_redirect(
                &self.destinations,
                self.snapshot.revision(),
                self.resolver.as_ref(),
                request.class,
                current,
                &response,
                hop,
                deadline,
                &cancellation,
            )
            .await?;
        }
        Err(SessionHttpError::RedirectLimitExceeded)
    }
}

impl RawSessionHttpTransport {
    pub fn new_explicit_danger() -> Result<Arc<Self>, SessionHttpError> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| SessionHttpError::Transport)?;
        Ok(Arc::new(Self { client }))
    }

    pub async fn execute(
        &self,
        request: SessionHttpRequest,
    ) -> Result<pi_ai::HttpResponse, SessionHttpError> {
        let deadline = Instant::now() + request.timeout;
        let cancellation = request.cancellation.unwrap_or_default();
        let mut current = RedirectRequest {
            method: request.method,
            authority: request.authority,
            path_and_query: request.path_and_query,
            headers: request.headers,
            body: request.body,
        };
        for hop in 0..=MAX_SESSION_HTTP_REDIRECTS {
            let response = send_raw(&self.client, &current, deadline, &cancellation).await?;
            if !is_redirect_status(response.status()) {
                let status = response.status();
                let headers = response.headers().clone();
                return Ok(pi_ai::HttpResponse::new(
                    status,
                    headers,
                    bounded_response_body(
                        response,
                        request.timeout,
                        cancellation,
                        request.max_response_bytes,
                    ),
                ));
            }
            if hop >= MAX_SESSION_HTTP_REDIRECTS {
                return Err(SessionHttpError::RedirectLimitExceeded);
            }
            current = redirect_request(&current, &response)?;
        }
        Err(SessionHttpError::RedirectLimitExceeded)
    }
}

/// Configured endpoints for the dynamic built-in HTTP destinations whose hosts
/// come from user settings (memory/Hindsight, live STT, realtime) or
/// environment (llama.cpp router base URL, Hugging Face endpoint). The static
/// built-in product destinations (web search, GitHub API, GitHub codeload
/// archives, the npm registry/tarball, and Hugging Face model/GGUF downloads)
/// are always derived by [`SessionHttpRequirements::derive_with_builtins`];
/// these optional fields add the configured dynamic ones so a confined
/// `workspace-write` session can authorize the exact endpoint the operator
/// configured without widening the operator's explicit `hostHttp` policy.
///
/// Kept free of environment reads so derivation stays deterministic in tests;
/// the application layer populates it from settings/env.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuiltinHttpEndpoints {
    /// Hindsight API base URL (`settings.memory.hindsightApiUrl`) when the
    /// memory backend is `hindsight`.
    pub memory_api_url: Option<String>,
    /// `/live` STT base URL (`settings.live.sttBaseUrl`).
    pub live_stt_url: Option<String>,
    /// Realtime (Codex Live) base URL (`settings.live.realtimeBaseUrl`).
    pub realtime_url: Option<String>,
    /// llama.cpp router base URL (`LLAMA_BASE_URL`).
    pub llama_router_url: Option<String>,
    /// Hugging Face endpoint override (`HF_ENDPOINT`); defaults to
    /// `https://huggingface.co` when `None`.
    pub hugging_face_url: Option<String>,
    /// Remote plugin marketplace index URL. No product default — a remote
    /// index requires explicit operator `hostHttp` authority, so this stays
    /// `None` unless an operator-configured source is supplied.
    pub plugin_marketplace_url: Option<String>,
}

pub struct SessionHttpRequirements {
    destinations: BTreeMap<HostHttpDestinationKey, HostHttpDestination>,
}

impl SessionHttpRequirements {
    pub fn derive(
        model: &pi_ai::Model,
        request_auth: &RequestAuth,
        stream: &pi_ai::StreamOptions,
    ) -> Result<Self, SessionHttpError> {
        Self::derive_with_image_generation(model, request_auth, stream, None)
    }

    pub(crate) fn derive_with_image_generation(
        model: &pi_ai::Model,
        request_auth: &RequestAuth,
        stream: &pi_ai::StreamOptions,
        image_generation: Option<(
            &pi_ai::Model,
            Option<&str>,
            &std::collections::HashMap<String, String>,
        )>,
    ) -> Result<Self, SessionHttpError> {
        Ok(Self {
            destinations: derive_core_destinations(model, request_auth, stream, image_generation)?,
        })
    }

    /// Derive the confined HTTP requirements for a session, enumerating the
    /// exact built-in product destinations (web search, GitHub API, GitHub
    /// codeload plugin archives, the npm registry/tarball, Hugging Face
    /// model/GGUF downloads) plus the dynamic destinations the operator
    /// configured (`builtins`). Used by the production sandbox-generation path
    /// so a confined `workspace-write` session can authorize each built-in
    /// tool out of the box; an explicit operator `hostHttp` policy is still
    /// never widened — it must independently satisfy the derived requirements.
    pub(crate) fn derive_with_builtins(
        model: &pi_ai::Model,
        request_auth: &RequestAuth,
        stream: &pi_ai::StreamOptions,
        image_generation: Option<(
            &pi_ai::Model,
            Option<&str>,
            &std::collections::HashMap<String, String>,
        )>,
        builtins: &BuiltinHttpEndpoints,
    ) -> Result<Self, SessionHttpError> {
        let mut destinations =
            derive_core_destinations(model, request_auth, stream, image_generation)?;
        for (class, host, port) in STATIC_BUILTIN_HOSTS.iter().copied() {
            let destination = dns_global_destination(class, host, port)?;
            destinations.insert(destination.canonical_key(), destination);
        }
        let hugging_face = builtins
            .hugging_face_url
            .as_deref()
            .unwrap_or(DEFAULT_HUGGING_FACE_URL);
        for class in [HostHttpClass::LlamaHuggingFace, HostHttpClass::LlamaGguf] {
            if let Some(destination) = destination_from_url(class, hugging_face)? {
                destinations.insert(destination.canonical_key(), destination);
            }
        }
        for (class, url) in [
            (HostHttpClass::MemoryApi, builtins.memory_api_url.as_deref()),
            (HostHttpClass::LiveStt, builtins.live_stt_url.as_deref()),
            (HostHttpClass::RealtimeApi, builtins.realtime_url.as_deref()),
            (
                HostHttpClass::LlamaRouter,
                builtins.llama_router_url.as_deref(),
            ),
            (
                HostHttpClass::PluginMarketplace,
                builtins.plugin_marketplace_url.as_deref(),
            ),
        ] {
            if let Some(destination) = destination_from_url(class, url.unwrap_or(""))? {
                destinations.insert(destination.canonical_key(), destination);
            }
        }
        Ok(Self { destinations })
    }
}

/// Static built-in product destinations whose hosts are fixed product
/// constants (not operator-configured). Always derived for a confined
/// session so the built-in tools work without an explicit `hostHttp` policy.
const STATIC_BUILTIN_HOSTS: &[(HostHttpClass, &str, u16)] = &[
    (HostHttpClass::WebSearch, "api.exa.ai", 443),
    (HostHttpClass::GithubApi, "api.github.com", 443),
    (HostHttpClass::PluginArchive, "codeload.github.com", 443),
    (HostHttpClass::NpmRegistry, "registry.npmjs.org", 443),
    (HostHttpClass::NpmTarball, "registry.npmjs.org", 443),
];

/// Default Hugging Face endpoint; overridden by `BuiltinHttpEndpoints::hugging_face_url`.
const DEFAULT_HUGGING_FACE_URL: &str = "https://huggingface.co";

/// Core derived destinations shared by every derivation path: the resolved
/// provider API endpoint(s), the optional image-generation endpoint, and the
/// OAuth refresh endpoint when stored OAuth is in use. Built-in product
/// destinations are layered on top by [`SessionHttpRequirements::derive_with_builtins`].
fn derive_core_destinations(
    model: &pi_ai::Model,
    request_auth: &RequestAuth,
    stream: &pi_ai::StreamOptions,
    image_generation: Option<(
        &pi_ai::Model,
        Option<&str>,
        &std::collections::HashMap<String, String>,
    )>,
) -> Result<BTreeMap<HostHttpDestinationKey, HostHttpDestination>, SessionHttpError> {
    let mut destinations = BTreeMap::new();
    for authority in pi_ai::resolve_provider_http_authorities(model, stream)
        .map_err(|_| SessionHttpError::InvalidRequest)?
    {
        if let Some(destination) = destination_from_resolved(
            HostHttpClass::ProviderApi,
            &authority,
            model_endpoint_is_operator_owned(model),
        )? {
            destinations.insert(destination.canonical_key(), destination);
        }
    }
    if let Some((image_model, base_url, env)) = image_generation {
        let authority =
            pi_ai::providers::resolve_image_generation_http_authority(image_model, base_url, env)
                .map_err(|_| SessionHttpError::InvalidRequest)?;
        if let Some(destination) = destination_from_resolved(
            HostHttpClass::ImageGeneration,
            &authority,
            image_endpoint_is_operator_owned(image_model, base_url),
        )? {
            destinations.insert(destination.canonical_key(), destination);
        }
    }
    if request_auth.uses_stored_oauth() {
        if let Some(destination) = oauth_refresh_destination(&model.provider)? {
            destinations.insert(destination.canonical_key(), destination);
        }
    }
    Ok(destinations)
}

/// Build a fixed-host global-scope HTTPS destination for a static built-in
/// product endpoint (e.g. `api.exa.ai`, `api.github.com`).
fn dns_global_destination(
    class: HostHttpClass,
    host: &str,
    port: u16,
) -> Result<HostHttpDestination, SessionHttpError> {
    HostHttpDestination::new(
        class,
        HostHttpScheme::Https,
        HostHttpHost::Dns {
            name: DnsName::parse(host.to_owned()).map_err(|_| SessionHttpError::InvalidRequest)?,
        },
        port,
        vec![IpScope::Global],
    )
    .map_err(|_| SessionHttpError::InvalidRequest)
}

/// Build a destination from a configured base URL, deriving the exact
/// scheme/host/port and the IP scope that matches the configured host so a
/// loopback self-hosted endpoint (e.g. a local llama.cpp router or whisper
/// STT server) is authorized while a public DNS endpoint stays global.
/// Returns `Ok(None)` when `raw` is empty so unset dynamic destinations are
/// simply omitted rather than rejected.
fn destination_from_url(
    class: HostHttpClass,
    raw: &str,
) -> Result<Option<HostHttpDestination>, SessionHttpError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let url = url::Url::parse(raw).map_err(|_| SessionHttpError::InvalidRequest)?;
    let scheme = match url.scheme() {
        "https" => HostHttpScheme::Https,
        "http" => HostHttpScheme::Http,
        _ => return Err(SessionHttpError::InvalidRequest),
    };
    let host = match url.host().ok_or(SessionHttpError::InvalidRequest)? {
        url::Host::Domain(name) => {
            sandbox_host(&pi_ai::ResolvedHttpHost::Dns(name.to_ascii_lowercase()))?
        }
        url::Host::Ipv4(address) => sandbox_host(&pi_ai::ResolvedHttpHost::Ip(address.into()))?,
        url::Host::Ipv6(address) => sandbox_host(&pi_ai::ResolvedHttpHost::Ip(address.into()))?,
    };
    let port = url
        .port_or_known_default()
        .ok_or(SessionHttpError::InvalidRequest)?;
    let scopes = match host {
        HostHttpHost::Ip { address } => vec![ip_scope(address)],
        HostHttpHost::Dns { .. } => vec![IpScope::Global],
    };
    HostHttpDestination::new(class, scheme, host, port, scopes)
        .map_err(|_| SessionHttpError::InvalidRequest)
        .map(Some)
}

pub fn resolve_session_http_policy(
    configured: Option<&HostHttpPolicySettings>,
    requirements: &SessionHttpRequirements,
) -> Result<HostHttpPolicy, SessionHttpError> {
    let destinations = if let Some(configured) = configured {
        configured
            .destinations
            .iter()
            .map(|destination| {
                let host = match &destination.host {
                    HostHttpHostWire::Dns { name } => HostHttpHost::Dns {
                        name: DnsName::parse(name.clone())
                            .map_err(|_| SessionHttpError::InvalidRequest)?,
                    },
                    HostHttpHostWire::Ip { address } => HostHttpHost::Ip { address: *address },
                };
                HostHttpDestination::new(
                    destination.class,
                    destination.scheme,
                    host,
                    destination.port,
                    destination.allowed_ip_scopes.clone(),
                )
                .map_err(|_| SessionHttpError::InvalidRequest)
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        requirements.destinations.values().cloned().collect()
    };
    let policy = HostHttpPolicy::new(destinations).map_err(|_| SessionHttpError::InvalidRequest)?;
    if requirements.destinations.keys().all(|required| {
        policy
            .destinations()
            .iter()
            .any(|row| &row.canonical_key() == required)
    }) {
        Ok(policy)
    } else {
        Err(SessionHttpError::MissingHostHttpDestination)
    }
}

pub fn authorize_destination(
    destinations: &BTreeMap<HostHttpDestinationKey, HostHttpDestination>,
    class: HostHttpClass,
    authority: &pi_ai::ResolvedHttpAuthority,
    addresses: &[IpAddr],
) -> Result<AuthorizedDestination, SessionHttpError> {
    let host = sandbox_host(authority.host())?;
    let destination = destinations
        .values()
        .find(|row| {
            row.class() == class
                && row.scheme() == sandbox_scheme(authority.scheme())
                && row.host() == &host
                && row.port() == authority.port()
        })
        .cloned()
        .ok_or(SessionHttpError::DestinationNotAuthorized)?;
    if addresses.is_empty() {
        return Err(SessionHttpError::DnsEmpty);
    }
    if addresses.iter().any(|address| {
        !destination
            .allowed_ip_scopes()
            .contains(&ip_scope(*address))
    }) {
        return Err(SessionHttpError::IpScopeDenied);
    }
    Ok(AuthorizedDestination {
        socket_addresses: addresses
            .iter()
            .map(|address| SocketAddr::new(*address, destination.port()))
            .collect::<Vec<_>>()
            .into_boxed_slice(),
        destination,
    })
}

pub async fn connect_pinned(
    request: RedirectRequest,
    authorized: &AuthorizedDestination,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<reqwest::Response, SessionHttpError> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none());
    if let pi_ai::ResolvedHttpHost::Dns(name) = request.authority.host() {
        builder = builder.resolve_to_addrs(name, &authorized.socket_addresses);
    }
    let client = builder.build().map_err(|_| SessionHttpError::Transport)?;
    send_raw(&client, &request, deadline, cancellation).await
}

pub async fn reauthorize_redirect(
    destinations: &BTreeMap<HostHttpDestinationKey, HostHttpDestination>,
    revision: PolicyRevision,
    resolver: &dyn DnsResolver,
    class: HostHttpClass,
    current: RedirectRequest,
    response: &reqwest::Response,
    hop: usize,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<(RedirectRequest, AuthorizedDestination), SessionHttpError> {
    if hop >= MAX_SESSION_HTTP_REDIRECTS {
        return Err(SessionHttpError::RedirectLimitExceeded);
    }
    let next = redirect_request(&current, response)?;
    let addresses =
        resolve_authority(revision, resolver, &next.authority, deadline, cancellation).await?;
    let authorized = authorize_destination(destinations, class, &next.authority, &addresses)
        .map_err(|_| SessionHttpError::RedirectDenied)?;
    Ok((next, authorized))
}

pub fn bounded_response_body(
    response: reqwest::Response,
    idle_timeout: Duration,
    cancellation: CancellationToken,
    max_response_bytes: NonZeroU64,
) -> pi_ai::HttpResponseBody {
    // The attempt timeout is a time-to-headers cap (applied by `send_raw` /
    // `resolve_authority`) and a per-chunk body idle bound: the body deadline
    // resets on every chunk, so an actively streaming SSE response is never
    // severed by a total deadline while a stalled body (no progress) still
    // fails instead of hanging forever.
    let stream = response.bytes_stream();
    Box::pin(futures_util::stream::unfold(
        (
            Box::pin(stream),
            0_u64,
            false,
            Instant::now() + idle_timeout,
        ),
        move |(mut stream, mut total, terminal, mut deadline)| {
            let cancellation = cancellation.clone();
            async move {
                if terminal {
                    return None;
                }
                tokio::select! {
                    () = cancellation.cancelled() => Some((Err(anyhow::Error::from(SessionHttpError::Cancelled)), (stream, total, true, deadline))),
                    () = tokio::time::sleep_until(deadline) => Some((Err(anyhow::Error::from(SessionHttpError::DeadlineExceeded)), (stream, total, true, deadline))),
                    item = stream.next() => match item {
                        None => None,
                        Some(Err(_)) => Some((Err(anyhow::Error::from(SessionHttpError::Transport)), (stream, total, true, deadline))),
                        Some(Ok(bytes)) => match total.checked_add(bytes.len() as u64) {
                            Some(next) if next <= max_response_bytes.get() => {
                                total = next;
                                deadline = Instant::now() + idle_timeout;
                                Some((Ok(bytes.to_vec()), (stream, total, false, deadline)))
                            },
                            _ => Some((Err(anyhow::Error::from(SessionHttpError::ResponseTooLarge)), (stream, total, true, deadline))),
                        },
                    }
                }
            }
        },
    ))
}

async fn resolve_authority(
    revision: PolicyRevision,
    resolver: &dyn DnsResolver,
    authority: &pi_ai::ResolvedHttpAuthority,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<Vec<IpAddr>, SessionHttpError> {
    match sandbox_host(authority.host())? {
        HostHttpHost::Ip { address } => Ok(vec![address]),
        HostHttpHost::Dns { name } => {
            tokio::select! {
                () = cancellation.cancelled() => Err(SessionHttpError::Cancelled),
                () = tokio::time::sleep_until(deadline) => Err(SessionHttpError::DeadlineExceeded),
                result = resolver.resolve(revision, &name, cancellation) => result.map(|value| value.addresses().to_vec()),
            }
        }
    }
}

async fn send_raw(
    client: &reqwest::Client,
    request: &RedirectRequest,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<reqwest::Response, SessionHttpError> {
    let url = request_url(&request.authority, &request.path_and_query)?;
    let mut builder = client.request(reqwest_method(request.method), url);
    for (name, value) in &request.headers {
        builder = builder.header(name, value);
    }
    if !request.body.is_empty() {
        builder = builder.body(request.body.clone());
    }
    tokio::select! {
        () = cancellation.cancelled() => Err(SessionHttpError::Cancelled),
        () = tokio::time::sleep_until(deadline) => Err(SessionHttpError::DeadlineExceeded),
        result = builder.send() => result.map_err(|_| SessionHttpError::Transport),
    }
}

fn redirect_request(
    current: &RedirectRequest,
    response: &reqwest::Response,
) -> Result<RedirectRequest, SessionHttpError> {
    if !is_redirect_status(response.status()) {
        return Err(SessionHttpError::RedirectDenied);
    }
    let location = response
        .headers()
        .get(http::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(SessionHttpError::RedirectDenied)?;
    let target = request_url(&current.authority, &current.path_and_query)?
        .join(location)
        .map_err(|_| SessionHttpError::RedirectDenied)?;
    if target.fragment().is_some() || !target.username().is_empty() || target.password().is_some() {
        return Err(SessionHttpError::RedirectDenied);
    }
    let authority = authority_from_url(&target)?;
    let authority_changed = authority != current.authority;
    let mut method = current.method;
    let mut body = current.body.clone();
    if response.status() == http::StatusCode::SEE_OTHER
        || ((response.status() == http::StatusCode::MOVED_PERMANENTLY
            || response.status() == http::StatusCode::FOUND)
            && current.method == pi_ai::HttpMethod::Post)
    {
        method = pi_ai::HttpMethod::Get;
        body.clear();
    }
    let mut headers = current.headers.clone();
    if authority_changed {
        headers.retain(|(name, _)| !is_secret_header(name));
    }
    if body.is_empty() {
        headers.retain(|(name, _)| {
            name != http::header::CONTENT_TYPE && name != http::header::CONTENT_ENCODING
        });
    }
    Ok(RedirectRequest {
        method,
        authority,
        path_and_query: path_and_query(&target),
        headers,
        body,
    })
}

fn request_url(
    authority: &pi_ai::ResolvedHttpAuthority,
    target: &str,
) -> Result<url::Url, SessionHttpError> {
    let scheme = match authority.scheme() {
        pi_ai::ResolvedHttpScheme::Http => "http",
        pi_ai::ResolvedHttpScheme::Https => "https",
    };
    let host = match authority.host() {
        pi_ai::ResolvedHttpHost::Dns(name) => name.clone(),
        pi_ai::ResolvedHttpHost::Ip(IpAddr::V4(address)) => address.to_string(),
        pi_ai::ResolvedHttpHost::Ip(IpAddr::V6(address)) => format!("[{address}]"),
    };
    url::Url::parse(&format!("{scheme}://{host}:{}{target}", authority.port()))
        .map_err(|_| SessionHttpError::InvalidRequest)
}

fn authority_from_url(url: &url::Url) -> Result<pi_ai::ResolvedHttpAuthority, SessionHttpError> {
    let scheme = match url.scheme() {
        "http" => pi_ai::ResolvedHttpScheme::Http,
        "https" => pi_ai::ResolvedHttpScheme::Https,
        _ => return Err(SessionHttpError::RedirectDenied),
    };
    let host = match url.host().ok_or(SessionHttpError::RedirectDenied)? {
        url::Host::Domain(name) => pi_ai::ResolvedHttpHost::Dns(name.to_ascii_lowercase()),
        url::Host::Ipv4(address) => pi_ai::ResolvedHttpHost::Ip(address.into()),
        url::Host::Ipv6(address) => pi_ai::ResolvedHttpHost::Ip(address.into()),
    };
    pi_ai::ResolvedHttpAuthority::new(
        scheme,
        host,
        url.port_or_known_default()
            .ok_or(SessionHttpError::RedirectDenied)?,
    )
    .map_err(|_| SessionHttpError::RedirectDenied)
}

fn path_and_query(url: &url::Url) -> String {
    url.query().map_or_else(
        || url.path().to_owned(),
        |query| format!("{}?{query}", url.path()),
    )
}

fn model_endpoint_is_operator_owned(model: &pi_ai::Model) -> bool {
    if pi_ai::is_extension_provider(&model.api) {
        return false;
    }
    pi_ai::get_model(&model.provider, &model.id)
        .is_some_and(|catalog| catalog.api == model.api && catalog.base_url == model.base_url)
}

fn image_endpoint_is_operator_owned(image_model: &pi_ai::Model, base_url: Option<&str>) -> bool {
    if base_url.map(str::trim).is_some_and(|url| !url.is_empty()) {
        return false;
    }
    model_endpoint_is_operator_owned(image_model)
}

fn destination_from_resolved(
    class: HostHttpClass,
    authority: &pi_ai::ResolvedHttpAuthority,
    operator_owned: bool,
) -> Result<Option<HostHttpDestination>, SessionHttpError> {
    let host = sandbox_host(authority.host())?;
    let scopes = match host {
        HostHttpHost::Ip { address } => vec![ip_scope(address)],
        HostHttpHost::Dns { .. } => vec![IpScope::Global],
    };
    if !operator_owned && scopes.iter().any(|scope| *scope != IpScope::Global) {
        return Ok(None);
    }
    HostHttpDestination::new(
        class,
        sandbox_scheme(authority.scheme()),
        host,
        authority.port(),
        scopes,
    )
    .map(Some)
    .map_err(|_| SessionHttpError::InvalidRequest)
}

fn oauth_refresh_destination(
    provider: &str,
) -> Result<Option<HostHttpDestination>, SessionHttpError> {
    let host = match provider {
        "anthropic" => Some("platform.claude.com"),
        "openai-codex" => Some("auth.openai.com"),
        "google-gemini-cli" => Some("oauth2.googleapis.com"),
        "xai" => Some("auth.x.ai"),
        "kimi-coding" => Some("auth.kimi.com"),
        "openrouter" => None,
        _ => None,
    };
    host.map(|host| {
        HostHttpDestination::new(
            HostHttpClass::AuthOauth,
            HostHttpScheme::Https,
            HostHttpHost::Dns {
                name: DnsName::parse(host.to_owned())
                    .map_err(|_| SessionHttpError::InvalidRequest)?,
            },
            443,
            vec![IpScope::Global],
        )
        .map_err(|_| SessionHttpError::InvalidRequest)
    })
    .transpose()
}

fn sandbox_scheme(scheme: pi_ai::ResolvedHttpScheme) -> HostHttpScheme {
    match scheme {
        pi_ai::ResolvedHttpScheme::Http => HostHttpScheme::Http,
        pi_ai::ResolvedHttpScheme::Https => HostHttpScheme::Https,
    }
}

fn sandbox_host(host: &pi_ai::ResolvedHttpHost) -> Result<HostHttpHost, SessionHttpError> {
    match host {
        pi_ai::ResolvedHttpHost::Dns(name) => {
            let name = name.to_ascii_lowercase();
            if name == "localhost" || name.ends_with(".localhost") {
                Ok(HostHttpHost::Ip {
                    address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                })
            } else {
                Ok(HostHttpHost::Dns {
                    name: DnsName::parse(name).map_err(|_| SessionHttpError::InvalidRequest)?,
                })
            }
        }
        pi_ai::ResolvedHttpHost::Ip(address) => Ok(HostHttpHost::Ip { address: *address }),
    }
}

fn reqwest_method(method: pi_ai::HttpMethod) -> reqwest::Method {
    match method {
        pi_ai::HttpMethod::Get => reqwest::Method::GET,
        pi_ai::HttpMethod::Post => reqwest::Method::POST,
        pi_ai::HttpMethod::Put => reqwest::Method::PUT,
        pi_ai::HttpMethod::Patch => reqwest::Method::PATCH,
        pi_ai::HttpMethod::Delete => reqwest::Method::DELETE,
    }
}

fn is_redirect_status(status: http::StatusCode) -> bool {
    matches!(
        status,
        http::StatusCode::MOVED_PERMANENTLY
            | http::StatusCode::FOUND
            | http::StatusCode::SEE_OTHER
            | http::StatusCode::TEMPORARY_REDIRECT
            | http::StatusCode::PERMANENT_REDIRECT
    )
}

fn is_secret_header(name: &http::HeaderName) -> bool {
    matches!(
        name.as_str(),
        "authorization"
            | "proxy-authorization"
            | "cookie"
            | "cookie2"
            | "x-api-key"
            | "x-goog-api-key"
            | "api-key"
    ) || name.as_str().contains("api-key")
        || name.as_str().contains("authorization")
}

fn ip_scope(address: IpAddr) -> IpScope {
    if let IpAddr::V6(address) = address
        && let Some(mapped) = address.to_ipv4_mapped()
    {
        return ip_scope(IpAddr::V4(mapped));
    }
    if address.is_loopback() {
        IpScope::Loopback
    } else if match address {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_unicast_link_local(),
    } {
        IpScope::LinkLocal
    } else if match address {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.octets()[0] == 0
                || (v4.octets()[0] == 100 && (64..=127).contains(&v4.octets()[1]))
                || (v4 >= Ipv4Addr::new(198, 18, 0, 0) && v4 <= Ipv4Addr::new(198, 19, 255, 255))
        }
        IpAddr::V6(v6) => {
            v6.is_unique_local()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xffc0 == 0xfec0)
                || (v6.segments()[0] == 0x2001 && v6.segments()[1] == 0x0db8)
        }
    } {
        IpScope::Private
    } else {
        IpScope::Global
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::{API_OPENAI_COMPLETIONS, Model, StreamOptions};

    fn openai_model() -> Model {
        // `resolve_provider_http_authorities` dispatches through the global
        // provider registry; register the builtin providers (idempotent Once)
        // so these tests are order-independent within the lib binary.
        pi_ai::providers::register_builtins();
        Model {
            id: "gpt-5".to_owned(),
            name: "gpt-5".to_owned(),
            api: API_OPENAI_COMPLETIONS.to_owned(),
            provider: "openai".to_owned(),
            base_url: "https://api.openai.com/v1".to_owned(),
            ..Model::default()
        }
    }

    fn host_str(host: &HostHttpHost) -> String {
        match host {
            HostHttpHost::Dns { name } => name.as_str().to_owned(),
            HostHttpHost::Ip { address } => address.to_string(),
        }
    }

    /// Asserts an exact destination is present in the derived policy.
    fn assert_destination(
        destinations: &[HostHttpDestination],
        class: HostHttpClass,
        scheme: HostHttpScheme,
        host: &str,
        port: u16,
        scope: IpScope,
    ) {
        let found = destinations
            .iter()
            .find(|row| row.class() == class)
            .unwrap_or_else(|| {
                panic!("derived policy missing destination for {class:?}");
            });
        assert_eq!(found.scheme(), scheme, "{class:?} scheme");
        assert_eq!(host_str(found.host()), host.to_owned(), "{class:?} host");
        assert_eq!(found.port(), port, "{class:?} port");
        assert!(
            found.allowed_ip_scopes().contains(&scope),
            "{class:?} must allow scope {scope:?}, got {:?}",
            found.allowed_ip_scopes()
        );
    }

    #[test]
    fn derive_enumerates_exact_static_builtin_destinations() {
        let model = openai_model();
        let requirements = SessionHttpRequirements::derive_with_builtins(
            &model,
            &crate::RequestAuth::default(),
            &StreamOptions::default(),
            None,
            &BuiltinHttpEndpoints::default(),
        )
        .expect("derive builtins");
        // Default (no operator hostHttp) uses the derived requirements as-is.
        let policy = resolve_session_http_policy(None, &requirements).expect("default policy");
        let destinations = policy.destinations();
        // The provider API destination is still derived.
        assert_destination(
            destinations,
            HostHttpClass::ProviderApi,
            HostHttpScheme::Https,
            "api.openai.com",
            443,
            IpScope::Global,
        );
        // Every static built-in product destination is enumerated with the
        // exact fixed host/scheme/port/scope so a confined workspace-write
        // session can authorize each built-in tool out of the box.
        assert_destination(
            destinations,
            HostHttpClass::WebSearch,
            HostHttpScheme::Https,
            "api.exa.ai",
            443,
            IpScope::Global,
        );
        assert_destination(
            destinations,
            HostHttpClass::GithubApi,
            HostHttpScheme::Https,
            "api.github.com",
            443,
            IpScope::Global,
        );
        assert_destination(
            destinations,
            HostHttpClass::PluginArchive,
            HostHttpScheme::Https,
            "codeload.github.com",
            443,
            IpScope::Global,
        );
        assert_destination(
            destinations,
            HostHttpClass::NpmRegistry,
            HostHttpScheme::Https,
            "registry.npmjs.org",
            443,
            IpScope::Global,
        );
        assert_destination(
            destinations,
            HostHttpClass::NpmTarball,
            HostHttpScheme::Https,
            "registry.npmjs.org",
            443,
            IpScope::Global,
        );
        assert_destination(
            destinations,
            HostHttpClass::LlamaHuggingFace,
            HostHttpScheme::Https,
            "huggingface.co",
            443,
            IpScope::Global,
        );
        assert_destination(
            destinations,
            HostHttpClass::LlamaGguf,
            HostHttpScheme::Https,
            "huggingface.co",
            443,
            IpScope::Global,
        );
    }

    #[test]
    fn derive_omits_unset_dynamic_destinations() {
        // With no configured dynamic endpoints, the dynamic classes are
        // omitted (not present as requirements), so a default policy never
        // forces an operator to authorize an endpoint they did not configure.
        let model = openai_model();
        let requirements = SessionHttpRequirements::derive_with_builtins(
            &model,
            &crate::RequestAuth::default(),
            &StreamOptions::default(),
            None,
            &BuiltinHttpEndpoints::default(),
        )
        .expect("derive builtins");
        let policy = resolve_session_http_policy(None, &requirements).expect("default policy");
        for class in [
            HostHttpClass::MemoryApi,
            HostHttpClass::LiveStt,
            HostHttpClass::RealtimeApi,
            HostHttpClass::LlamaRouter,
            HostHttpClass::PluginMarketplace,
        ] {
            assert!(
                !policy.destinations().iter().any(|row| row.class() == class),
                "unset dynamic destination {class:?} must not be derived"
            );
        }
    }

    #[test]
    fn derive_dynamic_destinations_use_exact_configured_scope() {
        let model = openai_model();
        let builtins = BuiltinHttpEndpoints {
            memory_api_url: Some("https://memory.example.test/api".to_owned()),
            live_stt_url: Some("http://127.0.0.1:8080".to_owned()),
            llama_router_url: Some("http://localhost:11434".to_owned()),
            hugging_face_url: Some("https://hf.example.test".to_owned()),
            ..BuiltinHttpEndpoints::default()
        };
        let requirements = SessionHttpRequirements::derive_with_builtins(
            &model,
            &crate::RequestAuth::default(),
            &StreamOptions::default(),
            None,
            &builtins,
        )
        .expect("derive builtins");
        let policy = resolve_session_http_policy(None, &requirements).expect("default policy");
        let destinations = policy.destinations();
        // A public DNS endpoint derives a global-scope HTTPS destination.
        assert_destination(
            destinations,
            HostHttpClass::MemoryApi,
            HostHttpScheme::Https,
            "memory.example.test",
            443,
            IpScope::Global,
        );
        // A loopback IP endpoint derives a loopback-scope HTTP destination so a
        // self-hosted llama.cpp router or whisper STT server is authorized.
        assert_destination(
            destinations,
            HostHttpClass::LiveStt,
            HostHttpScheme::Http,
            "127.0.0.1",
            8080,
            IpScope::Loopback,
        );
        // A loopback hostname endpoint derives a loopback-scope HTTP
        // destination so a self-hosted llama.cpp router or whisper STT server
        // is authorized; `localhost` is represented as the canonical loopback
        // IP literal because DNS names reject `localhost` by design.
        assert_destination(
            destinations,
            HostHttpClass::LlamaRouter,
            HostHttpScheme::Http,
            "127.0.0.1",
            11434,
            IpScope::Loopback,
        );
        // The Hugging Face endpoint override applies to both HF classes.
        assert_destination(
            destinations,
            HostHttpClass::LlamaHuggingFace,
            HostHttpScheme::Https,
            "hf.example.test",
            443,
            IpScope::Global,
        );
        assert_destination(
            destinations,
            HostHttpClass::LlamaGguf,
            HostHttpScheme::Https,
            "hf.example.test",
            443,
            IpScope::Global,
        );
    }

    #[test]
    fn derive_with_image_generation_stays_minimal() {
        // The non-builtin derivation path keeps its minimal contract (provider
        // only) so existing callers/tests that do not opt into builtins are
        // unaffected — built-in destinations are layered on only by the
        // production derive_with_builtins path.
        let model = openai_model();
        let requirements = SessionHttpRequirements::derive_with_image_generation(
            &model,
            &crate::RequestAuth::default(),
            &StreamOptions::default(),
            None,
        )
        .expect("derive");
        let policy = resolve_session_http_policy(None, &requirements).expect("default policy");
        assert_eq!(
            policy.destinations().len(),
            1,
            "minimal derive is provider-only"
        );
        assert_eq!(policy.destinations()[0].class(), HostHttpClass::ProviderApi);
    }

    #[test]
    fn operator_owned_localhost_provider_is_exact_loopback_and_untrusted_is_excluded() {
        pi_ai::providers::register_builtins();
        let model = Model {
            id: "local-openai".to_owned(),
            name: "local-openai".to_owned(),
            api: API_OPENAI_COMPLETIONS.to_owned(),
            provider: "local-openai".to_owned(),
            base_url: "http://localhost:8317/v1".to_owned(),
            ..Model::default()
        };
        let key = (model.provider.clone(), model.id.clone());
        pi_ai::replace_registered_models(&[], vec![model.clone()]);
        let requirements = SessionHttpRequirements::derive(
            &model,
            &crate::RequestAuth::default(),
            &StreamOptions::default(),
        );
        pi_ai::replace_registered_models(&[key], Vec::new());
        let policy =
            resolve_session_http_policy(None, &requirements.expect("derive localhost provider"))
                .expect("default localhost provider policy");
        let destination = policy
            .destinations()
            .iter()
            .find(|row| row.class() == HostHttpClass::ProviderApi)
            .expect("localhost provider destination");
        assert_eq!(destination.scheme(), HostHttpScheme::Http);
        assert_eq!(
            destination.host(),
            &HostHttpHost::Ip {
                address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            }
        );
        assert_eq!(destination.port(), 8317);
        assert_eq!(destination.allowed_ip_scopes(), &[IpScope::Loopback]);

        let authority = pi_ai::ResolvedHttpAuthority::new(
            pi_ai::ResolvedHttpScheme::Http,
            pi_ai::ResolvedHttpHost::Dns("localhost".to_owned()),
            8317,
        )
        .expect("localhost authority");
        assert!(
            destination_from_resolved(HostHttpClass::ProviderApi, &authority, false)
                .expect("untrusted localhost destination")
                .is_none(),
            "localhost must not auto-stage without operator ownership"
        );
    }

    #[tokio::test]
    async fn connect_pinned_dns_localhost_uses_authorized_loopback_socket() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind((Ipv4Addr::new(127, 0, 0, 2), 0))
            .await
            .expect("bind pinned loopback server");
        let pinned = listener.local_addr().expect("pinned loopback address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept pinned request");
            let mut request = vec![0_u8; 4096];
            let read = stream
                .read(&mut request)
                .await
                .expect("read pinned request");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\npinned",
                )
                .await
                .expect("write pinned response");
            String::from_utf8(request[..read].to_vec()).expect("request is UTF-8")
        });
        let authority = pi_ai::ResolvedHttpAuthority::new(
            pi_ai::ResolvedHttpScheme::Http,
            pi_ai::ResolvedHttpHost::Dns("localhost".to_owned()),
            pinned.port(),
        )
        .expect("localhost authority");
        let destination = destination_from_resolved(HostHttpClass::ProviderApi, &authority, true)
            .expect("localhost destination")
            .expect("operator-owned localhost is staged");
        let authorized = AuthorizedDestination {
            destination,
            socket_addresses: vec![pinned].into_boxed_slice(),
        };
        let response = connect_pinned(
            RedirectRequest {
                method: pi_ai::HttpMethod::Get,
                authority,
                path_and_query: "/pinned".to_owned(),
                headers: Vec::new(),
                body: Vec::new(),
            },
            &authorized,
            Instant::now() + Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .await
        .expect("connect through pinned localhost authority");
        assert_eq!(response.status(), http::StatusCode::OK);
        let request = server.await.expect("join pinned server");
        assert!(request.starts_with("GET /pinned HTTP/1.1\r\n"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains(&format!("host: localhost:{}", pinned.port()))
        );
    }

    #[test]
    fn ipv4_mapped_ipv6_keeps_embedded_scope() {
        assert_eq!(
            ip_scope(IpAddr::V6(Ipv4Addr::LOCALHOST.to_ipv6_mapped())),
            IpScope::Loopback,
        );
        assert_eq!(
            ip_scope(IpAddr::V6(Ipv4Addr::new(10, 0, 0, 1).to_ipv6_mapped())),
            IpScope::Private,
        );
        assert_eq!(
            ip_scope(IpAddr::V6(Ipv4Addr::new(169, 254, 1, 1).to_ipv6_mapped())),
            IpScope::LinkLocal,
        );
        assert_eq!(
            ip_scope(IpAddr::V6(Ipv4Addr::new(8, 8, 8, 8).to_ipv6_mapped())),
            IpScope::Global,
        );
    }

    fn private_cliproxy_model() -> Model {
        pi_ai::providers::register_builtins();
        Model {
            id: "deepseek-v4-flash".to_owned(),
            name: "deepseek-v4-flash".to_owned(),
            api: API_OPENAI_COMPLETIONS.to_owned(),
            provider: "cliproxy".to_owned(),
            base_url: "http://172.16.2.8:8317/v1".to_owned(),
            ..Model::default()
        }
    }

    fn assert_ip_destination(
        destination: &HostHttpDestination,
        address: IpAddr,
        port: u16,
        scope: IpScope,
    ) {
        assert_eq!(destination.host(), &HostHttpHost::Ip { address });
        assert_eq!(destination.port(), port);
        assert_eq!(destination.allowed_ip_scopes(), &[scope]);
    }

    #[test]
    fn operator_owned_literal_scopes_are_exact() {
        let cases = [
            (
                IpAddr::V4(Ipv4Addr::new(192, 168, 0, 76)),
                8317,
                IpScope::Private,
            ),
            (IpAddr::V4(Ipv4Addr::LOCALHOST), 8317, IpScope::Loopback),
            (
                IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1)),
                8080,
                IpScope::LinkLocal,
            ),
            (
                IpAddr::V6(Ipv4Addr::new(192, 168, 0, 76).to_ipv6_mapped()),
                8317,
                IpScope::Private,
            ),
            (
                IpAddr::V6(Ipv4Addr::LOCALHOST.to_ipv6_mapped()),
                8317,
                IpScope::Loopback,
            ),
            (
                IpAddr::V6(Ipv4Addr::new(169, 254, 1, 1).to_ipv6_mapped()),
                8080,
                IpScope::LinkLocal,
            ),
        ];
        for (address, port, scope) in cases {
            let authority = pi_ai::ResolvedHttpAuthority::new(
                pi_ai::ResolvedHttpScheme::Http,
                pi_ai::ResolvedHttpHost::Ip(address),
                port,
            )
            .expect("authority");
            let destination =
                destination_from_resolved(HostHttpClass::ProviderApi, &authority, true)
                    .expect("operator-owned destination")
                    .expect("operator-owned nonglobal IP is staged");
            assert_ip_destination(&destination, address, port, scope);
            assert!(
                destination_from_resolved(HostHttpClass::ProviderApi, &authority, false)
                    .expect("untrusted destination")
                    .is_none(),
                "{address} must not auto-stage without operator ownership"
            );
        }
    }

    #[test]
    fn catalog_private_provider_stages_exact_private_scope() {
        let model = private_cliproxy_model();
        let key = (model.provider.clone(), model.id.clone());
        pi_ai::replace_registered_models(&[], vec![model.clone()]);
        let requirements = SessionHttpRequirements::derive(
            &model,
            &crate::RequestAuth::default(),
            &StreamOptions::default(),
        );
        pi_ai::replace_registered_models(&[key], Vec::new());
        let requirements = requirements.expect("derive catalog private provider");
        let policy = resolve_session_http_policy(None, &requirements).expect("omitted hostHttp");
        let found = policy
            .destinations()
            .iter()
            .find(|row| row.class() == HostHttpClass::ProviderApi)
            .expect("catalog private provider is required");
        assert_ip_destination(
            found,
            IpAddr::V4(Ipv4Addr::new(172, 16, 2, 8)),
            8317,
            IpScope::Private,
        );
    }

    #[test]
    fn untrusted_private_provider_requires_explicit_host_http() {
        let mut model = private_cliproxy_model();
        // Distinct from the catalog test so a parallel registration cannot
        // make this foreign endpoint look operator-owned.
        model.id = "deepseek-v4-flash-foreign".to_owned();
        let requirements = SessionHttpRequirements::derive(
            &model,
            &crate::RequestAuth::default(),
            &StreamOptions::default(),
        )
        .expect("untrusted private derive omits rather than failing");
        assert!(
            !requirements
                .destinations
                .values()
                .any(|row| row.class() == HostHttpClass::ProviderApi),
            "project/foreign/plugin private endpoints must not auto-stage"
        );
        let omitted = resolve_session_http_policy(None, &requirements).expect("omitted hostHttp");
        assert!(
            !omitted
                .destinations()
                .iter()
                .any(|row| row.class() == HostHttpClass::ProviderApi),
            "omitted hostHttp must not invent a private provider destination"
        );
        let granted = HostHttpPolicySettings {
            destinations: vec![crate::settings::HostHttpDestinationWire {
                class: HostHttpClass::ProviderApi,
                scheme: HostHttpScheme::Http,
                host: HostHttpHostWire::Ip {
                    address: IpAddr::V4(Ipv4Addr::new(192, 168, 0, 76)),
                },
                port: 8317,
                allowed_ip_scopes: vec![IpScope::Private],
            }],
        };
        let policy = resolve_session_http_policy(Some(&granted), &requirements)
            .expect("explicit hostHttp grants the untrusted private endpoint");
        let found = policy
            .destinations()
            .iter()
            .find(|row| row.class() == HostHttpClass::ProviderApi)
            .expect("explicit hostHttp destination");
        assert_ip_destination(
            found,
            IpAddr::V4(Ipv4Addr::new(192, 168, 0, 76)),
            8317,
            IpScope::Private,
        );
    }

    #[test]
    fn project_image_override_does_not_stage_private_literal() {
        let model = openai_model();
        let image = Model {
            id: "local-image".to_owned(),
            name: "local-image".to_owned(),
            api: pi_ai::API_IMAGE_GEN.to_owned(),
            provider: "project-images".to_owned(),
            base_url: "http://172.16.2.9:8080/v1".to_owned(),
            image_generation: true,
            ..Model::default()
        };
        let env = std::collections::HashMap::new();
        let requirements = SessionHttpRequirements::derive_with_image_generation(
            &model,
            &crate::RequestAuth::default(),
            &StreamOptions::default(),
            Some((&image, Some("http://172.16.2.9:8080/v1"), &env)),
        )
        .expect("image override derive");
        assert!(
            !requirements
                .destinations
                .values()
                .any(|row| row.class() == HostHttpClass::ImageGeneration),
            "project/foreign image baseUrl must require explicit hostHttp"
        );
    }
}
