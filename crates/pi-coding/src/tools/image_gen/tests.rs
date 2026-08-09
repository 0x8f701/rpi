//! `generate_image` tool tests: end-to-end against a mock HTTP
//! `images/generations` server (b64_json responses), plus argument bounds,
//! capability gating, endpoint overrides, redaction, and workspace
//! containment.

use super::*;
use base64::Engine as _;
use pi_agent::{AgentToolResult, ToolCallContext};
use pi_ai::API_IMAGE_GEN;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A real 1x1 transparent PNG, base64-encoded.
const ONE_PX_PNG_B64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

fn tmpdir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pi-image-gen-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn make_ctx(args: Value) -> ToolCallContext {
    let (_ctrl, abort) = pi_agent::AbortController::new();
    std::mem::forget(_ctrl);
    ToolCallContext {
        tool_call_id: "test".to_string(),
        arguments: args,
        on_update: Arc::new(|_result: AgentToolResult| {}),
        abort,
        model: None,
    }
}

fn make_ctx_with_model(args: Value, model: pi_ai::Model) -> ToolCallContext {
    let mut ctx = make_ctx(args);
    ctx.model = Some(model);
    ctx
}

fn text_of(result: &AgentToolResult) -> String {
    match result.content.first() {
        Some(pi_ai::ContentBlock::Text { text, .. }) => text.clone(),
        _ => String::new(),
    }
}

/// Serves one HTTP response for one request; captures the raw request bytes.
async fn spawn_mock(response_body: String, status: &str) -> (String, Arc<Mutex<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let address = listener.local_addr().expect("mock address");
    let captured = Arc::new(Mutex::new(String::new()));
    let request = captured.clone();
    let status = status.to_owned();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept mock request");
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let read = socket.read(&mut chunk).await.expect("read request");
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        *request.lock().expect("request lock") = String::from_utf8_lossy(&buffer).into_owned();
        let body = response_body;
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    });
    (address.to_string(), captured)
}

/// Serves one 200 response; while the request is in flight it replaces the
/// workspace's `images` directory with a symlink pointing at `outside` — the
/// hostile swap the write-time guard must detect.
#[cfg(unix)]
async fn spawn_mock_swapping_images(cwd: &Path, outside: &Path, status: &str) -> (String, ()) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let address = listener.local_addr().expect("mock address");
    let cwd = cwd.to_path_buf();
    let outside = outside.to_path_buf();
    let status = status.to_owned();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept mock request");
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let read = socket.read(&mut chunk).await.expect("read request");
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        // The save target was validated before the network call; swap the
        // validated directory for an escaping symlink now.
        let images = cwd.join("images");
        let _ = std::fs::remove_dir_all(&images);
        std::os::unix::fs::symlink(&outside, &images).expect("swap images for symlink");
        let body = json_data_response(&[ONE_PX_PNG_B64]);
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    });
    (address.to_string(), ())
}

fn json_data_response(items: &[&str]) -> String {
    let data: Vec<Value> = items
        .iter()
        .map(|b64| json!({ "b64_json": b64 }))
        .collect();
    json!({ "data": data }).to_string()
}

fn image_model(base_url: String, image_generation: bool) -> pi_ai::Model {
    pi_ai::Model {
        id: "image-model".into(),
        name: "Image Model".into(),
        api: API_IMAGE_GEN.into(),
        provider: "test-images".into(),
        base_url,
        image_generation,
        ..pi_ai::Model::default()
    }
}

fn resolver_for(
    model: pi_ai::Model,
    api_key: Option<String>,
    base_url: Option<String>,
) -> ImageGenConfigFn {
    Arc::new(move |_spec: Option<String>| {
        let model = model.clone();
        let api_key = api_key.clone();
        let base_url = base_url.clone();
        Box::pin(async move {
            Ok(ImageGenConfig {
                model,
                base_url,
                api_key,
            })
        })
    })
}

fn tool_for(cwd: &Path, resolver: Option<ImageGenConfigFn>) -> AgentTool {
    generate_image_tool_for_workspace(
        crate::WorkspaceRoots::for_tool_factory(&cwd.to_string_lossy()),
        resolver,
    )
}

#[tokio::test]
async fn request_shape_base_join_bearer_and_body() {
    let (address, captured) =
        spawn_mock(json_data_response(&[ONE_PX_PNG_B64]), "200 OK").await;
    let cwd = tmpdir();
    let model = image_model(format!("http://{address}/v1"), true);
    let tool = tool_for(
        &cwd,
        Some(resolver_for(model, Some("sk-image-test".into()), None)),
    );

    let result = (tool.execute)(make_ctx(json!({
        "prompt": "a red png",
        "size": "1024x1024",
        "n": 1,
    })))
    .await
    .expect("generation");

    let text = text_of(&result);
    assert!(text.contains("Generated 1 image(s)"), "{text}");
    assert!(text.contains("images/image.png"), "{text}");
    assert!(text.contains("Prompt: a red png"), "{text}");

    let request = captured.lock().expect("request lock").clone();
    assert!(request.starts_with("POST /v1/images/generations "), "{request}");
    let lowered = request.to_ascii_lowercase();
    assert!(
        lowered.contains("authorization: bearer sk-image-test"),
        "{request}"
    );
    let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
    let parsed: Value = serde_json::from_str(body).expect("json body");
    assert_eq!(parsed["model"], "image-model");
    assert_eq!(parsed["prompt"], "a red png");
    assert_eq!(parsed["n"], 1);
    assert_eq!(parsed["size"], "1024x1024");
    assert_eq!(parsed["response_format"], "b64_json");

    // The decoded PNG was saved to the workspace.
    let saved = cwd.join("images/image.png");
    let bytes = std::fs::read(&saved).expect("saved image");
    let expected = base64::engine::general_purpose::STANDARD
        .decode(ONE_PX_PNG_B64)
        .expect("decode fixture");
    assert_eq!(bytes, expected);
}

#[tokio::test]
async fn saves_multiple_images_and_bounds_n() {
    let (address, captured) = spawn_mock(
        json_data_response(&[ONE_PX_PNG_B64, ONE_PX_PNG_B64]),
        "200 OK",
    )
    .await;
    let cwd = tmpdir();
    let model = image_model(format!("http://{address}/v1"), true);
    let tool = tool_for(&cwd, Some(resolver_for(model, Some("sk-image-test".into()), None)));

    let result = (tool.execute)(make_ctx(json!({
        "prompt": "two red pngs",
        "n": 2,
    })))
    .await
    .expect("generation of two");

    let text = text_of(&result);
    assert!(text.contains("Generated 2 image(s)"), "{text}");
    assert!(text.contains("images/image-1.png"), "{text}");
    assert!(text.contains("images/image-2.png"), "{text}");
    assert!(cwd.join("images/image-1.png").exists(), "first image saved");
    assert!(cwd.join("images/image-2.png").exists(), "second image saved");
    let _ = captured;

    // n above the bound is rejected before any network call.
    let tool = tool_for(&cwd, None);
    let error = (tool.execute)(make_ctx(json!({ "prompt": "x", "n": 5 })))
        .await
        .expect_err("n=5 must fail");
    assert!(
        error.to_string().contains("between 1 and 4"),
        "{error}"
    );
}

#[tokio::test]
async fn size_whitelist_is_enforced() {
    let cwd = tmpdir();
    let tool = tool_for(&cwd, None);
    let error = (tool.execute)(make_ctx(json!({
        "prompt": "a red png",
        "size": "512x1024",
    })))
    .await
    .expect_err("non-whitelisted size must fail");
    assert!(error.to_string().contains("Unsupported size"), "{error}");
    assert!(error.to_string().contains("256x256"), "{error}");
}

#[tokio::test]
async fn prompt_bounds_are_enforced() {
    let cwd = tmpdir();
    let tool = tool_for(&cwd, None);
    let error = (tool.execute)(make_ctx(json!({
        "prompt": "a".repeat(MAX_IMAGE_GEN_PROMPT_CHARS + 1),
    })))
    .await
    .expect_err("oversized prompt must fail");
    assert!(
        error.to_string().contains("exceeding the generate_image limit"),
        "{error}"
    );

    let error = (tool.execute)(make_ctx(json!({ "prompt": "   " })))
        .await
        .expect_err("blank prompt must fail");
    assert!(error.to_string().contains("must not be empty"), "{error}");
}

#[tokio::test]
async fn model_without_image_capability_errors_actionably() {
    let cwd = tmpdir();
    let model = image_model("http://127.0.0.1:1/v1".into(), false);
    let tool = tool_for(
        &cwd,
        Some(resolver_for(model, Some("sk-image-test".into()), None)),
    );
    let error = (tool.execute)(make_ctx(json!({ "prompt": "a red png" })))
        .await
        .expect_err("non-image model must fail");
    let message = error.to_string();
    assert!(message.contains("does not support image generation"), "{message}");
    assert!(message.contains("images.genModel"), "{message}");
}

#[tokio::test]
async fn gen_base_url_override_wins_over_model_base() {
    let (address, captured) =
        spawn_mock(json_data_response(&[ONE_PX_PNG_B64]), "200 OK").await;
    let cwd = tmpdir();
    let model = image_model("http://ignored.invalid/v1".into(), true);
    let tool = tool_for(
        &cwd,
        Some(resolver_for(
            model,
            Some("sk-image-test".into()),
            Some(format!("http://{address}/custom/v1")),
        )),
    );

    (tool.execute)(make_ctx(json!({ "prompt": "a red png" })))
        .await
        .expect("generation with override");

    let request = captured.lock().expect("request lock").clone();
    assert!(
        request.starts_with("POST /custom/v1/images/generations "),
        "{request}"
    );
}

#[tokio::test]
async fn errors_redact_the_api_key() {
    // The mock echoes the api key in its error body; neither the tool error
    // nor the client error may surface it.
    let (address, _) = spawn_mock(
        r#"{"error":{"message":"auth failed for sk-image-test"}}"#.into(),
        "500 Internal Server Error",
    )
    .await;
    let cwd = tmpdir();
    let model = image_model(format!("http://{address}/v1"), true);
    let tool = tool_for(
        &cwd,
        Some(resolver_for(model, Some("sk-image-test".into()), None)),
    );
    let error = (tool.execute)(make_ctx(json!({ "prompt": "a red png" })))
        .await
        .expect_err("server error must fail");
    let message = error.to_string();
    assert!(
        !message.contains("sk-image-test"),
        "api key leaked in error: {message}"
    );
    assert!(message.contains("auth failed"), "{message}");
}

#[tokio::test]
async fn workspace_escape_is_rejected() {
    let cwd = tmpdir();
    // A valid image-capable config so the run reaches the path containment
    // check (the escape must fail before any network call).
    let model = image_model("http://127.0.0.1:1/v1".into(), true);
    let tool = tool_for(
        &cwd,
        Some(resolver_for(model, Some("sk-image-test".into()), None)),
    );
    let error = (tool.execute)(make_ctx(json!({
        "prompt": "a red png",
        "path": "../outside.png",
    })))
    .await
    .expect_err("escape must fail");
    let message = error.to_string();
    assert!(
        message.contains("outside") || message.to_ascii_lowercase().contains("workspace"),
        "{message}"
    );
    assert!(!std::path::Path::new(&cwd)
        .parent()
        .unwrap()
        .join("outside.png")
        .exists());
}

#[tokio::test]
async fn explicit_file_path_with_mismatched_extension_is_rejected() {
    let (address, _) = spawn_mock(json_data_response(&[ONE_PX_PNG_B64]), "200 OK").await;
    let cwd = tmpdir();
    let model = image_model(format!("http://{address}/v1"), true);
    let tool = tool_for(
        &cwd,
        Some(resolver_for(model, Some("sk-image-test".into()), None)),
    );
    let error = (tool.execute)(make_ctx(json!({
        "prompt": "a red png",
        "path": "art.jpg",
    })))
    .await
    .expect_err("extension mismatch must fail");
    let message = error.to_string();
    assert!(message.contains("returned a png image"), "{message}");
    assert!(!cwd.join("art.jpg").exists());
}

#[tokio::test]
async fn explicit_file_path_saves_at_that_path() {
    let (address, _) = spawn_mock(json_data_response(&[ONE_PX_PNG_B64]), "200 OK").await;
    let cwd = tmpdir();
    let model = image_model(format!("http://{address}/v1"), true);
    let tool = tool_for(
        &cwd,
        Some(resolver_for(model, Some("sk-image-test".into()), None)),
    );
    let result = (tool.execute)(make_ctx(json!({
        "prompt": "a red png",
        "path": "art/hero.png",
    })))
    .await
    .expect("generation to explicit path");
    let text = text_of(&result);
    assert!(text.contains("art/hero.png"), "{text}");
    assert!(cwd.join("art/hero.png").exists(), "saved at explicit path");
}

#[tokio::test]
async fn standalone_tool_falls_back_to_turn_model() {
    // No resolver: the tool uses the turn's model and env credentials. The
    // mock is never reached — the client refuses the missing api key, which
    // proves the turn model was picked up.
    let cwd = tmpdir();
    let model = image_model("http://127.0.0.1:1/v1".into(), true);
    let tool = tool_for(&cwd, None);
    let error = (tool.execute)(make_ctx_with_model(
        json!({ "prompt": "a red png" }),
        model,
    ))
    .await
    .expect_err("missing api key must fail");
    assert!(
        error.to_string().contains("No API key for provider: test-images"),
        "{error}"
    );
}

#[tokio::test]
async fn revised_prompt_is_echoed_bounded() {
    let (address, _) = spawn_mock(
        json!({ "data": [{ "b64_json": ONE_PX_PNG_B64, "revised_prompt": "a red png, high quality" }] })
            .to_string(),
        "200 OK",
    )
    .await;
    let cwd = tmpdir();
    let model = image_model(format!("http://{address}/v1"), true);
    let tool = tool_for(
        &cwd,
        Some(resolver_for(model, Some("sk-image-test".into()), None)),
    );
    let result = (tool.execute)(make_ctx(json!({ "prompt": "a red png" })))
        .await
        .expect("generation");
    let text = text_of(&result);
    assert!(text.contains("revised prompt: a red png, high quality"), "{text}");
}

#[test]
fn save_target_names_files_by_count() {
    let cwd = tmpdir();
    let workspace = crate::WorkspaceRoots::for_tool_factory(&cwd.to_string_lossy());
    let target = SaveTarget::new(&workspace, "").expect("default target");
    let format = ImageFormat::Png;
    assert_eq!(
        target.resolve(format, 1, 0).unwrap().file_name().unwrap(),
        "image.png"
    );
    assert_eq!(
        target.resolve(format, 3, 1).unwrap().file_name().unwrap(),
        "image-2.png"
    );
}
    /// Returns a mock response with `count` copies of the 1x1 PNG.
    fn json_data_response_n(count: usize) -> String {
        let data: Vec<Value> = (0..count)
            .map(|_| json!({ "b64_json": ONE_PX_PNG_B64 }))
            .collect();
        json!({ "data": data }).to_string()
    }

    #[tokio::test]
    async fn prompt_at_exactly_the_bound_is_accepted() {
        // A prompt of exactly MAX_IMAGE_GEN_PROMPT_CHARS characters must pass
        // the bound check (boundary: one more would fail). This defends the
        // `>` vs `>=` choice in the tool's prompt gate.
        let (address, _) = spawn_mock(json_data_response_n(1), "200 OK").await;
        let cwd = tmpdir();
        let model = image_model(format!("http://{address}/v1"), true);
        let tool = tool_for(
            &cwd,
            Some(resolver_for(model, Some("sk-image-test".into()), None)),
        );
        let prompt = "a".repeat(MAX_IMAGE_GEN_PROMPT_CHARS);
        let result = (tool.execute)(make_ctx(json!({ "prompt": prompt })))
            .await
            .expect("boundary prompt accepted");
        assert!(text_of(&result).contains("Generated 1 image(s)"), "{}", text_of(&result));
    }

    #[tokio::test]
    async fn n_at_exactly_four_is_accepted() {
        // n=4 is the upper bound and must be accepted; one more (n=5) is
        // rejected. This defends the `> MAX_IMAGE_GEN_N` boundary.
        let (address, _) = spawn_mock(json_data_response_n(4), "200 OK").await;
        let cwd = tmpdir();
        let model = image_model(format!("http://{address}/v1"), true);
        let tool = tool_for(
            &cwd,
            Some(resolver_for(model, Some("sk-image-test".into()), None)),
        );
        let result = (tool.execute)(make_ctx(json!({ "prompt": "x", "n": 4 })))
            .await
            .expect("n=4 accepted");
        let text = text_of(&result);
        assert!(text.contains("Generated 4 image(s)"), "{text}");
        for i in 1..=4 {
            assert!(cwd.join(format!("images/image-{i}.png")).exists(), "image {i} saved");
        }
    }

    #[tokio::test]
    async fn capability_gate_creates_no_file_before_erroring() {
        // A model without image_generation must error before any filesystem
        // write — the default image directory is not created.
        let cwd = tmpdir();
        let model = image_model("http://127.0.0.1:1/v1".into(), false);
        let tool = tool_for(
            &cwd,
            Some(resolver_for(model, Some("sk-image-test".into()), None)),
        );
        let error = (tool.execute)(make_ctx(json!({ "prompt": "a red png" })))
            .await
            .expect_err("non-image model must fail");
        assert!(error.to_string().contains("does not support image generation"), "{error}");
        // The save target is resolved AFTER the capability gate, so no
        // `images` directory is created on a capability refusal.
        assert!(!cwd.join("images").exists(), "images dir created despite capability refusal");
    }

    #[tokio::test]
    async fn multiple_images_with_single_file_path_is_rejected() {
        // Requesting n>1 while naming a single image file is a contradiction
        // the tool must catch before any network call.
        let cwd = tmpdir();
        let model = image_model("http://127.0.0.1:1/v1".into(), true);
        let tool = tool_for(
            &cwd,
            Some(resolver_for(model, Some("sk-image-test".into()), None)),
        );
        let error = (tool.execute)(make_ctx(json!({
            "prompt": "a red png",
            "n": 2,
            "path": "single.png",
        })))
        .await
        .expect_err("n>1 with file path must fail");
        assert!(error.to_string().contains("single image file"), "{error}");
        assert!(error.to_string().contains("n=2"), "{error}");
        assert!(!cwd.join("single.png").exists());
    }

    #[tokio::test]
    async fn explicit_directory_path_saves_multiple_images() {
        // A non-extension `path` is treated as a directory; multiple images
        // save into it as image-1.png / image-2.png (the Dir resolve branch).
        let (address, _) = spawn_mock(json_data_response_n(2), "200 OK").await;
        let cwd = tmpdir();
        let model = image_model(format!("http://{address}/v1"), true);
        let tool = tool_for(
            &cwd,
            Some(resolver_for(model, Some("sk-image-test".into()), None)),
        );
        let result = (tool.execute)(make_ctx(json!({
            "prompt": "two pngs",
            "n": 2,
            "path": "out",
        })))
        .await
        .expect("generation into directory");
        let text = text_of(&result);
        assert!(text.contains("out/image-1.png"), "{text}");
        assert!(text.contains("out/image-2.png"), "{text}");
        assert!(cwd.join("out/image-1.png").exists());
        assert!(cwd.join("out/image-2.png").exists());
    }

    #[tokio::test]
    async fn corrupt_image_data_is_rejected_and_not_saved() {
        // The endpoint returns valid base64 that decodes to non-image bytes;
        // the inspect_image pre-check rejects it and nothing is written.
        let garbage = base64::engine::general_purpose::STANDARD
            .decode("aGVsbG8gd29ybGQ=") // "hello world"
            .expect("decode fixture");
        let b64_garbage = base64::engine::general_purpose::STANDARD.encode(&garbage);
        let (address, _) = spawn_mock(json_data_response(&[&b64_garbage]), "200 OK").await;
        let cwd = tmpdir();
        let model = image_model(format!("http://{address}/v1"), true);
        let tool = tool_for(
            &cwd,
            Some(resolver_for(model, Some("sk-image-test".into()), None)),
        );
        let error = (tool.execute)(make_ctx(json!({ "prompt": "a red png" })))
            .await
            .expect_err("corrupt image must fail");
        assert!(error.to_string().contains("not a supported image"), "{error}");
        // The save target directory is created eagerly before the network
        // call (by design), but no image file is written — the inspect_image
        // pre-check rejects the corrupt bytes before saving.
        assert!(!cwd.join("images/image.png").exists(), "corrupt image was saved");
    }

    #[tokio::test]
    async fn explicit_file_path_mismatch_on_jpeg_extension_is_rejected() {
        // A .jpeg extension must map to the "jpg" canonical name and still
        // mismatch a PNG payload — defending the jpeg→jpg normalization.
        let (address, _) = spawn_mock(json_data_response(&[ONE_PX_PNG_B64]), "200 OK").await;
        let cwd = tmpdir();
        let model = image_model(format!("http://{address}/v1"), true);
        let tool = tool_for(
            &cwd,
            Some(resolver_for(model, Some("sk-image-test".into()), None)),
        );
        let error = (tool.execute)(make_ctx(json!({
            "prompt": "a red png",
            "path": "shot.jpeg",
        })))
        .await
        .expect_err("jpeg/png mismatch must fail");
        assert!(error.to_string().contains("returned a png image"), "{error}");
        assert!(!cwd.join("shot.jpeg").exists());
    }

    #[test]
    fn has_image_extension_recognizes_all_whitelisted_extensions() {
        for ext in ["png", "jpg", "jpeg", "webp", "gif", "bmp", "PNG", "JPG"] {
            assert!(has_image_extension(&format!("x.{ext}")), "{ext} should be recognized");
        }
        // Non-image extensions and bare names are not image files.
        assert!(!has_image_extension("readme.txt"));
        assert!(!has_image_extension("noext"));
        assert!(!has_image_extension(""));
    }

    #[test]
    fn bounded_echo_truncates_long_prompt_with_marker() {
        let short = "hi";
        assert_eq!(bounded_echo(short), "hi");
        let long = "x".repeat(MAX_PROMPT_ECHO_CHARS + 10);
        let echoed = bounded_echo(&long);
        assert_eq!(echoed.len(), MAX_PROMPT_ECHO_CHARS + 3, "{}", echoed.len());
        assert!(echoed.ends_with("..."), "{echoed}");
    }

    #[test]
    fn format_extension_covers_supported_and_rejects_unknown() {
        assert_eq!(format_extension(ImageFormat::Png), Some("png"));
        assert_eq!(format_extension(ImageFormat::Jpeg), Some("jpg"));
        assert_eq!(format_extension(ImageFormat::Gif), Some("gif"));
        assert_eq!(format_extension(ImageFormat::Bmp), Some("bmp"));
        assert_eq!(format_extension(ImageFormat::WebP), Some("webp"));
        // A format outside the save whitelist (e.g. TIFF) has no extension,
        // so the saved file would never be mislabeled.
        assert_eq!(format_extension(ImageFormat::Tiff), None);
    }

    #[test]
    fn inspect_image_rejects_non_image_bytes() {
        let error = inspect_image(b"not an image at all").expect_err("non-image rejected");
        assert!(error.to_string().contains("format"), "{error}");
    }

    #[test]
    fn save_target_file_rejects_format_extension_mismatch() {
        let cwd = tmpdir();
        let workspace = crate::WorkspaceRoots::for_tool_factory(&cwd.to_string_lossy());
        let target = SaveTarget::new(&workspace, "out.png").expect("file target");
        // PNG matches; JPEG does not.
        assert!(target.resolve(ImageFormat::Png, 1, 0).is_ok());
        let error = target.resolve(ImageFormat::Jpeg, 1, 0).expect_err("mismatch");
        assert!(error.to_string().contains("returned a jpg image"), "{error}");
    }

    #[test]
    fn display_path_strips_workspace_prefix_for_relative_output() {
        let cwd = tmpdir();
        let workspace = crate::WorkspaceRoots::for_tool_factory(&cwd.to_string_lossy());
        let abs = cwd.join("images").join("image.png");
        assert_eq!(display_path(&workspace, &abs), "images/image.png");
        // An absolute path outside the workspace is reported verbatim.
        let outside = std::env::temp_dir().join("elsewhere.png");
        assert_eq!(display_path(&workspace, &outside), outside.to_string_lossy());
    }

    #[test]
    fn save_target_empty_path_creates_default_images_dir() {
        let cwd = tmpdir();
        let workspace = crate::WorkspaceRoots::for_tool_factory(&cwd.to_string_lossy());
        let target = SaveTarget::new(&workspace, "").expect("default dir");
        assert!(cwd.join("images").exists(), "default images dir created");
        assert!(matches!(target, SaveTarget::Dir(_)));
    }

    #[cfg(unix)]
    #[test]
    fn save_target_rejects_default_images_symlink_escaping_workspace() {
        // A pre-existing `images` symlink pointing outside the workspace
        // must be rejected by the default-path resolution (it goes through
        // resolve_scoped_path like any explicit path), never followed.
        let cwd = tmpdir();
        let outside = tmpdir();
        std::os::unix::fs::symlink(&outside, cwd.join("images")).expect("symlink images");
        let workspace = crate::WorkspaceRoots::for_tool_factory(&cwd.to_string_lossy());
        let error = SaveTarget::new(&workspace, "")
            .expect_err("default images symlink escaping the workspace must fail");
        let message = error.to_string();
        assert!(
            message.contains("escapes") || message.to_ascii_lowercase().contains("workspace"),
            "{message}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn default_save_rejects_parent_symlink_swap_before_write() {
        // The default `images` directory is validated before the network
        // call; a hostile swap of that directory for a symlink to outside
        // (done by the mock while the request is in flight) must be caught
        // at write time, and nothing may land outside the workspace.
        let cwd = tmpdir();
        let outside = tmpdir();
        let outside_clone = outside.clone();
        let cwd_clone = cwd.clone();
        let (address, _) = spawn_mock_swapping_images(&cwd_clone, &outside_clone, "200 OK").await;
        let model = image_model(format!("http://{address}/v1"), true);
        let tool = tool_for(
            &cwd,
            Some(resolver_for(model, Some("sk-image-test".into()), None)),
        );
        let error = (tool.execute)(make_ctx(json!({ "prompt": "a red png" })))
            .await
            .expect_err("symlink swap before write must fail");
        let message = error.to_string();
        assert!(
            message.contains("outside the workspace") || message.contains("symlink"),
            "{message}"
        );
        assert!(
            outside.read_dir().expect("outside dir").next().is_none(),
            "image bytes leaked outside the workspace: {}",
            outside.display()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn default_save_rejects_symlink_at_final_target() {
        // The final image file itself is a symlink to an outside location:
        // the write must refuse it (the O_NOFOLLOW guard) instead of
        // following the link.
        let cwd = tmpdir();
        let outside = tmpdir();
        let outside_file = outside.join("victim.png");
        std::fs::write(&outside_file, b"original").expect("outside victim");
        std::fs::create_dir_all(cwd.join("images")).expect("images dir");
        std::os::unix::fs::symlink(&outside_file, cwd.join("images").join("image.png"))
            .expect("symlink final target");
        let (address, _) = spawn_mock(json_data_response(&[ONE_PX_PNG_B64]), "200 OK").await;
        let model = image_model(format!("http://{address}/v1"), true);
        let tool = tool_for(
            &cwd,
            Some(resolver_for(model, Some("sk-image-test".into()), None)),
        );
        let error = (tool.execute)(make_ctx(json!({ "prompt": "a red png" })))
            .await
            .expect_err("symlink at the final target must fail");
        assert!(error.to_string().contains("symlink"), "{error}");
        assert_eq!(
            std::fs::read(&outside_file).expect("victim unchanged"),
            b"original",
            "the symlinked file must not be overwritten"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn explicit_path_rejects_parent_symlink_swap_before_write() {
        // Same guard for an explicit single-file path whose parent directory
        // is swapped for an escaping symlink while the request is in flight.
        let cwd = tmpdir();
        let outside = tmpdir();
        let outside_clone = outside.clone();
        let cwd_clone = cwd.clone();
        let (address, _) = spawn_mock_swapping_images(&cwd_clone, &outside_clone, "200 OK").await;
        let model = image_model(format!("http://{address}/v1"), true);
        let tool = tool_for(
            &cwd,
            Some(resolver_for(model, Some("sk-image-test".into()), None)),
        );
        let error = (tool.execute)(make_ctx(json!({
            "prompt": "a red png",
            "path": "images/shot.png",
        })))
        .await
        .expect_err("parent swap before write must fail");
        let message = error.to_string();
        assert!(
            message.contains("outside the workspace") || message.contains("symlink"),
            "{message}"
        );
        assert!(
            outside.read_dir().expect("outside dir").next().is_none(),
            "image bytes leaked outside the workspace: {}",
            outside.display()
        );
    }
