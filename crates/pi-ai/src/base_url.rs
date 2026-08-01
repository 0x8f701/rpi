//! Shared base-URL resolution helpers used by multiple providers.
//!
//! Cloudflare Workers AI and AI Gateway model URLs embed environment-variable
//! placeholders (`{CLOUDFLARE_ACCOUNT_ID}`, `{CLOUDFLARE_GATEWAY_ID}`) that
//! must be substituted before a request can be sent. Providers resolve these
//! through this module using the explicit env lookup they already carry
//! (`StreamOptions.env`).

use std::collections::HashMap;

use crate::AiError;

/// Env var supplying the Cloudflare account id substituted into
/// `{CLOUDFLARE_ACCOUNT_ID}` placeholders.
pub const CLOUDFLARE_ACCOUNT_ID_ENV: &str = "CLOUDFLARE_ACCOUNT_ID";

/// Env var supplying the Cloudflare gateway id substituted into
/// `{CLOUDFLARE_GATEWAY_ID}` placeholders.
pub const CLOUDFLARE_GATEWAY_ID_ENV: &str = "CLOUDFLARE_GATEWAY_ID";

const CLOUDFLARE_ACCOUNT_ID_PLACEHOLDER: &str = "{CLOUDFLARE_ACCOUNT_ID}";
const CLOUDFLARE_GATEWAY_ID_PLACEHOLDER: &str = "{CLOUDFLARE_GATEWAY_ID}";

/// Resolve Cloudflare placeholders in `base_url` against `env`.
///
/// URLs without placeholders pass through unchanged. Each placeholder present
/// in the URL is replaced with the corresponding value from `env` (typically
/// `StreamOptions.env`); a missing or empty value fails with an error that
/// names the variable and never echoes its value or the URL.
pub fn resolve_base_url(base_url: &str, env: &HashMap<String, String>) -> Result<String, AiError> {
    if !base_url.contains(CLOUDFLARE_ACCOUNT_ID_PLACEHOLDER)
        && !base_url.contains(CLOUDFLARE_GATEWAY_ID_PLACEHOLDER)
    {
        return Ok(base_url.to_string());
    }

    let mut resolved = base_url.to_string();
    if resolved.contains(CLOUDFLARE_ACCOUNT_ID_PLACEHOLDER) {
        resolved = resolved.replace(
            CLOUDFLARE_ACCOUNT_ID_PLACEHOLDER,
            env_value(env, CLOUDFLARE_ACCOUNT_ID_ENV)?,
        );
    }
    if resolved.contains(CLOUDFLARE_GATEWAY_ID_PLACEHOLDER) {
        resolved = resolved.replace(
            CLOUDFLARE_GATEWAY_ID_PLACEHOLDER,
            env_value(env, CLOUDFLARE_GATEWAY_ID_ENV)?,
        );
    }
    Ok(resolved)
}

/// Look up `name` in `env`, rejecting missing or blank values. The error names
/// the variable only, never the value.
fn env_value<'a>(env: &'a HashMap<String, String>, name: &str) -> Result<&'a str, AiError> {
    env.get(name)
        .map(String::as_str)
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| {
            AiError::Provider(format!(
                "Missing environment variable {name} for Cloudflare base URL"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::StreamOptions;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    const ACCOUNT_URL: &str =
        "https://api.cloudflare.com/client/v4/accounts/{CLOUDFLARE_ACCOUNT_ID}/ai/v1";
    const GATEWAY_URL: &str = "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/\
                               {CLOUDFLARE_GATEWAY_ID}/anthropic";

    #[test]
    fn substitutes_account_id_only() {
        let resolved =
            resolve_base_url(ACCOUNT_URL, &map(&[(CLOUDFLARE_ACCOUNT_ID_ENV, "acc-123")]))
                .expect("account id substitution");
        assert_eq!(
            resolved,
            "https://api.cloudflare.com/client/v4/accounts/acc-123/ai/v1"
        );
    }

    #[test]
    fn substitutes_gateway_id() {
        let resolved = resolve_base_url(
            GATEWAY_URL,
            &map(&[
                (CLOUDFLARE_ACCOUNT_ID_ENV, "acc-123"),
                (CLOUDFLARE_GATEWAY_ID_ENV, "gw-456"),
            ]),
        )
        .expect("gateway substitution");
        assert_eq!(
            resolved,
            "https://gateway.ai.cloudflare.com/v1/acc-123/gw-456/anthropic"
        );
    }

    #[test]
    fn resolves_from_stream_options_env() {
        let options = StreamOptions {
            env: map(&[
                (CLOUDFLARE_ACCOUNT_ID_ENV, "acc-123"),
                (CLOUDFLARE_GATEWAY_ID_ENV, "gw-456"),
            ]),
            ..Default::default()
        };
        let resolved = resolve_base_url(GATEWAY_URL, &options.env).expect("env lookup");
        assert_eq!(
            resolved,
            "https://gateway.ai.cloudflare.com/v1/acc-123/gw-456/anthropic"
        );
    }

    #[test]
    fn missing_account_id_error_names_variable_not_value() {
        let err = resolve_base_url(ACCOUNT_URL, &map(&[])).expect_err("missing account id");
        let message = err.to_string();
        assert!(
            message.contains(CLOUDFLARE_ACCOUNT_ID_ENV),
            "error should name {CLOUDFLARE_ACCOUNT_ID_ENV}: {message}"
        );
        assert!(
            !message.contains("api.cloudflare.com"),
            "error should not leak the URL: {message}"
        );
    }

    #[test]
    fn missing_gateway_id_error_names_variable_not_value() {
        let err = resolve_base_url(GATEWAY_URL, &map(&[(CLOUDFLARE_ACCOUNT_ID_ENV, "acc-123")]))
            .expect_err("missing gateway id");
        let message = err.to_string();
        assert!(
            message.contains(CLOUDFLARE_GATEWAY_ID_ENV),
            "error should name {CLOUDFLARE_GATEWAY_ID_ENV}: {message}"
        );
        assert!(
            !message.contains("acc-123"),
            "error should not leak the value: {message}"
        );
        assert!(
            !message.contains("gateway.ai.cloudflare.com"),
            "error should not leak the URL: {message}"
        );
    }

    #[test]
    fn blank_value_is_treated_as_missing() {
        let err = resolve_base_url(ACCOUNT_URL, &map(&[(CLOUDFLARE_ACCOUNT_ID_ENV, "  ")]))
            .expect_err("blank account id");
        assert!(err.to_string().contains(CLOUDFLARE_ACCOUNT_ID_ENV));
    }

    #[test]
    fn preserves_url_without_placeholders() {
        for url in [
            "https://api.openai.com/v1",
            "https://gateway.ai.cloudflare.com/v1/acc/gw/anthropic",
            "https://example.test/path?q=1",
        ] {
            let resolved = resolve_base_url(url, &map(&[])).expect("passthrough");
            assert_eq!(resolved, url);
        }
    }
}
