use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions as CapOpenOptions};
use pi_ai::ContentBlock;

use crate::image_pipeline;

const MAX_TEXT_BYTES: u64 = 8 * 1024 * 1024;
/// Maximum video attachments in one expanded prompt. Each video contributes
/// up to [`crate::video_extract::MAX_VIDEO_FRAMES`] JPEG frames, so more
/// than one would blow the prompt budget.
const MAX_VIDEO_ATTACHMENTS: usize = 1;
/// Maximum total image ContentBlocks (regular attachments plus video frames)
/// in one expanded prompt, mirroring the Web composer's attachment bound.
const MAX_PROMPT_IMAGES: usize = 12;
/// Total base64 budget for all attachment images — the 4 MiB prompt RPC
/// frame must carry the message text and JSON envelope on top of them.
const MAX_PROMPT_IMAGE_BASE64: usize = 3 * 1024 * 1024;

/// Aggregate attachment budget for one expanded prompt: bounded image count
/// and total base64, so the composed prompt always fits the 4 MiB RPC frame.
/// Returns an actionable error naming the violated cap.
fn enforce_attachment_budget(images: &[ContentBlock], video_attachments: usize) -> Result<()> {
    if video_attachments > MAX_VIDEO_ATTACHMENTS {
        bail!(
            "too many video attachments in one prompt (max {MAX_VIDEO_ATTACHMENTS}) — attach one video per prompt"
        );
    }
    if images.len() > MAX_PROMPT_IMAGES {
        bail!(
            "too many image attachments in one prompt (max {MAX_PROMPT_IMAGES}, including video frames)"
        );
    }
    let total_base64: usize = images
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image { data, .. } => Some(data.len()),
            _ => None,
        })
        .sum();
    if total_base64 > MAX_PROMPT_IMAGE_BASE64 {
        bail!(
            "attachments exceed the {} MiB prompt budget ({} base64 bytes)",
            MAX_PROMPT_IMAGE_BASE64 / 1024 / 1024,
            total_base64
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExpandedPrompt {
    pub prompt: String,
    pub images: Vec<ContentBlock>,
    pub file_count: usize,
}

pub fn expand_prompt(prompt: &str, cwd: &Path) -> Result<ExpandedPrompt> {
    let workspace = pi_coding::WorkspaceRoots::new(cwd, Vec::<PathBuf>::new())?;
    expand_prompt_in_workspace(prompt, &workspace)
}

pub fn expand_prompt_in_workspace(
    prompt: &str,
    workspace: &pi_coding::WorkspaceRoots,
) -> Result<ExpandedPrompt> {
    let arguments = parse_file_arguments(prompt)?;
    if arguments.is_empty() {
        return Ok(ExpandedPrompt {
            prompt: prompt.to_owned(),
            images: Vec::new(),
            file_count: 0,
        });
    }

    let file_count = arguments.len();
    let mut expanded = String::with_capacity(prompt.len());
    let mut images = Vec::new();
    let mut video_attachments = 0;
    let mut cursor = 0;

    for argument in arguments {
        expanded.push_str(&prompt[cursor..argument.start]);
        // Resolve containment, then open relative to the selected workspace
        // root capability without following the final component. The returned
        // descriptor is the only authority used for metadata, sniffing, and
        // reading, so path replacement cannot redirect the read outside the
        // configured roots.
        let mut file = open_contained_file(workspace, &argument.path)?;
        let metadata = file
            .metadata()
            .with_context(|| format!("could not inspect @{}", argument.path))?;
        if !metadata.is_file() {
            bail!("@{} is not a file", argument.path);
        }
        if metadata.len() == 0 {
            cursor = argument.end;
            continue;
        }
        // A supported video extension routes through the ffmpeg frame
        // extraction pipeline (TUI/Web parity: the same chronological JPEG
        // ContentBlocks and instruction marker as the Web upload endpoint).
        let is_video = crate::video_extract::sanitize_video_name(&argument.path).is_some();
        if is_video && metadata.len() > crate::video_extract::MAX_VIDEO_UPLOAD_BYTES as u64 {
            bail!(
                "video @{} exceeds the {} MiB limit",
                argument.path,
                crate::video_extract::MAX_VIDEO_UPLOAD_BYTES / 1024 / 1024
            );
        }
        // Sniff the head, then read the remainder — always from the same
        // pinned descriptor. The sniff buffer is zero-filled so read_exact
        // actually fills it (a zero-length Vec would read nothing).
        use std::io::Read as _;
        let sniff_length = usize::try_from(metadata.len().min(32)).unwrap_or(32);
        let mut bytes = vec![0; sniff_length];
        file.read_exact(&mut bytes)
            .with_context(|| format!("could not read @{}", argument.path))?;
        let sniffed_mime = image_pipeline::supported_mime(&bytes);
        if !is_video && sniffed_mime.is_some() && metadata.len() > image_pipeline::MAX_IMAGE_BYTES as u64
        {
            bail!(
                "image @{} exceeds the {} MiB limit",
                argument.path,
                image_pipeline::MAX_IMAGE_BYTES / 1024 / 1024
            );
        }
        if !is_video && sniffed_mime.is_none() && metadata.len() > MAX_TEXT_BYTES {
            bail!(
                "text file @{} exceeds the {} MiB limit",
                argument.path,
                MAX_TEXT_BYTES / 1024 / 1024
            );
        }
        file.read_to_end(&mut bytes)
            .with_context(|| format!("could not read @{}", argument.path))?;
        let escaped_name = escape_xml_attribute(&argument.path);

        if is_video {
            let video = crate::video_extract::extract_video(
                &crate::video_extract::ffmpeg_program(),
                crate::video_extract::VideoLimits::default(),
                bytes,
                &argument.path,
            )
            .map_err(|error| {
                anyhow!("could not process video @{}: {}", argument.path, error.message)
            })?;
            let video_name = escape_xml_attribute(&video.name);
            expanded.push_str("<file name=\"");
            expanded.push_str(&video_name);
            expanded.push_str("\">\n");
            expanded.push_str(&video.instruction);
            expanded.push_str("\n</file>\n");
            images.extend(video.into_content_blocks());
            video_attachments += 1;
        } else {
            let mime_type = sniffed_mime.or_else(|| image_pipeline::supported_mime(&bytes));
            if let Some(mime_type) = mime_type {
                let image = image_pipeline::process_image(bytes, Some(mime_type))
                    .with_context(|| format!("could not process image @{}", argument.path))?;
                expanded.push_str("<file name=\"");
                expanded.push_str(&escaped_name);
                expanded.push_str("\">");
                if let Some(hint) = image.dimension_hint() {
                    expanded.push_str(&hint);
                }
                expanded.push_str("</file>\n");
                images.push(image.into_content_block());
            } else {
                let text = String::from_utf8(bytes).with_context(|| {
                    format!(
                        "@{} is neither a supported image nor UTF-8 text",
                        argument.path
                    )
                })?;
                if text.chars().any(|character| {
                    character.is_control() && !matches!(character, '\n' | '\r' | '\t')
                }) {
                    bail!("@{} contains binary control bytes, not text", argument.path);
                }
                expanded.push_str("<file name=\"");
                expanded.push_str(&escaped_name);
                expanded.push_str("\">\n");
                expanded.push_str(&text);
                expanded.push_str("\n</file>\n");
            }
        }
        // Fail fast on the aggregate budget so a second video or an
        // attachment flood rejects the WHOLE expansion.
        enforce_attachment_budget(&images, video_attachments)?;
        cursor = argument.end;
    }
    expanded.push_str(&prompt[cursor..]);

    Ok(ExpandedPrompt {
        prompt: expanded,
        images,
        file_count,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct FileArgument {
    start: usize,
    end: usize,
    path: String,
}

fn parse_file_arguments(prompt: &str) -> Result<Vec<FileArgument>> {
    let bytes = prompt.as_bytes();
    let mut arguments = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'@' || !is_boundary(prompt, index) {
            index = next_char_boundary(prompt, index);
            continue;
        }
        let start = index;
        index += 1;
        if index >= bytes.len() {
            break;
        }
        let (path, end) = if bytes[index] == b'"' || bytes[index] == b'\'' {
            parse_quoted_path(prompt, index)?
        } else {
            parse_unquoted_path(prompt, index)
        };
        if path.is_empty() {
            index = end.max(index);
            continue;
        }
        arguments.push(FileArgument { start, end, path });
        index = end;
    }
    Ok(arguments)
}

fn parse_quoted_path(prompt: &str, quote_index: usize) -> Result<(String, usize)> {
    let quote = prompt.as_bytes()[quote_index];
    let mut result = String::new();
    let mut index = quote_index + 1;
    let mut escaped = false;
    while index < prompt.len() {
        let character = prompt[index..].chars().next().expect("character boundary");
        let next = index + character.len_utf8();
        if escaped {
            if character == char::from(quote) || character == '\\' {
                result.push(character);
            } else {
                result.push('\\');
                result.push(character);
            }
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == char::from(quote) {
            return Ok((result, next));
        } else {
            result.push(character);
        }
        index = next;
    }
    bail!("unterminated quoted @file path")
}

fn parse_unquoted_path(prompt: &str, start: usize) -> (String, usize) {
    let mut path = String::new();
    let mut index = start;
    let mut escaped = false;
    while index < prompt.len() {
        let character = prompt[index..].chars().next().expect("character boundary");
        let next = index + character.len_utf8();
        if escaped {
            path.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character.is_whitespace() || matches!(character, ')' | ']' | '}' | ',' | ';') {
            break;
        } else {
            path.push(character);
        }
        index = next;
    }
    if escaped {
        path.push('\\');
    }
    (path, index)
}

fn open_contained_file(
    workspace: &pi_coding::WorkspaceRoots,
    input: &str,
) -> Result<std::fs::File> {
    if input.contains('\0') {
        bail!("@file path contains a NUL byte");
    }
    let path = Path::new(input);
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.cwd().join(path)
    };
    let canonical = candidate
        .canonicalize()
        .map_err(|error| anyhow!("@file not found: {} ({error})", input))?;
    let (root, relative) = workspace
        .roots()
        .iter()
        .find_map(|root| canonical.strip_prefix(root).ok().map(|relative| (root, relative)))
        .filter(|(_, relative)| !relative.as_os_str().is_empty())
        .ok_or_else(|| {
            anyhow!("unsafe @file path {input:?}: path escapes the configured workspace roots")
        })?;
    let directory = Dir::open_ambient_dir(root, cap_std::ambient_authority())
        .with_context(|| format!("could not open workspace root for @{input}"))?;
    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    directory
        .open_with(relative, &options)
        .map(cap_std::fs::File::into_std)
        .with_context(|| format!("could not securely open @{input}"))
}

fn is_boundary(prompt: &str, index: usize) -> bool {
    index == 0
        || prompt[..index]
            .chars()
            .next_back()
            .is_some_and(|character| {
                character != '\\'
                    && (character.is_whitespace()
                        || matches!(character, '(' | '[' | '{' | '=' | ',' | ':'))
            })
}

fn next_char_boundary(prompt: &str, index: usize) -> usize {
    index + prompt[index..].chars().next().map_or(1, char::len_utf8)
}

fn escape_xml_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    use base64::Engine as _;
    use image::{DynamicImage, ImageFormat};

    #[test]
    fn expands_multiple_quoted_and_escaped_text_files() {
        let cwd = tempfile::tempdir().expect("tempdir");
        std::fs::write(cwd.path().join("one.txt"), "alpha").expect("write one");
        std::fs::write(cwd.path().join("two words.txt"), "beta").expect("write two");

        let expanded = expand_prompt(
            "compare @one.txt with @\"two words.txt\" and @two\\ words.txt",
            cwd.path(),
        )
        .expect("expand files");
        assert_eq!(expanded.file_count, 3);
        assert!(
            expanded
                .prompt
                .contains("<file name=\"one.txt\">\nalpha\n</file>")
        );
        assert_eq!(expanded.prompt.matches("beta").count(), 2);
        assert!(expanded.images.is_empty());
    }

    #[test]
    fn ignores_email_addresses_and_escaped_literals() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let prompt = "mail user@example.com and literal \\@missing.txt";
        let expanded = expand_prompt(prompt, cwd.path()).expect("no file arguments");
        assert_eq!(expanded.prompt, prompt);
        assert_eq!(expanded.file_count, 0);
    }

    #[test]
    fn rejects_missing_absolute_parent_and_symlink_escape() {
        let cwd = tempfile::tempdir().expect("tempdir");
        assert!(expand_prompt("@missing.txt", cwd.path()).is_err());
        assert!(expand_prompt("@/etc/passwd", cwd.path()).is_err());
        assert!(expand_prompt("@../secret.txt", cwd.path()).is_err());

        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().expect("outside");
            std::fs::write(outside.path().join("secret.txt"), "secret").expect("secret");
            std::os::unix::fs::symlink(
                outside.path().join("secret.txt"),
                cwd.path().join("link.txt"),
            )
            .expect("symlink");
            assert!(expand_prompt("@link.txt", cwd.path()).is_err());
        }
    }

    #[test]
    fn absolute_file_under_additional_root_is_allowed() {
        let cwd = tempfile::tempdir().expect("cwd");
        let additional = tempfile::tempdir().expect("additional");
        let file = additional.path().join("shared.txt");
        std::fs::write(&file, "shared").expect("write shared");
        let workspace = pi_coding::WorkspaceRoots::new(cwd.path(), [additional.path()])
            .expect("workspace");
        let expanded = expand_prompt_in_workspace(
            &format!("inspect @{}", file.display()),
            &workspace,
        )
        .expect("expand additional file");
        assert!(expanded.prompt.contains("shared"));
    }

    #[cfg(unix)]
    #[test]
    fn additional_root_symlink_escape_is_rejected() {
        let cwd = tempfile::tempdir().expect("cwd");
        let additional = tempfile::tempdir().expect("additional");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret.txt"), "secret").expect("secret");
        let link = additional.path().join("escape.txt");
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), &link).expect("symlink");
        let workspace = pi_coding::WorkspaceRoots::new(cwd.path(), [additional.path()])
            .expect("workspace");
        assert!(
            expand_prompt_in_workspace(&format!("inspect @{}", link.display()), &workspace)
                .is_err()
        );
    }

    #[test]
    fn image_argument_becomes_attachment_and_reference() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let image = DynamicImage::new_rgb8(2, 2);
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .expect("encode png");
        std::fs::write(cwd.path().join("shot.png"), &bytes).expect("write png");

        let expanded = expand_prompt("inspect @shot.png", cwd.path()).expect("expand image");
        assert!(expanded.prompt.contains("<file name=\"shot.png\"></file>"));
        assert_eq!(expanded.images.len(), 1);
        let ContentBlock::Image { data, mime_type } = &expanded.images[0] else {
            panic!("expected image block");
        };
        assert_eq!(mime_type, "image/png");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap(),
            bytes
        );
    }

    #[test]
    fn one_bad_argument_fails_the_whole_expansion() {
        let cwd = tempfile::tempdir().expect("tempdir");
        std::fs::write(cwd.path().join("good.txt"), "good").expect("write good");
        let error = expand_prompt("@good.txt @missing.txt", cwd.path())
            .expect_err("missing file rejects prompt");
        assert!(error.to_string().contains("not found"));
    }

    #[test]
    fn xml_attribute_names_are_escaped() {
        let cwd = tempfile::tempdir().expect("tempdir");
        std::fs::write(cwd.path().join("a&b.txt"), "ok").expect("write file");
        let expanded = expand_prompt("@\"a&b.txt\"", cwd.path()).expect("expand file");
        assert!(expanded.prompt.contains("name=\"a&amp;b.txt\""));
    }

    // TUI/Web parity: `@clip.mkv` must produce the same chronological JPEG
    // ContentBlocks plus the bounded instruction marker as the Web upload
    // endpoint, through the shared `video_extract` pipeline.
    #[cfg(unix)]
    #[test]
    fn video_argument_becomes_jpeg_frames_and_marker() {
        use crate::video_extract::test_support::{fake_ffmpeg, video_bytes};
        use crate::video_extract::with_ffmpeg_program;

        let (_dir, script) = fake_ffmpeg();
        let cwd = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            cwd.path().join("clip.mkv"),
            video_bytes("VALID 00:00:12.34 1280x720"),
        )
        .expect("write video");

        let expanded = with_ffmpeg_program(script, || {
            expand_prompt("analyze @clip.mkv", cwd.path()).expect("expand video")
        });
        assert_eq!(expanded.file_count, 1);
        assert_eq!(expanded.images.len(), crate::video_extract::MAX_VIDEO_FRAMES);
        for (index, block) in expanded.images.iter().enumerate() {
            let ContentBlock::Image { data, mime_type } = block else {
                panic!("expected image block at {index}");
            };
            assert_eq!(mime_type, "image/jpeg", "frame {index}");
            assert!(!data.is_empty(), "frame {index} must carry data");
        }
        // Marker: sanitized name, container, duration, chronological order.
        assert!(expanded.prompt.contains("<file name=\"clip.mkv\">"));
        assert!(expanded
            .prompt
            .contains("[Video attachment: clip.mkv (mkv container, 12.34s)"));
        assert!(expanded.prompt.contains("6 chronological JPEG frames"));
        let zero = expanded.prompt.find("0.00s").expect("first timestamp");
        let later = expanded.prompt.find("2.06s").expect("second timestamp");
        assert!(zero < later, "timestamps must be chronological");
        assert!(
            !expanded.prompt.contains("pi-video-"),
            "marker must not leak the work directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn corrupt_video_rejects_the_whole_expansion() {
        use crate::video_extract::test_support::{fake_ffmpeg, video_bytes};
        use crate::video_extract::with_ffmpeg_program;

        let (_dir, script) = fake_ffmpeg();
        let cwd = tempfile::tempdir().expect("tempdir");
        std::fs::write(cwd.path().join("clip.mkv"), video_bytes("CORRUPT"))
            .expect("write video");
        let error = with_ffmpeg_program(script, || {
            expand_prompt("analyze @clip.mkv", cwd.path()).expect_err("corrupt video rejects")
        });
        let message = error.to_string();
        assert!(message.contains("could not process video @clip.mkv"), "{message}");
        assert!(message.contains("not a decodable video"), "{message}");
        assert!(!message.contains("pi-video-"), "no path leak: {message}");
    }

    #[cfg(unix)]
    #[test]
    fn missing_ffmpeg_fails_video_expansion_actionably() {
        use crate::video_extract::test_support::video_bytes;
        use crate::video_extract::with_ffmpeg_program;

        let cwd = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            cwd.path().join("clip.mkv"),
            video_bytes("VALID 00:00:01.00 320x240"),
        )
        .expect("write video");
        let missing = cwd.path().join("no-such-ffmpeg");
        let error = with_ffmpeg_program(missing, || {
            expand_prompt("analyze @clip.mkv", cwd.path()).expect_err("missing ffmpeg rejects")
        });
        let message = error.to_string();
        assert!(message.contains("ffmpeg"), "{message}");
        assert!(message.contains("install"), "actionable: {message}");
    }

    #[cfg(unix)]
    #[test]
    fn two_videos_reject_the_whole_expansion() {
        use crate::video_extract::test_support::{fake_ffmpeg, video_bytes};
        use crate::video_extract::with_ffmpeg_program;

        let (_dir, script) = fake_ffmpeg();
        let cwd = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            cwd.path().join("a.mkv"),
            video_bytes("VALID 00:00:01.00 320x240"),
        )
        .expect("write a");
        std::fs::write(
            cwd.path().join("b.mkv"),
            video_bytes("VALID 00:00:01.00 320x240"),
        )
        .expect("write b");
        let error = with_ffmpeg_program(script, || {
            expand_prompt("analyze @a.mkv and @b.mkv", cwd.path())
                .expect_err("two videos must reject the whole expansion")
        });
        let message = error.to_string();
        assert!(message.contains("too many video attachments"), "{message}");
    }

    #[test]
    fn attachment_budget_bounds_image_count_and_base64() {
        let small = ContentBlock::Image {
            data: "AA==".into(),
            mime_type: "image/jpeg".into(),
        };
        enforce_attachment_budget(&[small.clone()], 1).expect("one frame fits");
        let at_cap = vec![small.clone(); MAX_PROMPT_IMAGES];
        enforce_attachment_budget(&at_cap, 1).expect("at cap fits");
        let too_many = vec![small.clone(); MAX_PROMPT_IMAGES + 1];
        let error = enforce_attachment_budget(&too_many, 1).expect_err("count cap");
        assert!(error.to_string().contains("too many image attachments"), "{error}");

        let big = ContentBlock::Image {
            data: "A".repeat(MAX_PROMPT_IMAGE_BASE64 + 1),
            mime_type: "image/jpeg".into(),
        };
        let error = enforce_attachment_budget(&[big], 1).expect_err("byte cap");
        assert!(error.to_string().contains("prompt budget"), "{error}");

        let error = enforce_attachment_budget(&[], 2).expect_err("video count cap");
        assert!(error.to_string().contains("too many video attachments"), "{error}");
    }
}
