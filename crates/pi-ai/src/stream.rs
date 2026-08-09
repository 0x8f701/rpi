use crate::{
    AiError, AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Context,
    ImageGenerationOptions, ImageGenerationResult, Model, SimpleStreamOptions, StopReason,
    StreamOptions, new_assistant_message_event_stream,
};
use anyhow::anyhow;
use std::collections::HashMap;

pub const ANTHROPIC_AUTH_TOKEN_ENV: &str = "ANTHROPIC_AUTH_TOKEN";
pub const ANTHROPIC_OAUTH_TOKEN_ENV: &str = "ANTHROPIC_OAUTH_TOKEN";
pub const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

pub async fn stream(
    model: Model,
    context: Context,
    mut options: StreamOptions,
) -> AssistantMessageEventStream {
    crate::providers::register_builtins();
    resolve_api_key(&model, &mut options);
    match crate::get_api_provider(&model.api) {
        Some(p) => (p.stream)(model, context, options).await,
        None => {
            error_stream(
                &model,
                format!("No API provider registered for api: {}", model.api),
            )
            .await
        }
    }
}

pub async fn stream_simple(
    model: Model,
    context: Context,
    mut options: SimpleStreamOptions,
) -> AssistantMessageEventStream {
    crate::providers::register_builtins();
    resolve_api_key(&model, &mut options.stream);
    match crate::get_api_provider(&model.api) {
        Some(p) => (p.stream_simple)(model, context, options).await,
        None => {
            error_stream(
                &model,
                format!("No API provider registered for api: {}", model.api),
            )
            .await
        }
    }
}

pub async fn complete(
    model: Model,
    context: Context,
    options: StreamOptions,
) -> Option<AssistantMessage> {
    stream(model, context, options).await.result().await
}

pub async fn complete_simple(
    model: Model,
    context: Context,
    options: SimpleStreamOptions,
) -> Option<AssistantMessage> {
    stream_simple(model, context, options).await.result().await
}

/// Generates images through the provider registered for `model.api`
/// (`imagegen` or `openrouter-images` route to the OpenAI-compatible
/// `images/generations` client). Base URL and auth resolve exactly like
/// streaming: the model's `base_url` (optionally overridden by
/// `options.base_url`) and the caller-supplied api key, falling back to env.
/// The model must route to a provider with an image-generation capability;
/// chat-only providers error clearly.
pub async fn generate_image(
    model: Model,
    mut options: ImageGenerationOptions,
) -> anyhow::Result<ImageGenerationResult> {
    crate::providers::register_builtins();
    if options.api_key.as_ref().is_none_or(|k| k.trim().is_empty()) {
        options.api_key = get_env_api_key(&model.provider, Some(&options.env));
    }
    match crate::get_api_provider(&model.api) {
        Some(provider) => match provider.generate_image {
            Some(generate) => generate(model, options).await,
            None => Err(anyhow!(
                "API {} does not support image generation",
                model.api
            )),
        },
        None => Err(AiError::UnknownApi(model.api).into()),
    }
}

fn resolve_api_key(model: &Model, options: &mut StreamOptions) {
    if options.api_key.as_ref().is_none_or(|k| k.trim().is_empty()) {
        options.api_key = get_env_api_key(&model.provider, Some(&options.env));
    }
}

fn provider_env_value(name: &str, env: Option<&HashMap<String, String>>) -> Option<String> {
    env.and_then(|e| e.get(name))
        .filter(|v| !v.is_empty())
        .cloned()
        .or_else(|| std::env::var(name).ok().filter(|v| !v.is_empty()))
}

/// Environment variable names that can supply credentials for `provider`.
///
/// For Anthropic, `ANTHROPIC_AUTH_TOKEN` is included for discovery/status only:
/// `get_env_api_key` never returns it because requests must send it as
/// `Authorization: Bearer` rather than as an API key.
fn api_key_env_vars(provider: &str) -> Option<&'static [&'static str]> {
    Some(match provider {
        "github-copilot" => &["COPILOT_GITHUB_TOKEN"],
        "anthropic" => &[
            ANTHROPIC_AUTH_TOKEN_ENV,
            ANTHROPIC_OAUTH_TOKEN_ENV,
            ANTHROPIC_API_KEY_ENV,
        ],
        "ant-ling" => &["ANT_LING_API_KEY"],
        "qwen-token-plan" => &["QWEN_TOKEN_PLAN_API_KEY"],
        "qwen-token-plan-cn" => &["QWEN_TOKEN_PLAN_CN_API_KEY"],
        "openai" | "openai-codex" => &["OPENAI_API_KEY"],
        "azure-openai-responses" => &["AZURE_OPENAI_API_KEY"],
        "nvidia" => &["NVIDIA_API_KEY"],
        "deepseek" => &["DEEPSEEK_API_KEY"],
        "google" => &["GEMINI_API_KEY"],
        "google-vertex" => &["GOOGLE_CLOUD_API_KEY"],
        "groq" => &["GROQ_API_KEY"],
        "cerebras" => &["CEREBRAS_API_KEY"],
        "xai" => &["XAI_API_KEY"],
        "radius" => &["RADIUS_API_KEY"],
        "openrouter" | "openrouter-images" => &["OPENROUTER_API_KEY"],
        "vercel-ai-gateway" => &["AI_GATEWAY_API_KEY"],
        "zai" => &["ZAI_API_KEY"],
        "zai-coding-cn" => &["ZAI_CODING_CN_API_KEY"],
        "mistral" => &["MISTRAL_API_KEY"],
        "minimax" => &["MINIMAX_API_KEY"],
        "minimax-cn" => &["MINIMAX_CN_API_KEY"],
        "moonshotai" | "moonshotai-cn" => &["MOONSHOT_API_KEY"],
        "huggingface" => &["HF_TOKEN"],
        "fireworks" => &["FIREWORKS_API_KEY"],
        "together" => &["TOGETHER_API_KEY"],
        "opencode" | "opencode-go" => &["OPENCODE_API_KEY"],
        "kimi-coding" => &["KIMI_API_KEY"],
        "cloudflare-workers-ai" | "cloudflare-ai-gateway" => &["CLOUDFLARE_API_KEY"],
        "xiaomi" => &["XIAOMI_API_KEY"],
        "xiaomi-token-plan-cn" => &["XIAOMI_TOKEN_PLAN_CN_API_KEY"],
        "xiaomi-token-plan-ams" => &["XIAOMI_TOKEN_PLAN_AMS_API_KEY"],
        "xiaomi-token-plan-sgp" => &["XIAOMI_TOKEN_PLAN_SGP_API_KEY"],
        _ => return None,
    })
}

/// Find configured environment variables that can provide an API key for a provider.
///
/// Reports actual API-key / token variables only. Ambient credential sources
/// (AWS profiles, Google ADC paths, etc.) are intentionally excluded.
pub fn find_env_keys(provider: &str, env: Option<&HashMap<String, String>>) -> Option<Vec<String>> {
    let vars = api_key_env_vars(provider)?;
    let found: Vec<String> = vars
        .iter()
        .copied()
        .filter(|name| provider_env_value(name, env).is_some())
        .map(str::to_string)
        .collect();
    if found.is_empty() { None } else { Some(found) }
}

/// Get API key for provider from known environment variables (e.g. `OPENAI_API_KEY`).
///
/// Anthropic notes:
/// - `ANTHROPIC_AUTH_TOKEN` means bearer auth is active and is never returned here
///   (so it is not replaced by an API key).
/// - `ANTHROPIC_OAUTH_TOKEN` wins over `ANTHROPIC_API_KEY`.
///
/// Cloud adapters use only explicit injected environment values; no HOME or
/// ambient credential files are read.
pub fn get_env_api_key(provider: &str, env: Option<&HashMap<String, String>>) -> Option<String> {
    if provider == "faux" {
        return Some("faux".into());
    }

    if let Some(env_keys) = find_env_keys(provider, env) {
        let api_key_env = if provider == "anthropic" {
            env_keys
                .iter()
                .find(|key| key.as_str() != ANTHROPIC_AUTH_TOKEN_ENV)
                .map(String::as_str)
        } else {
            env_keys.first().map(String::as_str)
        };
        if let Some(name) = api_key_env {
            return provider_env_value(name, env);
        }
    }

    if provider == "amazon-bedrock" {
        // Multiple AWS credential sources:
        // 1. AWS_PROFILE
        // 2. AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY
        // 3. AWS_BEARER_TOKEN_BEDROCK
        // 4. AWS_CONTAINER_CREDENTIALS_RELATIVE_URI
        // 5. AWS_CONTAINER_CREDENTIALS_FULL_URI
        // 6. AWS_WEB_IDENTITY_TOKEN_FILE
        if provider_env_value("AWS_PROFILE", env).is_some()
            || (provider_env_value("AWS_ACCESS_KEY_ID", env).is_some()
                && provider_env_value("AWS_SECRET_ACCESS_KEY", env).is_some())
            || provider_env_value("AWS_BEARER_TOKEN_BEDROCK", env).is_some()
            || provider_env_value("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", env).is_some()
            || provider_env_value("AWS_CONTAINER_CREDENTIALS_FULL_URI", env).is_some()
            || provider_env_value("AWS_WEB_IDENTITY_TOKEN_FILE", env).is_some()
        {
            return Some("<authenticated>".into());
        }
    }

    None
}

pub(crate) async fn error_stream(model: &Model, message: String) -> AssistantMessageEventStream {
    let s = new_assistant_message_event_stream();
    let mut m = AssistantMessage::pending(model);
    m.stop_reason = StopReason::Error;
    m.error_message = Some(message);
    s.push(AssistantMessageEvent::Error {
        reason: StopReason::Error,
        error: m.clone(),
    })
    .await;
    s.end(Some(m)).await;
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex};

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// Provider → primary env var (non-Anthropic single-key mappings).
    const PROVIDER_ENV_MAP: &[(&str, &str)] = &[
        ("ant-ling", "ANT_LING_API_KEY"),
        ("qwen-token-plan", "QWEN_TOKEN_PLAN_API_KEY"),
        ("qwen-token-plan-cn", "QWEN_TOKEN_PLAN_CN_API_KEY"),
        ("openai", "OPENAI_API_KEY"),
        ("openai-codex", "OPENAI_API_KEY"),
        ("azure-openai-responses", "AZURE_OPENAI_API_KEY"),
        ("nvidia", "NVIDIA_API_KEY"),
        ("deepseek", "DEEPSEEK_API_KEY"),
        ("google", "GEMINI_API_KEY"),
        ("google-vertex", "GOOGLE_CLOUD_API_KEY"),
        ("groq", "GROQ_API_KEY"),
        ("cerebras", "CEREBRAS_API_KEY"),
        ("xai", "XAI_API_KEY"),
        ("radius", "RADIUS_API_KEY"),
        ("openrouter", "OPENROUTER_API_KEY"),
        ("vercel-ai-gateway", "AI_GATEWAY_API_KEY"),
        ("zai", "ZAI_API_KEY"),
        ("zai-coding-cn", "ZAI_CODING_CN_API_KEY"),
        ("mistral", "MISTRAL_API_KEY"),
        ("minimax", "MINIMAX_API_KEY"),
        ("minimax-cn", "MINIMAX_CN_API_KEY"),
        ("moonshotai", "MOONSHOT_API_KEY"),
        ("moonshotai-cn", "MOONSHOT_API_KEY"),
        ("huggingface", "HF_TOKEN"),
        ("fireworks", "FIREWORKS_API_KEY"),
        ("together", "TOGETHER_API_KEY"),
        ("opencode", "OPENCODE_API_KEY"),
        ("opencode-go", "OPENCODE_API_KEY"),
        ("kimi-coding", "KIMI_API_KEY"),
        ("cloudflare-workers-ai", "CLOUDFLARE_API_KEY"),
        ("cloudflare-ai-gateway", "CLOUDFLARE_API_KEY"),
        ("xiaomi", "XIAOMI_API_KEY"),
        ("xiaomi-token-plan-cn", "XIAOMI_TOKEN_PLAN_CN_API_KEY"),
        ("xiaomi-token-plan-ams", "XIAOMI_TOKEN_PLAN_AMS_API_KEY"),
        ("xiaomi-token-plan-sgp", "XIAOMI_TOKEN_PLAN_SGP_API_KEY"),
        ("github-copilot", "COPILOT_GITHUB_TOKEN"),
    ];

    #[test]
    fn table_resolves_all_provider_env_mappings_from_scoped_env() {
        let _guard = env_lock();
        for (provider, env_name) in PROVIDER_ENV_MAP {
            let token = format!("test-token-for-{provider}");
            let env = map(&[(env_name, token.as_str())]);
            let got = get_env_api_key(provider, Some(&env));
            assert_eq!(
                got.as_deref(),
                Some(token.as_str()),
                "provider {provider} should read {env_name}"
            );
            assert_eq!(
                find_env_keys(provider, Some(&env)).as_deref(),
                Some(&[(*env_name).to_string()][..]),
                "find_env_keys({provider})"
            );
        }
    }

    #[test]
    fn unknown_provider_returns_none() {
        let env = map(&[("SOME_KEY", "x")]);
        assert_eq!(get_env_api_key("no-such-provider", Some(&env)), None);
        assert_eq!(find_env_keys("no-such-provider", Some(&env)), None);
    }

    #[test]
    fn faux_provider_returns_literal_without_env() {
        assert_eq!(get_env_api_key("faux", None).as_deref(), Some("faux"));
        assert_eq!(find_env_keys("faux", None), None);
    }

    #[test]
    fn empty_scoped_values_are_ignored() {
        let env = map(&[("OPENAI_API_KEY", "")]);
        assert_eq!(get_env_api_key("openai", Some(&env)), None);
    }

    #[test]
    fn anthropic_oauth_wins_over_api_key_and_auth_token_is_not_returned() {
        let env = map(&[
            (ANTHROPIC_AUTH_TOKEN_ENV, "auth-token"),
            (ANTHROPIC_OAUTH_TOKEN_ENV, "oauth-token"),
            (ANTHROPIC_API_KEY_ENV, "api-key"),
        ]);
        assert_eq!(
            find_env_keys("anthropic", Some(&env)).as_deref(),
            Some(
                &[
                    ANTHROPIC_AUTH_TOKEN_ENV.to_string(),
                    ANTHROPIC_OAUTH_TOKEN_ENV.to_string(),
                    ANTHROPIC_API_KEY_ENV.to_string(),
                ][..]
            )
        );
        assert_eq!(
            get_env_api_key("anthropic", Some(&env)).as_deref(),
            Some("oauth-token")
        );
    }

    #[test]
    fn anthropic_auth_token_alone_does_not_yield_api_key() {
        let env = map(&[(ANTHROPIC_AUTH_TOKEN_ENV, "auth-token")]);
        assert_eq!(
            find_env_keys("anthropic", Some(&env)).as_deref(),
            Some(&[ANTHROPIC_AUTH_TOKEN_ENV.to_string()][..])
        );
        assert_eq!(get_env_api_key("anthropic", Some(&env)), None);
    }

    #[test]
    fn anthropic_oauth_token_alone_is_api_key() {
        let env = map(&[(ANTHROPIC_OAUTH_TOKEN_ENV, "oauth-token")]);
        assert_eq!(
            get_env_api_key("anthropic", Some(&env)).as_deref(),
            Some("oauth-token")
        );
    }

    #[test]
    fn anthropic_falls_back_to_api_key() {
        let env = map(&[(ANTHROPIC_API_KEY_ENV, "api-key")]);
        assert_eq!(
            get_env_api_key("anthropic", Some(&env)).as_deref(),
            Some("api-key")
        );
    }

    #[test]
    fn anthropic_auth_token_with_api_key_skips_auth_token_value() {
        // AUTH_TOKEN is never returned as the api_key; OAuth/API key vars still can be.
        let env = map(&[
            (ANTHROPIC_AUTH_TOKEN_ENV, "auth-token"),
            (ANTHROPIC_API_KEY_ENV, "api-key"),
        ]);
        assert_eq!(
            get_env_api_key("anthropic", Some(&env)).as_deref(),
            Some("api-key")
        );
    }

    #[test]
    fn github_copilot_ignores_generic_github_tokens() {
        let env = map(&[("GH_TOKEN", "gh"), ("GITHUB_TOKEN", "github")]);
        assert_eq!(find_env_keys("github-copilot", Some(&env)), None);
        assert_eq!(get_env_api_key("github-copilot", Some(&env)), None);
    }

    #[test]
    fn bedrock_ambient_credentials_return_authenticated_sentinel() {
        let cases: &[&[(&str, &str)]] = &[
            &[("AWS_PROFILE", "default")],
            &[
                ("AWS_ACCESS_KEY_ID", "AKIAtest"),
                ("AWS_SECRET_ACCESS_KEY", "secret"),
            ],
            &[("AWS_BEARER_TOKEN_BEDROCK", "bearer")],
            &[("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "/v2/creds")],
            &[("AWS_CONTAINER_CREDENTIALS_FULL_URI", "http://example/creds")],
            &[("AWS_WEB_IDENTITY_TOKEN_FILE", "workspace/token")],
        ];
        for pairs in cases {
            let env = map(pairs);
            assert_eq!(
                get_env_api_key("amazon-bedrock", Some(&env)).as_deref(),
                Some("<authenticated>"),
                "pairs={pairs:?}"
            );
        }
        // access key alone is insufficient
        let env = map(&[("AWS_ACCESS_KEY_ID", "AKIAtest")]);
        assert_eq!(get_env_api_key("amazon-bedrock", Some(&env)), None);
    }

    #[test]
    fn vertex_api_key_is_explicitly_resolved() {
        let env = map(&[
            ("GOOGLE_CLOUD_API_KEY", "vertex-key"),
            ("GOOGLE_CLOUD_PROJECT", "proj"),
            ("GOOGLE_CLOUD_LOCATION", "us-central1"),
        ]);
        assert_eq!(
            get_env_api_key("google-vertex", Some(&env)).as_deref(),
            Some("vertex-key")
        );
    }

    #[test]
    fn vertex_credential_file_path_is_not_consumed() {
        let env = map(&[
            ("GOOGLE_APPLICATION_CREDENTIALS", "/workspace/adc.json"),
            ("GOOGLE_CLOUD_PROJECT", "proj"),
            ("GOOGLE_CLOUD_LOCATION", "us-central1"),
            ("GOOGLE_CLOUD_ACCESS_TOKEN", "explicit-access-token"),
        ]);
        assert_eq!(get_env_api_key("google-vertex", Some(&env)), None);
    }

    #[test]
    fn explicit_option_api_key_outranks_env() {
        let model = Model {
            id: "gpt-test".into(),
            name: "gpt-test".into(),
            api: "openai-completions".into(),
            provider: "openai".into(),
            base_url: "https://example.test".into(),
            ..Default::default()
        };
        let mut options = StreamOptions {
            api_key: Some("explicit-key".into()),
            env: map(&[("OPENAI_API_KEY", "env-key")]),
            ..Default::default()
        };
        resolve_api_key(&model, &mut options);
        assert_eq!(options.api_key.as_deref(), Some("explicit-key"));

        // empty / whitespace explicit falls through to env
        options.api_key = Some("   ".into());
        resolve_api_key(&model, &mut options);
        assert_eq!(options.api_key.as_deref(), Some("env-key"));

        options.api_key = None;
        resolve_api_key(&model, &mut options);
        assert_eq!(options.api_key.as_deref(), Some("env-key"));
    }

    #[test]
    fn scoped_env_is_preferred_lookup_source() {
        // Prefer scoped override map over ambient process env without mutating process env.
        let env = map(&[("OPENAI_API_KEY", "scoped-value")]);
        assert_eq!(
            get_env_api_key("openai", Some(&env)).as_deref(),
            Some("scoped-value")
        );
        // Empty map still falls through to process env (value not asserted — may be unset).
        let empty = HashMap::new();
        let _ = get_env_api_key("openai", Some(&empty));
    }
}
