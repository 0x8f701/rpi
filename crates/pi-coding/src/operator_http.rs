use std::fmt::{self, Debug, Formatter};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use http::StatusCode;

pub const OPERATOR_REQUEST_BODY_LIMIT: usize = 1024 * 1024;
pub const OPERATOR_RADIUS_RESPONSE_LIMIT: usize = 1024 * 1024;
pub const OPERATOR_JSON_RESPONSE_LIMIT: usize = 8 * 1024 * 1024;
pub const OPERATOR_ASSET_RESPONSE_LIMIT: usize = 256 * 1024 * 1024;
pub const OPERATOR_HTTP_TIMEOUT: Duration = Duration::from_secs(120);
const RPI_UPDATE_BASE_URL_ENV: &str = "RPI_UPDATE_BASE_URL";

pub struct SecretValue(String);
impl SecretValue {
    pub fn new(value: String) -> Result<Self, OperatorHttpError> {
        if value.is_empty() || value.len() > OPERATOR_REQUEST_BODY_LIMIT || value.contains('\0') {
            return Err(OperatorHttpError::Protocol);
        }
        Ok(Self(value))
    }
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}
impl Debug for SecretValue {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretValue([REDACTED])")
    }
}

pub enum AnthropicTokenBody {
    AuthorizationCode {
        code: SecretValue,
        state: SecretValue,
        code_verifier: SecretValue,
    },
    RefreshToken {
        refresh_token: SecretValue,
    },
}
impl AnthropicTokenBody {
    pub fn authorization_code(
        code: SecretValue,
        state: SecretValue,
        code_verifier: SecretValue,
    ) -> Result<Self, OperatorHttpError> {
        Ok(Self::AuthorizationCode {
            code,
            state,
            code_verifier,
        })
    }
    pub fn refresh(refresh_token: SecretValue) -> Result<Self, OperatorHttpError> {
        Ok(Self::RefreshToken { refresh_token })
    }
}

pub struct CodexDevicePollBody {
    device_auth_id: SecretValue,
    user_code: SecretValue,
}
impl CodexDevicePollBody {
    pub fn new(
        device_auth_id: SecretValue,
        user_code: SecretValue,
    ) -> Result<Self, OperatorHttpError> {
        Ok(Self {
            device_auth_id,
            user_code,
        })
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodexTokenFlow {
    AuthorizationCode,
    RefreshToken,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodexRedirectKind {
    Browser,
    Device,
}
pub struct CodexTokenBody {
    flow: CodexTokenFlow,
    code_or_refresh: SecretValue,
    code_verifier: Option<SecretValue>,
    redirect_kind: Option<CodexRedirectKind>,
}
impl CodexTokenBody {
    pub fn authorization_code(
        code: SecretValue,
        code_verifier: SecretValue,
        redirect: CodexRedirectKind,
    ) -> Result<Self, OperatorHttpError> {
        Ok(Self {
            flow: CodexTokenFlow::AuthorizationCode,
            code_or_refresh: code,
            code_verifier: Some(code_verifier),
            redirect_kind: Some(redirect),
        })
    }
    pub fn refresh(refresh_token: SecretValue) -> Result<Self, OperatorHttpError> {
        Ok(Self {
            flow: CodexTokenFlow::RefreshToken,
            code_or_refresh: refresh_token,
            code_verifier: None,
            redirect_kind: None,
        })
    }
}

pub enum GoogleTokenBody {
    AuthorizationCode {
        code: SecretValue,
        code_verifier: SecretValue,
    },
    RefreshToken {
        refresh_token: SecretValue,
    },
}
impl GoogleTokenBody {
    pub fn authorization_code(
        code: SecretValue,
        code_verifier: SecretValue,
    ) -> Result<Self, OperatorHttpError> {
        Ok(Self::AuthorizationCode {
            code,
            code_verifier,
        })
    }
    pub fn refresh(refresh_token: SecretValue) -> Result<Self, OperatorHttpError> {
        Ok(Self::RefreshToken { refresh_token })
    }
}

pub struct GoogleProjectId(String);
pub struct GoogleTierId(String);
impl GoogleProjectId {
    pub fn parse(value: String) -> Result<Self, OperatorHttpError> {
        validate_identifier(value, 128).map(Self)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl GoogleTierId {
    pub fn parse(value: String) -> Result<Self, OperatorHttpError> {
        validate_identifier(value, 128).map(Self)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
pub struct GoogleCodeAssistLoadBody {
    project: Option<GoogleProjectId>,
}
pub struct GoogleCodeAssistOnboardBody {
    tier: GoogleTierId,
    project: Option<GoogleProjectId>,
}
impl GoogleCodeAssistLoadBody {
    pub fn new(project: Option<GoogleProjectId>) -> Self {
        Self { project }
    }
}
impl GoogleCodeAssistOnboardBody {
    pub fn new(tier: GoogleTierId, project: Option<GoogleProjectId>) -> Self {
        Self { tier, project }
    }
}

pub struct XaiDeviceCodeBody;
pub struct XaiTokenBody {
    device_code: SecretValue,
}
pub struct XaiRefreshBody {
    refresh_token: SecretValue,
}
impl XaiTokenBody {
    pub fn new(device_code: SecretValue) -> Result<Self, OperatorHttpError> {
        Ok(Self { device_code })
    }
}
impl XaiRefreshBody {
    pub fn new(refresh_token: SecretValue) -> Result<Self, OperatorHttpError> {
        Ok(Self { refresh_token })
    }
}
pub struct KimiDeviceCodeBody;
pub struct KimiTokenBody {
    device_code: SecretValue,
}
pub struct KimiRefreshBody {
    refresh_token: SecretValue,
}
impl KimiTokenBody {
    pub fn new(device_code: SecretValue) -> Result<Self, OperatorHttpError> {
        Ok(Self { device_code })
    }
}
impl KimiRefreshBody {
    pub fn new(refresh_token: SecretValue) -> Result<Self, OperatorHttpError> {
        Ok(Self { refresh_token })
    }
}
pub struct OpenRouterKeyExchangeBody {
    code: SecretValue,
    code_verifier: SecretValue,
}
impl OpenRouterKeyExchangeBody {
    pub fn new(code: SecretValue, code_verifier: SecretValue) -> Result<Self, OperatorHttpError> {
        Ok(Self {
            code,
            code_verifier,
        })
    }
}

pub struct GoogleOperationName(String);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReleaseChannel {
    Stable,
    Prerelease,
}
impl GoogleOperationName {
    pub fn parse(value: String) -> Result<Self, OperatorHttpError> {
        let suffix = value
            .strip_prefix("operations/")
            .ok_or(OperatorHttpError::Protocol)?;
        if suffix.is_empty()
            || suffix.len() > 200
            || !suffix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._~-".contains(&b))
        {
            return Err(OperatorHttpError::Protocol);
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
/// Exact release-asset download URL carried by [`OperatorHttpRequest::SelfUpdateAsset`].
///
/// Basic structural validation only (http/https scheme, non-empty host); the
/// transport re-validates the destination against the active self-update base
/// (or the fixed GitHub release download host) so a metadata endpoint can
/// never redirect an asset fetch to an arbitrary authority.
pub struct ReleaseDownloadUrl(url::Url);
impl ReleaseDownloadUrl {
    pub fn parse(value: String) -> Result<Self, OperatorHttpError> {
        let url = url::Url::parse(&value).map_err(|_| OperatorHttpError::DestinationNotCompiled)?;
        if !matches!(url.scheme(), "https" | "http") || url.host_str().map_or(true, str::is_empty) {
            return Err(OperatorHttpError::DestinationNotCompiled);
        }
        // The URL parser leniently rewrites `https:///no-host` into a
        // path-shaped host (`https://no-host/`), so additionally require the
        // literal input to carry an authority after `://`.
        let authority = value
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or("")
            .split(['/', '?', '#'])
            .next()
            .unwrap_or("");
        if authority.is_empty() {
            return Err(OperatorHttpError::DestinationNotCompiled);
        }
        Ok(Self(url))
    }
    pub(crate) fn as_url(&self) -> &url::Url {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperatorOperation {
    AnthropicToken,
    CodexDeviceCode,
    CodexDevicePoll,
    CodexToken,
    GoogleToken,
    GoogleCodeAssistLoad,
    GoogleCodeAssistOnboard,
    GoogleCodeAssistPoll,
    XaiDeviceCode,
    XaiToken,
    XaiRefresh,
    KimiDeviceCode,
    KimiToken,
    KimiRefresh,
    OpenRouterKeyExchange,
    RadiusCatalog,
    SelfUpdateMetadata,
    SelfUpdateAsset,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperatorResponseEncoding {
    Json,
    Form,
}
pub struct OperatorStructuredResponse {
    operation: OperatorOperation,
    status: StatusCode,
    encoding: OperatorResponseEncoding,
    body: Box<[u8]>,
}
impl OperatorStructuredResponse {
    pub const fn operation(&self) -> OperatorOperation {
        self.operation
    }
    pub const fn status(&self) -> StatusCode {
        self.status
    }
    pub const fn encoding(&self) -> OperatorResponseEncoding {
        self.encoding
    }
    pub fn body(&self) -> &[u8] {
        &self.body
    }
    pub fn into_body(self) -> Box<[u8]> {
        self.body
    }
}
impl Debug for OperatorStructuredResponse {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("OperatorStructuredResponse")
            .field("operation", &self.operation)
            .field("status", &self.status)
            .field("encoding", &self.encoding)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}
pub struct OperatorAssetResponse {
    operation: OperatorOperation,
    status: StatusCode,
    body: Box<[u8]>,
}
impl OperatorAssetResponse {
    pub const fn operation(&self) -> OperatorOperation {
        self.operation
    }
    pub const fn status(&self) -> StatusCode {
        self.status
    }
    pub fn body(&self) -> &[u8] {
        &self.body
    }
    pub(crate) fn into_body(self) -> Box<[u8]> {
        self.body
    }
}
impl Debug for OperatorAssetResponse {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("OperatorAssetResponse")
            .field("operation", &self.operation)
            .field("status", &self.status)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}
pub enum OperatorHttpResponse {
    Structured(OperatorStructuredResponse),
    Asset(OperatorAssetResponse),
}
impl Debug for OperatorHttpResponse {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Structured(value) => Debug::fmt(value, f),
            Self::Asset(value) => Debug::fmt(value, f),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OperatorHttpError {
    #[error("operator destination is not compiled")]
    DestinationNotCompiled,
    #[error("operator endpoint override is unsupported")]
    EndpointOverrideUnsupported,
    #[error("environment variable {legacy} is no longer supported; use {canonical} instead")]
    LegacyEnvironment {
        legacy: &'static str,
        canonical: &'static str,
    },
    #[error("operator redirect denied")]
    RedirectDenied,
    #[error("operator request body exceeds {limit} bytes")]
    RequestTooLarge { limit: usize },
    #[error("operator response exceeds {limit} bytes")]
    ResponseTooLarge { limit: usize },
    #[error("operator request timed out after {timeout_ms} ms")]
    Timeout { timeout_ms: u64 },
    #[error("operator request was cancelled")]
    Cancelled,
    #[error("operator HTTP protocol failure")]
    Protocol,
}
#[derive(Debug, thiserror::Error)]
pub enum InitialAuthError {
    #[error("initial authentication transport is unsupported for the selected provider")]
    TransportUnsupported,
    #[error("initial authentication operator request failed: {0}")]
    Operator(#[from] OperatorHttpError),
}

pub enum OperatorHttpRequest {
    AnthropicToken {
        body: AnthropicTokenBody,
    },
    CodexDeviceCode,
    CodexDevicePoll {
        body: CodexDevicePollBody,
    },
    CodexToken {
        body: CodexTokenBody,
    },
    GoogleToken {
        body: GoogleTokenBody,
    },
    GoogleCodeAssistLoad {
        body: GoogleCodeAssistLoadBody,
        bearer: SecretValue,
    },
    GoogleCodeAssistOnboard {
        body: GoogleCodeAssistOnboardBody,
        bearer: SecretValue,
    },
    GoogleCodeAssistPoll {
        operation: GoogleOperationName,
        bearer: SecretValue,
    },
    XaiDeviceCode {
        body: XaiDeviceCodeBody,
    },
    XaiToken {
        body: XaiTokenBody,
    },
    XaiRefresh {
        body: XaiRefreshBody,
    },
    KimiDeviceCode {
        body: KimiDeviceCodeBody,
    },
    KimiToken {
        body: KimiTokenBody,
    },
    KimiRefresh {
        body: KimiRefreshBody,
    },
    OpenRouterKeyExchange {
        body: OpenRouterKeyExchangeBody,
    },
    RadiusCatalog {
        bearer: SecretValue,
    },
    SelfUpdateMetadata {
        channel: ReleaseChannel,
    },
    SelfUpdateAsset {
        url: ReleaseDownloadUrl,
    },
}

impl OperatorHttpRequest {
    pub const fn operation(&self) -> OperatorOperation {
        match self {
            Self::AnthropicToken { .. } => OperatorOperation::AnthropicToken,
            Self::CodexDeviceCode => OperatorOperation::CodexDeviceCode,
            Self::CodexDevicePoll { .. } => OperatorOperation::CodexDevicePoll,
            Self::CodexToken { .. } => OperatorOperation::CodexToken,
            Self::GoogleToken { .. } => OperatorOperation::GoogleToken,
            Self::GoogleCodeAssistLoad { .. } => OperatorOperation::GoogleCodeAssistLoad,
            Self::GoogleCodeAssistOnboard { .. } => OperatorOperation::GoogleCodeAssistOnboard,
            Self::GoogleCodeAssistPoll { .. } => OperatorOperation::GoogleCodeAssistPoll,
            Self::XaiDeviceCode { .. } => OperatorOperation::XaiDeviceCode,
            Self::XaiToken { .. } => OperatorOperation::XaiToken,
            Self::XaiRefresh { .. } => OperatorOperation::XaiRefresh,
            Self::KimiDeviceCode { .. } => OperatorOperation::KimiDeviceCode,
            Self::KimiToken { .. } => OperatorOperation::KimiToken,
            Self::KimiRefresh { .. } => OperatorOperation::KimiRefresh,
            Self::OpenRouterKeyExchange { .. } => OperatorOperation::OpenRouterKeyExchange,
            Self::RadiusCatalog { .. } => OperatorOperation::RadiusCatalog,
            Self::SelfUpdateMetadata { .. } => OperatorOperation::SelfUpdateMetadata,
            Self::SelfUpdateAsset { .. } => OperatorOperation::SelfUpdateAsset,
        }
    }
}

impl Debug for OperatorHttpRequest {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("OperatorHttpRequest")
            .field("operation", &self.operation())
            .finish()
    }
}

pub struct OperatorHttpTransport {
    client: reqwest::Client,
    self_update_base: Option<url::Url>,
}

impl OperatorHttpTransport {
    /// Construct the operator transport.
    ///
    /// `RPI_UPDATE_BASE_URL` is the documented self-update override honored by
    /// `install.sh`/`install.ps1` and the Rust self-updater: a
    /// GitHub-Releases-shaped API base that re-points release metadata and
    /// asset downloads at a trusted mirror (notably the local deterministic
    /// release fixture used by the install/self-update E2E). It is parsed and
    /// pinned here once, so only the [`OperatorOperation::SelfUpdateMetadata`]
    /// and [`OperatorOperation::SelfUpdateAsset`] operations can honor it;
    /// every auth operation stays fail-closed against any endpoint override.
    ///
    /// The override must be `https`, or `http` bound to a loopback host
    /// (`127.0.0.1`, `localhost`, or `::1`); any other form is rejected so a
    /// misconfigured override can never widen authority silently. The
    /// operator-owned Kimi auth overrides (`KIMI_CODE_OAUTH_HOST`,
    /// `KIMI_OAUTH_HOST`) remain unsupported here.
    pub fn new() -> Result<Arc<Self>, OperatorHttpError> {
        if std::env::var_os("KIMI_CODE_OAUTH_HOST").is_some()
            || std::env::var_os("KIMI_OAUTH_HOST").is_some()
        {
            return Err(OperatorHttpError::EndpointOverrideUnsupported);
        }
        let self_update_base = read_self_update_base()?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| OperatorHttpError::Protocol)?;
        Ok(Arc::new(Self {
            client,
            self_update_base,
        }))
    }

    #[must_use]
    pub(crate) fn self_update_base(&self) -> Option<&url::Url> {
        self.self_update_base.as_ref()
    }

    pub async fn execute(
        &self,
        request: OperatorHttpRequest,
    ) -> Result<OperatorHttpResponse, OperatorHttpError> {
        let operation = request.operation();
        let deadline = tokio::time::Instant::now() + OPERATOR_HTTP_TIMEOUT;
        let compiled = compile_request(request, self.self_update_base.as_ref())?;
        if compiled.body.len() > OPERATOR_REQUEST_BODY_LIMIT {
            return Err(OperatorHttpError::RequestTooLarge {
                limit: OPERATOR_REQUEST_BODY_LIMIT,
            });
        }
        let mut builder = self
            .client
            .request(compiled.method, compiled.url)
            .header(http::header::ACCEPT, compiled.accept)
            .header(
                http::header::USER_AGENT,
                concat!("rpi/", env!("CARGO_PKG_VERSION")),
            );
        if let Some(content_type) = compiled.content_type {
            builder = builder.header(http::header::CONTENT_TYPE, content_type);
        }
        if let Some(bearer) = compiled.bearer {
            builder = builder.bearer_auth(bearer.expose());
        }
        if !compiled.body.is_empty() {
            builder = builder.body(compiled.body);
        }
        let mut response = tokio::time::timeout_at(deadline, builder.send())
            .await
            .map_err(|_| operator_timeout())?
            .map_err(|_| OperatorHttpError::Protocol)?;
        if operation == OperatorOperation::SelfUpdateAsset {
            for hop in 0..=2 {
                if !response.status().is_redirection() {
                    break;
                }
                if hop == 2 {
                    return Err(OperatorHttpError::RedirectDenied);
                }
                let location = response
                    .headers()
                    .get(http::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or(OperatorHttpError::RedirectDenied)?;
                let next = response
                    .url()
                    .join(location)
                    .map_err(|_| OperatorHttpError::RedirectDenied)?;
                // GitHub release downloads redirect to release-assets; a
                // self-update override may redirect only within its own
                // pinned origin. Anything else is denied.
                let github_assets = next.scheme() == "https"
                    && next.port_or_known_default() == Some(443)
                    && next.host_str() == Some("release-assets.githubusercontent.com");
                let same_origin = next.origin() == response.url().origin();
                if !github_assets && !same_origin {
                    return Err(OperatorHttpError::RedirectDenied);
                }
                response = tokio::time::timeout_at(
                    deadline,
                    self.client
                        .get(next)
                        .header(http::header::ACCEPT, "application/octet-stream")
                        .header(
                            http::header::USER_AGENT,
                            concat!("rpi/", env!("CARGO_PKG_VERSION")),
                        )
                        .send(),
                )
                .await
                .map_err(|_| operator_timeout())?
                .map_err(|_| OperatorHttpError::Protocol)?;
            }
        } else if response.status().is_redirection() {
            return Err(OperatorHttpError::RedirectDenied);
        }
        let status = response.status();
        let limit = match operation {
            OperatorOperation::SelfUpdateAsset => OPERATOR_ASSET_RESPONSE_LIMIT,
            OperatorOperation::RadiusCatalog => OPERATOR_RADIUS_RESPONSE_LIMIT,
            _ => OPERATOR_JSON_RESPONSE_LIMIT,
        };
        validate_content_type(&response, operation)?;
        let body = read_bounded(response, limit, deadline).await?;
        if operation == OperatorOperation::SelfUpdateAsset {
            return Ok(OperatorHttpResponse::Asset(OperatorAssetResponse {
                operation,
                status,
                body,
            }));
        }
        Ok(OperatorHttpResponse::Structured(
            OperatorStructuredResponse {
                operation,
                status,
                encoding: compiled.encoding,
                body,
            },
        ))
    }
}

struct CompiledOperatorRequest {
    method: reqwest::Method,
    url: url::Url,
    accept: &'static str,
    content_type: Option<&'static str>,
    encoding: OperatorResponseEncoding,
    bearer: Option<SecretValue>,
    body: Vec<u8>,
}

fn compile_request(
    request: OperatorHttpRequest,
    self_update_base: Option<&url::Url>,
) -> Result<CompiledOperatorRequest, OperatorHttpError> {
    const ANTHROPIC_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
    const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
    const GOOGLE_CLIENT_ID: &str =
        "681255809395-oo8ft2oprdnq9e3aqf6av3hmdib135j.apps.googleusercontent.com";
    const XAI_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
    const KIMI_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
    let mut compiled = CompiledOperatorRequest {
        method: reqwest::Method::POST,
        url: url::Url::parse("https://invalid.invalid").map_err(|_| OperatorHttpError::Protocol)?,
        accept: "application/json",
        content_type: Some("application/json"),
        encoding: OperatorResponseEncoding::Json,
        bearer: None,
        body: Vec::new(),
    };
    match request {
        OperatorHttpRequest::AnthropicToken { body } => {
            compiled.url = parsed("https://platform.claude.com/v1/oauth/token")?;
            compiled.body = match body {
                AnthropicTokenBody::AuthorizationCode { code, state, code_verifier } => serde_json::to_vec(&serde_json::json!({
                    "grant_type": "authorization_code", "client_id": ANTHROPIC_CLIENT_ID, "code": code.expose(),
                    "state": state.expose(), "redirect_uri": "http://localhost:53692/callback", "code_verifier": code_verifier.expose()
                })),
                AnthropicTokenBody::RefreshToken { refresh_token } => serde_json::to_vec(&serde_json::json!({
                    "grant_type": "refresh_token", "client_id": ANTHROPIC_CLIENT_ID, "refresh_token": refresh_token.expose()
                })),
            }.map_err(|_| OperatorHttpError::Protocol)?;
        }
        OperatorHttpRequest::CodexDeviceCode => {
            compiled.url = parsed("https://auth.openai.com/api/accounts/deviceauth/usercode")?;
            compiled.body = serde_json::to_vec(&serde_json::json!({"client_id": CODEX_CLIENT_ID}))
                .map_err(|_| OperatorHttpError::Protocol)?;
        }
        OperatorHttpRequest::CodexDevicePoll { body } => {
            compiled.url = parsed("https://auth.openai.com/api/accounts/deviceauth/token")?;
            compiled.body = serde_json::to_vec(&serde_json::json!({"device_auth_id": body.device_auth_id.expose(), "user_code": body.user_code.expose()}))
                .map_err(|_| OperatorHttpError::Protocol)?;
        }
        OperatorHttpRequest::CodexToken { body } => {
            compiled.url = parsed("https://auth.openai.com/oauth/token")?;
            compiled.content_type = Some("application/x-www-form-urlencoded");
            compiled.encoding = OperatorResponseEncoding::Form;
            let pairs = match body.flow {
                CodexTokenFlow::AuthorizationCode => vec![
                    ("grant_type", "authorization_code"),
                    ("client_id", CODEX_CLIENT_ID),
                    ("code", body.code_or_refresh.expose()),
                    (
                        "code_verifier",
                        body.code_verifier
                            .as_ref()
                            .ok_or(OperatorHttpError::Protocol)?
                            .expose(),
                    ),
                    (
                        "redirect_uri",
                        match body.redirect_kind.ok_or(OperatorHttpError::Protocol)? {
                            CodexRedirectKind::Browser => "http://localhost:1455/auth/callback",
                            CodexRedirectKind::Device => {
                                "https://auth.openai.com/deviceauth/callback"
                            }
                        },
                    ),
                ],
                CodexTokenFlow::RefreshToken => vec![
                    ("grant_type", "refresh_token"),
                    ("client_id", CODEX_CLIENT_ID),
                    ("refresh_token", body.code_or_refresh.expose()),
                ],
            };
            compiled.body = form(&pairs).into_bytes();
        }
        OperatorHttpRequest::GoogleToken { body } => {
            compiled.url = parsed("https://oauth2.googleapis.com/token")?;
            compiled.content_type = Some("application/x-www-form-urlencoded");
            compiled.encoding = OperatorResponseEncoding::Form;
            compiled.body = match body {
                GoogleTokenBody::AuthorizationCode {
                    code,
                    code_verifier,
                } => form(&[
                    ("client_id", GOOGLE_CLIENT_ID),
                    ("code", code.expose()),
                    ("grant_type", "authorization_code"),
                    ("redirect_uri", "http://localhost:8085/oauth2callback"),
                    ("code_verifier", code_verifier.expose()),
                ]),
                GoogleTokenBody::RefreshToken { refresh_token } => form(&[
                    ("client_id", GOOGLE_CLIENT_ID),
                    ("refresh_token", refresh_token.expose()),
                    ("grant_type", "refresh_token"),
                ]),
            }
            .into_bytes();
        }
        OperatorHttpRequest::GoogleCodeAssistLoad { body, bearer } => {
            compiled.url = parsed("https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist")?;
            compiled.bearer = Some(bearer);
            compiled.body = google_load_json(body)?;
        }
        OperatorHttpRequest::GoogleCodeAssistOnboard { body, bearer } => {
            compiled.url = parsed("https://cloudcode-pa.googleapis.com/v1internal:onboardUser")?;
            compiled.bearer = Some(bearer);
            compiled.body = google_onboard_json(body)?;
        }
        OperatorHttpRequest::GoogleCodeAssistPoll { operation, bearer } => {
            compiled.method = reqwest::Method::GET;
            compiled.url = parsed(&format!(
                "https://cloudcode-pa.googleapis.com/v1internal/{}",
                operation.as_str()
            ))?;
            compiled.content_type = None;
            compiled.bearer = Some(bearer);
        }
        OperatorHttpRequest::XaiDeviceCode { .. } => {
            compiled.url = parsed("https://auth.x.ai/oauth2/device/code")?;
            compiled.content_type = Some("application/x-www-form-urlencoded");
            compiled.encoding = OperatorResponseEncoding::Form;
            compiled.body = form(&[
                ("client_id", XAI_CLIENT_ID),
                (
                    "scope",
                    "openid profile email offline_access grok-cli:access api:access",
                ),
                ("referrer", "rpi"),
            ])
            .into_bytes();
        }
        OperatorHttpRequest::XaiToken { body } => {
            compiled.url = parsed("https://auth.x.ai/oauth2/token")?;
            compiled.content_type = Some("application/x-www-form-urlencoded");
            compiled.encoding = OperatorResponseEncoding::Form;
            compiled.body = form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", XAI_CLIENT_ID),
                ("device_code", body.device_code.expose()),
            ])
            .into_bytes();
        }
        OperatorHttpRequest::XaiRefresh { body } => {
            compiled.url = parsed("https://auth.x.ai/oauth2/token")?;
            compiled.content_type = Some("application/x-www-form-urlencoded");
            compiled.encoding = OperatorResponseEncoding::Form;
            compiled.body = form(&[
                ("grant_type", "refresh_token"),
                ("client_id", XAI_CLIENT_ID),
                ("refresh_token", body.refresh_token.expose()),
            ])
            .into_bytes();
        }
        OperatorHttpRequest::KimiDeviceCode { .. } => {
            compiled.url = parsed("https://auth.kimi.com/api/oauth/device_authorization")?;
            compiled.content_type = Some("application/x-www-form-urlencoded");
            compiled.encoding = OperatorResponseEncoding::Form;
            compiled.body = form(&[("client_id", KIMI_CLIENT_ID)]).into_bytes();
        }
        OperatorHttpRequest::KimiToken { body } => {
            compiled.url = parsed("https://auth.kimi.com/api/oauth/token")?;
            compiled.content_type = Some("application/x-www-form-urlencoded");
            compiled.encoding = OperatorResponseEncoding::Form;
            compiled.body = form(&[
                ("client_id", KIMI_CLIENT_ID),
                ("device_code", body.device_code.expose()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .into_bytes();
        }
        OperatorHttpRequest::KimiRefresh { body } => {
            compiled.url = parsed("https://auth.kimi.com/api/oauth/token")?;
            compiled.content_type = Some("application/x-www-form-urlencoded");
            compiled.encoding = OperatorResponseEncoding::Form;
            compiled.body = form(&[
                ("client_id", KIMI_CLIENT_ID),
                ("grant_type", "refresh_token"),
                ("refresh_token", body.refresh_token.expose()),
            ])
            .into_bytes();
        }
        OperatorHttpRequest::OpenRouterKeyExchange { body } => {
            compiled.url = parsed("https://openrouter.ai/api/v1/auth/keys")?;
            compiled.body = serde_json::to_vec(&serde_json::json!({"code": body.code.expose(), "code_verifier": body.code_verifier.expose()}))
                .map_err(|_| OperatorHttpError::Protocol)?;
        }
        OperatorHttpRequest::RadiusCatalog { bearer } => {
            compiled.method = reqwest::Method::GET;
            compiled.url = parsed("https://radius.pi.dev/v1/config")?;
            compiled.content_type = None;
            compiled.bearer = Some(bearer);
        }
        OperatorHttpRequest::SelfUpdateMetadata { channel } => {
            compiled.method = reqwest::Method::GET;
            compiled.url = match self_update_base {
                Some(base) => match channel {
                    ReleaseChannel::Stable => {
                        // install.sh semantics: `${base}/latest` is a literal
                        // path suffix, so a base without a trailing slash
                        // still appends `/latest` (never replaces the last
                        // path segment like `Url::join` would).
                        let mut url = base.clone();
                        let path = format!("{}/latest", url.path().trim_end_matches('/'));
                        url.set_path(&path);
                        url
                    }
                    ReleaseChannel::Prerelease => base.clone(),
                },
                None => parsed(match channel {
                    ReleaseChannel::Stable => {
                        "https://api.github.com/repos/0x8f701/rpi/releases/latest"
                    }
                    ReleaseChannel::Prerelease => {
                        "https://api.github.com/repos/0x8f701/rpi/releases"
                    }
                })?,
            };
            compiled.content_type = None;
            // GITHUB_TOKEN authenticates only the fixed GitHub API endpoint,
            // matching install.sh/install.ps1: never sent to release-asset
            // hosts or a custom RPI_UPDATE_BASE_URL endpoint.
            compiled.bearer = self_update_github_token_from(self_update_base, || {
                std::env::var("GITHUB_TOKEN").ok()
            })?;
        }
        OperatorHttpRequest::SelfUpdateAsset { url } => {
            compiled.method = reqwest::Method::GET;
            let candidate = url.as_url().clone();
            let allowed = match self_update_base {
                Some(base) => candidate.origin() == base.origin(),
                None => {
                    candidate.scheme() == "https"
                        && candidate.host_str() == Some("github.com")
                        && candidate
                            .port_or_known_default()
                            .is_none_or(|port| port == 443)
                        && candidate
                            .path()
                            .starts_with("/0x8f701/rpi/releases/download/")
                }
            };
            if !allowed {
                return Err(OperatorHttpError::RedirectDenied);
            }
            compiled.url = candidate;
            compiled.accept = "application/octet-stream";
            compiled.content_type = None;
        }
    }
    Ok(compiled)
}

fn validate_content_type(
    response: &reqwest::Response,
    operation: OperatorOperation,
) -> Result<(), OperatorHttpError> {
    if operation == OperatorOperation::SelfUpdateAsset {
        return Ok(());
    }
    let content_type = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .ok_or(OperatorHttpError::Protocol)?;
    if content_type.split(';').next().is_some_and(|value| {
        let value = value.trim();
        value == "application/json"
            || value == "application/x-www-form-urlencoded"
            || value.ends_with("+json")
    }) {
        Ok(())
    } else {
        Err(OperatorHttpError::Protocol)
    }
}

async fn read_bounded(
    response: reqwest::Response,
    limit: usize,
    deadline: tokio::time::Instant,
) -> Result<Box<[u8]>, OperatorHttpError> {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    loop {
        let chunk = tokio::time::timeout_at(deadline, stream.next())
            .await
            .map_err(|_| operator_timeout())?;
        let Some(chunk) = chunk else { break };
        let chunk = chunk.map_err(|_| OperatorHttpError::Protocol)?;
        if body
            .len()
            .checked_add(chunk.len())
            .is_none_or(|size| size > limit)
        {
            return Err(OperatorHttpError::ResponseTooLarge { limit });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.into_boxed_slice())
}

fn operator_timeout() -> OperatorHttpError {
    OperatorHttpError::Timeout {
        timeout_ms: OPERATOR_HTTP_TIMEOUT.as_millis() as u64,
    }
}

fn validate_identifier(value: String, limit: usize) -> Result<String, OperatorHttpError> {
    if value.is_empty()
        || value.len() > limit
        || !value.is_ascii()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
    {
        return Err(OperatorHttpError::Protocol);
    }
    Ok(value)
}

fn parsed(value: &str) -> Result<url::Url, OperatorHttpError> {
    url::Url::parse(value).map_err(|_| OperatorHttpError::DestinationNotCompiled)
}

fn form(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs.iter().copied())
        .finish()
}

fn google_load_json(body: GoogleCodeAssistLoadBody) -> Result<Vec<u8>, OperatorHttpError> {
    let project = body.project.as_ref().map(GoogleProjectId::as_str);
    serde_json::to_vec(&serde_json::json!({
        "cloudaicompanionProject": project,
        "metadata": {"ideType": "IDE_UNSPECIFIED", "platform": "PLATFORM_UNSPECIFIED", "pluginType": "GEMINI", "duetProject": project}
    })).map_err(|_| OperatorHttpError::Protocol)
}

fn google_onboard_json(body: GoogleCodeAssistOnboardBody) -> Result<Vec<u8>, OperatorHttpError> {
    let project = body.project.as_ref().map(GoogleProjectId::as_str);
    let mut value = serde_json::json!({
        "tierId": body.tier.as_str(),
        "metadata": {"ideType": "IDE_UNSPECIFIED", "platform": "PLATFORM_UNSPECIFIED", "pluginType": "GEMINI"}
    });
    if let Some(project) = project {
        value["cloudaicompanionProject"] = serde_json::Value::String(project.to_owned());
        value["metadata"]["duetProject"] = serde_json::Value::String(project.to_owned());
    }
    serde_json::to_vec(&value).map_err(|_| OperatorHttpError::Protocol)
}

fn self_update_github_token_from(
    self_update_base: Option<&url::Url>,
    read_token: impl FnOnce() -> Option<String>,
) -> Result<Option<SecretValue>, OperatorHttpError> {
    if self_update_base.is_some() {
        return Ok(None);
    }
    read_token()
        .filter(|token| !token.trim().is_empty())
        .map(SecretValue::new)
        .transpose()
}

/// Read and validate the `RPI_UPDATE_BASE_URL` self-update override.
///
/// Returns `None` when unset. When set, the value must parse to a URL whose
/// scheme is `https`, or `http` bound to a loopback host (`127.0.0.1`,
/// `localhost`, `::1`); the trailing slash is normalized so stable metadata
/// suffixing and `base.origin()` comparisons stay predictable. Any other form
/// is rejected as an unsupported endpoint override.

fn read_self_update_base_from(raw: Option<&str>) -> Result<Option<url::Url>, OperatorHttpError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let mut base =
        url::Url::parse(raw).map_err(|_| OperatorHttpError::EndpointOverrideUnsupported)?;
    let loopback = matches!(
        base.host_str(),
        Some("127.0.0.1") | Some("localhost") | Some("::1") | Some("[::1]")
    );
    if !(base.scheme() == "https" || (base.scheme() == "http" && loopback)) {
        return Err(OperatorHttpError::EndpointOverrideUnsupported);
    }
    let normalized_path = base.path().trim_end_matches('/').to_owned();
    base.set_path(&normalized_path);
    Ok(Some(base))
}

fn read_self_update_base() -> Result<Option<url::Url>, OperatorHttpError> {
    let raw = std::env::var(RPI_UPDATE_BASE_URL_ENV).ok();
    read_self_update_base_from(raw.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_self_update_base_accepts_https_and_loopback_http() {
        let https = read_self_update_base_from(Some(" https://mirror.example/releases/ "))
            .expect("https override")
            .expect("configured base");
        assert_eq!(https.as_str(), "https://mirror.example/releases");

        for raw in [
            "http://127.0.0.1:8080/releases/",
            "http://localhost:8080/releases",
            "http://[::1]:8080/releases/",
        ] {
            assert!(read_self_update_base_from(Some(raw)).is_ok(), "{raw}");
        }
        assert!(read_self_update_base_from(None).unwrap().is_none());
        assert!(read_self_update_base_from(Some("  ")).unwrap().is_none());
        assert!(matches!(
            read_self_update_base_from(Some("http://mirror.example/releases")),
            Err(OperatorHttpError::EndpointOverrideUnsupported)
        ));
    }

    #[test]
    fn github_token_is_read_only_for_fixed_github_metadata() {
        let base = url::Url::parse("https://mirror.example/releases").unwrap();
        let custom = self_update_github_token_from(Some(&base), || {
            panic!("custom update base must not read GITHUB_TOKEN")
        })
        .expect("custom base token scope");
        assert!(custom.is_none());

        let github = self_update_github_token_from(None, || Some("fixture-token".to_owned()))
            .expect("fixed GitHub token scope");
        assert!(github.is_some());
        let empty =
            self_update_github_token_from(None, || Some("  ".to_owned())).expect("empty token");
        assert!(empty.is_none());
    }

    #[test]
    fn release_download_url_accepts_http_and_https_with_host() {
        assert!(
            ReleaseDownloadUrl::parse(
                "https://github.com/0x8f701/rpi/releases/download/v1/rpi-1.tar.gz".to_owned()
            )
            .is_ok()
        );
        assert!(ReleaseDownloadUrl::parse("http://127.0.0.1:8080/SHA256SUMS".to_owned()).is_ok());
        assert!(ReleaseDownloadUrl::parse("ftp://example/asset".to_owned()).is_err());
        assert!(ReleaseDownloadUrl::parse("https:///no-host".to_owned()).is_err());
        assert!(ReleaseDownloadUrl::parse("not a url".to_owned()).is_err());
    }

    /// Default (no override): metadata compiles to the fixed GitHub API URL
    /// for both channels. (The override case below pins `bearer.is_none()`
    /// deterministically; the default `bearer` depends on GITHUB_TOKEN, so it
    /// is not asserted here to stay hermetic.)
    #[test]
    fn self_update_metadata_compiles_to_fixed_github_without_override() {
        let stable = compile_request(
            OperatorHttpRequest::SelfUpdateMetadata {
                channel: ReleaseChannel::Stable,
            },
            None,
        )
        .expect("stable metadata");
        assert_eq!(
            stable.url.as_str(),
            "https://api.github.com/repos/0x8f701/rpi/releases/latest"
        );
        let pre = compile_request(
            OperatorHttpRequest::SelfUpdateMetadata {
                channel: ReleaseChannel::Prerelease,
            },
            None,
        )
        .expect("prerelease metadata");
        assert_eq!(
            pre.url.as_str(),
            "https://api.github.com/repos/0x8f701/rpi/releases"
        );
    }

    /// Override: metadata re-points at the pinned base (`${base}/latest` for
    /// stable, the base itself for the prerelease list) and never carries a
    /// GITHUB_TOKEN, matching install.sh.
    #[test]
    fn self_update_metadata_uses_pinned_override_base() {
        let base = url::Url::parse("http://127.0.0.1:8080/releases").unwrap();
        let stable = compile_request(
            OperatorHttpRequest::SelfUpdateMetadata {
                channel: ReleaseChannel::Stable,
            },
            Some(&base),
        )
        .expect("override stable metadata");
        assert_eq!(stable.url.as_str(), "http://127.0.0.1:8080/releases/latest");
        assert!(stable.bearer.is_none());
        let pre = compile_request(
            OperatorHttpRequest::SelfUpdateMetadata {
                channel: ReleaseChannel::Prerelease,
            },
            Some(&base),
        )
        .expect("override prerelease metadata");
        assert_eq!(pre.url.as_str(), "http://127.0.0.1:8080/releases");
    }

    /// Default: an asset URL must be the fixed GitHub release download host
    /// and path prefix; anything else is denied as a redirect.
    #[test]
    fn self_update_asset_allows_only_github_download_without_override() {
        let ok = ReleaseDownloadUrl::parse(
            "https://github.com/0x8f701/rpi/releases/download/v1.2.3/rpi-1.2.3-x86_64-unknown-linux-gnu.tar.gz".to_owned(),
        ).expect("valid github asset");
        assert!(compile_request(OperatorHttpRequest::SelfUpdateAsset { url: ok }, None).is_ok());

        let foreign =
            ReleaseDownloadUrl::parse("https://evil.example/v1.2.3/asset.tar.gz".to_owned())
                .expect("parsed");
        assert!(matches!(
            compile_request(OperatorHttpRequest::SelfUpdateAsset { url: foreign }, None),
            Err(OperatorHttpError::RedirectDenied)
        ));

        let github_other_path =
            ReleaseDownloadUrl::parse("https://github.com/evil/repo".to_owned()).expect("parsed");
        assert!(matches!(
            compile_request(
                OperatorHttpRequest::SelfUpdateAsset {
                    url: github_other_path
                },
                None
            ),
            Err(OperatorHttpError::RedirectDenied)
        ));
    }

    /// Override: an asset URL must share the pinned base origin; a metadata
    /// endpoint cannot redirect downloads to a different authority.
    #[test]
    fn self_update_asset_requires_override_origin_match() {
        let base = url::Url::parse("http://127.0.0.1:8080/releases").unwrap();
        let ok = ReleaseDownloadUrl::parse("http://127.0.0.1:8080/SHA256SUMS".to_owned())
            .expect("same origin");
        assert!(
            compile_request(
                OperatorHttpRequest::SelfUpdateAsset { url: ok },
                Some(&base)
            )
            .is_ok()
        );

        let foreign = ReleaseDownloadUrl::parse(
            "https://github.com/0x8f701/rpi/releases/download/v1/asset.tar.gz".to_owned(),
        )
        .expect("parsed");
        assert!(matches!(
            compile_request(
                OperatorHttpRequest::SelfUpdateAsset { url: foreign },
                Some(&base)
            ),
            Err(OperatorHttpError::RedirectDenied)
        ));
    }
}
