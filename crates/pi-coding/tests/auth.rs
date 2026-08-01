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
        Credential::ApiKey {
            key: Some("other-secret".to_owned()),
            env: HashMap::new(),
        },
    );
    write_credentials_atomic(&path, &initial).expect("write initial auth file");

    let manager = AuthManager::new(path.clone()).expect("auth manager");
    let interaction = ApiKeyInteraction {
        secret: "new-secret".to_owned(),
    };
    let logged_in = manager
        .login(Some("test-provider"), Some(AuthType::ApiKey), &interaction)
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
        .logout(Some("test-provider"), &interaction)
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
        .modify("custom", |_| async {
            Ok(Some(Credential::ApiKey {
                key: Some("prefix-${TOKEN}".to_owned()),
                env: HashMap::from([("TOKEN".to_owned(), "stored".to_owned())]),
            }))
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
        Credential::ApiKey { key, env } => {
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
        .modify("custom", |_| async {
            Ok(Some(Credential::ApiKey {
                key: Some("$$/$!/$TOKEN/${TOKEN}".to_owned()),
                env: HashMap::from([("TOKEN".to_owned(), "secret-value".to_owned())]),
            }))
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
        .modify("custom", |_| async {
            Ok(Some(Credential::ApiKey {
                key: Some("pre-${1bad}-post".to_owned()),
                env: HashMap::new(),
            }))
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
        .modify("custom", |_| async {
            Ok(Some(Credential::ApiKey {
                key: Some("!echo secret-command-output".to_owned()),
                env: HashMap::new(),
            }))
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
        .modify("custom", |_| async {
            Ok(Some(Credential::ApiKey {
                // Adjacent literal must never appear in the missing-var error.
                key: Some("prefix-super-secret-token-${MISSING_AUTH_VAR}".to_owned()),
                env: HashMap::from([(
                    "OTHER".to_owned(),
                    "credential-secret-must-not-leak".to_owned(),
                )]),
            }))
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
        .modify("builtin-like", |_| async {
            Ok(Some(Credential::ApiKey {
                key: Some("$A/${B}".to_owned()),
                env: HashMap::from([
                    ("A".to_owned(), "cred-a".to_owned()),
                    ("B".to_owned(), "cred-b".to_owned()),
                ]),
            }))
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
