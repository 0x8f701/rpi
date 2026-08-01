use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
use pi_ai::*;
use pi_ai::providers::{
    DEFAULT_RADIUS_GATEWAY, RadiusCatalog, RadiusCatalogSnapshot, RadiusGatewayConfig,
    RadiusGatewayModel, RadiusRefreshOptions, models_from_config,
};
use serde_json::json;
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::TcpListener};

async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = socket.read(&mut buffer).await.expect("read request");
        if count == 0 { break; }
        request.extend_from_slice(&buffer[..count]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") { break; }
    }
    String::from_utf8(request).expect("UTF-8 request")
}

fn config(id: &str, base_url: &str) -> RadiusGatewayConfig {
    RadiusGatewayConfig {
        base_url: base_url.into(),
        models: vec![RadiusGatewayModel {
            id: id.into(),
            name: format!("Model {id}"),
            reasoning: true,
            thinking_level_map: None,
            input: vec!["text".into(), "image".into()],
            cost: ModelCost { input:1.0,output:2.0,cache_read:0.5,cache_write:0.25,tiers:vec![] },
            context_window: 32_000,
            max_tokens: 4_096,
        }],
    }
}


#[test]
fn refresh_options_debug_omits_header_values() {
    let options = RadiusRefreshOptions {
        headers: std::collections::HashMap::from([
            ("Authorization".into(), "Bearer must-not-leak".into()),
            ("X-API-Key".into(), "also-secret".into()),
        ]),
        timeout_ms: Some(123),
        max_retries: 2,
        max_retry_delay_ms: Some(456),
        ..Default::default()
    };
    let debug = format!("{options:?}");
    assert!(debug.contains("Authorization"));
    assert!(debug.contains("X-API-Key"));
    assert!(debug.contains("header_count: 2"));
    assert!(debug.contains("timeout_ms: Some(123)"));
    assert!(!debug.contains("must-not-leak"));
    assert!(!debug.contains("also-secret"));
    assert!(!debug.contains("Bearer"));
}
#[tokio::test]
async fn authenticated_refresh_applies_only_after_valid_catalog() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let request = read_request(&mut socket).await;
        assert!(request.starts_with("GET /v1/config "));
        assert!(request.to_ascii_lowercase().contains("authorization: bearer radius-secret"));
        let body = serde_json::to_string(&config("dynamic", "https://stream.radius.test/v1")).expect("catalog JSON");
        let response = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
        socket.write_all(response.as_bytes()).await.expect("response");
    });
    let provider = "radius-refresh-fixture";
    let catalog = RadiusCatalog::new(provider, format!("http://{address}")).expect("catalog");
    assert!(get_models(provider).is_empty());
    let snapshot = catalog.refresh("radius-secret", RadiusRefreshOptions::default()).await.expect("refresh");
    assert_eq!(snapshot.models.len(), 1);
    let registered = get_model(provider, "dynamic").expect("registered model");
    assert_eq!(registered.api, API_PI_MESSAGES);
    assert_eq!(registered.base_url, "https://stream.radius.test/v1");
}

#[tokio::test]
async fn invalid_refresh_retains_stale_snapshot_and_catalog() {
    let provider = "radius-invalid-fixture";
    let catalog = RadiusCatalog::new(provider, "http://127.0.0.1:1").expect("catalog");
    let stale = RadiusCatalogSnapshot { models:models_from_config(provider,config("old","https://old.radius.test/v1")).expect("models"),checked_at:Some(7) };
    catalog.restore_snapshot(stale.clone()).expect("restore");

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut socket).await;
        let body = json!({"baseUrl":"", "models":[]}).to_string();
        let response = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
        socket.write_all(response.as_bytes()).await.expect("response");
    });
    let failing = RadiusCatalog::new(provider, format!("http://{address}")).expect("catalog");
    failing.restore_snapshot(stale.clone()).expect("restore stale");
    assert!(failing.refresh("secret", RadiusRefreshOptions::default()).await.is_err());
    assert_eq!(failing.snapshot(), stale);
    assert!(get_model(provider, "old").is_some());
}

#[tokio::test]
async fn oversized_streamed_refresh_retains_stale_catalog() {
    let provider = "radius-oversized-fixture";
    let stale = RadiusCatalogSnapshot {
        models: models_from_config(provider, config("old", "https://old.radius.test/v1"))
            .expect("models"),
        checked_at: Some(8),
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n")
            .await
            .expect("response headers");
        let oversized = vec![b'x'; 1024 * 1024 + 1];
        let _ = socket.write_all(&oversized).await;
    });
    let catalog = RadiusCatalog::new(provider, format!("http://{address}"))
        .expect("catalog");
    catalog
        .restore_snapshot(stale.clone())
        .expect("restore stale");
    let error = catalog
        .refresh("secret", RadiusRefreshOptions::default())
        .await
        .expect_err("oversized response");
    assert!(error.to_string().contains("exceeds 1048576 bytes"));
    assert_eq!(catalog.snapshot(), stale);
    assert!(get_model(provider, "old").is_some());
}

#[tokio::test]
async fn oversized_declared_length_is_rejected_before_body_read() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 1048577\r\nconnection: close\r\n\r\n")
            .await
            .expect("response headers");
    });
    let catalog = RadiusCatalog::new("radius-length-cap-fixture", format!("http://{address}"))
        .expect("catalog");
    let error = catalog
        .refresh("secret", RadiusRefreshOptions::default())
        .await
        .expect_err("oversized declared length");
    assert!(error.to_string().contains("exceeds 1048576 bytes"));
    assert!(catalog.snapshot().models.is_empty());
}

#[tokio::test]
async fn refresh_retries_and_secret_is_redacted_on_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let attempts = Arc::new(AtomicUsize::new(0));
    let server_attempts = attempts.clone();
    tokio::spawn(async move {
        for attempt in 0..2 {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let request = read_request(&mut socket).await;
            assert!(request.to_ascii_lowercase().contains("authorization: bearer top-secret"));
            server_attempts.fetch_add(1, Ordering::SeqCst);
            let response = if attempt == 0 {
                "HTTP/1.1 503 Service Unavailable\r\nretry-after-ms: 0\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned()
            } else {
                let body = json!({"error":{"message":"top-secret rejected"}}).to_string();
                format!("HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len())
            };
            socket.write_all(response.as_bytes()).await.expect("response");
        }
    });
    let catalog = RadiusCatalog::new("radius-redact-fixture", format!("http://{address}")).expect("catalog");
    let error = catalog.refresh("top-secret", RadiusRefreshOptions { max_retries:1,..Default::default() }).await.expect_err("failure");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(error.to_string().contains("[REDACTED]"));
    assert!(!error.to_string().contains("top-secret"));
    assert!(catalog.snapshot().models.is_empty());
}

#[tokio::test]
async fn refresh_redacts_overridden_authorization_from_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let request = read_request(&mut socket).await;
        let lower = request.to_ascii_lowercase();
        assert!(lower.contains("authorization: bearer override-secret"));
        assert!(!lower.contains("bearer default-secret"));
        assert_eq!(
            request
                .lines()
                .filter(|line| line.to_ascii_lowercase().starts_with("authorization:"))
                .count(),
            1
        );
        let body = json!({"error":{"message":"override-secret rejected default-secret"}}).to_string();
        let response = format!("HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
        socket.write_all(response.as_bytes()).await.expect("response");
    });
    let catalog = RadiusCatalog::new("radius-override-redact-fixture", format!("http://{address}"))
        .expect("catalog");
    let error = catalog
        .refresh(
            "default-secret",
            RadiusRefreshOptions {
                headers: std::collections::HashMap::from([(
                    "Authorization".into(),
                    "Bearer override-secret".into(),
                )]),
                ..Default::default()
            },
        )
        .await
        .expect_err("failure");
    let error = error.to_string();
    assert!(error.contains("[REDACTED]"));
    assert!(!error.contains("default-secret"));
    assert!(!error.contains("override-secret"));
}

#[tokio::test]
async fn cancelled_refresh_retains_stale_snapshot() {
    let provider = "radius-cancel-fixture";
    let catalog = RadiusCatalog::new(provider, DEFAULT_RADIUS_GATEWAY).expect("catalog");
    let stale = RadiusCatalogSnapshot { models:models_from_config(provider,config("old","https://old.radius.test/v1")).expect("models"),checked_at:Some(9) };
    catalog.restore_snapshot(stale.clone()).expect("restore");
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    let error = catalog.refresh("secret", RadiusRefreshOptions { abort_signal:Some(token),..Default::default() }).await.expect_err("cancelled");
    assert_eq!(error.to_string(), "Request was aborted");
    assert_eq!(catalog.snapshot(), stale);
    assert!(get_model(provider, "old").is_some());
}

#[test]
fn same_provider_instances_replace_one_authoritative_set() {
    let provider = "radius-multi-instance-fixture";
    let first = RadiusCatalog::new(provider, DEFAULT_RADIUS_GATEWAY).expect("first catalog");
    let second = RadiusCatalog::new(provider, DEFAULT_RADIUS_GATEWAY).expect("second catalog");
    first
        .restore_snapshot(RadiusCatalogSnapshot {
            models: models_from_config(provider, config("first", "https://first.test/v1"))
                .expect("first models"),
            checked_at: Some(1),
        })
        .expect("first restore");
    second
        .restore_snapshot(RadiusCatalogSnapshot {
            models: models_from_config(provider, config("second", "https://second.test/v1"))
                .expect("second models"),
            checked_at: Some(2),
        })
        .expect("second restore");
    assert!(get_model(provider, "first").is_none());
    assert!(get_model(provider, "second").is_some());
}
