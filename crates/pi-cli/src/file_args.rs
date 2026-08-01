use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use pi_ai::ContentBlock;

use crate::image_pipeline;

const MAX_TEXT_BYTES: u64 = 8 * 1024 * 1024;

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
    let mut cursor = 0;

    for argument in arguments {
        expanded.push_str(&prompt[cursor..argument.start]);
        let resolved = resolve_contained_file(workspace, &argument.path)?;
        let metadata = std::fs::metadata(&resolved)
            .with_context(|| format!("could not inspect @{}", argument.path))?;
        if !metadata.is_file() {
            bail!("@{} is not a file", argument.path);
        }
        if metadata.len() == 0 {
            cursor = argument.end;
            continue;
        }
        let sniff_length = usize::try_from(metadata.len().min(32)).unwrap_or(32);
        let mut sniff = vec![0; sniff_length];
        if sniff_length > 0 {
            use std::io::Read as _;
            let mut file = std::fs::File::open(&resolved)
                .with_context(|| format!("could not read @{}", argument.path))?;
            file.read_exact(&mut sniff)
                .with_context(|| format!("could not read @{}", argument.path))?;
        }
        let sniffed_mime = image_pipeline::supported_mime(&sniff);
        if sniffed_mime.is_some() && metadata.len() > image_pipeline::MAX_IMAGE_BYTES as u64 {
            bail!(
                "image @{} exceeds the {} MiB limit",
                argument.path,
                image_pipeline::MAX_IMAGE_BYTES / 1024 / 1024
            );
        }
        if sniffed_mime.is_none() && metadata.len() > MAX_TEXT_BYTES {
            bail!(
                "text file @{} exceeds the {} MiB limit",
                argument.path,
                MAX_TEXT_BYTES / 1024 / 1024
            );
        }
        let escaped_name = escape_xml_attribute(&argument.path);

        let bytes = std::fs::read(&resolved)
            .with_context(|| format!("could not read @{}", argument.path))?;
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
            if text
                .chars()
                .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
            {
                bail!("@{} contains binary control bytes, not text", argument.path);
            }
            expanded.push_str("<file name=\"");
            expanded.push_str(&escaped_name);
            expanded.push_str("\">\n");
            expanded.push_str(&text);
            expanded.push_str("\n</file>\n");
        }
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

fn resolve_contained_file(
    workspace: &pi_coding::WorkspaceRoots,
    input: &str,
) -> Result<PathBuf> {
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
    if !workspace
        .roots()
        .iter()
        .any(|root| canonical.starts_with(root))
    {
        bail!("unsafe @file path {input:?}: path escapes the configured workspace roots");
    }
    Ok(canonical)
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
}
