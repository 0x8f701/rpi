use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use http::StatusCode;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::{Url, form_urlencoded};
use uuid::Uuid;

use crate::operator_http::{
    AnthropicTokenBody, CodexDevicePollBody, CodexRedirectKind, CodexTokenBody,
    GoogleCodeAssistLoadBody, GoogleCodeAssistOnboardBody, GoogleOperationName, GoogleProjectId,
    GoogleTierId, GoogleTokenBody, InitialAuthError, KimiDeviceCodeBody, KimiTokenBody,
    OpenRouterKeyExchangeBody, OperatorHttpError, OperatorHttpRequest, OperatorHttpResponse,
    OperatorHttpTransport, SecretValue, XaiDeviceCodeBody, XaiTokenBody,
};
use crate::{AuthEvent, AuthInteraction, AuthPrompt, AuthPromptOption, Credential, RequestAuth};

pub const SUPPORTED_PROVIDERS: &[&str] = &[
    "anthropic",
    "openai-codex",
    "google-gemini-cli",
    "xai",
    "openrouter",
    "kimi-coding",
];

const ANTHROPIC_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const ANTHROPIC_AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const ANTHROPIC_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const ANTHROPIC_REDIRECT_URI: &str = "http://localhost:53692/callback";
const ANTHROPIC_SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_AUTH_BASE: &str = "https://auth.openai.com";
const CODEX_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CODEX_DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";

const GOOGLE_CLIENT_ID: &str =
    "681255809395-oo8ft2oprdnq9e3aqf6av3hmdib135j.apps.googleusercontent.com";
const GOOGLE_REDIRECT_URI: &str = "http://localhost:8085/oauth2callback";
const GOOGLE_AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

const XAI_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const XAI_TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";

const KIMI_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";

const REFRESH_SKEW_MILLIS: i64 = 5 * 60 * 1000;

pub async fn login(
    provider: &str,
    interaction: &dyn AuthInteraction,
    operator: &OperatorHttpTransport,
) -> Result<Credential> {
    match provider {
        "anthropic" => login_anthropic(interaction, operator).await,
        "openai-codex" => login_codex(interaction, operator).await,
        "google-gemini-cli" => login_google(interaction, operator).await,
        "xai" => login_xai(interaction, operator).await,
        "openrouter" => login_openrouter(interaction, operator).await,
        "kimi-coding" => login_kimi(interaction, operator).await,
        _ => bail!("provider {provider:?} does not support OAuth login"),
    }
}

pub(crate) fn supports_initial_refresh(provider: &str) -> bool {
    matches!(
        provider,
        "anthropic" | "openai-codex" | "google-gemini-cli" | "xai" | "openrouter" | "kimi-coding"
    )
}

pub async fn refresh_initial_with_operator(
    provider: &str,
    credential: &Credential,
    operator: &OperatorHttpTransport,
) -> std::result::Result<Credential, InitialAuthError> {
    if !supports_initial_refresh(provider) {
        return Err(InitialAuthError::TransportUnsupported);
    }
    if provider == "openrouter" {
        return Ok(credential.clone());
    }
    let (refresh, _, _, fields) = credential
        .oauth_parts()
        .ok_or(InitialAuthError::TransportUnsupported)?;
    let request = match provider {
        "anthropic" => OperatorHttpRequest::AnthropicToken {
            body: AnthropicTokenBody::refresh(SecretValue::new(refresh.to_owned())?)?,
        },
        "openai-codex" => OperatorHttpRequest::CodexToken {
            body: CodexTokenBody::refresh(SecretValue::new(refresh.to_owned())?)?,
        },
        "google-gemini-cli" => OperatorHttpRequest::GoogleToken {
            body: GoogleTokenBody::refresh(SecretValue::new(refresh.to_owned())?)?,
        },
        "xai" => OperatorHttpRequest::XaiRefresh {
            body: crate::operator_http::XaiRefreshBody::new(SecretValue::new(refresh.to_owned())?)?,
        },
        "kimi-coding" => OperatorHttpRequest::KimiRefresh {
            body: crate::operator_http::KimiRefreshBody::new(SecretValue::new(
                refresh.to_owned(),
            )?)?,
        },
        _ => return Err(InitialAuthError::TransportUnsupported),
    };
    let action = match provider {
        "anthropic" => "Anthropic token refresh",
        "openai-codex" => "OpenAI Codex token refresh",
        "google-gemini-cli" => "Google Cloud token refresh",
        "xai" => "xAI token refresh",
        "kimi-coding" => "Kimi Code token refresh",
        _ => return Err(InitialAuthError::TransportUnsupported),
    };
    let (status, body) = execute_operator_json(operator, request).await?;
    if !status.is_success() {
        return Err(OperatorHttpError::Protocol.into());
    }
    let skew = if matches!(provider, "openai-codex" | "kimi-coding") {
        0
    } else {
        REFRESH_SKEW_MILLIS
    };
    let mut extra = fields.clone();
    if provider == "google-gemini-cli" {
        let project_id = required_field_string(fields, "projectId", "Google credential")
            .map_err(|_| OperatorHttpError::Protocol)?;
        extra.insert("projectId".to_owned(), Value::String(project_id));
    }
    if provider == "openai-codex" {
        let access = required_string(&body, "access_token", action)
            .map_err(|_| OperatorHttpError::Protocol)?;
        let account_id = codex_account_id(&access).map_err(|_| OperatorHttpError::Protocol)?;
        extra.insert("accountId".to_owned(), Value::String(account_id));
    }
    credential_from_token(&body, Some(refresh), skew, extra)
        .map_err(|_| OperatorHttpError::Protocol.into())
}

pub async fn refresh_with_executor(
    provider: &str,
    credential: &Credential,
    executor: pi_ai::SharedHttpExecutor,
) -> Result<Credential> {
    let (refresh, _, _, fields) = credential
        .oauth_parts()
        .ok_or_else(|| anyhow!("stored credential for provider {provider:?} is not OAuth"))?;
    let (url, content_type, body, action, skew) = match provider {
        "anthropic" => (
            ANTHROPIC_TOKEN_URL,
            "application/json",
            serde_json::to_vec(&json!({
                "grant_type": "refresh_token",
                "client_id": ANTHROPIC_CLIENT_ID,
                "refresh_token": refresh,
            }))?,
            "Anthropic token refresh",
            REFRESH_SKEW_MILLIS,
        ),
        "openai-codex" => (
            "https://auth.openai.com/oauth/token",
            "application/x-www-form-urlencoded",
            form_urlencoded::Serializer::new(String::new())
                .append_pair("grant_type", "refresh_token")
                .append_pair("refresh_token", refresh)
                .append_pair("client_id", CODEX_CLIENT_ID)
                .finish()
                .into_bytes(),
            "OpenAI Codex token refresh",
            0,
        ),
        "google-gemini-cli" => (
            GOOGLE_TOKEN_URL,
            "application/x-www-form-urlencoded",
            form_urlencoded::Serializer::new(String::new())
                .extend_pairs(google_refresh_form(refresh))
                .finish()
                .into_bytes(),
            "Google Cloud token refresh",
            REFRESH_SKEW_MILLIS,
        ),
        "xai" => (
            XAI_TOKEN_URL,
            "application/x-www-form-urlencoded",
            form_urlencoded::Serializer::new(String::new())
                .append_pair("grant_type", "refresh_token")
                .append_pair("client_id", XAI_CLIENT_ID)
                .append_pair("refresh_token", refresh)
                .finish()
                .into_bytes(),
            "xAI token refresh",
            REFRESH_SKEW_MILLIS,
        ),
        "openrouter" => return Ok(credential.clone()),
        "kimi-coding" => (
            "https://auth.kimi.com/api/oauth/token",
            "application/x-www-form-urlencoded",
            form_urlencoded::Serializer::new(String::new())
                .append_pair("client_id", KIMI_CLIENT_ID)
                .append_pair("grant_type", "refresh_token")
                .append_pair("refresh_token", refresh)
                .finish()
                .into_bytes(),
            "Kimi Code token refresh",
            0,
        ),
        _ => bail!("provider {provider:?} does not support OAuth refresh"),
    };
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static(content_type),
    );
    headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );
    let request = pi_ai::request_from_url(
        pi_ai::HttpMethod::Post,
        url,
        &headers,
        body,
        &pi_ai::StreamOptions::default(),
        pi_ai::MAX_HTTP_STREAM_RESPONSE_BYTES,
    )?;
    let response = executor.execute(request).await?;
    let status = response.status();
    let bytes = pi_ai::collect_response_body(response).await?;
    let body: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("{action} returned invalid JSON"))?;
    if !status.is_success() {
        bail!("{action} failed (HTTP {status})")
    }
    let mut extra = fields.clone();
    if provider == "google-gemini-cli" {
        let project_id = required_field_string(fields, "projectId", "Google credential")?;
        extra.insert("projectId".to_owned(), Value::String(project_id));
    }
    if provider == "openai-codex" {
        let access = required_string(&body, "access_token", action)?;
        extra.insert(
            "accountId".to_owned(),
            Value::String(codex_account_id(&access)?),
        );
    }
    credential_from_token(&body, Some(refresh), skew, extra)
}

fn google_refresh_form<'a>(refresh: &'a str) -> [(&'static str, &'a str); 3] {
    [
        ("client_id", GOOGLE_CLIENT_ID),
        ("refresh_token", refresh),
        ("grant_type", "refresh_token"),
    ]
}

fn google_code_exchange_form<'a>(code: &'a str, verifier: &'a str) -> [(&'static str, &'a str); 5] {
    [
        ("client_id", GOOGLE_CLIENT_ID),
        ("code", code),
        ("grant_type", "authorization_code"),
        ("redirect_uri", GOOGLE_REDIRECT_URI),
        ("code_verifier", verifier),
    ]
}

pub fn to_request_auth(provider: &str, credential: &Credential) -> Result<RequestAuth> {
    let (_, access, _, fields) = credential
        .oauth_parts()
        .ok_or_else(|| anyhow!("stored credential for provider {provider:?} is not OAuth"))?;
    let mut headers = HashMap::new();
    let mut env = HashMap::new();
    let available_model_ids = crate::auth::available_model_ids(fields);
    let api_key = match provider {
        "anthropic" | "github-copilot" | "xai" | "openrouter" => access.to_owned(),
        "openai-codex" => {
            headers.insert(
                "chatgpt-account-id".to_owned(),
                required_field_string(fields, "accountId", "OpenAI Codex credential")?,
            );
            access.to_owned()
        }
        "google-gemini-cli" => {
            env.insert(
                "GOOGLE_CLOUD_PROJECT".to_owned(),
                required_field_string(fields, "projectId", "Google credential")?,
            );
            access.to_owned()
        }
        "kimi-coding" => {
            headers.insert("Authorization".to_owned(), format!("Bearer {access}"));
            String::new()
        }
        _ => bail!("provider {provider:?} does not support OAuth request auth"),
    };
    Ok(RequestAuth {
        api_key,
        headers,
        env,
        kind: crate::RequestAuthKind::StoredOAuth,
        available_model_ids,
    })
}

async fn login_anthropic(
    interaction: &dyn AuthInteraction,
    operator: &OperatorHttpTransport,
) -> Result<Credential> {
    let (verifier, challenge) = generate_pkce();
    let state = verifier.clone();
    let authorize_url = with_query(
        ANTHROPIC_AUTHORIZE_URL,
        &[
            ("code", "true"),
            ("client_id", ANTHROPIC_CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", ANTHROPIC_REDIRECT_URI),
            ("scope", ANTHROPIC_SCOPES),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &state),
        ],
    )?;
    interaction.notify(AuthEvent::AuthUrl {
        url: authorize_url,
        instructions: Some(
            "Complete login in your browser, then paste the final redirect URL or authorization code."
                .to_owned(),
        ),
    });
    let input = interaction
        .prompt(AuthPrompt::ManualCode {
            message: "Paste the Anthropic authorization code or final redirect URL:".to_owned(),
            placeholder: Some(ANTHROPIC_REDIRECT_URI.to_owned()),
        })
        .await?;
    let (code, returned_state) = parse_authorization_input(&input)?;
    if returned_state
        .as_deref()
        .is_some_and(|value| value != state)
    {
        bail!("Anthropic OAuth state mismatch")
    }
    let body = AnthropicTokenBody::authorization_code(
        SecretValue::new(code)?,
        SecretValue::new(returned_state.unwrap_or(state))?,
        SecretValue::new(verifier)?,
    )?;
    let (status, body) =
        execute_operator_json(operator, OperatorHttpRequest::AnthropicToken { body }).await?;
    ensure_success(status, "Anthropic token exchange")?;
    credential_from_token(&body, None, REFRESH_SKEW_MILLIS, Map::new())
}
async fn login_codex(
    interaction: &dyn AuthInteraction,
    operator: &OperatorHttpTransport,
) -> Result<Credential> {
    let method = interaction
        .prompt(AuthPrompt::Select {
            message: "Select OpenAI Codex login method:".to_owned(),
            options: vec![
                AuthPromptOption {
                    id: "browser".to_owned(),
                    label: "Browser login".to_owned(),
                    description: Some("Use a local OAuth callback".to_owned()),
                },
                AuthPromptOption {
                    id: "device_code".to_owned(),
                    label: "Device code login".to_owned(),
                    description: Some("Best for remote or headless terminals".to_owned()),
                },
            ],
        })
        .await?;
    match method.as_str() {
        "browser" => login_codex_browser(interaction, operator).await,
        "device_code" => login_codex_device(interaction, operator).await,
        _ => bail!("unsupported OpenAI Codex login method"),
    }
}
async fn login_codex_browser(
    interaction: &dyn AuthInteraction,
    operator: &OperatorHttpTransport,
) -> Result<Credential> {
    let (verifier, challenge) = generate_pkce();
    let state = Uuid::new_v4().simple().to_string();
    let authorize_url = with_query(
        &format!("{CODEX_AUTH_BASE}/oauth/authorize"),
        &[
            ("response_type", "code"),
            ("client_id", CODEX_CLIENT_ID),
            ("redirect_uri", CODEX_REDIRECT_URI),
            ("scope", "openid profile email offline_access"),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &state),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", "rpi"),
        ],
    )?;
    interaction.notify(AuthEvent::AuthUrl {
        url: authorize_url,
        instructions: Some("Complete login, then paste the final redirect URL or code.".to_owned()),
    });
    let input = interaction
        .prompt(AuthPrompt::ManualCode {
            message: "Paste the OpenAI authorization code or final redirect URL:".to_owned(),
            placeholder: Some(CODEX_REDIRECT_URI.to_owned()),
        })
        .await?;
    let (code, returned_state) = parse_authorization_input(&input)?;
    if returned_state
        .as_deref()
        .is_some_and(|value| value != state)
    {
        bail!("OpenAI Codex OAuth state mismatch")
    }
    exchange_codex_code(operator, code, verifier, CodexRedirectKind::Browser).await
}
async fn login_codex_device(
    interaction: &dyn AuthInteraction,
    operator: &OperatorHttpTransport,
) -> Result<Credential> {
    let (status, body) =
        execute_operator_json(operator, OperatorHttpRequest::CodexDeviceCode).await?;
    ensure_success(status, "OpenAI Codex device code request")?;
    let device_auth_id = required_string(&body, "device_auth_id", "OpenAI Codex device code")?;
    let user_code = required_string(&body, "user_code", "OpenAI Codex device code")?;
    let interval = number_or_string(&body, "interval").unwrap_or(5.0).max(1.0);
    interaction.notify(AuthEvent::DeviceCode {
        user_code: user_code.clone(),
        verification_uri: CODEX_DEVICE_VERIFICATION_URI.to_owned(),
        interval_seconds: Some(interval),
        expires_in_seconds: Some(15.0 * 60.0),
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15 * 60);
    loop {
        if tokio::time::Instant::now() >= deadline {
            bail!("OpenAI Codex device flow timed out")
        }
        let request = OperatorHttpRequest::CodexDevicePoll {
            body: CodexDevicePollBody::new(
                SecretValue::new(device_auth_id.clone())?,
                SecretValue::new(user_code.clone())?,
            )?,
        };
        let (status, body) = execute_operator_json(operator, request).await?;
        if status.is_success() {
            let code = required_string(
                &body,
                "authorization_code",
                "OpenAI Codex device authorization",
            )?;
            let verifier =
                required_string(&body, "code_verifier", "OpenAI Codex device authorization")?;
            return exchange_codex_code(operator, code, verifier, CodexRedirectKind::Device).await;
        }
        let error = oauth_error_code(&body);
        if status.as_u16() != 403
            && status.as_u16() != 404
            && error.as_deref() != Some("deviceauth_authorization_pending")
        {
            bail!("OpenAI Codex device authorization failed (HTTP {status})")
        }
        tokio::time::sleep(Duration::from_secs_f64(interval)).await;
    }
}

async fn exchange_codex_code(
    operator: &OperatorHttpTransport,
    code: String,
    verifier: String,
    redirect: CodexRedirectKind,
) -> Result<Credential> {
    let body = CodexTokenBody::authorization_code(
        SecretValue::new(code)?,
        SecretValue::new(verifier)?,
        redirect,
    )?;
    let (status, body) =
        execute_operator_json(operator, OperatorHttpRequest::CodexToken { body }).await?;
    ensure_success(status, "OpenAI Codex token exchange")?;
    let access = required_string(&body, "access_token", "OpenAI Codex token exchange")?;
    let mut fields = Map::new();
    fields.insert(
        "accountId".to_owned(),
        Value::String(codex_account_id(&access)?),
    );
    credential_from_token(&body, None, 0, fields)
}
async fn login_google(
    interaction: &dyn AuthInteraction,
    operator: &OperatorHttpTransport,
) -> Result<Credential> {
    let (verifier, challenge) = generate_pkce();
    let state = verifier.clone();
    let url = with_query(
        GOOGLE_AUTHORIZE_URL,
        &[
            ("client_id", GOOGLE_CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", GOOGLE_REDIRECT_URI),
            (
                "scope",
                "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile",
            ),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &state),
            ("access_type", "offline"),
            ("prompt", "consent"),
        ],
    )?;
    interaction.notify(AuthEvent::AuthUrl {
        url,
        instructions: Some(
            "Complete Google sign-in, then paste the final redirect URL.".to_owned(),
        ),
    });
    let input = interaction
        .prompt(AuthPrompt::ManualCode {
            message: "Paste the Google OAuth final redirect URL:".to_owned(),
            placeholder: Some(GOOGLE_REDIRECT_URI.to_owned()),
        })
        .await?;
    let (code, returned_state) = parse_authorization_input(&input)?;
    if returned_state
        .as_deref()
        .is_some_and(|value| value != state)
    {
        bail!("Google OAuth state mismatch")
    }
    let token =
        GoogleTokenBody::authorization_code(SecretValue::new(code)?, SecretValue::new(verifier)?)?;
    let (status, body) =
        execute_operator_json(operator, OperatorHttpRequest::GoogleToken { body: token }).await?;
    ensure_success(status, "Google token exchange")?;
    let access = required_string(&body, "access_token", "Google token exchange")?;
    let project_id = discover_google_project(operator, &access, interaction).await?;
    let mut fields = Map::new();
    fields.insert("projectId".to_owned(), Value::String(project_id));
    credential_from_token(&body, None, REFRESH_SKEW_MILLIS, fields)
}

async fn discover_google_project(
    operator: &OperatorHttpTransport,
    access: &str,
    interaction: &dyn AuthInteraction,
) -> Result<String> {
    let configured_project = std::env::var("GOOGLE_CLOUD_PROJECT")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::env::var("GOOGLE_CLOUD_PROJECT_ID")
                .ok()
                .filter(|value| !value.is_empty())
        });
    interaction.notify(AuthEvent::Progress {
        message: "Checking for a Google Cloud Code Assist project...".to_owned(),
    });
    let project = configured_project
        .clone()
        .map(GoogleProjectId::parse)
        .transpose()?;
    let (status, body) = execute_operator_json(
        operator,
        OperatorHttpRequest::GoogleCodeAssistLoad {
            body: GoogleCodeAssistLoadBody::new(project),
            bearer: SecretValue::new(access.to_owned())?,
        },
    )
    .await?;
    ensure_success(status, "Google Cloud Code Assist account lookup")?;
    if body.get("currentTier").is_some() {
        if let Some(project) = body.get("cloudaicompanionProject").and_then(Value::as_str) {
            return Ok(project.to_owned());
        }
        if let Some(project) = configured_project {
            return Ok(project);
        }
        bail!("this Google account requires GOOGLE_CLOUD_PROJECT or GOOGLE_CLOUD_PROJECT_ID")
    }
    let tier_id = body
        .get("allowedTiers")
        .and_then(Value::as_array)
        .and_then(|tiers| {
            tiers
                .iter()
                .find(|tier| tier.get("isDefault").and_then(Value::as_bool) == Some(true))
                .or_else(|| tiers.first())
        })
        .and_then(|tier| tier.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("free-tier");
    if tier_id != "free-tier" && configured_project.is_none() {
        bail!("this Google account requires GOOGLE_CLOUD_PROJECT or GOOGLE_CLOUD_PROJECT_ID")
    }
    interaction.notify(AuthEvent::Progress {
        message: "Provisioning a Google Cloud Code Assist project...".to_owned(),
    });
    let tier = GoogleTierId::parse(tier_id.to_owned())?;
    let project = configured_project
        .clone()
        .map(GoogleProjectId::parse)
        .transpose()?;
    let (status, mut operation) = execute_operator_json(
        operator,
        OperatorHttpRequest::GoogleCodeAssistOnboard {
            body: GoogleCodeAssistOnboardBody::new(tier, project),
            bearer: SecretValue::new(access.to_owned())?,
        },
    )
    .await?;
    ensure_success(status, "Google Cloud Code Assist onboarding")?;
    while operation.get("done").and_then(Value::as_bool) != Some(true) {
        let name = required_string(&operation, "name", "Google onboarding operation")?;
        tokio::time::sleep(Duration::from_secs(5)).await;
        let (status, body) = execute_operator_json(
            operator,
            OperatorHttpRequest::GoogleCodeAssistPoll {
                operation: GoogleOperationName::parse(name)?,
                bearer: SecretValue::new(access.to_owned())?,
            },
        )
        .await?;
        ensure_success(status, "Google Cloud Code Assist onboarding poll")?;
        operation = body;
    }
    operation
        .pointer("/response/cloudaicompanionProject/id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or(configured_project)
        .ok_or_else(|| anyhow!("Google Cloud Code Assist returned no project id"))
}

async fn login_xai(
    interaction: &dyn AuthInteraction,
    operator: &OperatorHttpTransport,
) -> Result<Credential> {
    let (status, body) = execute_operator_json(
        operator,
        OperatorHttpRequest::XaiDeviceCode {
            body: XaiDeviceCodeBody,
        },
    )
    .await?;
    ensure_success(status, "xAI device authorization")?;
    let device = DeviceAuthorization::parse(&body, "xAI")?;
    interaction.notify(AuthEvent::DeviceCode {
        user_code: device.user_code.clone(),
        verification_uri: device.verification_uri.clone(),
        interval_seconds: Some(device.interval_seconds),
        expires_in_seconds: Some(device.expires_in_seconds),
    });
    poll_operator_device_code(operator, device, "xAI", REFRESH_SKEW_MILLIS, |code| {
        Ok(OperatorHttpRequest::XaiToken {
            body: XaiTokenBody::new(SecretValue::new(code)?)?,
        })
    })
    .await
}

async fn login_kimi(
    interaction: &dyn AuthInteraction,
    operator: &OperatorHttpTransport,
) -> Result<Credential> {
    let (status, body) = execute_operator_json(
        operator,
        OperatorHttpRequest::KimiDeviceCode {
            body: KimiDeviceCodeBody,
        },
    )
    .await?;
    ensure_success(status, "Kimi Code device authorization")?;
    let device = DeviceAuthorization::parse(&body, "Kimi Code")?;
    interaction.notify(AuthEvent::DeviceCode {
        user_code: device.user_code.clone(),
        verification_uri: device.verification_uri.clone(),
        interval_seconds: Some(device.interval_seconds),
        expires_in_seconds: Some(device.expires_in_seconds),
    });
    poll_operator_device_code(operator, device, "Kimi Code", 0, |code| {
        Ok(OperatorHttpRequest::KimiToken {
            body: KimiTokenBody::new(SecretValue::new(code)?)?,
        })
    })
    .await
}

async fn login_openrouter(
    interaction: &dyn AuthInteraction,
    operator: &OperatorHttpTransport,
) -> Result<Credential> {
    let (verifier, challenge) = generate_pkce();
    let callback_host = callback_host();
    let listener = TcpListener::bind((callback_host.as_str(), 0))
        .await
        .with_context(|| format!("binding OpenRouter callback server on {callback_host}"))?;
    let port = listener
        .local_addr()
        .context("reading OpenRouter callback address")?
        .port();
    let path = format!("/oauth/callback/{}", Uuid::new_v4());
    let callback_url = format!("http://{callback_host}:{port}{path}");
    let authorize_url = with_query(
        "https://openrouter.ai/auth",
        &[
            ("callback_url", &callback_url),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
        ],
    )?;
    interaction.notify(AuthEvent::Progress {
        message: format!("Listening for the OpenRouter OAuth callback on {callback_url}"),
    });
    interaction.notify(AuthEvent::AuthUrl {
        url: authorize_url,
        instructions: Some("Complete sign-in in your browser.".to_owned()),
    });
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5 * 60), listener.accept())
        .await
        .map_err(|_| anyhow!("OpenRouter OAuth login timed out"))?
        .context("accepting OpenRouter OAuth callback")?;
    let mut request = Vec::new();
    socket
        .read_to_end(&mut request)
        .await
        .context("reading OpenRouter OAuth callback")?;
    let first_line = String::from_utf8_lossy(&request)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    let target = first_line
        .strip_prefix("GET ")
        .and_then(|line| line.split(' ').next())
        .ok_or_else(|| anyhow!("invalid OpenRouter OAuth callback request"))?;
    let callback = Url::parse(&format!("http://{callback_host}{target}"))
        .context("parsing OpenRouter OAuth callback")?;
    if callback.path() != path {
        respond_callback(&mut socket, 404, "OAuth callback route not found.").await?;
        bail!("OpenRouter OAuth callback route did not match")
    }
    if let Some(error) = callback
        .query_pairs()
        .find_map(|(name, value)| (name == "error").then(|| value.into_owned()))
    {
        respond_callback(&mut socket, 400, "OpenRouter authorization was denied.").await?;
        bail!("OpenRouter authorization failed: {error}")
    }
    let code = callback
        .query_pairs()
        .find_map(|(name, value)| (name == "code").then(|| value.into_owned()))
        .ok_or_else(|| anyhow!("OpenRouter returned no authorization code"))?;
    let body =
        OpenRouterKeyExchangeBody::new(SecretValue::new(code)?, SecretValue::new(verifier)?)?;
    let (status, body) = execute_operator_json(
        operator,
        OperatorHttpRequest::OpenRouterKeyExchange { body },
    )
    .await?;
    if !status.is_success() {
        respond_callback(&mut socket, 502, "OpenRouter key exchange failed.").await?;
        bail!("OpenRouter key exchange failed (HTTP {status})")
    }
    let key = required_string(&body, "key", "OpenRouter key exchange")?;
    respond_callback(
        &mut socket,
        200,
        "Signed in to OpenRouter. You may close this page.",
    )
    .await?;
    Ok(Credential::OAuth {
        refresh: String::new(),
        access: key,
        expires: i64::MAX,
        fields: Map::new(),
    })
}

async fn respond_callback(
    socket: &mut tokio::net::TcpStream,
    status: u16,
    message: &str,
) -> Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Bad Gateway",
    };
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>rpi login</title><p>{}</p>",
        html_escape(message)
    );
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket
        .write_all(response.as_bytes())
        .await
        .context("writing OAuth callback response")
}

#[derive(Clone)]
struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval_seconds: f64,
    expires_in_seconds: f64,
}

impl DeviceAuthorization {
    fn parse(body: &Value, provider: &str) -> Result<Self> {
        let verification = body
            .get("verification_uri_complete")
            .and_then(Value::as_str)
            .or_else(|| body.get("verification_uri").and_then(Value::as_str))
            .ok_or_else(|| {
                anyhow!("{provider} device authorization response is missing verification_uri")
            })?;
        let verification_uri = trusted_browser_url(verification, provider)?;
        Ok(Self {
            device_code: required_string(body, "device_code", provider)?,
            user_code: required_string(body, "user_code", provider)?,
            verification_uri,
            interval_seconds: number_or_string(body, "interval").unwrap_or(5.0).max(1.0),
            expires_in_seconds: number_or_string(body, "expires_in").unwrap_or(15.0 * 60.0),
        })
    }
}

async fn poll_operator_device_code<F>(
    operator: &OperatorHttpTransport,
    device: DeviceAuthorization,
    provider: &str,
    skew: i64,
    request: F,
) -> Result<Credential>
where
    F: Fn(String) -> std::result::Result<OperatorHttpRequest, OperatorHttpError>,
{
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs_f64(device.expires_in_seconds.max(1.0));
    let mut interval = Duration::from_secs_f64(device.interval_seconds.max(1.0));
    tokio::time::sleep(interval).await;
    loop {
        if tokio::time::Instant::now() >= deadline {
            bail!("{provider} device flow timed out")
        }
        let (status, body) =
            execute_operator_json(operator, request(device.device_code.clone())?).await?;
        if status.is_success() {
            return credential_from_token(&body, None, skew, Map::new());
        }
        match oauth_error_code(&body).as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += Duration::from_secs(5),
            Some("access_denied" | "authorization_denied") => {
                bail!("{provider} device authorization was denied")
            }
            Some("expired_token") => bail!("{provider} device code expired"),
            _ => bail!("{provider} device authorization failed (HTTP {status})"),
        }
        tokio::time::sleep(interval).await;
    }
}

fn credential_from_token(
    body: &Value,
    previous_refresh: Option<&str>,
    skew_millis: i64,
    fields: Map<String, Value>,
) -> Result<Credential> {
    let access = required_string(body, "access_token", "OAuth token")?;
    let refresh = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| previous_refresh.map(str::to_owned))
        .ok_or_else(|| anyhow!("OAuth token response is missing refresh_token"))?;
    let lifetime = number_or_string(body, "expires_in").unwrap_or(3600.0);
    let expires =
        chrono::Utc::now().timestamp_millis() + (lifetime * 1000.0).round() as i64 - skew_millis;
    Ok(Credential::OAuth {
        refresh,
        access,
        expires,
        fields,
    })
}

async fn execute_operator_json(
    operator: &OperatorHttpTransport,
    request: OperatorHttpRequest,
) -> std::result::Result<(StatusCode, Value), OperatorHttpError> {
    let response = operator.execute(request).await?;
    let OperatorHttpResponse::Structured(response) = response else {
        return Err(OperatorHttpError::Protocol);
    };
    let body = serde_json::from_slice(response.body()).map_err(|_| OperatorHttpError::Protocol)?;
    Ok((response.status(), body))
}

fn ensure_success(status: StatusCode, action: &str) -> Result<()> {
    if status.is_success() {
        Ok(())
    } else {
        bail!("{action} failed (HTTP {status})")
    }
}

fn required_string(body: &Value, field: &str, action: &str) -> Result<String> {
    body.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("{action} response is missing {field}"))
}

fn required_field_string(fields: &Map<String, Value>, field: &str, action: &str) -> Result<String> {
    fields
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("{action} is missing {field}"))
}

fn number_or_string(body: &Value, field: &str) -> Option<f64> {
    body.get(field).and_then(|value| {
        value
            .as_f64()
            .or_else(|| value.as_str().and_then(|value| value.parse::<f64>().ok()))
    })
}

fn oauth_error_code(body: &Value) -> Option<String> {
    match body.get("error") {
        Some(Value::String(error)) => Some(error.clone()),
        Some(Value::Object(error)) => error.get("code").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

fn generate_pkce() -> (String, String) {
    let verifier = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn with_query(base: &str, fields: &[(&str, &str)]) -> Result<String> {
    let mut url = Url::parse(base).with_context(|| format!("parsing OAuth URL {base}"))?;
    url.set_query(Some(
        &form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields.iter().copied())
            .finish(),
    ));
    Ok(url.into())
}

fn parse_authorization_input(input: &str) -> Result<(String, Option<String>)> {
    let value = input.trim();
    if value.is_empty() {
        bail!("authorization code must not be empty")
    }
    if let Ok(url) = Url::parse(value) {
        let code = url
            .query_pairs()
            .find_map(|(name, value)| (name == "code").then(|| value.into_owned()))
            .ok_or_else(|| anyhow!("redirect URL is missing authorization code"))?;
        let state = url
            .query_pairs()
            .find_map(|(name, value)| (name == "state").then(|| value.into_owned()));
        return Ok((code, state));
    }
    if let Some((code, state)) = value.split_once('#') {
        return Ok((code.to_owned(), Some(state.to_owned())));
    }
    if value.contains("code=") {
        let pairs = form_urlencoded::parse(value.as_bytes()).collect::<Vec<_>>();
        let code = pairs
            .iter()
            .find_map(|(name, value)| (name == "code").then(|| value.to_string()))
            .ok_or_else(|| anyhow!("authorization response is missing code"))?;
        let state = pairs
            .iter()
            .find_map(|(name, value)| (name == "state").then(|| value.to_string()));
        return Ok((code, state));
    }
    Ok((value.to_owned(), None))
}

#[cfg(test)]
mod tests {
    use super::{google_code_exchange_form, google_refresh_form};

    #[test]
    fn google_native_token_forms_never_send_client_secret() {
        for fields in [
            google_refresh_form("refresh").to_vec(),
            google_code_exchange_form("code", "verifier").to_vec(),
        ] {
            assert!(fields.iter().all(|(name, _)| *name != "client_secret"));
        }
    }
}

fn codex_account_id(access_token: &str) -> Result<String> {
    let payload = access_token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow!("OpenAI Codex access token is not a JWT"))?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .context("decoding OpenAI Codex token claims")?;
    let claims: Value =
        serde_json::from_slice(&bytes).context("parsing OpenAI Codex token claims")?;
    claims
        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("OpenAI Codex access token has no account id"))
}

fn trusted_browser_url(value: &str, provider: &str) -> Result<String> {
    let url = Url::parse(value)
        .with_context(|| format!("{provider} returned an invalid verification URL"))?;
    if url.scheme() != "https" && url.scheme() != "http" {
        bail!("{provider} returned an untrusted verification URL")
    }
    Ok(url.into())
}

fn callback_host() -> String {
    std::env::var("PI_OAUTH_CALLBACK_HOST")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "127.0.0.1".to_owned())
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
