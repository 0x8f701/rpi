use pi_ai::providers::{
    DEFAULT_RADIUS_GATEWAY, RadiusCatalog, RadiusCatalogSnapshot, RadiusGatewayConfig,
    RadiusGatewayModel, models_from_config,
};
use pi_ai::{ModelCost, RadiusCatalogStore, get_model};
use serde_json::{Value, json};
use std::fs;
use std::sync::{Arc, Barrier};
use tempfile::TempDir;

fn snapshot(provider: &str, id: &str, checked_at: i64) -> RadiusCatalogSnapshot {
    let config = RadiusGatewayConfig {
        base_url: format!("https://{provider}.example.test/v1"),
        models: vec![RadiusGatewayModel {
            id: id.into(),
            name: format!("Model {id}"),
            reasoning: true,
            thinking_level_map: None,
            input: vec!["text".into(), "image".into()],
            cost: ModelCost::default(),
            context_window: 32_000,
            max_tokens: 4_096,
        }],
    };
    RadiusCatalogSnapshot {
        models: models_from_config(provider, config).expect("valid snapshot models"),
        checked_at: Some(checked_at),
    }
}

#[tokio::test]
async fn successful_refresh_survives_a_fresh_catalog_instance_offline() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let directory = TempDir::new().expect("temporary store directory");
    let path = directory.path().join("models-store.json");
    let provider = "radius-persist-fixture";
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind gateway");
    let address = listener.local_addr().expect("gateway address");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept refresh");
        let mut request = [0_u8; 4096];
        let count = socket.read(&mut request).await.expect("read refresh request");
        assert!(String::from_utf8_lossy(&request[..count]).starts_with("GET /v1/config "));
        let body = serde_json::to_string(&RadiusGatewayConfig {
            base_url: "https://persisted.example.test/v1".into(),
            models: vec![RadiusGatewayModel {
                id: "offline".into(),
                name: "Offline".into(),
                reasoning: true,
                thinking_level_map: None,
                input: vec!["text".into()],
                cost: ModelCost::default(),
                context_window: 32_000,
                max_tokens: 4_096,
            }],
        })
        .expect("serialize gateway config");
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write refresh response");
    });
    let first = RadiusCatalog::with_store(provider, format!("http://{address}"), &path)
        .expect("first catalog");
    let stored = first
        .refresh("secret", Default::default())
        .await
        .expect("successful refresh");

    let fresh = RadiusCatalog::with_store(provider, DEFAULT_RADIUS_GATEWAY, &path)
        .expect("fresh catalog");
    let restored = fresh
        .restore_stored_snapshot()
        .expect("restore stored snapshot")
        .expect("stored snapshot exists");
    assert_eq!(restored, stored);
    assert_eq!(fresh.snapshot(), stored);
    assert!(get_model(provider, "offline").is_some());
}

#[test]
fn corrupt_store_fails_closed_without_deleting_live_catalog() {
    let directory = TempDir::new().expect("temporary store directory");
    let path = directory.path().join("models-store.json");
    let provider = "radius-corrupt-store-fixture";
    let catalog = RadiusCatalog::with_store(provider, DEFAULT_RADIUS_GATEWAY, &path)
        .expect("catalog");
    let live = snapshot(provider, "live", 7);
    catalog
        .restore_snapshot(live.clone())
        .expect("install live snapshot");
    fs::write(&path, b"{not valid json").expect("corrupt store");

    assert!(catalog.restore_stored_snapshot().is_err());
    assert_eq!(catalog.snapshot(), live);
    assert!(get_model(provider, "live").is_some());
}

#[test]
fn invalid_stored_model_is_never_registered() {
    let directory = TempDir::new().expect("temporary store directory");
    let path = directory.path().join("models-store.json");
    let provider = "radius-invalid-store-fixture";
    let invalid = json!({
        "version": 1,
        "providers": {
            provider: {
                "models": [{
                    "id": "poisoned",
                    "name": "Poisoned",
                    "api": "openai-completions",
                    "provider": provider,
                    "baseUrl": "https://invalid.example.test/v1",
                    "reasoning": false,
                    "input": ["text"],
                    "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0},
                    "contextWindow": 32000,
                    "maxTokens": 4096
                }],
                "checkedAt": 1
            }
        }
    });
    fs::write(&path, serde_json::to_vec(&invalid).expect("serialize invalid store"))
        .expect("write invalid store");
    let catalog = RadiusCatalog::with_store(provider, DEFAULT_RADIUS_GATEWAY, &path)
        .expect("catalog");

    assert!(catalog.restore_stored_snapshot().is_err());
    assert!(catalog.snapshot().models.is_empty());
    assert!(get_model(provider, "poisoned").is_none());
}

#[test]
fn writes_are_atomic_and_leave_no_temporary_files() {
    let directory = TempDir::new().expect("temporary store directory");
    let path = directory.path().join("models-store.json");
    let store = RadiusCatalogStore::new(&path);
    store
        .write("radius-atomic-one", &snapshot("radius-atomic-one", "one", 1))
        .expect("first write");
    store
        .write("radius-atomic-one", &snapshot("radius-atomic-one", "two", 2))
        .expect("replacement write");

    let document: Value = serde_json::from_slice(&fs::read(&path).expect("read store"))
        .expect("complete JSON document");
    assert_eq!(document["version"], 1);
    assert_eq!(
        store
            .read("radius-atomic-one")
            .expect("read entry")
            .expect("entry")
            .models[0]
            .id,
        "two"
    );
    let entries = fs::read_dir(directory.path())
        .expect("read directory")
        .map(|entry| entry.expect("directory entry").file_name())
        .collect::<Vec<_>>();
    assert_eq!(entries, vec![std::ffi::OsString::from("models-store.json")]);
}

#[test]
fn concurrent_provider_writes_preserve_both_entries() {
    let directory = TempDir::new().expect("temporary store directory");
    let path = directory.path().join("models-store.json");
    let start = Arc::new(Barrier::new(3));
    let first_start = start.clone();
    let first_path = path.clone();
    let first = std::thread::spawn(move || {
        first_start.wait();
        RadiusCatalogStore::new(first_path)
            .write("radius-concurrent-one", &snapshot("radius-concurrent-one", "one", 1))
            .expect("write first provider");
    });
    let second_start = start.clone();
    let second_path = path.clone();
    let second = std::thread::spawn(move || {
        second_start.wait();
        RadiusCatalogStore::new(second_path)
            .write("radius-concurrent-two", &snapshot("radius-concurrent-two", "two", 2))
            .expect("write second provider");
    });
    start.wait();
    first.join().expect("first writer");
    second.join().expect("second writer");

    let store = RadiusCatalogStore::new(path);
    assert_eq!(
        store
            .read("radius-concurrent-one")
            .expect("read first")
            .expect("first entry")
            .models[0]
            .id,
        "one"
    );
    assert_eq!(
        store
            .read("radius-concurrent-two")
            .expect("read second")
            .expect("second entry")
            .models[0]
            .id,
        "two"
    );
}

#[tokio::test]
async fn persistence_failure_rolls_back_in_memory_catalog() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let directory = TempDir::new().expect("temporary store directory");
    let store_path = directory.path().join("models-store.json");
    fs::create_dir(&store_path).expect("make store path unusable as a file");
    let provider = "radius-write-failure-fixture";
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind gateway");
    let address = listener.local_addr().expect("gateway address");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept refresh");
        let mut request = [0_u8; 4096];
        socket.read(&mut request).await.expect("read refresh request");
        let config = RadiusGatewayConfig {
            base_url: "https://new.example.test/v1".into(),
            models: vec![RadiusGatewayModel {
                id: "new".into(),
                name: "New".into(),
                reasoning: false,
                thinking_level_map: None,
                input: vec!["text".into()],
                cost: ModelCost::default(),
                context_window: 32_000,
                max_tokens: 4_096,
            }],
        };
        let body = serde_json::to_string(&config).expect("serialize config");
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write refresh response");
    });
    let catalog = RadiusCatalog::with_store(provider, format!("http://{address}"), &store_path)
        .expect("catalog");
    let stale = snapshot(provider, "stale", 1);
    catalog
        .restore_snapshot(stale.clone())
        .expect("install stale snapshot");

    assert!(catalog.refresh("secret", Default::default()).await.is_err());
    assert_eq!(catalog.snapshot(), stale);
    assert!(get_model(provider, "stale").is_some());
    assert!(get_model(provider, "new").is_none());
}

#[test]
fn unsupported_version_does_not_register_models() {
    let directory = TempDir::new().expect("temporary store directory");
    let path = directory.path().join("models-store.json");
    fs::write(&path, br#"{"version":2,"providers":{}}"#).expect("write future store");
    let store = RadiusCatalogStore::new(&path);
    let error = store.read("radius-version-fixture").expect_err("future version fails");
    assert!(error.to_string().contains("unsupported Radius catalog store version"));
}
