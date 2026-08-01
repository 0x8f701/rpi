//! Model pattern resolution with thinking-level suffix parsing and custom-id
//! provider fallback. Port of `coding/resolve.go`.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use pi_ai::Model;

/// The model used when none is specified. The Go port has no settings manager,
/// so an empty spec resolves to this fixed default.
pub const DEFAULT_MODEL_SPEC: &str = "anthropic/claude-sonnet-4-5";

/// pi's `defaultModelPerProvider` (model-resolver.ts): the per-provider default
/// model id used by `build_fallback_model` when synthesizing a custom-id model.
static DEFAULT_MODEL_PER_PROVIDER: &[(&str, &str)] = &[
    ("amazon-bedrock", "us.anthropic.claude-opus-4-6-v1"),
    ("ant-ling", "Ring-2.6-1T"),
    ("anthropic", "claude-opus-4-8"),
    ("openai", "gpt-5.5"),
    ("azure-openai-responses", "gpt-5.4"),
    ("openai-codex", "gpt-5.5"),
    ("radius", "auto"),
    ("nvidia", "nvidia/nemotron-3-super-120b-a12b"),
    ("deepseek", "deepseek-v4-pro"),
    ("google", "gemini-3.1-pro-preview"),
    ("google-vertex", "gemini-3.1-pro-preview"),
    ("github-copilot", "gpt-5.4"),
    ("openrouter", "moonshotai/kimi-k2.6"),
    ("vercel-ai-gateway", "zai/glm-5.1"),
    ("xai", "grok-4.5"),
    ("groq", "openai/gpt-oss-120b"),
    ("cerebras", "zai-glm-4.7"),
    ("zai", "glm-5.1"),
    ("zai-coding-cn", "glm-5.1"),
    ("mistral", "devstral-medium-latest"),
    ("minimax", "MiniMax-M2.7"),
    ("minimax-cn", "MiniMax-M2.7"),
    ("moonshotai", "kimi-k2.6"),
    ("moonshotai-cn", "kimi-k2.6"),
    ("huggingface", "moonshotai/Kimi-K2.6"),
    ("fireworks", "accounts/fireworks/models/kimi-k2p6"),
    ("together", "moonshotai/Kimi-K2.6"),
    ("opencode", "kimi-k2.6"),
    ("opencode-go", "kimi-k2.6"),
    ("kimi-coding", "kimi-for-coding"),
    ("cloudflare-workers-ai", "@cf/moonshotai/kimi-k2.6"),
    ("cloudflare-ai-gateway", "workers-ai/@cf/moonshotai/kimi-k2.6"),
    ("qwen-token-plan", "qwen3.7-max"),
    ("qwen-token-plan-cn", "qwen3.7-max"),
    ("xiaomi", "mimo-v2.5-pro"),
    ("xiaomi-token-plan-cn", "mimo-v2.5-pro"),
    ("xiaomi-token-plan-ams", "mimo-v2.5-pro"),
    ("xiaomi-token-plan-sgp", "mimo-v2.5-pro"),
];

/// Looks up the per-provider default model id (pi `defaultModelPerProvider`).
pub fn default_model_per_provider(provider: &str) -> Option<&'static str> {
    DEFAULT_MODEL_PER_PROVIDER
        .iter()
        .find(|(p, _)| *p == provider)
        .map(|(_, id)| *id)
}

/// pi's `VALID_THINKING_LEVELS` (cli/args.ts:57).
const VALID_THINKING_LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh"];

fn is_valid_thinking_level(s: &str) -> bool {
    VALID_THINKING_LEVELS.contains(&s)
}

/// The result of resolving a model spec.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub model: Model,
    /// The level parsed from a `:<level>` suffix in the spec, or `""` when none.
    pub thinking_level: String,
    /// Carries pi's non-fatal resolution warnings (e.g. the custom-id fallback).
    pub warning: String,
}

/// Resolves a model spec to a `Model` from the catalog (an empty spec resolves to
/// `DEFAULT_MODEL_SPEC`). Source-compat shorthand for `resolve_model_pattern`.
pub fn resolve_model(spec: &str) -> Result<Model, String> {
    resolve_model_pattern(spec).map(|r| r.model)
}

/// Ports pi's `resolveCliModel` (model-resolver.ts): a slash-prefix is treated as a
/// provider ONLY when it matches a known provider; otherwise the whole string is
/// matched as a model id across providers (OpenRouter-style ids contain slashes).
/// Matching is case-insensitive and a `:<thinkingLevel>` suffix
/// (off|minimal|low|medium|high|xhigh) is parsed off and returned alongside. An
/// unknown model under a known provider falls back to a synthetic custom-id model
/// with a warning (pi `buildFallbackModel`); a thinking-level suffix is stripped
/// from the custom id first.
pub fn resolve_model_pattern(spec: &str) -> Result<ResolvedModel, String> {
    let spec = if spec.is_empty() { DEFAULT_MODEL_SPEC } else { spec };
    let available = all_catalog_models();
    if available.is_empty() {
        return Err("No models available. Check your installation or add models to models.json.".to_string());
    }

    // Canonical provider lookup (case-insensitive).
    let mut provider_by_lower: HashMap<String, String> = HashMap::new();
    for m in &available {
        provider_by_lower
            .entry(m.provider.to_lowercase())
            .or_insert_with(|| m.provider.clone());
    }

    let mut pattern = spec;
    let mut provider = String::new();
    let mut inferred_provider = false;

    // Interpret "provider/model" only when the prefix before the FIRST slash is a
    // known provider; otherwise the slash belongs to the model id.
    if let Some(slash) = spec.find('/') {
        if let Some(canonical) = provider_by_lower.get(&spec[..slash].to_lowercase()) {
            provider = canonical.clone();
            pattern = &spec[slash + 1..];
            inferred_provider = true;
        }
    }

    // No provider inferred: try exact matches on the raw input first (handles
    // model ids that naturally contain slashes).
    if provider.is_empty() {
        if let Some(m) = exact_model_match(spec, &available) {
            return Ok(ResolvedModel { model: m.clone(), thinking_level: String::new(), warning: String::new() });
        }
    }

    let candidates: Vec<&Model> = if provider.is_empty() {
        available.iter().collect()
    } else {
        available.iter().filter(|m| m.provider == provider).collect()
    };

    if let Some((m, level, warning)) = parse_model_pattern(pattern, &candidates, false) {
        return Ok(ResolvedModel { model: m.clone(), thinking_level: level, warning });
    }

    // Provider inferred from the slash but nothing matched within it: fall back to
    // matching the full input as a raw model id across all models.
    if inferred_provider {
        if let Some(m) = exact_model_match(spec, &available) {
            return Ok(ResolvedModel { model: m.clone(), thinking_level: String::new(), warning: String::new() });
        }
        let all_refs: Vec<&Model> = available.iter().collect();
        if let Some((m, lvl, warn)) = parse_model_pattern(spec, &all_refs, false) {
            return Ok(ResolvedModel { model: m.clone(), thinking_level: lvl, warning: warn });
        }
    }

    if !provider.is_empty() {
        // Parse a thinking-level suffix from the pattern before building the
        // fallback model, so the suffix neither leaks into the custom model id
        // nor into the warning.
        let (fallback_pattern, fallback_thinking) = match pattern.rfind(':') {
            Some(last_colon) => {
                let suffix = &pattern[last_colon + 1..];
                if is_valid_thinking_level(suffix) {
                    (&pattern[..last_colon], suffix.to_string())
                } else {
                    (pattern, String::new())
                }
            }
            None => (pattern, String::new()),
        };
        if let Some(mut fb) = build_fallback_model(&provider, fallback_pattern, &available) {
            // A requested thinking level (other than off) on a custom-id fallback
            // must set reasoning:true even when the template is non-reasoning, so
            // the level is honored.
            if !fallback_thinking.is_empty() && fallback_thinking != "off" {
                fb.reasoning = true;
            }
            let mut fb_warning = format!(
                "Model {:?} not found for provider {:?}. Using custom model id.",
                fallback_pattern, provider
            );
            // Mirror Go: concatenate the parse warning if non-empty (it is "" on
            // this path, since parse_model_pattern already returned None).
            let warning = parse_model_pattern(pattern, &candidates, false)
                .map(|(_, _, w)| w)
                .unwrap_or_default();
            if !warning.is_empty() {
                fb_warning = format!("{warning} {fb_warning}");
            }
            return Ok(ResolvedModel {
                model: fb,
                thinking_level: fallback_thinking,
                warning: fb_warning,
            });
        }
    }

    let display = if provider.is_empty() {
        spec.to_string()
    } else {
        format!("{provider}/{pattern}")
    };
    Err(format!(
        "Model \"{}\" not found. Use --list-models to see available models.",
        display
    ))
}

/// Lists every registered model in deterministic order (providers and ids sorted;
/// pi's registry order is models.json order).
fn all_catalog_models() -> Vec<Model> {
    let mut providers = pi_ai::get_providers();
    providers.sort();
    let mut out: Vec<Model> = Vec::new();
    for p in providers {
        let mut models = pi_ai::get_models(&p);
        models.sort_by(|a, b| a.id.cmp(&b.id));
        out.extend(models);
    }
    out
}

/// Finds a model whose id or "provider/id" equals the reference
/// case-insensitively (resolveCliModel's pre-provider exact check).
fn exact_model_match<'a>(reference: &str, models: &'a [Model]) -> Option<&'a Model> {
    let lower = reference.to_lowercase();
    for m in models {
        if m.id.to_lowercase() == lower
            || format!("{}/{}", m.provider, m.id).to_lowercase() == lower
        {
            return Some(m);
        }
    }
    None
}

/// Ports pi's `parseModelPattern`: try the full pattern as a model; otherwise
/// split on the LAST colon — a valid thinking-level suffix recurses on the prefix
/// and surfaces the level; an invalid suffix either recurses with a warning
/// (scope mode) or fails (strict mode, `allow_invalid_level_fallback=false`).
fn parse_model_pattern<'a>(
    pattern: &str,
    models: &'a [&Model],
    allow_invalid_level_fallback: bool,
) -> Option<(&'a Model, String, String)> {
    if let Some(m) = try_match_model(pattern, models) {
        return Some((m, String::new(), String::new()));
    }
    let last_colon = pattern.rfind(':')?;
    let prefix = &pattern[..last_colon];
    let suffix = &pattern[last_colon + 1..];
    if is_valid_thinking_level(suffix) {
        let (m, _, warning) = parse_model_pattern(prefix, models, allow_invalid_level_fallback)?;
        // pi: only use the level if the inner recursion was clean.
        let level = if warning.is_empty() { suffix.to_string() } else { String::new() };
        return Some((m, level, warning));
    }
    if !allow_invalid_level_fallback {
        // Strict mode: treat the suffix as part of the model id and fail rather
        // than accidentally resolving to a different model.
        return None;
    }
    let (m, _, _) = parse_model_pattern(prefix, models, allow_invalid_level_fallback)?;
    Some((
        m,
        String::new(),
        format!("Invalid thinking level {:?} in pattern {:?}. Using default instead.", suffix, pattern),
    ))
}

/// Ports pi's `tryMatchModel`: exact reference match first, then case-insensitive
/// substring matching on id/name, preferring aliases (un-dated ids) and otherwise
/// the latest dated version.
fn try_match_model<'a>(pattern: &str, models: &'a [&Model]) -> Option<&'a Model> {
    if let Some(m) = find_exact_model_reference_match(pattern, models) {
        return Some(m);
    }
    let lower = pattern.to_lowercase();
    let mut matches: Vec<&Model> = Vec::new();
    for m in models {
        if m.id.to_lowercase().contains(&lower) || m.name.to_lowercase().contains(&lower) {
            matches.push(m);
        }
    }
    if matches.is_empty() {
        return None;
    }
    let mut aliases: Vec<&Model> = Vec::new();
    let mut dated: Vec<&Model> = Vec::new();
    for m in matches {
        if is_model_alias(&m.id) {
            aliases.push(m);
        } else {
            dated.push(m);
        }
    }
    // pi sorts descending by id (b.localeCompare(a)) and takes the first.
    if !aliases.is_empty() {
        aliases.sort_by(|a, b| b.id.cmp(&a.id));
        aliases.first().copied()
    } else {
        dated.sort_by(|a, b| b.id.cmp(&a.id));
        dated.first().copied()
    }
}

/// Ports pi's `findExactModelReferenceMatch`: a canonical "provider/id" match,
/// then a provider+id split match, then a bare id match — each only when
/// unambiguous (a unique match).
fn find_exact_model_reference_match<'a>(reference: &str, models: &'a [&Model]) -> Option<&'a Model> {
    let trimmed = reference.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_lowercase();

    let canonical: Vec<&Model> = models
        .iter()
        .copied()
        .filter(|m| format!("{}/{}", m.provider, m.id).to_lowercase() == lower)
        .collect();
    if canonical.len() == 1 {
        return Some(canonical[0]);
    }
    if canonical.len() > 1 {
        return None;
    }

    if let Some(slash) = trimmed.find('/') {
        let provider_part = trimmed[..slash].trim();
        let model_id = trimmed[slash + 1..].trim();
        if !provider_part.is_empty() && !model_id.is_empty() {
            let by_provider: Vec<&Model> = models
                .iter()
                .copied()
                .filter(|m| m.provider.eq_ignore_ascii_case(provider_part) && m.id.eq_ignore_ascii_case(model_id))
                .collect();
            if by_provider.len() == 1 {
                return Some(by_provider[0]);
            }
            if by_provider.len() > 1 {
                return None;
            }
        }
    }

    let by_id: Vec<&Model> = models
        .iter()
        .copied()
        .filter(|m| m.id.to_lowercase() == lower)
        .collect();
    if by_id.len() == 1 {
        return Some(by_id[0]);
    }
    None
}

static MODEL_DATE_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-\d{8}$").unwrap());

/// Ports pi's `isAlias`: `-latest` ids and ids without a `-YYYYMMDD` date suffix
/// are aliases.
fn is_model_alias(id: &str) -> bool {
    if id.ends_with("-latest") {
        return true;
    }
    !MODEL_DATE_PATTERN.is_match(id)
}

/// Ports pi's `buildFallbackModel`: clone the provider's default (or first) model
/// with the requested custom id.
fn build_fallback_model(provider: &str, model_id: &str, models: &[Model]) -> Option<Model> {
    let provider_models: Vec<&Model> = models.iter().filter(|m| m.provider == provider).collect();
    if provider_models.is_empty() {
        return None;
    }
    let mut base = provider_models[0].clone();
    if let Some(default_id) = default_model_per_provider(provider) {
        for m in provider_models.iter().copied() {
            if m.id == default_id {
                base = m.clone();
                break;
            }
        }
    }
    base.id = model_id.to_string();
    base.name = model_id.to_string();
    Some(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_model_per_provider_openai() {
        let cases = [
            ("openai", "gpt-5.5"),
            ("azure-openai-responses", "gpt-5.4"),
            ("github-copilot", "gpt-5.4"),
            ("openai-codex", "gpt-5.5"),
        ];
        for (provider, want) in cases {
            assert_eq!(
                default_model_per_provider(provider),
                Some(want),
                "default model for {provider:?}"
            );
        }
    }

    #[test]
    fn default_model_per_provider_qwen_and_radius() {
        assert_eq!(default_model_per_provider("qwen-token-plan"), Some("qwen3.7-max"));
        assert_eq!(default_model_per_provider("qwen-token-plan-cn"), Some("qwen3.7-max"));
        assert_eq!(default_model_per_provider("radius"), Some("auto"));

        // Lock the consequence: the qwen-token-plan fallback must inherit
        // qwen3.7-max's limits, never MiniMax-M2.5's.
        for provider in ["qwen-token-plan", "qwen-token-plan-cn"] {
            let Some(id) = default_model_per_provider(provider) else { continue };
            let Some(tmpl) = pi_ai::get_model(provider, id) else {
                panic!("{provider}/{id} missing from catalog");
            };
            assert_eq!(tmpl.context_window, 1_000_000, "{provider} context_window");
            assert_eq!(tmpl.max_tokens, 131_072, "{provider} max_tokens");
        }
    }

    #[test]
    fn resolve_unknown_error_text() {
        let err = resolve_model_pattern("definitely-not-a-model-xyz").unwrap_err();
        assert_eq!(
            err,
            "Model \"definitely-not-a-model-xyz\" not found. Use --list-models to see available models."
        );
    }

    #[test]
    fn resolve_custom_id_fallback() {
        let r = resolve_model_pattern("anthropic/my-custom-model-id").unwrap();
        assert_eq!(r.model.provider, "anthropic");
        assert_eq!(r.model.id, "my-custom-model-id");
        assert_eq!(r.model.name, "my-custom-model-id");
        assert!(r.warning.contains("Model \"my-custom-model-id\" not found for provider \"anthropic\". Using custom model id."), "warning: {}", r.warning);
        assert_eq!(r.thinking_level, "", "fallback without suffix must not carry a level");
    }

    #[test]
    fn resolve_custom_id_fallback_thinking_suffix() {
        let r = resolve_model_pattern("anthropic/my-custom-model-id:high").unwrap();
        assert_eq!(r.model.provider, "anthropic");
        assert_eq!(r.model.id, "my-custom-model-id", "suffix leaked into custom id");
        assert_eq!(r.thinking_level, "high");
        assert!(r.warning.contains("Model \"my-custom-model-id\" not found for provider \"anthropic\". Using custom model id."), "warning must quote stripped id: {}", r.warning);
    }

    #[test]
    fn resolve_custom_id_fallback_thinking_sets_reasoning() {
        let r = resolve_model_pattern("mistral/my-custom-model-id:high").unwrap();
        assert_eq!(r.model.id, "my-custom-model-id");
        assert_eq!(r.thinking_level, "high");
        assert!(r.model.reasoning, "requested thinking level must set reasoning:true on the fallback model");
    }

    #[test]
    fn resolve_custom_id_fallback_thinking_off_keeps_reasoning_false() {
        let r = resolve_model_pattern("mistral/my-custom-model-id:off").unwrap();
        assert_eq!(r.model.id, "my-custom-model-id");
        assert_eq!(r.thinking_level, "off");
        assert!(!r.model.reasoning, ":off must not enable reasoning on a non-reasoning fallback template");
    }

    #[test]
    fn resolve_custom_id_fallback_all_levels() {
        for level in ["off", "minimal", "low", "medium", "high", "xhigh"] {
            let r = resolve_model_pattern(&format!("anthropic/my-custom-model-id:{level}")).unwrap();
            assert_eq!(r.model.id, "my-custom-model-id", "level {level}: suffix leaked");
            assert_eq!(r.thinking_level, level, "level {level}: wrong level");
        }
    }

    #[test]
    fn resolve_custom_id_fallback_invalid_suffix() {
        let r = resolve_model_pattern("anthropic/my-custom-model-id:banana").unwrap();
        assert_eq!(r.model.provider, "anthropic");
        assert_eq!(r.model.id, "my-custom-model-id:banana", "invalid suffix must stay in the id");
        assert_eq!(r.thinking_level, "", "invalid suffix must not surface a level");
        assert!(r.warning.contains("Model \"my-custom-model-id:banana\" not found for provider \"anthropic\". Using custom model id."), "warning: {}", r.warning);
    }

    #[test]
    fn resolve_thinking_level_suffix() {
        let r = resolve_model_pattern("anthropic/claude-sonnet-4-5:high").unwrap();
        assert_eq!(r.model.id, "claude-sonnet-4-5");
        assert_eq!(r.thinking_level, "high");

        // Bare-id pattern with suffix.
        let r = resolve_model_pattern("claude-sonnet-4-5:xhigh").unwrap();
        assert_eq!(r.thinking_level, "xhigh");

        // No suffix → empty level.
        let r = resolve_model_pattern("anthropic/claude-sonnet-4-5").unwrap();
        assert_eq!(r.thinking_level, "");
    }

    #[test]
    fn resolve_case_insensitive() {
        let r = resolve_model_pattern("ANTHROPIC/CLAUDE-SONNET-4-5").unwrap();
        assert_eq!(r.model.provider, "anthropic");
        assert_eq!(r.model.id, "claude-sonnet-4-5");
    }

    #[test]
    fn resolve_openrouter_slashed_id() {
        // A slash prefix that is NOT a known provider is part of the model id.
        let r = resolve_model_pattern("ai21/jamba-large-1.7").unwrap();
        assert_eq!(r.model.provider, "openrouter");
        assert_eq!(r.model.id, "ai21/jamba-large-1.7");
    }

    #[test]
    fn resolve_provider_prefix_falls_back_to_full_id() {
        // A slash prefix that IS a known provider is preferred — but when nothing
        // matches within it, the full input falls back to a raw model id across all
        // providers (openrouter-style ids).
        let r = resolve_model_pattern("anthropic/claude-opus-4.8-fast").unwrap();
        assert_eq!(r.model.provider, "openrouter");
        assert_eq!(r.model.id, "anthropic/claude-opus-4.8-fast");
    }

    #[test]
    fn resolve_xai_fallback_default() {
        assert_eq!(default_model_per_provider("xai"), Some("grok-4.5"));
        let r = resolve_model_pattern("xai/my-custom-grok").unwrap();
        assert_eq!(r.model.provider, "xai");
        assert_eq!(r.model.id, "my-custom-grok");
        // The template is the grok-4.5 catalog entry (clone carries its limits).
        let tmpl = resolve_model("xai/grok-4.5").unwrap();
        assert_eq!(r.model.context_window, tmpl.context_window);
        assert_eq!(r.model.max_tokens, tmpl.max_tokens);
    }

    #[test]
    fn empty_spec_resolves_to_default() {
        let r = resolve_model_pattern("").unwrap();
        assert_eq!(r.model.provider, "anthropic");
        assert_eq!(r.model.id, "claude-sonnet-4-5");
    }

    #[test]
    fn is_model_alias_works() {
        assert!(is_model_alias("claude-sonnet-4-5"));
        assert!(is_model_alias("grok-latest"));
        assert!(!is_model_alias("grok-4.5-20240301"));
    }
}