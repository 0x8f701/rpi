//! Model/provider fallback-chain resolution and diagnostics.
//!
//! Mirrors installed OMP `@oh-my-pi/pi-coding-agent` sources:
//! - `src/session/retry-fallback-chains.ts`
//! - `src/session/turn-recovery.ts` (`#tryRetryModelFallback`, hard-error eligibility)
//!
//! Provider transport retries remain in `pi_ai` (`send_with_retry` / `StreamOptions`).
//! Session same-model auto-retry remains in [`crate::Session`]'s `execute_with_retries`.
//! This module owns only ordered model/provider failover above those layers.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use pi_ai::Model;
use regex::Regex;


/// Configured fallback chains keyed by role, exact model selector, or provider wildcard.
pub type RetryFallbackChains = BTreeMap<String, Vec<String>>;


/// Parsed model selector used by retry fallback resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryFallbackSelector {
    pub raw: String,
    pub provider: String,
    pub id: String,
    pub thinking_level: Option<String>,
}

/// Minimal model lookup needed by fallback-chain resolution.
pub trait RetryFallbackModelLookup {
    fn find(&self, provider: &str, id: &str) -> Option<Model>;
    fn has_provider(&self, provider: &str) -> bool;
}

/// Catalog-backed lookup used by production sessions.
#[derive(Clone, Copy, Debug, Default)]
pub struct CatalogModelLookup;

impl RetryFallbackModelLookup for CatalogModelLookup {
    fn find(&self, provider: &str, id: &str) -> Option<Model> {
        pi_ai::get_model(provider, id)
    }

    fn has_provider(&self, provider: &str) -> bool {
        let needle = provider.to_ascii_lowercase();
        pi_ai::get_providers()
            .into_iter()
            .any(|name| name.eq_ignore_ascii_case(&needle))
    }
}

/// In-memory lookup used by focused tests.
#[derive(Clone, Debug, Default)]
pub struct MapModelLookup {
    models: HashMap<(String, String), Model>,
    providers: BTreeSet<String>,
}

impl MapModelLookup {
    #[must_use]
    pub fn new(models: impl IntoIterator<Item = Model>) -> Self {
        let mut lookup = Self::default();
        for model in models {
            lookup.insert(model);
        }
        lookup
    }

    pub fn insert(&mut self, model: Model) {
        self.providers
            .insert(model.provider.to_ascii_lowercase());
        self.models.insert(
            (
                model.provider.to_ascii_lowercase(),
                model.id.to_ascii_lowercase(),
            ),
            model,
        );
    }
}

impl RetryFallbackModelLookup for MapModelLookup {
    fn find(&self, provider: &str, id: &str) -> Option<Model> {
        self.models
            .get(&(provider.to_ascii_lowercase(), id.to_ascii_lowercase()))
            .cloned()
    }

    fn has_provider(&self, provider: &str) -> bool {
        self.providers.contains(&provider.to_ascii_lowercase())
    }
}

/// Inputs shared by startup and runtime fallback-chain resolution.
#[derive(Clone, Debug)]
pub struct RetryFallbackResolutionContext<'a, L: RetryFallbackModelLookup> {
    pub chains: &'a RetryFallbackChains,
    pub model_roles: &'a BTreeMap<String, String>,
    pub model_lookup: &'a L,
}

/// Active configured fallback chain retained while its selected model remains active.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveRetryFallbackState {
    pub role: String,
}

const THINKING_LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Formats a concrete model (and optional thinking level) as a fallback selector.
#[must_use]
pub fn format_retry_fallback_selector(model: &Model, thinking_level: Option<&str>) -> String {
    let base = format!("{}/{}", model.provider, model.id);
    match thinking_level.map(str::trim).filter(|level| !level.is_empty()) {
        Some(level) => format!("{base}:{level}"),
        None => base,
    }
}

/// Whether a fallback-chain key is a model selector rather than a role.
#[must_use]
pub fn is_retry_fallback_model_key(key: &str) -> bool {
    key.contains('/')
}

/// Whether a fallback-chain key or entry is a provider wildcard.
#[must_use]
pub fn is_retry_fallback_wildcard_key(key: &str) -> bool {
    key.ends_with("/*")
}

/// Splits a wildcard selector into provider and optional model-id prefix.
#[must_use]
pub fn parse_retry_fallback_wildcard(
    key: &str,
    is_known_provider: impl Fn(&str) -> bool,
) -> (String, Option<String>) {
    let template = key.trim_end_matches("/*");
    match template.find('/') {
        Some(slash) if !is_known_provider(template) => (
            template[..slash].to_owned(),
            Some(template[slash + 1..].to_owned()),
        ),
        _ => (template.to_owned(), None),
    }
}

/// Parses a configured retry fallback selector.
#[must_use]
pub fn parse_retry_fallback_selector(
    selector: &str,
    model_lookup: Option<&impl RetryFallbackModelLookup>,
) -> Option<RetryFallbackSelector> {
    let trimmed = selector.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut provider = String::new();
    let mut id = trimmed.to_owned();
    if let Some(slash) = trimmed.find('/') {
        let prefix = &trimmed[..slash];
        let rest = &trimmed[slash + 1..];
        let known = model_lookup.is_some_and(|lookup| lookup.has_provider(prefix))
            || rest.is_empty()
            || !rest.contains('/');
        if known || model_lookup.is_none() {
            provider = prefix.to_owned();
            id = rest.to_owned();
        }
    }
    if provider.is_empty() || id.is_empty() {
        return None;
    }

    let (id, thinking_level) = split_thinking_suffix(&id, model_lookup, &provider);
    Some(RetryFallbackSelector {
        raw: trimmed.to_owned(),
        provider,
        id,
        thinking_level,
    })
}

fn split_thinking_suffix(
    id: &str,
    model_lookup: Option<&impl RetryFallbackModelLookup>,
    provider: &str,
) -> (String, Option<String>) {
    let Some(colon) = id.rfind(':') else {
        return (id.to_owned(), None);
    };
    let (base, suffix) = id.split_at(colon);
    let level = &suffix[1..];
    if base.is_empty() || !THINKING_LEVELS.iter().any(|known| known.eq_ignore_ascii_case(level)) {
        return (id.to_owned(), None);
    }
    if let Some(lookup) = model_lookup {
        if lookup.find(provider, id).is_some() && lookup.find(provider, base).is_none() {
            return (id.to_owned(), None);
        }
    }
    (base.to_owned(), Some(level.to_ascii_lowercase()))
}

/// Apply the configured default chain to roles without their own chain.
#[must_use]
pub fn expand_default_retry_fallback_chains(
    configured_chains: &RetryFallbackChains,
    role_names: impl IntoIterator<Item = impl AsRef<str>>,
) -> RetryFallbackChains {
    let mut chains = configured_chains.clone();
    let Some(default_chain) = chains.get("default").cloned() else {
        return chains;
    };
    for role in role_names {
        let role = role.as_ref();
        if role != "default" && !chains.contains_key(role) {
            chains.insert(role.to_owned(), default_chain.clone());
        }
    }
    chains
}

/// Validates configured fallback chains and returns each actionable diagnostic.
#[must_use]
pub fn validate_retry_fallback_chains(
    chains: &RetryFallbackChains,
    model_lookup: &impl RetryFallbackModelLookup,
) -> Vec<String> {
    let mut warnings = Vec::new();
    for (key, chain) in chains {
        let key_kind = if is_retry_fallback_model_key(key) {
            "model"
        } else {
            "role"
        };
        if is_retry_fallback_model_key(key) {
            if is_retry_fallback_wildcard_key(key) {
                let (provider, _) = parse_retry_fallback_wildcard(key, |candidate| {
                    model_lookup.has_provider(candidate)
                });
                if !model_lookup.has_provider(&provider) {
                    warnings.push(format!(
                        "retry.fallbackChains wildcard key references unknown provider: {key}"
                    ));
                }
            } else if let Some(parsed) = parse_retry_fallback_selector(key, Some(model_lookup)) {
                if model_lookup.find(&parsed.provider, &parsed.id).is_none() {
                    warnings.push(format!(
                        "retry.fallbackChains key references unknown model: {key}"
                    ));
                }
            } else {
                warnings.push(format!(
                    "Invalid model selector key in retry.fallbackChains: {key}"
                ));
            }
        }
        if chain.is_empty() {
            warnings.push(format!(
                "Fallback chain for {key_kind} '{key}' must be a non-empty array of selector strings."
            ));
            continue;
        }
        for selector in chain {
            if selector.trim().is_empty() {
                warnings.push(format!(
                    "Fallback chain for {key_kind} '{key}' contains an empty selector."
                ));
                continue;
            }
            if is_retry_fallback_wildcard_key(selector) {
                let (provider, _) = parse_retry_fallback_wildcard(selector, |candidate| {
                    model_lookup.has_provider(candidate)
                });
                if !model_lookup.has_provider(&provider) {
                    warnings.push(format!(
                        "Fallback chain for {key_kind} '{key}' references unknown provider: {selector}"
                    ));
                }
                continue;
            }
            let Some(parsed) = parse_retry_fallback_selector(selector, Some(model_lookup)) else {
                warnings.push(format!(
                    "Invalid fallback selector format in {key_kind} '{key}': {selector}"
                ));
                continue;
            };
            if model_lookup.find(&parsed.provider, &parsed.id).is_none() {
                warnings.push(format!(
                    "Fallback chain for {key_kind} '{key}' references unknown model: {selector}"
                ));
            }
        }
    }
    warnings
}

/// Resolve the chain key for a concrete selector by specificity.
#[must_use]
pub fn resolve_retry_fallback_chain_key<L: RetryFallbackModelLookup>(
    context: &RetryFallbackResolutionContext<'_, L>,
    current_selector: &str,
    current_model: Option<&Model>,
    role_hint: Option<&str>,
) -> Option<String> {
    let parsed_configured =
        parse_retry_fallback_selector(current_selector, Some(context.model_lookup));
    let current_plain_selector = current_model.map(|model| {
        format_retry_fallback_selector(
            model,
            parsed_configured
                .as_ref()
                .and_then(|value| value.thinking_level.as_deref()),
        )
    });
    let parsed_current = parsed_configured.clone().or_else(|| {
        current_plain_selector.as_deref().and_then(|selector| {
            parse_retry_fallback_selector(selector, Some(context.model_lookup))
        })
    });
    let Some(parsed_current) = parsed_current else {
        if let Some(role) = role_hint.filter(|role| context.chains.contains_key(*role)) {
            return Some(role.to_owned());
        }
        return None;
    };
    let current_base_selector = format_retry_fallback_base_selector(&parsed_current);
    let current_plain_base_selector = current_plain_selector
        .as_deref()
        .filter(|plain| *plain != current_selector)
        .and_then(|plain| parse_retry_fallback_selector(plain, Some(context.model_lookup)))
        .map(|selector| format_retry_fallback_base_selector(&selector));

    for key in context.chains.keys() {
        if is_retry_fallback_model_key(key)
            && !is_retry_fallback_wildcard_key(key)
            && selector_matches_current(
                get_retry_fallback_primary_selector(context, key).as_ref(),
                current_selector,
                &current_base_selector,
                current_plain_selector.as_deref(),
                current_plain_base_selector.as_deref(),
            )
        {
            return Some(key.clone());
        }
    }

    let mut wildcard_match: Option<String> = None;
    let mut wildcard_prefix_length = -1_i32;
    for key in context.chains.keys() {
        if !is_retry_fallback_wildcard_key(key) {
            continue;
        }
        let (provider, id_prefix) = parse_retry_fallback_wildcard(key, |candidate| {
            context.model_lookup.has_provider(candidate)
        });
        if !provider.eq_ignore_ascii_case(&parsed_current.provider) {
            continue;
        }
        if let Some(prefix) = id_prefix.as_deref() {
            if !parsed_current.id.starts_with(&format!("{prefix}/")) {
                continue;
            }
        }
        let prefix_length = id_prefix.as_ref().map_or(0, String::len) as i32;
        if prefix_length > wildcard_prefix_length {
            wildcard_match = Some(key.clone());
            wildcard_prefix_length = prefix_length;
        }
    }
    if let Some(key) = wildcard_match {
        return Some(key);
    }

    if let Some(role) = role_hint.filter(|role| context.chains.contains_key(*role)) {
        return Some(role.to_owned());
    }
    for key in context.chains.keys() {
        if is_retry_fallback_model_key(key) {
            continue;
        }
        if selector_matches_current(
            get_retry_fallback_primary_selector(context, key).as_ref(),
            current_selector,
            &current_base_selector,
            current_plain_selector.as_deref(),
            current_plain_base_selector.as_deref(),
        ) {
            return Some(key.clone());
        }
    }

    if context
        .chains
        .get("default")
        .is_some_and(|chain| !chain.is_empty())
        && get_retry_fallback_primary_selector(context, "default").is_none()
    {
        return Some("default".to_owned());
    }
    None
}

/// Return the candidates after the current selector in an effective chain.
#[must_use]
pub fn find_retry_fallback_candidates<L: RetryFallbackModelLookup>(
    context: &RetryFallbackResolutionContext<'_, L>,
    chain_key: &str,
    current_selector: &str,
    current_model: Option<&Model>,
    allow_missing_primary: bool,
) -> Vec<RetryFallbackSelector> {
    let chain = get_retry_fallback_effective_chain(
        context,
        chain_key,
        current_selector,
        current_model,
        allow_missing_primary,
    );
    let parsed_configured =
        parse_retry_fallback_selector(current_selector, Some(context.model_lookup));
    let current_plain_selector = current_model.map(|model| {
        format_retry_fallback_selector(
            model,
            parsed_configured
                .as_ref()
                .and_then(|value| value.thinking_level.as_deref()),
        )
    });
    let parsed_current = parsed_configured.clone().or_else(|| {
        current_plain_selector.as_deref().and_then(|selector| {
            parse_retry_fallback_selector(selector, Some(context.model_lookup))
        })
    });
    let Some(parsed_current) = parsed_current else {
        return chain;
    };
    if chain.len() <= 1 {
        return Vec::new();
    }
    let current_base_selector = format_retry_fallback_base_selector(&parsed_current);
    let current_plain_base_selector = current_plain_selector
        .as_deref()
        .filter(|plain| *plain != current_selector)
        .and_then(|plain| parse_retry_fallback_selector(plain, Some(context.model_lookup)))
        .map(|selector| format_retry_fallback_base_selector(&selector));

    if let Some(exact_index) = chain.iter().position(|selector| {
        selector.raw == current_selector
            || current_plain_selector
                .as_deref()
                .is_some_and(|plain| selector.raw == plain)
    }) {
        return chain[exact_index + 1..].to_vec();
    }
    if let Some(base_index) = chain.iter().position(|selector| {
        let selector_base = format_retry_fallback_base_selector(selector);
        selector_base == current_base_selector
            || current_plain_base_selector
                .as_deref()
                .is_some_and(|plain| selector_base == plain)
    }) {
        return chain[base_index + 1..].to_vec();
    }
    chain[1..].to_vec()
}

/// Redacts secret-shaped fragments from terminal retry diagnostics.
#[must_use]
pub fn redact_retry_diagnostic(text: &str) -> String {
    let mut out = text.to_owned();
    for (pattern, replacement) in [
        (
            r#"(?i)\b(AWS_(?:ACCESS_KEY_ID|SECRET_ACCESS_KEY|SESSION_TOKEN))\s*[:=]\s*(?:"[^"]*"|'[^']*'|\S+)"#,
            "$1=[REDACTED]",
        ),
        (
            r"(?i)(api[_-]?key|token|secret|password|authorization)\s*[:=]\s*(?:bearer\s+)?\S+",
            "$1=[REDACTED]",
        ),
        (r"(?i)bearer\s+[a-z0-9._\-]+", "Bearer [REDACTED]"),
        (r"\bAKIA[0-9A-Z]{16}\b", "[REDACTED]"),
        (
            r"(?i)\b(?:sk|pk|rk|ghp|gho|ghu|ghs|ghr|xox[baprs])[-_][A-Za-z0-9\-_]{8,}\b",
            "[REDACTED]",
        ),
    ] {
        if let Ok(re) = Regex::new(pattern) {
            out = re.replace_all(&out, replacement).into_owned();
        }
    }
    out
}

/// Aggregates exhausted retry/fallback diagnostics with secret redaction.
#[must_use]
pub fn aggregate_retry_diagnostics(errors: &[String]) -> String {
    let mut seen = BTreeSet::new();
    let mut parts = Vec::new();
    for error in errors {
        let redacted = redact_retry_diagnostic(error.trim());
        if redacted.is_empty() || !seen.insert(redacted.clone()) {
            continue;
        }
        parts.push(redacted);
    }
    if parts.is_empty() {
        "provider error".to_owned()
    } else {
        parts.join(" | ")
    }
}

/// True when a hard (non-retryable) error may still consult a fallback chain.
#[must_use]
pub fn is_hard_error_fallback_eligible(
    stop_reason_error: bool,
    is_retryable: bool,
    has_tool_call: bool,
    is_context_overflow: bool,
    is_abort: bool,
    model_fallback_enabled: bool,
    has_candidates: bool,
) -> bool {
    stop_reason_error
        && !is_retryable
        && !has_tool_call
        && !is_context_overflow
        && !is_abort
        && model_fallback_enabled
        && has_candidates
}

fn format_retry_fallback_base_selector(selector: &RetryFallbackSelector) -> String {
    format!("{}/{}", selector.provider, selector.id)
}

fn get_retry_fallback_primary_selector<L: RetryFallbackModelLookup>(
    context: &RetryFallbackResolutionContext<'_, L>,
    chain_key: &str,
) -> Option<RetryFallbackSelector> {
    if is_retry_fallback_wildcard_key(chain_key) {
        return None;
    }
    if is_retry_fallback_model_key(chain_key) {
        return parse_retry_fallback_selector(chain_key, Some(context.model_lookup));
    }
    context
        .model_roles
        .get(chain_key)
        .and_then(|selector| parse_retry_fallback_selector(selector, Some(context.model_lookup)))
}

fn selector_matches_current(
    primary: Option<&RetryFallbackSelector>,
    current_selector: &str,
    current_base_selector: &str,
    current_plain_selector: Option<&str>,
    current_plain_base_selector: Option<&str>,
) -> bool {
    let Some(primary) = primary else {
        return false;
    };
    if primary.raw == current_selector
        || current_plain_selector.is_some_and(|plain| primary.raw == plain)
    {
        return true;
    }
    let base = format_retry_fallback_base_selector(primary);
    base == current_base_selector || current_plain_base_selector.is_some_and(|plain| base == plain)
}

fn parse_retry_fallback_chain_entry<L: RetryFallbackModelLookup>(
    context: &RetryFallbackResolutionContext<'_, L>,
    entry: &str,
    current: Option<&RetryFallbackSelector>,
) -> Option<RetryFallbackSelector> {
    if !is_retry_fallback_wildcard_key(entry) {
        return parse_retry_fallback_selector(entry, Some(context.model_lookup));
    }
    let current = current?;
    let (provider, id_prefix) = parse_retry_fallback_wildcard(entry, |candidate| {
        context.model_lookup.has_provider(candidate)
    });
    let bare_id = current
        .id
        .rsplit('/')
        .next()
        .unwrap_or(current.id.as_str())
        .to_owned();
    let id = if let Some(prefix) = id_prefix {
        format!("{prefix}/{bare_id}")
    } else if bare_id != current.id
        && context.model_lookup.find(&provider, &current.id).is_none()
        && context.model_lookup.find(&provider, &bare_id).is_some()
    {
        bare_id
    } else {
        current.id.clone()
    };
    Some(RetryFallbackSelector {
        raw: format!("{provider}/{id}"),
        provider,
        id,
        thinking_level: None,
    })
}

fn get_retry_fallback_effective_chain<L: RetryFallbackModelLookup>(
    context: &RetryFallbackResolutionContext<'_, L>,
    chain_key: &str,
    current_selector: &str,
    current_model: Option<&Model>,
    allow_missing_primary: bool,
) -> Vec<RetryFallbackSelector> {
    let parsed_configured =
        parse_retry_fallback_selector(current_selector, Some(context.model_lookup));
    let parsed_current = parsed_configured.clone().or_else(|| {
        current_model.map(|model| RetryFallbackSelector {
            raw: format!("{}/{}", model.provider, model.id),
            provider: model.provider.clone(),
            id: model.id.clone(),
            thinking_level: None,
        })
    });
    let mut seen = BTreeSet::new();
    let mut chain = Vec::new();
    if is_retry_fallback_wildcard_key(chain_key) {
        if let Some(current) = parsed_current.clone() {
            seen.insert(current.raw.clone());
            chain.push(current);
        }
    } else if let Some(primary) = get_retry_fallback_primary_selector(context, chain_key) {
        seen.insert(primary.raw.clone());
        chain.push(primary);
    } else if (chain_key == "default" || allow_missing_primary)
        && let Some(current) = parsed_current.clone()
    {
        seen.insert(current.raw.clone());
        chain.push(current);
    } else if !allow_missing_primary {
        return Vec::new();
    }

    for selector in context.chains.get(chain_key).into_iter().flatten() {
        let Some(parsed) =
            parse_retry_fallback_chain_entry(context, selector, parsed_current.as_ref())
        else {
            continue;
        };
        if !seen.insert(parsed.raw.clone()) {
            continue;
        }
        chain.push(parsed);
    }
    chain
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(provider: &str, id: &str) -> Model {
        Model {
            id: id.to_owned(),
            name: id.to_owned(),
            provider: provider.to_owned(),
            api: "test".to_owned(),
            base_url: "http://localhost".to_owned(),
            ..Model::default()
        }
    }

    fn lookup() -> MapModelLookup {
        MapModelLookup::new([
            model("primary", "main"),
            model("backup", "spare"),
            model("backup", "other"),
            model("google", "gemini-x"),
            model("openrouter", "google/gemini-x"),
        ])
    }

    #[test]
    fn resolves_default_chain_and_skips_primary_success_path() {
        let chains = BTreeMap::from([(
            "default".to_owned(),
            vec!["backup/spare".to_owned(), "backup/other".to_owned()],
        )]);
        let roles = BTreeMap::new();
        let models = lookup();
        let context = RetryFallbackResolutionContext {
            chains: &chains,
            model_roles: &roles,
            model_lookup: &models,
        };
        let key = resolve_retry_fallback_chain_key(
            &context,
            "primary/main",
            Some(&model("primary", "main")),
            None,
        );
        assert_eq!(key.as_deref(), Some("default"));
        let candidates = find_retry_fallback_candidates(
            &context,
            "default",
            "primary/main",
            Some(&model("primary", "main")),
            true,
        );
        assert_eq!(
            candidates
                .iter()
                .map(|selector| selector.raw.as_str())
                .collect::<Vec<_>>(),
            ["backup/spare", "backup/other"]
        );
    }

    #[test]
    fn provider_wildcard_entry_swaps_provider_and_keeps_id() {
        let chains = BTreeMap::from([("google/*".to_owned(), vec!["openrouter/*".to_owned()])]);
        let roles = BTreeMap::new();
        let models = lookup();
        let context = RetryFallbackResolutionContext {
            chains: &chains,
            model_roles: &roles,
            model_lookup: &models,
        };
        let key = resolve_retry_fallback_chain_key(
            &context,
            "google/gemini-x",
            Some(&model("google", "gemini-x")),
            None,
        )
        .expect("wildcard key");
        assert_eq!(key, "google/*");
        let candidates = find_retry_fallback_candidates(
            &context,
            &key,
            "google/gemini-x",
            Some(&model("google", "gemini-x")),
            false,
        );
        assert_eq!(candidates[0].raw, "openrouter/gemini-x");
    }

    #[test]
    fn invalid_entries_produce_actionable_diagnostics() {
        let chains = BTreeMap::from([(
            "default".to_owned(),
            vec![
                String::new(),
                "not-a-selector".to_owned(),
                "missing/model".to_owned(),
            ],
        )]);
        let warnings = validate_retry_fallback_chains(&chains, &lookup());
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("empty selector"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("Invalid fallback selector"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("unknown model"))
        );
    }

    #[test]
    fn diagnostics_redact_secrets_and_dedupe() {
        let message = aggregate_retry_diagnostics(&[
            "Authorization: Bearer sk-abc1234567890secret".to_owned(),
            "Authorization: Bearer sk-abc1234567890secret".to_owned(),
            "api_key=super-secret-value".to_owned(),
        ]);
        assert!(!message.contains("sk-abc"));
        assert!(!message.contains("super-secret-value"));
        assert!(message.contains("[REDACTED]"));
    }
}
