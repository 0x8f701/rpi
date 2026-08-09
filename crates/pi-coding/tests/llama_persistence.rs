//! Offline persistence coverage for the llama.cpp router manager
//! (`crates/pi-coding/src/llama.rs` — 10% unit coverage before this file).
//!
//! The network-touching paths (live catalog refresh, GGUF downloads) stay
//! untested here; these tests drive the deterministic file-layer contracts
//! through the public `LlamaManager` API: settings read-back with the
//! group/other-permission safety check, catalog cache round-trip, the
//! installed-GGUF catalog round-trip, and missing-file defaults. No network,
//! no router, no environment mutation.

use std::fs;
use std::path::Path;

use pi_ai::Model;
use pi_coding::llama::{
    InstalledGgufCatalog, InstalledGgufFile, InstalledGgufModel, LlamaCatalogSnapshot,
    LlamaManager,
};

fn manager(root: &Path) -> LlamaManager {
    LlamaManager::new(root.to_path_buf())
}

#[test]
fn settings_round_trip_and_secret_permission_gate() {
    let root = tempfile::tempdir().expect("llama root");
    let manager = manager(root.path());
    let settings_path = manager.settings_path();

    // Missing settings file reads as None.
    assert!(manager.settings().expect("settings").is_none());
    assert!(manager.effective_settings().expect("effective").is_none());

    // A persisted settings file round-trips through settings().
    fs::write(
        &settings_path,
        br#"{"baseUrl":"http://127.0.0.1:8080/v1","apiKey":"llama-secret"}"#,
    )
    .expect("write settings");
    let settings = manager
        .settings()
        .expect("settings read")
        .expect("settings present");
    assert_eq!(settings.base_url, "http://127.0.0.1:8080/v1");
    assert_eq!(settings.api_key.as_deref(), Some("llama-secret"));

    // The router key file must not be group/other readable: a default 0644
    // file is refused by effective_settings() (the runtime view).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&settings_path, fs::Permissions::from_mode(0o644))
            .expect("set 0644");
        let err = manager
            .effective_settings()
            .expect_err("0644 settings must be refused");
        assert!(
            err.to_string().contains("must not be accessible by group or other"),
            "{err}"
        );

        // 0600 passes the gate and round-trips.
        fs::set_permissions(&settings_path, fs::Permissions::from_mode(0o600))
            .expect("set 0600");
        let effective = manager
            .effective_settings()
            .expect("0600 settings accepted")
            .expect("present");
        assert_eq!(effective.base_url, "http://127.0.0.1:8080/v1");
    }
}

#[test]
fn catalog_cache_round_trip_and_apply() {
    let root = tempfile::tempdir().expect("llama root");
    let manager = manager(root.path());

    // No cache yet: empty.
    assert!(manager.cached_catalog().expect("catalog").is_none());
    assert!(manager.load_cached_catalog().expect("load").is_empty());

    // Persist a snapshot (the shape the live refresh writes).
    let mut model = Model::default();
    model.id = "llama-catalog-model".to_owned();
    model.api = "llama".to_owned();
    model.provider = "llama".to_owned();
    model.base_url = "http://127.0.0.1:8080/v1".into();
    let snapshot = LlamaCatalogSnapshot {
        checked_at: 1_700_000_000_000,
        models: vec![model],
    };
    fs::write(
        manager.catalog_path(),
        serde_json::to_vec_pretty(&snapshot).expect("serialize snapshot"),
    )
    .expect("write catalog");

    let loaded = manager
        .cached_catalog()
        .expect("catalog read")
        .expect("catalog present");
    assert_eq!(loaded.checked_at, 1_700_000_000_000);
    assert_eq!(loaded.models.len(), 1);
    assert_eq!(loaded.models[0].id, "llama-catalog-model");

    // load_cached_catalog applies the persisted catalog to the registry.
    let applied = manager.load_cached_catalog().expect("load cached");
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].id, "llama-catalog-model");
}

#[test]
fn installed_gguf_catalog_round_trip_and_default() {
    let root = tempfile::tempdir().expect("llama root");
    let manager = manager(root.path());

    // Missing installed file defaults to an empty catalog.
    assert_eq!(manager.installed().expect("installed").models.len(), 0);

    // A populated installed.json round-trips through the public type.
    let catalog = InstalledGgufCatalog {
        models: vec![InstalledGgufModel {
            repository: "org/quant-model".to_owned(),
            quantization: "Q4_K_M".to_owned(),
            installed_at: 1_700_000_000_000,
            files: vec![InstalledGgufFile {
                source: "https://huggingface.co/org/quant-model/resolve/main/model.Q4_K_M.gguf"
                    .to_owned(),
                relative_path: "models/org/quant-model/Q4_K_M/model.Q4_K_M.gguf".into(),
                size: 4_200_000_000,
                sha256: Some("ab12cd34".repeat(8)),
            }],
        }],
    };
    fs::write(
        manager.installed_path(),
        serde_json::to_vec_pretty(&catalog).expect("serialize installed"),
    )
    .expect("write installed");

    let loaded = manager.installed().expect("installed read");
    assert_eq!(loaded.models.len(), 1);
    assert_eq!(loaded.models[0].repository, "org/quant-model");
    assert_eq!(loaded.models[0].files[0].size, 4_200_000_000);
    let sha = loaded.models[0].files[0]
        .sha256
        .as_deref()
        .expect("sha256 present");
    assert_eq!(sha, "ab12cd34".repeat(8));
}

#[test]
fn manager_paths_are_derived_and_stable() {
    let root = tempfile::tempdir().expect("llama root");
    let manager = manager(root.path());
    assert_eq!(manager.root(), root.path());
    assert_eq!(manager.settings_path(), root.path().join("router.json"));
    assert_eq!(manager.catalog_path(), root.path().join("catalog.json"));
    assert_eq!(manager.installed_path(), root.path().join("installed.json"));
    assert_eq!(manager.models_dir(), root.path().join("models"));
}
