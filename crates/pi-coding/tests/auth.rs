use std::collections::{BTreeMap, HashMap};

use anyhow::{Result, bail};
use async_trait::async_trait;
use pi_coding::{
    AuthEvent, AuthInteraction, AuthManager, AuthPrompt, AuthStorage, AuthType, Credential,
    load_credentials, write_credentials_atomic,
};

struct ApiKeyInteraction {
    secret: String,
}

#[async_trait]
impl AuthInteraction for ApiKeyInteraction {
    async fn prompt(&self, prompt: AuthPrompt) -> Result<String> {
        match prompt {
            AuthPrompt::Secret { .. } => Ok(self.secret.clone()),
            unexpected => bail!("unexpected authentication prompt: {unexpected:?}"),
        }
    }

    fn notify(&self, _event: AuthEvent) {}
}

#[tokio::test]
async fn api_key_login_and_logout_preserve_other_providers() {
    let directory = tempfile::tempdir().expect("temporary auth directory");
    let path = directory.path().join("auth.json");
    let mut initial = BTreeMap::new();
    initial.insert(
        "other-provider".to_owned(),
        api_key_credential("other-secret"),
    );
    write_credentials_atomic(&path, &initial).expect("write initial auth file");

    let manager = AuthManager::new(path.clone()).expect("auth manager");
    let interaction = ApiKeyInteraction {
        secret: "new-secret".to_owned(),
    };
    let logged_in = manager
        .login(Some("test-provider"), Some(AuthType::ApiKey), None, &interaction)
        .await
        .expect("login with API key");
    assert_eq!(logged_in.provider_id, "test-provider");
    assert_eq!(logged_in.credential_type, AuthType::ApiKey);

    let after_login = load_credentials(&path).expect("load credentials after login");
    assert_eq!(after_login.len(), 2);
    assert!(after_login.contains_key("other-provider"));
    let resolved = manager
        .resolve_stored("test-provider", None)
        .await
        .expect("resolve stored API key")
        .expect("stored API key exists");
    assert_eq!(resolved.api_key, "new-secret");

    let logged_out = manager
        .logout(Some("test-provider"), None, &interaction)
        .await
        .expect("logout selected provider");
    assert_eq!(logged_out.provider_id, "test-provider");
    let after_logout = load_credentials(&path).expect("load credentials after logout");
    assert_eq!(after_logout.len(), 1);
    assert!(after_logout.contains_key("other-provider"));
    assert!(!after_logout.contains_key("test-provider"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path)
                .expect("auth file metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[tokio::test]
async fn stored_api_key_env_overlay_is_resolved_without_persisting_override() {
    let directory = tempfile::tempdir().expect("temporary auth directory");
    let path = directory.path().join("auth.json");
    let storage = AuthStorage::new(path.clone());
    storage
        .modify("custom", None, |_| async {
            let mut credential = api_key_credential("prefix-${TOKEN}");
            match &mut credential {
                Credential::ApiKey { env, .. } => {
                    env.insert("TOKEN".to_owned(), "stored".to_owned());
                }
                Credential::OAuth { .. } => panic!("expected API-key credential"),
            }
            Ok(Some(credential))
        })
        .await
        .expect("store templated API key");

    let manager = AuthManager::new(path.clone()).expect("auth manager");
    let request_env = HashMap::from([("TOKEN".to_owned(), "runtime".to_owned())]);
    let resolved = manager
        .resolve_stored("custom", Some(&request_env))
        .await
        .expect("resolve credential")
        .expect("credential exists");
    assert_eq!(resolved.api_key, "prefix-runtime");

    let stored = load_credentials(&path).expect("reload stored credential");
    match stored.get("custom").expect("custom credential") {
        Credential::ApiKey { key, env, .. } => {
            assert_eq!(key.as_deref(), Some("prefix-${TOKEN}"));
            assert_eq!(env.get("TOKEN").map(String::as_str), Some("stored"));
        }
        Credential::OAuth { .. } => panic!("expected API-key credential"),
    }
}

#[tokio::test]
async fn stored_api_key_literal_escapes_match_models_config() {
    let directory = tempfile::tempdir().expect("temporary auth directory");
    let path = directory.path().join("auth.json");
    let storage = AuthStorage::new(path.clone());
    storage
        .modify("custom", None, |_| async {
            let mut credential = api_key_credential("$$/$!/$TOKEN/${TOKEN}");
            match &mut credential {
                Credential::ApiKey { env, .. } => {
                    env.insert("TOKEN".to_owned(), "secret-value".to_owned());
                }
                Credential::OAuth { .. } => panic!("expected API-key credential"),
            }
            Ok(Some(credential))
        })
        .await
        .expect("store templated API key with escapes");

    let manager = AuthManager::new(path).expect("auth manager");
    let resolved = manager
        .resolve_stored("custom", Some(&HashMap::new()))
        .await
        .expect("resolve credential")
        .expect("credential exists");
    assert_eq!(resolved.api_key, "$/!/secret-value/secret-value");
}

#[tokio::test]
async fn stored_api_key_invalid_braced_name_is_left_literal() {
    let directory = tempfile::tempdir().expect("temporary auth directory");
    let path = directory.path().join("auth.json");
    let storage = AuthStorage::new(path.clone());
    storage
        .modify("custom", None, |_| async {
            Ok(Some(api_key_credential("pre-${1bad}-post")))
        })
        .await
        .expect("store key with invalid braced name");

    let manager = AuthManager::new(path).expect("auth manager");
    let resolved = manager
        .resolve_stored("custom", Some(&HashMap::new()))
        .await
        .expect("resolve credential")
        .expect("credential exists");
    assert_eq!(resolved.api_key, "pre-${1bad}-post");
}

#[tokio::test]
async fn stored_api_key_command_value_is_rejected() {
    let directory = tempfile::tempdir().expect("temporary auth directory");
    let path = directory.path().join("auth.json");
    let storage = AuthStorage::new(path.clone());
    storage
        .modify("custom", None, |_| async {
            Ok(Some(api_key_credential("!echo secret-command-output")))
        })
        .await
        .expect("store command-valued key");

    let manager = AuthManager::new(path).expect("auth manager");
    let error = manager
        .resolve_stored("custom", None)
        .await
        .expect_err("command-valued keys must fail");
    let message = format!("{error:#}");
    assert!(
        message.contains("command-valued"),
        "unexpected error: {message}"
    );
    assert!(
        !message.contains("secret-command-output"),
        "command payload must not leak into errors: {message}"
    );
}

#[tokio::test]
async fn missing_env_error_is_sanitized_without_secret_values() {
    let directory = tempfile::tempdir().expect("temporary auth directory");
    let path = directory.path().join("auth.json");
    let storage = AuthStorage::new(path.clone());
    storage
        .modify("custom", None, |_| async {
            // Adjacent literal must never appear in the missing-var error.
            let mut credential = api_key_credential("prefix-super-secret-token-${MISSING_AUTH_VAR}");
            match &mut credential {
                Credential::ApiKey { env, .. } => {
                    env.insert(
                        "OTHER".to_owned(),
                        "credential-secret-must-not-leak".to_owned(),
                    );
                }
                Credential::OAuth { .. } => panic!("expected API-key credential"),
            }
            Ok(Some(credential))
        })
        .await
        .expect("store key referencing missing var");

    let manager = AuthManager::new(path).expect("auth manager");
    let request_env = HashMap::from([(
        "UNUSED".to_owned(),
        "request-secret-must-not-leak".to_owned(),
    )]);
    let error = manager
        .resolve_stored("custom", Some(&request_env))
        .await
        .expect_err("missing env must fail");
    let message = format!("{error:#}");
    assert!(
        message.contains("MISSING_AUTH_VAR"),
        "error should name the missing variable: {message}"
    );
    assert!(
        message.contains("auth.json"),
        "error should name the credential source: {message}"
    );
    for leak in [
        "super-secret-token",
        "credential-secret-must-not-leak",
        "request-secret-must-not-leak",
        "prefix-",
    ] {
        assert!(
            !message.contains(leak),
            "secret material {leak:?} must not appear in error: {message}"
        );
    }
}

#[tokio::test]
async fn request_env_overrides_credential_env_for_interpolation() {
    let directory = tempfile::tempdir().expect("temporary auth directory");
    let path = directory.path().join("auth.json");
    let storage = AuthStorage::new(path.clone());
    storage
        .modify("builtin-like", None, |_| async {
            let mut credential = api_key_credential("$A/${B}");
            match &mut credential {
                Credential::ApiKey { env, .. } => {
                    env.insert("A".to_owned(), "cred-a".to_owned());
                    env.insert("B".to_owned(), "cred-b".to_owned());
                }
                Credential::OAuth { .. } => panic!("expected API-key credential"),
            }
            Ok(Some(credential))
        })
        .await
        .expect("store multi-var key");

    let manager = AuthManager::new(path).expect("auth manager");
    let from_credential = manager
        .resolve_stored("builtin-like", Some(&HashMap::new()))
        .await
        .expect("resolve from credential env")
        .expect("credential exists");
    assert_eq!(from_credential.api_key, "cred-a/cred-b");

    let request_env = HashMap::from([("A".to_owned(), "req-a".to_owned())]);
    let mixed = manager
        .resolve_stored("builtin-like", Some(&request_env))
        .await
        .expect("resolve with request override")
        .expect("credential exists");
    assert_eq!(mixed.api_key, "req-a/cred-b");
}

#[test]
fn oauth_request_auth_adapts_provider_metadata_without_network() {
    let codex = Credential::OAuth {
        refresh: "refresh-secret".to_owned(),
        access: "access-secret".to_owned(),
        expires: i64::MAX,
        fields: serde_json::Map::from_iter([(
            "accountId".to_owned(),
            serde_json::Value::String("account-123".to_owned()),
        )]),
    };
    let codex_auth =
        pi_coding::to_request_auth("openai-codex", &codex).expect("adapt OpenAI Codex credential");
    assert_eq!(codex_auth.api_key, "access-secret");
    assert_eq!(
        codex_auth
            .headers
            .get("chatgpt-account-id")
            .map(String::as_str),
        Some("account-123")
    );

    let copilot = Credential::OAuth {
        refresh: "refresh-secret".to_owned(),
        access: "copilot-secret".to_owned(),
        expires: i64::MAX,
        fields: serde_json::Map::from_iter([(
            "availableModelIds".to_owned(),
            serde_json::json!(["gpt-5.4", "claude-sonnet-4.6"]),
        )]),
    };
    let copilot_auth = pi_coding::to_request_auth("github-copilot", &copilot)
        .expect("adapt GitHub Copilot credential");
    assert_eq!(copilot_auth.api_key, "copilot-secret");
    assert_eq!(
        copilot_auth.available_model_ids.as_deref(),
        Some(&["gpt-5.4".to_owned(), "claude-sonnet-4.6".to_owned(),][..])
    );
    assert!(!format!("{copilot_auth:?}").contains("copilot-secret"));
    assert!(!format!("{copilot_auth:?}").contains("gpt-5.4"));

    let invalid_copilot = Credential::OAuth {
        refresh: "refresh-secret".to_owned(),
        access: "copilot-secret".to_owned(),
        expires: i64::MAX,
        fields: serde_json::Map::from_iter([(
            "availableModelIds".to_owned(),
            serde_json::json!(["gpt-5.4", 7]),
        )]),
    };
    let invalid_auth = pi_coding::to_request_auth("github-copilot", &invalid_copilot)
        .expect("ignore invalid GitHub Copilot metadata");
    assert!(invalid_auth.available_model_ids.is_none());

    let oversized_copilot = Credential::OAuth {
        refresh: "refresh-secret".to_owned(),
        access: "copilot-secret".to_owned(),
        expires: i64::MAX,
        fields: serde_json::Map::from_iter([(
            "availableModelIds".to_owned(),
            serde_json::json!(["x".repeat(513)]),
        )]),
    };
    let oversized_auth = pi_coding::to_request_auth("github-copilot", &oversized_copilot)
        .expect("ignore oversized GitHub Copilot metadata");
    assert!(oversized_auth.available_model_ids.is_none());

    let google = Credential::OAuth {
        refresh: "refresh-secret".to_owned(),
        access: "access-secret".to_owned(),
        expires: i64::MAX,
        fields: serde_json::Map::from_iter([(
            "projectId".to_owned(),
            serde_json::Value::String("project-123".to_owned()),
        )]),
    };
    let google_auth =
        pi_coding::to_request_auth("google-gemini-cli", &google).expect("adapt Google credential");
    assert_eq!(google_auth.api_key, "access-secret");
    assert_eq!(
        google_auth
            .env
            .get("GOOGLE_CLOUD_PROJECT")
            .map(String::as_str),
        Some("project-123")
    );

    let kimi = Credential::OAuth {
        refresh: "refresh-secret".to_owned(),
        access: "access-secret".to_owned(),
        expires: i64::MAX,
        fields: serde_json::Map::new(),
    };
    let kimi_auth =
        pi_coding::to_request_auth("kimi-coding", &kimi).expect("adapt Kimi credential");
    assert!(kimi_auth.api_key.is_empty());
    assert_eq!(
        kimi_auth.headers.get("Authorization").map(String::as_str),
        Some("Bearer access-secret")
    );
}

fn api_key_credential(value: &str) -> Credential {
    Credential::ApiKey {
        key: Some(value.to_owned()),
        env: HashMap::new(),
        extra: serde_json::Map::new(),
    }
}

#[tokio::test]
async fn login_with_scope_writes_scoped_slot_and_preserves_unscoped() {
    let directory = tempfile::tempdir().expect("temporary auth directory");
    let path = directory.path().join("auth.json");
    let manager = AuthManager::new(path.clone()).expect("auth manager");
    let interaction = ApiKeyInteraction {
        secret: "work-secret".to_owned(),
    };
    let logged_in = manager
        .login(Some("test-provider"), Some(AuthType::ApiKey), Some("work"), &interaction)
        .await
        .expect("scoped login");
    assert_eq!(logged_in.provider_id, "test-provider");
    assert_eq!(logged_in.scope.as_deref(), Some("work"));

    let file: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&path).expect("read auth.json"),
    )
    .expect("parse auth.json");
    let scoped_key = file
        .get("scopes")
        .expect("scopes section")
        .get("work")
        .expect("work scope")
        .get("test-provider")
        .expect("provider entry")
        .get("key")
        .expect("key field");
    assert_eq!(scoped_key.as_str(), Some("work-secret"));
    assert!(
        file.get("test-provider").is_none(),
        "scoped login must not create an unscoped slot"
    );

    let resolved = manager
        .resolve_stored_with_scope("test-provider", None, Some("work"))
        .await
        .expect("resolve scoped credential")
        .expect("credential exists");
    assert_eq!(resolved.api_key, "work-secret");

    // Unscoped login for the same provider coexists with the scoped slot.
    let unscoped_interaction = ApiKeyInteraction {
        secret: "default-secret".to_owned(),
    };
    manager
        .login(Some("test-provider"), Some(AuthType::ApiKey), None, &unscoped_interaction)
        .await
        .expect("unscoped login");
    let resolved_default = manager
        .resolve_stored_with_scope("test-provider", None, None)
        .await
        .expect("resolve default")
        .expect("default credential exists");
    assert_eq!(resolved_default.api_key, "default-secret");
    let resolved_work = manager
        .resolve_stored_with_scope("test-provider", None, Some("work"))
        .await
        .expect("resolve work")
        .expect("work credential exists");
    assert_eq!(resolved_work.api_key, "work-secret");
}

#[tokio::test]
async fn scoped_resolution_prefers_scope_match_then_unscoped_fallback() {
    let directory = tempfile::tempdir().expect("temporary auth directory");
    let path = directory.path().join("auth.json");
    let storage = AuthStorage::new(path.clone());
    storage
        .modify("custom", None, |_| async {
            Ok(Some(api_key_credential("default-key")))
        })
        .await
        .expect("store unscoped credential");
    storage
        .modify("custom", Some("work"), |_| async {
            Ok(Some(api_key_credential("work-key")))
        })
        .await
        .expect("store work-scoped credential");

    let manager = AuthManager::new(path).expect("auth manager");
    let matched = manager
        .resolve_stored_with_scope("custom", None, Some("work"))
        .await
        .expect("resolve with matching scope")
        .expect("credential exists");
    assert_eq!(matched.api_key, "work-key", "scope match must win");

    let fallback = manager
        .resolve_stored_with_scope("custom", None, Some("personal"))
        .await
        .expect("resolve with unmatched scope")
        .expect("credential exists");
    assert_eq!(
        fallback.api_key, "default-key",
        "unmatched scope must fall back to the unscoped credential"
    );

    let default = manager
        .resolve_stored_with_scope("custom", None, None)
        .await
        .expect("resolve without active scope")
        .expect("credential exists");
    assert_eq!(default.api_key, "default-key");
}

#[tokio::test]
async fn missing_scope_errors_actionably_without_secret_leakage() {
    let directory = tempfile::tempdir().expect("temporary auth directory");
    let path = directory.path().join("auth.json");
    let storage = AuthStorage::new(path.clone());
    storage
        .modify("custom", Some("work"), |_| async {
            Ok(Some(api_key_credential("super-secret-work-key")))
        })
        .await
        .expect("store scoped credential");

    let manager = AuthManager::new(path).expect("auth manager");
    let error = manager
        .resolve_stored_with_scope("custom", None, None)
        .await
        .expect_err("scoped-only provider must fail without an active scope");
    let message = format!("{error:#}");
    assert!(message.contains("custom"), "{message}");
    assert!(message.contains("\"work\""), "must name the scope: {message}");
    assert!(message.contains("PI_AUTH_SCOPE"), "{message}");
    assert!(
        !message.contains("super-secret-work-key"),
        "secret must not leak: {message}"
    );

    let mismatch = manager
        .resolve_stored_with_scope("custom", None, Some("personal"))
        .await
        .expect_err("unmatched scope without unscoped fallback must fail");
    let message = format!("{mismatch:#}");
    assert!(message.contains("personal"), "{message}");
    assert!(message.contains("work"), "{message}");
    assert!(
        !message.contains("super-secret-work-key"),
        "secret must not leak: {message}"
    );
}

#[tokio::test]
async fn resolve_stored_with_scope_resolves_through_the_ambient_entry_point() {
    // The ambient `resolve_stored` applies `active_auth_scope()`; its explicit
    // variant is the deterministic core the env/settings preference feeds.
    // (The workspace forbids process-env mutation in tests, so the preference
    // itself is unit-tested via `pi_coding::resolve_scope_preference`.)
    let directory = tempfile::tempdir().expect("temporary auth directory");
    let path = directory.path().join("auth.json");
    let storage = AuthStorage::new(path.clone());
    storage
        .modify("custom", None, |_| async {
            Ok(Some(api_key_credential("default-key")))
        })
        .await
        .expect("store unscoped credential");
    storage
        .modify("custom", Some("work"), |_| async {
            Ok(Some(api_key_credential("work-key")))
        })
        .await
        .expect("store work-scoped credential");

    let manager = AuthManager::new(path).expect("auth manager");
    let resolved = manager
        .resolve_stored_with_scope("custom", None, Some("work"))
        .await
        .expect("resolve with active scope")
        .expect("credential exists");
    assert_eq!(resolved.api_key, "work-key");
    let default = manager
        .resolve_stored_with_scope("custom", None, None)
        .await
        .expect("resolve default")
        .expect("credential exists");
    assert_eq!(default.api_key, "default-key");
}

#[tokio::test]
async fn storage_listing_never_exposes_secrets_or_oauth_values() {
    let directory = tempfile::tempdir().expect("temporary auth directory");
    let path = directory.path().join("auth.json");
    let storage = AuthStorage::new(path.clone());
    storage
        .modify("custom", Some("work"), |_| async {
            Ok(Some(api_key_credential("list-secret-key")))
        })
        .await
        .expect("store scoped API key");
    storage
        .modify("oauth-provider", None, |_| async {
            Ok(Some(Credential::OAuth {
                refresh: "list-refresh-secret".to_owned(),
                access: "list-access-secret".to_owned(),
                expires: i64::MAX,
                fields: serde_json::Map::new(),
            }))
        })
        .await
        .expect("store OAuth credential");

    let listed = storage.list().await.expect("list credentials");
    assert_eq!(listed.len(), 2);
    let custom = listed
        .iter()
        .find(|entry| entry.provider_id == "custom")
        .expect("custom entry");
    assert_eq!(custom.scope.as_deref(), Some("work"));
    assert_eq!(custom.credential_type, AuthType::ApiKey);
    let oauth = listed
        .iter()
        .find(|entry| entry.provider_id == "oauth-provider")
        .expect("oauth entry");
    assert_eq!(oauth.credential_type, AuthType::OAuth);

    let serialized = serde_json::to_string(&listed).expect("serialize listing");
    let debug = format!("{listed:?}");
    for secret in [
        "list-secret-key",
        "list-refresh-secret",
        "list-access-secret",
    ] {
        assert!(!serialized.contains(secret), "{secret} leaked in {serialized}");
        assert!(!debug.contains(secret), "{secret} leaked in {debug}");
    }
}

#[tokio::test]
async fn scoped_logout_removes_only_the_selected_slot() {
    let directory = tempfile::tempdir().expect("temporary auth directory");
    let path = directory.path().join("auth.json");
    let storage = AuthStorage::new(path.clone());
    storage
        .modify("custom", None, |_| async {
            Ok(Some(api_key_credential("default-key")))
        })
        .await
        .expect("store unscoped credential");
    storage
        .modify("custom", Some("work"), |_| async {
            Ok(Some(api_key_credential("work-key")))
        })
        .await
        .expect("store scoped credential");

    let manager = AuthManager::new(path).expect("auth manager");
    let interaction = ApiKeyInteraction {
        secret: "unused".to_owned(),
    };
    let logged_out = manager
        .logout(Some("custom"), Some("work"), &interaction)
        .await
        .expect("logout scoped slot");
    assert_eq!(logged_out.provider_id, "custom");
    assert_eq!(logged_out.scope.as_deref(), Some("work"));

    let remaining = manager
        .resolve_stored_with_scope("custom", None, None)
        .await
        .expect("resolve remaining default")
        .expect("default credential survives");
    assert_eq!(remaining.api_key, "default-key");
    assert!(
        storage
            .read("custom", Some("work"))
            .await
            .expect("read removed scope slot")
            .is_none(),
        "scoped slot must be gone"
    );
    // Resolution still works for the removed scope by falling back to the
    // unscoped default.
    assert_eq!(
        manager
            .resolve_stored_with_scope("custom", None, Some("work"))
            .await
            .expect("resolve removed scope")
            .expect("credential exists")
            .api_key,
        "default-key"
    );

    let logged_out_default = manager
        .logout(Some("custom"), None, &interaction)
        .await
        .expect("logout unscoped slot");
    assert_eq!(logged_out_default.scope, None);
    assert!(
        manager
            .resolve_stored_with_scope("custom", None, None)
            .await
            .expect("resolve after both removed")
            .is_none(),
        "no credential may remain after both slots are removed"
    );
}
