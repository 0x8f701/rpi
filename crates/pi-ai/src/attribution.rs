//! Attribution defaults for provider/model request headers.
//!
//! Faithful port of upstream `packages/coding-agent/src/core/provider-attribution.ts`
//! (earendil-works/pi, commit 83cbfc6521761a2a071f1bf335c4ed709580ab57):
//!
//! - OpenRouter (`provider == "openrouter"` or base URL containing
//!   `openrouter.ai`): `HTTP-Referer`, `X-OpenRouter-Title`,
//!   `X-OpenRouter-Categories`.
//! - OpenCode (`provider == "opencode" | "opencode-go"` or base URL host
//!   `opencode.ai`): `x-opencode-session` / `x-opencode-client` when a session
//!   id is present.
//! - Cloudflare (`provider == "cloudflare-workers-ai" | "cloudflare-ai-gateway"`
//!   or base URL host `api.cloudflare.com` / `gateway.ai.cloudflare.com`):
//!   default `User-Agent`.
//!
//! The OpenRouter and Cloudflare defaults are gated on install telemetry
//! upstream (`isInstallTelemetryEnabled`). No source-verified setting is wired
//! into this port yet, so they are deferred: callers pass
//! `install_telemetry_enabled = false` and only the non-gated OpenCode session
//! headers are emitted. They are never silently enabled.
//!
//! The module is pure: it performs no network or environment access. The
//! caller passes the install-telemetry flag and session id explicitly.
//! Caller-supplied headers always win; defaults only fill missing keys.

use std::collections::HashMap;

use crate::Model;

const OPENROUTER_HOST: &str = "openrouter.ai";
const OPENCODE_HOST: &str = "opencode.ai";
const CLOUDFLARE_API_HOST: &str = "api.cloudflare.com";
const CLOUDFLARE_AI_GATEWAY_HOST: &str = "gateway.ai.cloudflare.com";

const OPENROUTER_REFERER: &str = "https://pi.dev";
const OPENROUTER_TITLE: &str = "rpi";
const OPENROUTER_CATEGORIES: &str = "cli-agent";
const OPENCODE_CLIENT: &str = "rpi";
const CLOUDFLARE_USER_AGENT: &str = "rpi-coding-agent";

/// Lowercase hostname of `url`, or `None` when the URL cannot be parsed.
///
/// Mirrors upstream `new URL(baseUrl).hostname`: a scheme (`://`) is required
/// and the hostname is matched case-insensitively. Base URLs in the model
/// catalog are all absolute (`https://host/...`).
fn hostname(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let rest = rest.split_once('@').map(|(_, rest)| rest).unwrap_or(rest);
    let host = rest.split(|c| c == '/' || c == '?' || c == '#').next()?;
    let host = host.trim();
    if host.is_empty() { None } else { Some(host) }
}

fn is_openrouter(model: &Model) -> bool {
    model.provider == "openrouter" || model.base_url.contains(OPENROUTER_HOST)
}

fn is_opencode(model: &Model) -> bool {
    model.provider == "opencode"
        || model.provider == "opencode-go"
        || hostname(&model.base_url).is_some_and(|host| host.eq_ignore_ascii_case(OPENCODE_HOST))
}

fn is_cloudflare(model: &Model) -> bool {
    model.provider == "cloudflare-workers-ai"
        || model.provider == "cloudflare-ai-gateway"
        || hostname(&model.base_url)
            .is_some_and(|host| host.eq_ignore_ascii_case(CLOUDFLARE_API_HOST))
        || hostname(&model.base_url)
            .is_some_and(|host| host.eq_ignore_ascii_case(CLOUDFLARE_AI_GATEWAY_HOST))
}

/// Merge attribution defaults for a provider/model with caller-supplied
/// headers, filling only missing keys.
///
/// Returns `None` when no headers would be sent at all. Precedence (lowest to
/// highest) matches upstream `mergeProviderAttributionHeaders`:
///
/// 1. OpenCode session/client headers (when `session_id` is present);
/// 2. install-telemetry-gated defaults (OpenRouter, then Cloudflare);
/// 3. caller-supplied `existing` headers.
///
/// `install_telemetry_enabled` mirrors upstream `isInstallTelemetryEnabled`
/// and gates the OpenRouter/Cloudflare defaults. No source-verified setting is
/// wired yet, so providers pass `false` — those defaults are deferred, not
/// silently enabled. OpenCode session headers are not gated and still apply.
/// This module never touches the environment itself.
pub fn merge_provider_attribution_headers(
    model: &Model,
    session_id: Option<&str>,
    install_telemetry_enabled: bool,
    existing: &HashMap<String, String>,
) -> Option<HashMap<String, String>> {
    let mut merged: HashMap<String, String> = HashMap::new();

    if is_opencode(model) {
        if let Some(session_id) = session_id.filter(|session| !session.is_empty()) {
            merged.insert("x-opencode-session".into(), session_id.to_string());
            merged.insert("x-opencode-client".into(), OPENCODE_CLIENT.into());
        }
    }

    if install_telemetry_enabled {
        if is_openrouter(model) {
            merged.insert("HTTP-Referer".into(), OPENROUTER_REFERER.into());
            merged.insert("X-OpenRouter-Title".into(), OPENROUTER_TITLE.into());
            merged.insert(
                "X-OpenRouter-Categories".into(),
                OPENROUTER_CATEGORIES.into(),
            );
        } else if is_cloudflare(model) {
            merged.insert("User-Agent".into(), CLOUDFLARE_USER_AGENT.into());
        }
    }

    merged.extend(
        existing
            .iter()
            .map(|(name, value)| (name.clone(), value.clone())),
    );

    if merged.is_empty() {
        None
    } else {
        Some(merged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Case {
        name: &'static str,
        provider: &'static str,
        base_url: &'static str,
        session_id: Option<&'static str>,
        telemetry: bool,
        existing: &'static [(&'static str, &'static str)],
        expected: Option<&'static [(&'static str, &'static str)]>,
    }

    fn model(provider: &str, base_url: &str) -> Model {
        Model {
            provider: provider.into(),
            base_url: base_url.into(),
            ..Model::default()
        }
    }

    fn existing(headers: &[(&str, &str)]) -> HashMap<String, String> {
        headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    fn sorted(mut headers: HashMap<String, String>) -> Vec<(String, String)> {
        let mut pairs: Vec<(String, String)> = headers.into_iter().collect();
        pairs.sort();
        pairs
    }

    fn run(case: &Case) -> Option<HashMap<String, String>> {
        merge_provider_attribution_headers(
            &model(case.provider, case.base_url),
            case.session_id,
            case.telemetry,
            &existing(case.existing),
        )
    }

    const OPENROUTER_DEFAULTS: &[(&str, &str)] = &[
        ("HTTP-Referer", "https://pi.dev"),
        ("X-OpenRouter-Title", "rpi"),
        ("X-OpenRouter-Categories", "cli-agent"),
    ];
    const SESSION_DEFAULTS: &[(&str, &str)] = &[
        ("x-opencode-session", "opencode-session"),
        ("x-opencode-client", "rpi"),
    ];

    fn cases() -> Vec<Case> {
        vec![
            Case {
                name: "openrouter_provider_defaults",
                provider: "openrouter",
                base_url: "https://openrouter.ai/api/v1",
                session_id: None,
                telemetry: true,
                existing: &[],
                expected: Some(OPENROUTER_DEFAULTS),
            },
            Case {
                name: "openrouter_host_substring",
                provider: "custom-openrouter",
                base_url: "https://openrouter.ai/api/v1",
                session_id: None,
                telemetry: true,
                existing: &[],
                expected: Some(OPENROUTER_DEFAULTS),
            },
            Case {
                name: "openrouter_legacy_substring",
                provider: "custom-openrouter",
                base_url: "not-a-url-openrouter.ai",
                session_id: None,
                telemetry: true,
                existing: &[],
                expected: Some(OPENROUTER_DEFAULTS),
            },
            Case {
                name: "openrouter_telemetry_disabled",
                provider: "openrouter",
                base_url: "https://openrouter.ai/api/v1",
                session_id: None,
                telemetry: false,
                existing: &[],
                expected: None,
            },
            Case {
                name: "openrouter_provider_headers_override",
                provider: "openrouter",
                base_url: "https://openrouter.ai/api/v1",
                session_id: None,
                telemetry: true,
                existing: &[
                    ("HTTP-Referer", "https://provider.example"),
                    ("X-OpenRouter-Categories", "provider-category"),
                ],
                expected: Some(&[
                    ("HTTP-Referer", "https://provider.example"),
                    ("X-OpenRouter-Title", "rpi"),
                    ("X-OpenRouter-Categories", "provider-category"),
                ]),
            },
            Case {
                name: "openrouter_request_headers_override",
                provider: "openrouter",
                base_url: "https://openrouter.ai/api/v1",
                session_id: None,
                telemetry: true,
                existing: &[("X-OpenRouter-Title", "request-title")],
                expected: Some(&[
                    ("HTTP-Referer", "https://pi.dev"),
                    ("X-OpenRouter-Title", "request-title"),
                    ("X-OpenRouter-Categories", "cli-agent"),
                ]),
            },
            Case {
                name: "openrouter_precedes_cloudflare",
                provider: "openrouter",
                base_url: "https://api.cloudflare.com",
                session_id: None,
                telemetry: true,
                existing: &[],
                expected: Some(OPENROUTER_DEFAULTS),
            },
            Case {
                name: "caller_header_case_is_preserved",
                provider: "openrouter",
                base_url: "https://openrouter.ai/api/v1",
                session_id: None,
                telemetry: true,
                existing: &[("http-referer", "lower")],
                expected: Some(&[
                    ("http-referer", "lower"),
                    ("HTTP-Referer", "https://pi.dev"),
                    ("X-OpenRouter-Title", "rpi"),
                    ("X-OpenRouter-Categories", "cli-agent"),
                ]),
            },
            Case {
                name: "opencode_session_defaults",
                provider: "opencode",
                base_url: "https://opencode.ai/zen/v1",
                session_id: Some("opencode-session"),
                telemetry: true,
                existing: &[],
                expected: Some(SESSION_DEFAULTS),
            },
            Case {
                name: "opencode_hostname_match",
                provider: "custom-zen",
                base_url: "https://opencode.ai/zen/v1",
                session_id: Some("s1"),
                telemetry: true,
                existing: &[],
                expected: Some(&[("x-opencode-session", "s1"), ("x-opencode-client", "rpi")]),
            },
            Case {
                name: "opencode_go_provider",
                provider: "opencode-go",
                base_url: "https://opencode.example/v1",
                session_id: Some("s2"),
                telemetry: true,
                existing: &[],
                expected: Some(&[("x-opencode-session", "s2"), ("x-opencode-client", "rpi")]),
            },
            Case {
                name: "opencode_without_session",
                provider: "opencode",
                base_url: "https://opencode.ai/zen/v1",
                session_id: None,
                telemetry: true,
                existing: &[],
                expected: None,
            },
            Case {
                name: "empty_session_id_skipped",
                provider: "opencode",
                base_url: "https://opencode.ai/zen/v1",
                session_id: Some(""),
                telemetry: true,
                existing: &[],
                expected: None,
            },
            Case {
                name: "non_opencode_ignores_session",
                provider: "openai",
                base_url: "https://api.openai.com/v1",
                session_id: Some("s3"),
                telemetry: true,
                existing: &[],
                expected: None,
            },
            Case {
                name: "opencode_configured_headers_override",
                provider: "opencode",
                base_url: "https://opencode.ai/zen/v1",
                session_id: Some("opencode-session"),
                telemetry: true,
                existing: &[
                    ("x-opencode-session", "configured-session"),
                    ("x-opencode-client", "configured-client"),
                ],
                expected: Some(&[
                    ("x-opencode-session", "configured-session"),
                    ("x-opencode-client", "configured-client"),
                ]),
            },
            Case {
                name: "opencode_session_not_telemetry_gated",
                provider: "opencode",
                base_url: "https://opencode.ai/zen/v1",
                session_id: Some("s4"),
                telemetry: false,
                existing: &[],
                expected: Some(&[("x-opencode-session", "s4"), ("x-opencode-client", "rpi")]),
            },
            Case {
                name: "bare_host_without_scheme_not_matched",
                provider: "custom",
                base_url: "opencode.ai",
                session_id: Some("s"),
                telemetry: true,
                existing: &[],
                expected: None,
            },
            Case {
                name: "cloudflare_workers_ai_user_agent",
                provider: "cloudflare-workers-ai",
                base_url: "https://api.cloudflare.com/client/v4",
                session_id: None,
                telemetry: true,
                existing: &[],
                expected: Some(&[("User-Agent", "rpi-coding-agent")]),
            },
            Case {
                name: "cloudflare_ai_gateway_user_agent",
                provider: "cloudflare-ai-gateway",
                base_url: "https://gateway.ai.cloudflare.com/v1",
                session_id: None,
                telemetry: true,
                existing: &[],
                expected: Some(&[("User-Agent", "rpi-coding-agent")]),
            },
            Case {
                name: "cloudflare_api_hostname_match",
                provider: "custom-cf",
                base_url: "https://api.cloudflare.com/client/v4",
                session_id: None,
                telemetry: true,
                existing: &[],
                expected: Some(&[("User-Agent", "rpi-coding-agent")]),
            },
            Case {
                name: "cloudflare_gateway_hostname_match",
                provider: "custom-cf",
                base_url: "https://gateway.ai.cloudflare.com/v1/1234/workers-ai",
                session_id: None,
                telemetry: true,
                existing: &[],
                expected: Some(&[("User-Agent", "rpi-coding-agent")]),
            },
            Case {
                name: "cloudflare_uppercase_hostname",
                provider: "custom-cf",
                base_url: "HTTPS://GATEWAY.AI.CLOUDFLARE.COM/v1",
                session_id: None,
                telemetry: true,
                existing: &[],
                expected: Some(&[("User-Agent", "rpi-coding-agent")]),
            },
            Case {
                name: "cloudflare_telemetry_disabled",
                provider: "cloudflare-workers-ai",
                base_url: "https://api.cloudflare.com",
                session_id: None,
                telemetry: false,
                existing: &[],
                expected: None,
            },
            Case {
                name: "cloudflare_user_agent_override",
                provider: "cloudflare-workers-ai",
                base_url: "https://api.cloudflare.com",
                session_id: None,
                telemetry: true,
                existing: &[("User-Agent", "my-agent")],
                expected: Some(&[("User-Agent", "my-agent")]),
            },
            Case {
                name: "unrelated_provider_no_headers",
                provider: "together",
                base_url: "https://api.together.ai/v1",
                session_id: None,
                telemetry: true,
                existing: &[],
                expected: None,
            },
            Case {
                name: "existing_headers_preserved_without_defaults",
                provider: "together",
                base_url: "https://api.together.ai/v1",
                session_id: None,
                telemetry: true,
                existing: &[("x-custom", "v")],
                expected: Some(&[("x-custom", "v")]),
            },
            Case {
                name: "empty_base_url_no_defaults",
                provider: "custom",
                base_url: "",
                session_id: None,
                telemetry: true,
                existing: &[],
                expected: None,
            },
        ]
    }

    #[test]
    fn attribution_defaults_and_override_precedence() {
        for case in cases() {
            let actual = run(&case).map(sorted);
            let expected = case.expected.map(|headers| {
                let mut pairs: Vec<(String, String)> = headers
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.to_string()))
                    .collect();
                pairs.sort();
                pairs
            });
            assert_eq!(actual, expected, "case: {}", case.name);
        }
    }

    #[test]
    fn returns_none_when_no_headers_would_be_sent() {
        let empty = Case {
            name: "no_headers",
            provider: "custom",
            base_url: "https://example.test/v1",
            session_id: None,
            telemetry: true,
            existing: &[],
            expected: None,
        };
        assert_eq!(run(&empty), None);
    }
}
