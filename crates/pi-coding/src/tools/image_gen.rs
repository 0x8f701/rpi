//! `generate_image` tool: bounded image generation through the provider
//! subsystem (`pi_ai::generate_image`, OpenAI-compatible
//! `images/generations`).
//!
//! Model/endpoint/credential resolution mirrors streaming: the session's
//! active model (or an explicit `model` argument / `settings.images.genModel`
//! spec) must declare image capability; `settings.images.genBaseUrl` /
//! `genApiKey` override the endpoint and credential for self-hosted services.
//! Nothing is vendor-hardcoded.
//!
//! Bounds mirror `inspect_image` (see `tools/image.rs`): prompts are capped at
//! 4 KiB, `n` at 4, sizes are whitelisted to 256/512/1024 square, decoded
//! output is capped at 128 MiB per image with a 16 MP header pre-check, and
//! the save path is workspace-contained. The result returns file paths plus a
//! bounded prompt echo — image bytes never enter the transcript.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use pi_agent::{AbortSignal, AgentTool, AgentToolResult, ToolCallContext, ToolCapability};
use pi_ai::{
    IMAGE_GEN_SIZES, ImageGenerationOptions, MAX_IMAGE_GEN_N, MAX_IMAGE_GEN_PIXELS,
    MAX_IMAGE_GEN_PROMPT_CHARS,
};

use crate::redact::redact_secrets;
use crate::resolve::resolve_model;
use crate::tools::paths::resolve_scoped_path;
use crate::truncate::format_size;
use crate::WorkspaceRoots;

use ::image as image;
use image::ImageFormat;

/// Maximum prompt characters echoed back in the tool result (bounded output).
const MAX_PROMPT_ECHO_CHARS: usize = 200;

/// Configuration resolved for the `generate_image` tool: the model to use
/// (must declare image capability), the effective endpoint override, and the
/// credential. [`Debug`] redacts the api key.
#[derive(Clone)]
pub struct ImageGenConfig {
    pub model: pi_ai::Model,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

impl std::fmt::Debug for ImageGenConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ImageGenConfig")
            .field("model", &self.model.id)
            .field("base_url", &self.base_url)
            .field(
                "has_api_key",
                &self.api_key.as_ref().is_some_and(|key| !key.trim().is_empty()),
            )
            .finish()
    }
}

/// Resolves the `generate_image` configuration from the live session model,
/// settings (`images.genModel`/`genBaseUrl`/`genApiKey`), and the session auth
/// resolver. The optional spec (the tool's `model` argument) overrides the
/// resolved model. `None` on standalone construction: the tool then falls back
/// to the turn's model and env-resolved credentials.
pub type ImageGenConfigFn =
    std::sync::Arc<dyn Fn(Option<String>) -> pi_agent::BoxFuture<Result<ImageGenConfig>> + Send + Sync>;

/// Builds the `generate_image` tool rooted at `cwd` (workspace-contained).
pub(crate) fn generate_image_tool(cwd: &str, resolver: Option<ImageGenConfigFn>) -> AgentTool {
    generate_image_tool_for_workspace(crate::WorkspaceRoots::for_tool_factory(cwd), resolver)
}

/// Workspace-aware variant used by sessions with explicit additional roots.
pub(crate) fn generate_image_tool_for_workspace(
    workspace: WorkspaceRoots,
    resolver: Option<ImageGenConfigFn>,
) -> AgentTool {
    let params = super::s_object(
        vec![
            (
                "prompt",
                super::s_string(&format!(
                    "Text prompt describing the image to generate (bounded to {} characters).",
                    MAX_IMAGE_GEN_PROMPT_CHARS
                )),
            ),
            (
                "model",
                super::s_string(
                    "Optional model spec (provider/id or bare id) of an image-capable model. \
                     Defaults to the active model or settings images.genModel.",
                ),
            ),
            (
                "size",
                super::s_string(&format!(
                    "Optional square output size; one of {}. Defaults to the endpoint's default.",
                    IMAGE_GEN_SIZES.join(", ")
                )),
            ),
            (
                "n",
                super::s_number(&format!(
                    "Optional number of images to generate (1-{}); default 1.",
                    MAX_IMAGE_GEN_N
                )),
            ),
            (
                "path",
                super::s_string(
                    "Optional save location: a directory or a single image file path \
                     (.png/.jpg/.webp/.gif/.bmp). Defaults to <cwd>/images. Relative paths \
                     resolve inside the workspace.",
                ),
            ),
        ],
        vec!["prompt"],
    );
    let description = format!(
        "Generate images through the configured image-generation provider (OpenAI-compatible \
         images/generations). Saves the image(s) to the workspace and returns the file path(s), \
         dimensions, and a bounded prompt echo. Prompt bounded to {} characters, at most {} \
         images per call, sizes restricted to {}. The endpoint and credential resolve from the \
         model configuration (settings images.genModel / images.genBaseUrl / images.genApiKey).",
        MAX_IMAGE_GEN_PROMPT_CHARS,
        MAX_IMAGE_GEN_N,
        IMAGE_GEN_SIZES.join(", ")
    );
    AgentTool::new("generate_image", description, params, move |ctx: ToolCallContext| {
        let workspace = workspace.clone();
        let resolver = resolver.clone();
        async move {
            run_generate_image(&workspace, resolver.as_ref(), ctx.model.as_ref(), ctx.arguments, ctx.abort)
                .await
        }
    })
    .with_capability(ToolCapability::Write)
    .with_prompt_guidelines(vec![
        "Use generate_image to create images; it returns workspace file paths, not image bytes."
            .to_string(),
        "Keep prompts concise and specific; the tool caps prompts at 4096 characters."
            .to_string(),
    ])
}

/// Resolves the model/config: session resolver first, then a standalone
/// fallback (explicit spec or the turn's model, credentials from env).
async fn resolve_gen_config(
    resolver: Option<&ImageGenConfigFn>,
    model_arg: Option<&str>,
    fallback_model: Option<&pi_ai::Model>,
) -> Result<ImageGenConfig> {
    if let Some(resolver) = resolver {
        return (resolver)(model_arg.map(str::to_owned)).await;
    }
    let model = if let Some(spec) = model_arg {
        resolve_model(spec).map_err(|error| anyhow!("{error}"))?
    } else if let Some(model) = fallback_model {
        model.clone()
    } else {
        return Err(anyhow!(
            "Image generation is not configured for this session: no image-capable model is \
             available. Configure settings images.genModel (with an image-capable model), or run \
             in a session whose model declares imageGeneration: true."
        ));
    };
    Ok(ImageGenConfig {
        model,
        base_url: None,
        api_key: None,
    })
}

/// Single-purpose execution: validates args, resolves the provider
/// configuration, calls the image-generation client, validates each decoded
/// image, and saves it into the workspace.
async fn run_generate_image(
    workspace: &WorkspaceRoots,
    resolver: Option<&ImageGenConfigFn>,
    turn_model: Option<&pi_ai::Model>,
    args: serde_json::Value,
    abort: AbortSignal,
) -> Result<AgentToolResult> {
    super::check_aborted(&abort)?;

    // Prompt (bounded before any network call).
    let prompt = super::arg_str(&args, "prompt");
    if prompt.trim().is_empty() {
        return Err(anyhow!("Prompt must not be empty."));
    }
    let prompt_chars = prompt.chars().count();
    if prompt_chars > MAX_IMAGE_GEN_PROMPT_CHARS {
        return Err(anyhow!(
            "Prompt is {prompt_chars} characters, exceeding the generate_image limit of {} \
             characters.",
            MAX_IMAGE_GEN_PROMPT_CHARS
        ));
    }

    // n (bounded).
    let n = super::arg_int(&args, "n")?.unwrap_or(1);
    if n < 1 || n as u32 > MAX_IMAGE_GEN_N {
        return Err(anyhow!(
            "n must be between 1 and {} (got {n}).",
            MAX_IMAGE_GEN_N
        ));
    }
    let n = n as u32;

    // Size whitelist.
    let size_arg = super::arg_str(&args, "size");
    let size = if size_arg.trim().is_empty() {
        None
    } else {
        if !IMAGE_GEN_SIZES.contains(&size_arg.as_str()) {
            return Err(anyhow!(
                "Unsupported size {size_arg:?}; expected one of {}.",
                IMAGE_GEN_SIZES.join(", ")
            ));
        }
        Some(size_arg)
    };

    // Optional explicit model spec.
    let model_arg = super::arg_str(&args, "model");
    let model_arg = (!model_arg.trim().is_empty()).then(|| model_arg.trim().to_owned());

    // Save location: an explicit single file (n must be 1), an explicit
    // directory, or the default `<cwd>/images`.
    let path_arg = super::arg_str(&args, "path");
    if n > 1 && has_image_extension(path_arg.trim()) {
        return Err(anyhow!(
            "{path_arg:?} names a single image file but n={n}; either request one image or give a \
             directory path."
        ));
    }

    // Resolve model/endpoint/credential and gate on the model's declared
    // image capability BEFORE touching the filesystem.
    let config = resolve_gen_config(resolver, model_arg.as_deref(), turn_model).await?;
    super::check_aborted(&abort)?;

    if !config.model.image_generation {
        return Err(anyhow!(
            "Model {:?} does not support image generation. Configure an image-capable model \
             (one that declares imageGeneration: true) via settings images.genModel, or set the \
             model's api to \"imagegen\" / \"openrouter-images\" with a baseUrl and api key.",
            config.model.id
        ));
    }

    let save_target = SaveTarget::new(workspace, &path_arg)?;

    let options = ImageGenerationOptions {
        prompt: prompt.clone(),
        n: Some(n),
        size,
        base_url: config.base_url,
        api_key: config.api_key,
        ..ImageGenerationOptions::default()
    };
    let result = pi_ai::generate_image(config.model.clone(), options)
        .await
        .map_err(|error| anyhow!(redact_secrets(&error.to_string())))?;
    super::check_aborted(&abort)?;

    // Bounded result: paths + dimensions + a bounded prompt echo. Image bytes
    // never enter the transcript.
    let mut out = String::new();
    out.push_str(&format!(
        "Generated {} image(s) with model {}.\n",
        result.images.len(),
        config.model.id
    ));
    out.push_str(&format!("Prompt: {}\n", bounded_echo(&prompt)));
    for (index, image) in result.images.iter().enumerate() {
        super::check_aborted(&abort)?;
        // Format + 16 MP header pre-check BEFORE saving (mirrors inspect_image):
        // a hostile payload cannot exceed the decoded-memory bound.
        let (format, width, height) = inspect_image(&image.data).map_err(|error| {
            anyhow!("Generated image {} is not a supported image: {error}", index + 1)
        })?;
        if u64::from(width) * u64::from(height) > MAX_IMAGE_GEN_PIXELS {
            return Err(anyhow!(
                "Generated image {} is {width}x{height}, exceeding the {} megapixel decode \
                 limit.",
                index + 1,
                MAX_IMAGE_GEN_PIXELS / 1_000_000
            ));
        }
        let abs = save_target.resolve(format, n, index)?;
        save_image_bytes(workspace, &abs, &image.data)?;
        out.push_str(&format!(
            "  {} ({}x{}, {})\n",
            display_path(workspace, &abs),
            width,
            height,
            format_size(image.data.len())
        ));
        if let Some(revised) = &image.revised_prompt {
            out.push_str(&format!("  revised prompt: {}\n", bounded_echo(revised)));
        }
    }
    Ok(super::text_result(out))
}

/// Echoes at most [`MAX_PROMPT_ECHO_CHARS`] characters with a truncation
/// marker (bounded output).
fn bounded_echo(text: &str) -> String {
    let chars: String = text.chars().take(MAX_PROMPT_ECHO_CHARS).collect();
    if text.chars().count() > MAX_PROMPT_ECHO_CHARS {
        format!("{chars}...")
    } else {
        chars
    }
}

fn has_image_extension(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp"
            )
        })
}

/// Detects the image format from magic bytes and reads the header dimensions.
/// Rejects unknown formats and corrupt images.
fn inspect_image(data: &[u8]) -> Result<(ImageFormat, u32, u32)> {
    let mut reader = image::ImageReader::new(Cursor::new(data));
    reader = reader
        .with_guessed_format()
        .map_err(|error| anyhow!("could not determine the image format: {error}"))?;
    let format = reader
        .format()
        .ok_or_else(|| anyhow!("unsupported or unrecognized image format"))?;
    let (width, height) = reader
        .into_dimensions()
        .map_err(|error| anyhow!("could not decode dimensions: {error} (corrupt image)"))?;
    Ok((format, width, height))
}

/// Extension for a detected format. Unsupported formats are rejected so the
/// saved file always matches its extension.
fn format_extension(format: ImageFormat) -> Option<&'static str> {
    match format {
        ImageFormat::Png => Some("png"),
        ImageFormat::Jpeg => Some("jpg"),
        ImageFormat::Gif => Some("gif"),
        ImageFormat::Bmp => Some("bmp"),
        ImageFormat::WebP => Some("webp"),
        _ => None,
    }
}

/// Where generated images are saved. Resolved (with workspace containment)
/// before the network call so a bad path fails fast and the target directory
/// exists by the time images arrive.
#[derive(Debug)]
enum SaveTarget {
    /// An explicit single-file path (`path` ended with an image extension;
    /// n must be 1).
    File(PathBuf),
    /// An explicit or default directory (`<cwd>/images` unless `path` names a
    /// directory); files are named `image.ext` / `image-{i}.ext`.
    Dir(PathBuf),
}

impl SaveTarget {
    /// Resolves `path` (possibly empty) into a contained save target. The
    /// directory is created eagerly; single-file parents are created too.
    /// The default `<cwd>/images` goes through the same scoped-path
    /// resolution as explicit paths, so a pre-existing `images` symlink
    /// pointing outside the workspace is rejected instead of followed.
    fn new(workspace: &WorkspaceRoots, path: &str) -> Result<Self> {
        let path = path.trim();
        if path.is_empty() {
            let resolved = resolve_scoped_path("images", workspace)?;
            let dir = PathBuf::from(resolved);
            std::fs::create_dir_all(&dir).map_err(|error| {
                anyhow!("Could not create image output directory {}: {}", dir.display(), error)
            })?;
            return Ok(Self::Dir(dir));
        }
        let resolved = resolve_scoped_path(path, workspace)?;
        if has_image_extension(path) {
            let file = PathBuf::from(&resolved);
            if let Some(parent) = file.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent).map_err(|error| {
                    anyhow!("Could not create image output directory {}: {}", parent.display(), error)
                })?;
            }
            return Ok(Self::File(file));
        }
        let dir = PathBuf::from(&resolved);
        std::fs::create_dir_all(&dir).map_err(|error| {
            anyhow!("Could not create image output directory {}: {}", dir.display(), error)
        })?;
        Ok(Self::Dir(dir))
    }

    /// The absolute path for image `index` (0-based) of `n`.
    fn resolve(&self, format: ImageFormat, n: u32, index: usize) -> Result<PathBuf> {
        let ext = format_extension(format).ok_or_else(|| {
            anyhow!(
                "unsupported image format returned by the endpoint (expected PNG, JPEG, GIF, \
                 BMP, or WebP)"
            )
        })?;
        match self {
            Self::File(path) => {
                // Verify the user's extension matches the actual format so the
                // saved file is never mislabeled.
                let user_ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let expected = if user_ext == "jpeg" { "jpg" } else { user_ext.as_str() };
                if expected != ext {
                    return Err(anyhow!(
                        "The endpoint returned a {ext} image but {} names a .{user_ext} file.",
                        path.display()
                    ));
                }
                Ok(path.clone())
            }
            Self::Dir(dir) => {
                let name = if n == 1 {
                    format!("image.{ext}")
                } else {
                    format!("image-{}.{ext}", index + 1)
                };
                Ok(dir.join(name))
            }
        }
    }
}

/// Saves image bytes to `abs` with a symlink-swap guard. The parent
/// directory is re-canonicalized and re-checked against the workspace roots
/// at write time (a symlink swapped in after [`SaveTarget::new`] validated
/// the location resolves outside and is rejected), and the final component
/// is opened with `O_NOFOLLOW` so an existing or racing symlink at the file
/// itself cannot redirect the write outside the workspace.
fn save_image_bytes(workspace: &WorkspaceRoots, abs: &Path, data: &[u8]) -> Result<()> {
    let parent = abs.parent().ok_or_else(|| {
        anyhow!(
            "Could not save generated image {}: path has no parent directory.",
            abs.display()
        )
    })?;
    let canonical_parent = std::fs::canonicalize(parent).map_err(|error| {
        anyhow!("Could not save generated image {}: {}", abs.display(), error)
    })?;
    if !workspace.roots().iter().any(|root| canonical_parent.starts_with(root)) {
        return Err(anyhow!(
            "Refusing to save generated image {}: the output directory now resolves outside \
             the workspace (symlink swap?).",
            abs.display()
        ));
    }
    let name = abs.file_name().ok_or_else(|| {
        anyhow!("Could not save generated image {}: path has no file name.", abs.display())
    })?;
    let final_path = canonical_parent.join(name);
    if std::fs::symlink_metadata(&final_path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(anyhow!(
            "Refusing to save generated image {}: the target is a symlink.",
            final_path.display()
        ));
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_NOFOLLOW closes the race between the symlink_metadata check
        // above and the open: a symlink landing here makes the open fail.
        options.custom_flags(nix::libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(&final_path)
        .map_err(|error| anyhow!("Could not save generated image {}: {}", abs.display(), error))?;
    use std::io::Write as _;
    file.write_all(data)
        .map_err(|error| anyhow!("Could not save generated image {}: {}", abs.display(), error))?;
    Ok(())
}

/// Prefers a workspace-relative path for the result so the transcript stays
/// concise and portable.
fn display_path(workspace: &WorkspaceRoots, abs: &Path) -> String {
    let cwd = workspace.cwd();
    abs.strip_prefix(cwd)
        .map(|relative| relative.to_string_lossy().into_owned())
        .unwrap_or_else(|_| abs.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests;
