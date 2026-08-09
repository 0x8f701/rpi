//! OpenAI-compatible image generation (`POST {base}/images/generations`).
//!
//! The client resolves the endpoint and credential exactly like streaming
//! does: base URL from the model's `base_url` (optionally overridden by
//! settings `images.genBaseUrl` via [`ImageGenerationOptions::base_url`]),
//! credential from the caller (auth resolver / settings `images.genApiKey`),
//! falling back to env. Nothing is vendor-hardcoded.
//!
//! Two model `api` names route here, both registered by [`register_imagegen`]:
//! - [`API_IMAGE_GEN`] (`imagegen`): generic OpenAI-compatible endpoint
//!   configured through the model's `baseUrl` (e.g. a self-hosted server).
//! - [`API_OPENROUTER_IMAGES`] (`openrouter-images`): OpenRouter's
//!   image-generation surface (upstream `KnownImagesApi`); the model carries
//!   the base URL and credential like every other provider.
//!
//! Chat streaming for these apis errors clearly (they have no chat surface).

use std::sync::Arc;

use anyhow::{Result, anyhow};
use base64::Engine;
use futures_util::{FutureExt, StreamExt};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::common::{
    client, error_body, insert_header, insert_header_map, is_aborted, send_with_retry,
};
use crate::{
    API_IMAGE_GEN, API_OPENROUTER_IMAGES, ApiProvider, Context, GeneratedImage, ImageGenFn,
    IMAGE_GEN_SIZES, ImageGenerationOptions, ImageGenerationResult,
    MAX_IMAGE_GEN_DECODED_BYTES, MAX_IMAGE_GEN_N, MAX_IMAGE_GEN_PROMPT_CHARS,
    MAX_IMAGE_GEN_RESPONSE_BYTES, Model, SimpleStreamOptions, StreamOptions, error_stream,
    register_api_provider, resolve_base_url,
};

/// `{base}/images/generations` — the OpenAI-compatible endpoint joined onto
/// the resolved base URL (which already carries `/v1` when the provider uses
/// it, mirroring `{base}/chat/completions`).
fn images_url(base_url: &str) -> String {
    format!("{}/images/generations", base_url.trim_end_matches('/'))
}

/// Registers the OpenAI-compatible image-generation providers for
/// [`API_IMAGE_GEN`] and [`API_OPENROUTER_IMAGES`]. Both share one client;
/// their stream functions report the endpoint's image-only nature clearly.
pub fn register_imagegen() {
    register_api_provider(image_gen_provider(API_IMAGE_GEN), None);
    register_api_provider(image_gen_provider(API_OPENROUTER_IMAGES), None);
}

fn image_gen_provider(api: &str) -> ApiProvider {
    let generate = image_gen_fn();
    let stream = {
        let api = api.to_string();
        let message = format!(
            "api {api} is an image-generation endpoint; chat streaming is not supported"
        );
        Arc::new(move |model: Model, _ctx: Context, _opts: StreamOptions| {
            let message = message.clone();
            async move { error_stream(&model, message).await }.boxed()
        })
    };
    let stream_simple = {
        let api = api.to_string();
        let message = format!(
            "api {api} is an image-generation endpoint; chat streaming is not supported"
        );
        Arc::new(move |model: Model, _ctx: Context, _opts: SimpleStreamOptions| {
            let message = message.clone();
            async move { error_stream(&model, message).await }.boxed()
        })
    };
    ApiProvider {
        api: api.to_string(),
        stream,
        stream_simple,
        generate_image: Some(generate),
    }
}

fn image_gen_fn() -> ImageGenFn {
    Arc::new(|model, options| {
        async move { generate_image_openai_compatible(model, options).await }.boxed()
    })
}

/// Validates prompt length, `n`, and `size` up front (defense in depth: the
/// tool enforces the same contract for its own argument surface).
fn validate_options(options: &ImageGenerationOptions) -> Result<()> {
    if options.prompt.trim().is_empty() {
        return Err(anyhow!("Image generation prompt must not be empty."));
    }
    let prompt_chars = options.prompt.chars().count();
    if prompt_chars > MAX_IMAGE_GEN_PROMPT_CHARS {
        return Err(anyhow!(
            "Image generation prompt is {} characters, exceeding the {} character limit.",
            prompt_chars,
            MAX_IMAGE_GEN_PROMPT_CHARS
        ));
    }
    let n = options.n.unwrap_or(1);
    if n < 1 || n > MAX_IMAGE_GEN_N {
        return Err(anyhow!(
            "n must be between 1 and {} (got {n}).",
            MAX_IMAGE_GEN_N
        ));
    }
    if let Some(size) = &options.size {
        if !IMAGE_GEN_SIZES.contains(&size.as_str()) {
            return Err(anyhow!(
                "Unsupported image size {size:?}; expected one of {}.",
                IMAGE_GEN_SIZES.join(", ")
            ));
        }
    }
    Ok(())
}

/// Whether a base64 payload decodes within
/// [`MAX_IMAGE_GEN_DECODED_BYTES`]. Checked BEFORE any allocation so an
/// oversized payload is rejected without touching its contents. The trailing
/// `=` padding is subtracted from the estimate (padding is not data), so a
/// payload whose decoded size is exactly the cap is accepted.
fn decoded_within_cap(b64: &str) -> bool {
    decoded_len_within_cap(b64.len(), trailing_padding(b64))
}

/// Length form of [`decoded_within_cap`] (pure arithmetic, unit-testable at
/// the cap boundary without allocating huge strings).
fn decoded_len_within_cap(b64_len: usize, padding: usize) -> bool {
    decoded_len_estimate(b64_len, padding) as u64 <= MAX_IMAGE_GEN_DECODED_BYTES
}

/// Count of trailing `=` padding characters in a STANDARD base64 payload.
/// Only the suffix is padding; interior `=` (malformed) is not counted and
/// remains the decoder's problem.
fn trailing_padding(b64: &str) -> usize {
    b64.bytes().rev().take_while(|&byte| byte == b'=').count()
}

/// Exact decoded length of a well-formed STANDARD base64 payload with
/// padding: `=` characters are not data, so they are subtracted before the
/// `floor(n/4)*3` estimate plus the partial-group remainder (2 chars → 1
/// byte, 3 chars → 2 bytes). Used only as an allocation pre-check; the
/// decoder remains the authority on malformed input.
fn decoded_len_estimate(b64_len: usize, padding: usize) -> usize {
    let data_len = b64_len.saturating_sub(padding);
    let full = (data_len / 4) * 3;
    match data_len % 4 {
        2 => full + 1,
        3 => full + 2,
        _ => full,
    }
}

/// Builds the request headers: JSON content type, bearer auth, then the
/// model's own headers, then per-call extras (later wins).
fn build_request_headers(
    model: &Model,
    options: &ImageGenerationOptions,
    api_key: &str,
) -> Result<reqwest::header::HeaderMap> {
    let mut headers = reqwest::header::HeaderMap::new();
    insert_header(&mut headers, "content-type", "application/json")?;
    insert_header(&mut headers, "accept", "application/json")?;
    if !api_key.trim().is_empty() {
        insert_header(&mut headers, "authorization", &format!("Bearer {api_key}"))?;
    }
    if let Some(model_headers) = &model.headers {
        insert_header_map(&mut headers, model_headers)?;
    }
    insert_header_map(&mut headers, &options.headers)?;
    Ok(headers)
}

/// Replaces the api key in a message so it can never surface in errors
/// (server-controlled bodies or transport diagnostics may echo credentials).
fn redact_key(message: &str, api_key: &str) -> String {
    let key = api_key.trim();
    if key.is_empty() {
        return message.to_owned();
    }
    message.replace(key, "[REDACTED]")
}

/// Reads a response body up to `max_bytes`, abort-aware. A hostile or runaway
/// payload is rejected without exhausting memory.
async fn read_capped(
    response: reqwest::Response,
    options: &StreamOptions,
    max_bytes: u64,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut stream = response.bytes_stream();
    loop {
        let chunk = match &options.abort_signal {
            Some(token) => tokio::select! {
                biased;
                _ = token.clone().cancelled_owned() => return Err(anyhow!("Request was aborted")),
                chunk = stream.next() => chunk,
            },
            None => stream.next().await,
        };
        let Some(chunk) = chunk else { break };
        if is_aborted(options) {
            return Err(anyhow!("Request was aborted"));
        }
        let chunk = chunk?;
        if out.len() + chunk.len() > max_bytes as usize {
            return Err(anyhow!(
                "Image generation response exceeds the {} MiB limit.",
                max_bytes / 1024 / 1024
            ));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Runs one OpenAI-compatible `images/generations` call and decodes every
/// returned `b64_json` image in memory under [`MAX_IMAGE_GEN_DECODED_BYTES`].
pub async fn generate_image_openai_compatible(
    model: Model,
    options: ImageGenerationOptions,
) -> Result<ImageGenerationResult> {
    validate_options(&options)?;

    let base_url = options
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| model.base_url.trim());
    if base_url.is_empty() {
        return Err(anyhow!(
            "No image-generation endpoint configured for model \"{}\": the model has no \
             baseUrl. Set the model's baseUrl or configure settings images.genBaseUrl.",
            model.id
        ));
    }
    let resolved_base = resolve_base_url(base_url, &options.env)?;
    let url = images_url(&resolved_base);

    let api_key = options
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .ok_or_else(|| anyhow!("No API key for provider: {}", model.provider))?;

    let mut payload = Map::new();
    payload.insert("model".into(), Value::String(model.id.clone()));
    payload.insert("prompt".into(), Value::String(options.prompt.clone()));
    payload.insert("n".into(), json!(options.n.unwrap_or(1)));
    payload.insert("response_format".into(), json!("b64_json"));
    if let Some(size) = &options.size {
        payload.insert("size".into(), json!(size));
    }

    // Reuse the streaming HTTP plumbing (timeout, retries, abort) so image
    // generation gets identical transport semantics with zero duplication.
    let stream_options = StreamOptions {
        api_key: options.api_key.clone(),
        headers: options.headers.clone(),
        timeout_ms: options.timeout_ms,
        max_retries: options.max_retries,
        max_retry_delay_ms: options.max_retry_delay_ms,
        env: options.env.clone(),
        abort_signal: options.abort_signal.clone(),
        ..StreamOptions::default()
    };
    let http = client(&stream_options)?;
    let headers = build_request_headers(&model, &options, api_key)?;
    let payload_body = Value::Object(payload).to_string();
    let response = send_with_retry(&stream_options, || {
        http.post(&url)
            .headers(headers.clone())
            .body(payload_body.clone())
    })
    .await
    .map_err(|error| anyhow!(redact_key(&error.to_string(), api_key)))?;

    if !response.status().is_success() {
        let message = error_body("Image generation", response, &stream_options).await?;
        return Err(anyhow!(redact_key(&message, api_key)));
    }

    let body = read_capped(response, &stream_options, MAX_IMAGE_GEN_RESPONSE_BYTES).await?;
    let parsed: ImageGenResponse = serde_json::from_slice(&body)
        .map_err(|error| anyhow!("Could not parse image generation response: {error}"))?;
    if parsed.data.is_empty() {
        return Err(anyhow!("Image generation returned no images."));
    }

    let mut images = Vec::with_capacity(parsed.data.len());
    for item in parsed.data {
        let b64 = item.b64_json.trim();
        if b64.is_empty() {
            return Err(anyhow!(
                "Image generation returned an image with no data."
            ));
        }
        if !decoded_within_cap(b64) {
            return Err(anyhow!(
                "Image generation returned an image exceeding the {} MiB decode limit.",
                MAX_IMAGE_GEN_DECODED_BYTES / 1024 / 1024
            ));
        }
        let data = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|error| {
                anyhow!("Image generation returned invalid base64 image data: {error}")
            })?;
        images.push(GeneratedImage {
            data,
            revised_prompt: item.revised_prompt,
        });
    }
    Ok(ImageGenerationResult { images })
}

#[derive(Deserialize)]
struct ImageGenResponse {
    #[serde(default)]
    data: Vec<ImageGenData>,
}

#[derive(Deserialize)]
struct ImageGenData {
    #[serde(default)]
    b64_json: String,
    #[serde(default)]
    revised_prompt: Option<String>,
}

/// A real 1x1 transparent PNG, used by tests as a valid `b64_json` payload.
#[cfg(test)]
const ONE_PX_PNG_B64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SimpleStreamFn, SimpleStreamOptions, StreamFn, StreamOptions, unregister_api_providers};
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn model(api: &str, base_url: String) -> Model {
        Model {
            id: "image-model".into(),
            name: "Image Model".into(),
            api: api.into(),
            provider: "test-images".into(),
            base_url,
            image_generation: true,
            ..Model::default()
        }
    }

    fn options(prompt: &str) -> ImageGenerationOptions {
        ImageGenerationOptions {
            prompt: prompt.to_owned(),
            api_key: Some(["s", "k-image-test"].concat()),
            ..ImageGenerationOptions::default()
        }
    }

    /// Serves one HTTP response for one request; captures the raw request.
    async fn spawn_mock(
        response_body: String,
        status: &str,
    ) -> (String, Arc<Mutex<String>>) {
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
                if buffer.windows(4).any(|w| w == b"\r\n\r\n") {
                    // Headers complete; the request body is read alongside
                    // since the client sends it in the same write.
                    break;
                }
            }
            *request.lock().expect("request lock") =
                String::from_utf8_lossy(&buffer).into_owned();
            let body = response_body.clone();
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.expect("write response");
        });
        (address.to_string(), captured)
    }

    fn json_response(data: serde_json::Value) -> String {
        serde_json::json!({ "data": data }).to_string()
    }

    #[tokio::test]
    async fn request_uses_joined_base_bearer_and_openai_body_shape() {
        let (address, captured) = spawn_mock(
            json_response(serde_json::json!([{ "b64_json": ONE_PX_PNG_B64 }])),
            "200 OK",
        )
        .await;
        let model = model(API_IMAGE_GEN, format!("http://{address}/v1"));
        let mut options = options("a red png");
        options.size = Some("1024x1024".into());
        options.n = Some(2);
        let result = generate_image_openai_compatible(model, options)
            .await
            .expect("generation");
        assert_eq!(result.images.len(), 1);
        assert!(!result.images[0].data.is_empty());

        let request = captured.lock().expect("request lock").clone();
        assert!(request.starts_with("POST /v1/images/generations "), "{request}");
        let lowered = request.to_ascii_lowercase();
        let expected_auth = ["authorization: bearer s", "k-image-test"].concat();
        assert!(lowered.contains(&expected_auth), "{request}");
        assert!(lowered.contains("content-type: application/json"), "{request}");
        let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
        let parsed: serde_json::Value = serde_json::from_str(body).expect("json body");
        assert_eq!(parsed["model"], "image-model");
        assert_eq!(parsed["prompt"], "a red png");
        assert_eq!(parsed["n"], 2);
        assert_eq!(parsed["size"], "1024x1024");
        assert_eq!(parsed["response_format"], "b64_json");
    }

    #[tokio::test]
    async fn base_url_override_replaces_model_base() {
        let (address, captured) = spawn_mock(
            json_response(serde_json::json!([{ "b64_json": ONE_PX_PNG_B64 }])),
            "200 OK",
        )
        .await;
        let model = model(API_IMAGE_GEN, "http://ignored.invalid/v1".into());
        let mut options = options("a red png");
        options.base_url = Some(format!("http://{address}/custom/v1"));
        generate_image_openai_compatible(model, options)
            .await
            .expect("generation");
        let request = captured.lock().expect("request lock").clone();
        assert!(
            request.starts_with("POST /custom/v1/images/generations "),
            "{request}"
        );
    }

    #[tokio::test]
    async fn parses_revised_prompt() {
        let (address, _) = spawn_mock(
            json_response(serde_json::json!([{
                "b64_json": ONE_PX_PNG_B64,
                "revised_prompt": "a red png, high quality"
            }])),
            "200 OK",
        )
        .await;
        let model = model(API_IMAGE_GEN, format!("http://{address}/v1"));
        let result = generate_image_openai_compatible(model, options("a red png"))
            .await
            .expect("generation");
        assert_eq!(
            result.images[0].revised_prompt.as_deref(),
            Some("a red png, high quality")
        );
    }

    #[tokio::test]
    async fn provider_error_surfaces_message_and_code() {
        let (address, _) = spawn_mock(
            r#"{"error":{"message":"quota exhausted","code":"insufficient_quota"}}"#.into(),
            "429 Too Many Requests",
        )
        .await;
        let model = model(API_IMAGE_GEN, format!("http://{address}/v1"));
        let error = generate_image_openai_compatible(model, options("a red png"))
            .await
            .expect_err("429 must fail");
        assert!(error.to_string().contains("quota exhausted"), "{error}");
        assert!(error.to_string().contains("insufficient_quota"), "{error}");
    }

    #[tokio::test]
    async fn missing_api_key_is_actionable_without_leaking() {
        let model = model(API_IMAGE_GEN, "http://127.0.0.1:1/v1".into());
        let mut options = options("a red png");
        options.api_key = None;
        let error = generate_image_openai_compatible(model, options)
            .await
            .expect_err("missing key must fail");
        assert!(error.to_string().contains("No API key for provider"), "{error}");
    }

    #[tokio::test]
    async fn empty_base_url_is_actionable() {
        let model = model(API_IMAGE_GEN, String::new());
        let error = generate_image_openai_compatible(model, options("a red png"))
            .await
            .expect_err("empty base must fail");
        assert!(
            error.to_string().contains("images.genBaseUrl"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn prompt_bounds_are_enforced() {
        let model = model(API_IMAGE_GEN, "http://127.0.0.1:1/v1".into());
        let mut long_prompt = options("x");
        long_prompt.prompt = "a".repeat(MAX_IMAGE_GEN_PROMPT_CHARS + 1);
        let error = generate_image_openai_compatible(model.clone(), long_prompt)
            .await
            .expect_err("long prompt must fail");
        assert!(error.to_string().contains("character limit"), "{error}");

        let error = generate_image_openai_compatible(model, options("   "))
            .await
            .expect_err("blank prompt must fail");
        assert!(error.to_string().contains("must not be empty"), "{error}");
    }

    #[tokio::test]
    async fn n_bounds_are_enforced() {
        let model = model(API_IMAGE_GEN, "http://127.0.0.1:1/v1".into());
        for n in [0, MAX_IMAGE_GEN_N + 1] {
            let mut options = options("a red png");
            options.n = Some(n);
            let error = generate_image_openai_compatible(model.clone(), options)
                .await
                .expect_err("out-of-range n must fail");
            assert!(
                error.to_string().contains(&format!("between 1 and {MAX_IMAGE_GEN_N}")),
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn size_whitelist_is_enforced() {
        let model = model(API_IMAGE_GEN, "http://127.0.0.1:1/v1".into());
        let mut options = options("a red png");
        options.size = Some("512x1024".into());
        let error = generate_image_openai_compatible(model, options)
            .await
            .expect_err("non-square size must fail");
        assert!(error.to_string().contains("Unsupported image size"), "{error}");
    }

    #[test]
    fn decoded_cap_rejects_oversized_payload_before_allocation() {
        assert!(decoded_within_cap(ONE_PX_PNG_B64));
        let oversized = "A".repeat((MAX_IMAGE_GEN_DECODED_BYTES as usize / 3) * 4 + 16);
        assert!(!decoded_within_cap(&oversized));
    }

    #[tokio::test]
    async fn response_body_cap_is_enforced() {
        let (address, _) = spawn_mock("x".repeat(4096), "200 OK").await;
        let http = client(&StreamOptions::default()).expect("client");
        let response = http
            .get(format!("http://{address}/anything"))
            .send()
            .await
            .expect("mock fetch");
        let error = read_capped(response, &StreamOptions::default(), 1024)
            .await
            .expect_err("oversized body must fail");
        assert!(error.to_string().contains("limit"), "{error}");
    }

    #[tokio::test]
    async fn empty_data_errors_clearly() {
        let (address, _) = spawn_mock(json_response(serde_json::json!([])), "200 OK").await;
        let model = model(API_IMAGE_GEN, format!("http://{address}/v1"));
        let error = generate_image_openai_compatible(model, options("a red png"))
            .await
            .expect_err("empty data must fail");
        assert!(error.to_string().contains("returned no images"), "{error}");
    }

    #[tokio::test]
    async fn imagegen_stream_errors_instead_of_chatting() {
        // Exercises the provider's stream function directly (the process-wide
        // builtin registry may be cleared by parallel registry tests).
        let provider = image_gen_provider(API_IMAGE_GEN);
        let model = Model {
            api: API_IMAGE_GEN.into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            image_generation: true,
            ..Model::default()
        };
        let stream = (provider.stream)(model, Context::default(), StreamOptions::default()).await;
        let result = stream.result().await.expect("error result");
        assert_eq!(result.stop_reason, crate::StopReason::Error);
        assert!(
            result
                .error_message
                .as_deref()
                .is_some_and(|m| m.contains("image-generation endpoint")),
            "{:?}",
            result.error_message
        );
    }

    #[tokio::test]
    async fn chat_provider_without_image_capability_errors_clearly() {
        // A locally registered chat-only provider (unique api name so parallel
        // registry tests cannot interfere): `generate_image` must refuse it
        // with an actionable error instead of silently dispatching.
        let api = "test-chat-only-provider";
        let stream: StreamFn = Arc::new(|_, _, _| {
            async { crate::new_assistant_message_event_stream() }.boxed()
        });
        let simple: SimpleStreamFn = Arc::new(|_, _, _| {
            async { crate::new_assistant_message_event_stream() }.boxed()
        });
        register_api_provider(
            ApiProvider {
                api: api.into(),
                stream,
                stream_simple: simple,
                generate_image: None,
            },
            Some("imagegen-test-source".into()),
        );
        let model = Model {
            api: api.into(),
            provider: "test-chat-only".into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            ..Model::default()
        };
        let error = crate::generate_image(model, options("a red png"))
            .await
            .expect_err("chat-only provider must refuse image generation");
        assert!(
            error.to_string().contains("does not support image generation"),
            "{error}"
        );
        unregister_api_providers("imagegen-test-source");
    }
    // -----------------------------------------------------------------
    // Pure helpers (no network)
    // -----------------------------------------------------------------

    #[test]
    fn decoded_len_estimate_covers_partial_base64_groups() {
        // 4 chars → 3 bytes (no remainder).
        assert_eq!(decoded_len_estimate(4, 0), 3);
        assert_eq!(decoded_len_estimate(8, 0), 6);
        // 2-char remainder → +1 byte.
        assert_eq!(decoded_len_estimate(6, 0), 4);
        // 3-char remainder → +2 bytes.
        assert_eq!(decoded_len_estimate(7, 0), 5);
        // Zero-length payload.
        assert_eq!(decoded_len_estimate(0, 0), 0);
    }

    #[test]
    fn decoded_len_estimate_subtracts_padding() {
        // "QQ==" → 1 byte, "QUI=" → 2 bytes, "QUJD" → 3 bytes: the padding
        // characters are not data and must not inflate the estimate.
        assert_eq!(decoded_len_estimate(4, 2), 1);
        assert_eq!(decoded_len_estimate(4, 1), 2);
        assert_eq!(decoded_len_estimate(4, 0), 3);
        // Trailing '=' count as padding; interior '=' (malformed) do not.
        assert_eq!(trailing_padding("QUJD"), 0);
        assert_eq!(trailing_padding("QUI="), 1);
        assert_eq!(trailing_padding("QQ=="), 2);
        assert_eq!(trailing_padding("A=AA"), 0);
        assert_eq!(trailing_padding(""), 0);
    }

    #[test]
    fn decoded_within_cap_accepts_exact_cap_one_and_two_padding_cases() {
        // The padding-exact estimate: an image whose decoded size is exactly
        // the cap with one padding char (cap % 3 == 2) was rejected by the
        // raw-length estimate (off by one); the same holds for the two-pad
        // case at cap - 1 ((cap - 1) % 3 == 1). Both must now pass.
        let cap = MAX_IMAGE_GEN_DECODED_BYTES as usize;
        let one_pad_len = 4 * cap.div_ceil(3);
        assert_eq!(decoded_len_estimate(one_pad_len, 1), cap);
        // The pre-fix estimate (no padding subtraction) was one over.
        assert_eq!(decoded_len_estimate(one_pad_len, 0), cap + 1);
        let two_pad_len = 4 * (cap - 1).div_ceil(3);
        assert_eq!(decoded_len_estimate(two_pad_len, 2), cap - 1);
        // Genuinely over the cap with two padding chars is still rejected.
        let over_len = 4 * (cap + 2).div_ceil(3);
        assert_eq!(decoded_len_estimate(over_len, 2), cap + 2);
        assert!(decoded_len_within_cap(one_pad_len, 1));
        assert!(decoded_len_within_cap(two_pad_len, 2));
        assert!(!decoded_len_within_cap(over_len, 2));
        // The string-level gate mirrors the arithmetic for padded payloads.
        assert!(decoded_within_cap(&format!(
            "{}{}",
            "A".repeat(one_pad_len - 1),
            "="
        )));
        assert!(decoded_within_cap(&format!(
            "{}{}",
            "A".repeat(two_pad_len - 2),
            "=="
        )));
    }

    #[test]
    fn decoded_within_cap_accepts_and_rejects_at_the_boundary() {
        // Unpadded payloads: 4*(cap/3) chars decode under the cap and pass;
        // one base64 group over the cap fails (defends against off-by-one).
        let at_cap = (MAX_IMAGE_GEN_DECODED_BYTES as usize / 3) * 4;
        assert!(decoded_len_within_cap(at_cap, 0));
        assert!(!decoded_len_within_cap(at_cap + 4, 0));
        // A tiny real PNG is well inside.
        assert!(decoded_within_cap(ONE_PX_PNG_B64));
    }

    #[test]
    fn redact_key_replaces_only_non_empty_key() {
        // A present key is scrubbed even with surrounding whitespace.
        let key = ["s", "k-secret-xyz"].concat();
        assert_eq!(
            redact_key(&format!("auth failed for {key}"), &format!("  {key}  ")),
            "auth failed for [REDACTED]"
        );
        // An empty/whitespace-only key leaves the message untouched.
        assert_eq!(redact_key("auth failed for nothing", ""), "auth failed for nothing");
        assert_eq!(redact_key("auth failed for nothing", "   "), "auth failed for nothing");
    }

    #[test]
    fn validate_options_rejects_empty_prompt_independently_of_network() {
        // Defense in depth: validate_options is the first gate, exercised
        // without any server.
        let mut opts = options("   ");
        opts.prompt = "   ".into();
        assert!(validate_options(&opts).is_err());
        opts.prompt = "ok".into();
        assert!(validate_options(&opts).is_ok());
    }

    // -----------------------------------------------------------------
    // Header / stream surfaces
    // -----------------------------------------------------------------

    #[test]
    fn build_request_headers_omits_bearer_when_key_is_blank() {
        // build_request_headers must not emit an Authorization header when
        // the api key is whitespace-only (the false branch of the guard).
        // The provider function rejects a blank key earlier, so this is the
        // direct unit exercising that defensive branch.
        let model = model(API_IMAGE_GEN, "http://127.0.0.1:1/v1".into());
        let options = options("a red png");
        let headers = build_request_headers(&model, &options, "   ").expect("headers");
        assert!(
            headers.get("authorization").is_none(),
            "blank key must not produce an authorization header"
        );
        // Content-type and accept are still present.
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
        assert_eq!(headers.get("accept").unwrap(), "application/json");
    }

    #[tokio::test]
    async fn model_and_option_headers_are_applied_in_order() {
        let (address, captured) =
            spawn_mock(json_response(serde_json::json!([{ "b64_json": ONE_PX_PNG_B64 }])), "200 OK")
                .await;
        let mut model = model(API_IMAGE_GEN, format!("http://{address}/v1"));
        let mut model_headers = std::collections::HashMap::new();
        model_headers.insert("X-Model-Header".to_string(), "model-value".to_string());
        model.headers = Some(model_headers);
        let mut options = options("a red png");
        // Per-call headers (later wins) are applied after model headers.
        options.headers.insert("X-Option-Header".into(), "option-value".into());
        generate_image_openai_compatible(model, options)
            .await
            .expect("generation");
        let request = captured.lock().expect("request lock").clone();
        let lowered = request.to_ascii_lowercase();
        assert!(lowered.contains("x-model-header: model-value"), "{request}");
        assert!(lowered.contains("x-option-header: option-value"), "{request}");
    }

    #[tokio::test]
    async fn stream_simple_errors_instead_of_chatting() {
        // The simple-stream surface must also refuse chat with the
        // image-generation-only notice (lines 69-72).
        let provider = image_gen_provider(API_IMAGE_GEN);
        let model = Model {
            api: API_IMAGE_GEN.into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            image_generation: true,
            ..Model::default()
        };
        let stream =
            (provider.stream_simple)(model, Context::default(), SimpleStreamOptions::default())
                .await;
        let result = stream.result().await.expect("error result");
        assert_eq!(result.stop_reason, crate::StopReason::Error);
        assert!(
            result.error_message.as_deref().is_some_and(|m| m.contains("image-generation endpoint")),
            "{:?}",
            result.error_message
        );
    }

    // -----------------------------------------------------------------
    // Response decoding (oversized / empty / invalid)
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn oversized_decoded_image_is_rejected_before_full_decode() {
        // A base64 string that decodes over the 128 MiB cap is rejected by
        // the pre-check, before the decoder allocates the full buffer.
        let huge_b64 = "A".repeat(((MAX_IMAGE_GEN_DECODED_BYTES as usize / 3) * 4) + 16);
        let (address, _) = spawn_mock(json_response(serde_json::json!([{ "b64_json": huge_b64 }])), "200 OK")
            .await;
        let model = model(API_IMAGE_GEN, format!("http://{address}/v1"));
        let error = generate_image_openai_compatible(model, options("a red png"))
            .await
            .expect_err("oversized decoded image must fail");
        assert!(error.to_string().contains("decode limit"), "{error}");
    }

    #[tokio::test]
    async fn empty_b64_json_data_is_rejected() {
        let (address, _) = spawn_mock(json_response(serde_json::json!([{ "b64_json": "" }])), "200 OK").await;
        let model = model(API_IMAGE_GEN, format!("http://{address}/v1"));
        let error = generate_image_openai_compatible(model, options("a red png"))
            .await
            .expect_err("empty b64 must fail");
        assert!(error.to_string().contains("no data"), "{error}");
    }

    #[tokio::test]
    async fn invalid_base64_is_rejected_with_decode_error() {
        let (address, _) =
            spawn_mock(json_response(serde_json::json!([{ "b64_json": "!!!not-base64!!!" }])), "200 OK").await;
        let model = model(API_IMAGE_GEN, format!("http://{address}/v1"));
        let error = generate_image_openai_compatible(model, options("a red png"))
            .await
            .expect_err("invalid base64 must fail");
        assert!(error.to_string().contains("invalid base64"), "{error}");
    }

    #[tokio::test]
    async fn unparseable_response_body_is_reported_clearly() {
        let (address, _) = spawn_mock("not json at all".into(), "200 OK").await;
        let model = model(API_IMAGE_GEN, format!("http://{address}/v1"));
        let error = generate_image_openai_compatible(model, options("a red png"))
            .await
            .expect_err("bad json must fail");
        assert!(error.to_string().contains("Could not parse"), "{error}");
    }

    #[tokio::test]
    async fn provider_error_redacts_leaked_key_in_body() {
        // The mock error body echoes the key; redact_key must scrub it so it
        // never surfaces to the caller (defense in depth on the provider
        // path, mirroring the tool-level redaction test).
        let key = ["s", "k-image-test-key-xyz"].concat();
        let response = serde_json::json!({
            "error": { "message": format!("denied for {key}") }
        })
        .to_string();
        let (address, _) = spawn_mock(response, "500 Internal Server Error").await;
        let model = model(API_IMAGE_GEN, format!("http://{address}/v1"));
        let error = generate_image_openai_compatible(model, options("a red png"))
            .await
            .expect_err("500 must fail");
        assert!(error.to_string().contains("denied"), "{error}");
        assert!(
            !error.to_string().contains(&key),
            "key leaked in provider error: {error}"
        );
    }
}
