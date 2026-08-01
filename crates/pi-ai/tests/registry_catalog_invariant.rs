//! Registry/catalog invariant: every statically-cataloged model API must be
//! registered, every known protocol constant centralized in `types.rs`, and
//! dynamic Radius models installed only after a validated refresh.

use std::collections::BTreeSet;

use pi_ai::*;
use pi_ai::providers::{
    models_from_config, register_builtins, RadiusCatalog, RadiusCatalogSnapshot,
    RadiusGatewayConfig, RadiusGatewayModel,
};


fn registered_apis() -> BTreeSet<String> {
    register_builtins();
    get_api_providers().into_iter().map(|p| p.api).collect()
}

fn embedded_catalog_apis() -> BTreeSet<String> {
    load_builtin_models();
    get_providers()
        .into_iter()
        .flat_map(|p| builtin_models(&p))
        .map(|m| m.api)
        .collect()
}

#[test]
fn known_api_constants_are_centralized_with_expected_values() {
    assert_eq!(API_OPENAI_COMPLETIONS, "openai-completions");
    assert_eq!(API_OPENAI_RESPONSES, "openai-responses");
    assert_eq!(API_AZURE_OPENAI_RESPONSES, "azure-openai-responses");
    assert_eq!(API_OPENAI_CODEX_RESPONSES, "openai-codex-responses");
    assert_eq!(API_ANTHROPIC_MESSAGES, "anthropic-messages");
    assert_eq!(API_BEDROCK_CONVERSE_STREAM, "bedrock-converse-stream");
    assert_eq!(API_GOOGLE_GENERATIVE_AI, "google-generative-ai");
    assert_eq!(API_GOOGLE_VERTEX, "google-vertex");
    assert_eq!(API_MISTRAL_CONVERSATIONS, "mistral-conversations");
    assert_eq!(API_PI_MESSAGES, "pi-messages");

    let unique: BTreeSet<&str> = KNOWN_CATALOG_APIS.iter().copied().collect();
    assert_eq!(
        unique.len(),
        KNOWN_CATALOG_APIS.len(),
        "KNOWN_CATALOG_APIS contains duplicates"
    );
    assert_eq!(KNOWN_CATALOG_APIS.len(), 10, "expected exactly 10 known catalog APIs");
}

#[test]
fn every_embedded_catalog_api_is_registered() {
    let registered = registered_apis();
    let catalog = embedded_catalog_apis();

    let unregistered: Vec<&str> = catalog
        .iter()
        .filter(|api| !registered.contains(api.as_str()))
        .map(String::as_str)
        .collect();
    assert!(
        unregistered.is_empty(),
        "embedded catalog APIs without a registered adapter: {unregistered:?}"
    );
}

#[test]
fn every_registered_api_is_a_known_constant_or_faux() {
    let registered = registered_apis();
    let mut allowed: BTreeSet<&str> = KNOWN_CATALOG_APIS.iter().copied().collect();
    allowed.insert(API_FAUX);

    let unknown: Vec<&str> = registered
        .iter()
        .filter(|api| !allowed.contains(api.as_str()))
        .map(String::as_str)
        .collect();
    assert!(
        unknown.is_empty(),
        "registered APIs not declared as known constants or faux: {unknown:?}"
    );
}

#[test]
fn no_duplicate_api_registration() {
    let registered = registered_apis();
    let apis: Vec<&str> = registered.iter().map(String::as_str).collect();
    let unique: BTreeSet<&str> = apis.iter().copied().collect();
    assert_eq!(
        apis.len(),
        unique.len(),
        "duplicate API registration detected: {apis:?}"
    );
}

#[test]
fn unknown_api_returns_none_clearly() {
    register_builtins();
    assert!(
        get_api_provider("does-not-exist-api").is_none(),
        "unknown API must return None, not a silent fallback"
    );
}


#[test]
fn all_known_catalog_apis_are_registered() {
    let registered = registered_apis();
    for &api in KNOWN_CATALOG_APIS {
        assert!(
            registered.contains(api),
            "known catalog API {api:?} is not registered"
        );
    }
}

#[test]
fn pi_messages_api_registered_but_absent_from_embedded_catalog() {
    // The pi-messages adapter is registered (register_pi_messages) even though
    // no embedded catalog model uses it — Radius models are dynamic only.
    register_builtins();
    assert!(get_api_provider(API_PI_MESSAGES).is_some());

    load_builtin_models();
    for provider in get_providers() {
        for model in builtin_models(&provider) {
            assert_ne!(
                model.api, API_PI_MESSAGES,
                "pi-messages model {}/{} must not appear in the embedded catalog",
                model.provider, model.id
            );
        }
    }
}

#[test]
fn radius_models_installed_only_after_valid_restore() {
    let provider = "radius-invariant-fixture";
    load_builtin_models();
    assert!(
        get_model(provider, "dynamic").is_none(),
        "Radius model must not exist before a valid refresh"
    );

    let config = RadiusGatewayConfig {
        base_url: "https://stream.radius.test/v1".into(),
        models: vec![RadiusGatewayModel {
            id: "dynamic".into(),
            name: "Dynamic".into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec!["text".into()],
            cost: ModelCost::default(),
            context_window: 32000,
            max_tokens: 4096,
        }],
    };
    let models = models_from_config(provider, config).expect("valid Radius config");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].api, API_PI_MESSAGES);
    assert_eq!(models[0].provider, provider);

    let catalog = RadiusCatalog::new(provider, "https://radius.pi.dev").expect("catalog");
    catalog
        .restore_snapshot(RadiusCatalogSnapshot { models, checked_at: Some(42) })
        .expect("valid restore");

    let installed =
        get_model(provider, "dynamic").expect("dynamic model installed after restore");
    assert_eq!(installed.api, API_PI_MESSAGES);
    assert_eq!(installed.provider, provider);

    // The pi-messages adapter is registered, so the model can stream.
    assert!(get_api_provider(API_PI_MESSAGES).is_some());
}