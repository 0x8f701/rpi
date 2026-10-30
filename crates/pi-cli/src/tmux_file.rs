//! Classify and persist a composer file for `POST /upload/tmux-file`.
//! The path is pasted into a tmux session; the bytes live in a temp file
//! that a spawned timer deletes.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use http::StatusCode;
use uuid::Uuid;

pub(crate) const MAX_TMUX_FILE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const TMUX_FILE_DELETE_AFTER: Duration = Duration::from_secs(300);
const MAX_NAME_CHARS: usize = 120;

const TEXT_EXTENSIONS: &[&str] = &[
    "adoc", "astro", "bash", "bat", "c", "cc", "cfg", "cjs", "clj", "cljs", "cmd", "conf", "cpp",
    "cr", "cs", "css", "csv", "cxx", "dart", "dockerfile", "editorconfig", "elm", "env", "erl",
    "ex", "exs", "fish", "fs", "fsx", "gitattributes", "gitignore", "go", "gradle", "groovy", "h",
    "hh", "hpp", "hs", "htm", "html", "hxx", "ini", "java", "jl", "js", "json", "json5", "jsonc",
    "jsx", "kt", "kts", "less", "lock", "log", "lua", "m", "makefile", "markdown", "md", "mjs",
    "ml", "nim", "php", "pl", "pm", "properties", "ps1", "purs", "py", "r", "rb", "rs", "rst",
    "sass", "scala", "scss", "sh", "sql", "sv", "svelte", "svg", "swift", "tex", "toml", "ts",
    "tsv", "tsx", "txt", "v", "vue", "xml", "yaml", "yml", "zig", "zsh",
];

const TEXT_BASENAMES: &[&str] = &[".editorconfig", ".env", ".gitignore", ".npmrc", "dockerfile", "makefile"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TmuxFileKind {
    Png,
    Jpeg,
    Gif,
    Webp,
    Pdf,
    Text,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TmuxFileAccept {
    pub kind: TmuxFileKind,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TmuxFileError {
    UnsupportedType,
    TooLarge,
    InvalidName,
}

impl TmuxFileError {
    pub(crate) fn status(self) -> StatusCode {
        match self {
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::UnsupportedType | Self::InvalidName => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        }
    }

    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::TooLarge => "upload exceeds the 8 MiB tmux-file limit",
            Self::UnsupportedType => "unsupported tmux file — png, jpeg, gif, webp, pdf, or UTF-8 text",
            Self::InvalidName => "invalid X-File-Name header",
        }
    }
}

pub(crate) fn accept_tmux_file(name: &str, bytes: &[u8]) -> Result<TmuxFileAccept, TmuxFileError> {
    if bytes.len() > MAX_TMUX_FILE_BYTES {
        return Err(TmuxFileError::TooLarge);
    }
    let Some(name) = sanitize_tmux_file_name(name) else {
        return Err(TmuxFileError::InvalidName);
    };
    let kind = match sniff_kind(bytes) {
        Some(kind) => {
            if !name_matches_kind(&name, kind) {
                return Err(TmuxFileError::UnsupportedType);
            }
            kind
        }
        None => {
            if !is_utf8_text(bytes) || !is_text_name(&name) {
                return Err(TmuxFileError::UnsupportedType);
            }
            TmuxFileKind::Text
        }
    };
    Ok(TmuxFileAccept { kind, name })
}

pub(crate) fn write_tmux_file(accepted: &TmuxFileAccept, bytes: &[u8]) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir();
    let file_name = format!("rpi-tmux-file-{}-{}", Uuid::now_v7(), accepted.name);
    let path = dir.join(file_name);
    write_tmux_file_at(&path, bytes)?;
    Ok(path)
}

fn write_tmux_file_at(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let file = create_tmux_file(path)?;
    #[cfg(all(test, unix))]
    apply_unix_persist_faults(&file, path);
    persist_or_abort(file, path, bytes)
}

#[cfg(all(test, unix))]
thread_local! {
    static RETARGET_FD_TO_DEV_FULL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static DENY_PARENT_UNLINK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(all(test, unix))]
struct UnixPersistFaults;

#[cfg(all(test, unix))]
impl Drop for UnixPersistFaults {
    fn drop(&mut self) {
        RETARGET_FD_TO_DEV_FULL.set(false);
        DENY_PARENT_UNLINK.set(false);
    }
}

#[cfg(all(test, unix))]
fn arm_unix_persist_faults(retarget_dev_full: bool, deny_unlink: bool) -> UnixPersistFaults {
    RETARGET_FD_TO_DEV_FULL.set(retarget_dev_full);
    DENY_PARENT_UNLINK.set(deny_unlink);
    UnixPersistFaults
}

#[cfg(all(test, unix))]
fn apply_unix_persist_faults(file: &std::fs::File, path: &Path) {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt;
    if RETARGET_FD_TO_DEV_FULL.get() {
        let full = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .expect("open /dev/full");
        nix::unistd::dup2(full.as_raw_fd(), file.as_raw_fd()).expect("dup2 /dev/full");
    }
    if DENY_PARENT_UNLINK.get() {
        let parent = path.parent().expect("parent");
        let mut permissions = std::fs::metadata(parent).expect("parent meta").permissions();
        permissions.set_mode(0o555);
        std::fs::set_permissions(parent, permissions).expect("deny unlink");
    }
}

fn create_tmux_file(path: &Path) -> Result<std::fs::File, String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(file) => Ok(file),
        Err(error) => Err(format!("create tmux file failed: {error}")),
    }
}

fn persist_or_abort(mut file: std::fs::File, path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Err(error) = std::io::Write::write_all(&mut file, bytes) {
        return Err(abort_partial_tmux_file(
            file,
            path,
            format!("write tmux file failed: {error}"),
        ));
    }
    if let Err(error) = file.sync_all() {
        return Err(abort_partial_tmux_file(
            file,
            path,
            format!("sync tmux file failed: {error}"),
        ));
    }
    Ok(())
}

fn abort_partial_tmux_file(file: std::fs::File, path: &Path, primary: String) -> String {
    drop(file);
    match remove_tmux_file_or_retry(path.to_path_buf()) {
        Ok(()) => primary,
        Err(cleanup) => format!("{primary}; {cleanup}"),
    }
}

pub(crate) fn schedule_tmux_file_delete(path: PathBuf, delay: Duration) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        if let Err(error) = remove_tmux_file(&path) {
            eprintln!("tmux-file cleanup failed: {error}");
        }
    });
}

pub(crate) fn remove_tmux_file_or_retry(path: PathBuf) -> Result<(), String> {
    match remove_tmux_file(&path) {
        Ok(()) => Ok(()),
        Err(error) => {
            schedule_tmux_file_delete(path, TMUX_FILE_DELETE_AFTER);
            Err(format!("{error} (retry scheduled)"))
        }
    }
}

pub(crate) fn remove_tmux_file(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove tmux file failed: {error}")),
    }
}

fn sniff_kind(bytes: &[u8]) -> Option<TmuxFileKind> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some(TmuxFileKind::Png);
    }
    if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return Some(TmuxFileKind::Jpeg);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(TmuxFileKind::Gif);
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some(TmuxFileKind::Webp);
    }
    if bytes.starts_with(b"%PDF") {
        return Some(TmuxFileKind::Pdf);
    }
    None
}

fn sanitize_tmux_file_name(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.len() > 4096 {
        return None;
    }
    let raw = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let lower = raw.to_ascii_lowercase();
    if TEXT_BASENAMES.binary_search(&lower.as_str()).is_ok() {
        return Some(lower);
    }
    let (stem, extension) = raw.rsplit_once('.')?;
    let extension = extension.to_ascii_lowercase();
    if !is_allowed_extension(&extension) {
        return None;
    }
    let stem: String = stem
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect();
    let stem = stem.trim_matches('-');
    let stem = if stem.is_empty() { "file" } else { stem };
    let stem: String = stem.chars().take(MAX_NAME_CHARS.saturating_sub(extension.len() + 1)).collect();
    Some(format!("{stem}.{extension}"))
}

fn is_allowed_extension(extension: &str) -> bool {
    matches!(extension, "png" | "jpg" | "jpeg" | "gif" | "webp" | "pdf") || is_text_extension(extension)
}

fn is_text_extension(extension: &str) -> bool {
    TEXT_EXTENSIONS.binary_search(&extension).is_ok()
}

fn is_text_name(name: &str) -> bool {
    if TEXT_BASENAMES.binary_search(&name).is_ok() {
        return true;
    }
    name.rsplit_once('.')
        .is_some_and(|(_, extension)| is_text_extension(extension))
}

fn name_matches_kind(name: &str, kind: TmuxFileKind) -> bool {
    let extension = name.rsplit_once('.').map(|(_, extension)| extension).unwrap_or("");
    match kind {
        TmuxFileKind::Png => extension == "png",
        TmuxFileKind::Jpeg => extension == "jpg" || extension == "jpeg",
        TmuxFileKind::Gif => extension == "gif",
        TmuxFileKind::Webp => extension == "webp",
        TmuxFileKind::Pdf => extension == "pdf",
        TmuxFileKind::Text => is_text_name(name),
    }
}

fn is_utf8_text(bytes: &[u8]) -> bool {
    if bytes.contains(&0) {
        return false;
    }
    std::str::from_utf8(bytes).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn leftover_named_tmux_files(name: &str) -> Vec<PathBuf> {
        let suffix = format!("-{name}");
        std::fs::read_dir(std::env::temp_dir())
            .expect("temp")
            .map(|entry| entry.expect("temp entry").path())
            .filter(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("rpi-tmux-file-") && n.ends_with(&suffix))
            })
            .collect()
    }

    #[test]
    fn png_magic_and_name_are_accepted() {
        let bytes = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 1];
        let accepted = accept_tmux_file("shot.png", &bytes).expect("png");
        assert_eq!(accepted.kind, TmuxFileKind::Png);
        assert_eq!(accepted.name, "shot.png");
    }

    #[test]
    fn jpeg_accepts_jpg_name() {
        let bytes = [0xFF, 0xD8, 0xFF, 0xE0, 0, 0];
        let accepted = accept_tmux_file("photo.jpg", &bytes).expect("jpeg");
        assert_eq!(accepted.kind, TmuxFileKind::Jpeg);
    }

    #[test]
    fn pdf_magic_is_required() {
        assert!(accept_tmux_file("doc.pdf", b"%PDF-1.4\n").is_ok());
        assert_eq!(
            accept_tmux_file("doc.pdf", b"not a pdf"),
            Err(TmuxFileError::UnsupportedType)
        );
    }

    #[test]
    fn png_magic_rejects_text_name() {
        let bytes = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(
            accept_tmux_file("notes.rs", &bytes),
            Err(TmuxFileError::UnsupportedType)
        );
    }

    #[test]
    fn utf8_rust_is_accepted() {
        let accepted = accept_tmux_file("main.rs", b"fn main() {}\n").expect("rs");
        assert_eq!(accepted.kind, TmuxFileKind::Text);
        assert_eq!(accepted.name, "main.rs");
    }

    #[test]
    fn nul_bytes_are_not_text() {
        assert_eq!(
            accept_tmux_file("main.rs", b"fn\0main"),
            Err(TmuxFileError::UnsupportedType)
        );
    }

    #[test]
    fn oversized_bytes_are_rejected() {
        let bytes = vec![b'a'; MAX_TMUX_FILE_BYTES + 1];
        assert_eq!(
            accept_tmux_file("notes.txt", &bytes),
            Err(TmuxFileError::TooLarge)
        );
    }

    #[test]
    fn path_components_are_stripped() {
        let accepted = accept_tmux_file("../evil.rs", b"fn x() {}").expect("name");
        assert_eq!(accepted.name, "evil.rs");
    }

    #[tokio::test]
    async fn scheduled_delete_removes_the_file() {
        let path = std::env::temp_dir().join(format!("rpi-tmux-file-test-{}", Uuid::now_v7()));
        std::fs::write(&path, b"x").expect("write");
        schedule_tmux_file_delete(path.clone(), Duration::from_millis(40));
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(!path.exists(), "timer must delete the temp file");
    }

    #[tokio::test]
    async fn scheduled_delete_of_missing_file_is_success() {
        let path = std::env::temp_dir().join(format!("rpi-tmux-file-missing-{}", Uuid::now_v7()));
        assert!(!path.exists());
        schedule_tmux_file_delete(path.clone(), Duration::from_millis(20));
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(!path.exists());
    }

    #[test]
    fn remove_missing_file_is_success() {
        let path = std::env::temp_dir().join(format!("rpi-tmux-file-absent-{}", Uuid::now_v7()));
        remove_tmux_file(&path).expect("not found is success");
    }

    #[test]
    fn remove_directory_propagates() {
        let path = std::env::temp_dir().join(format!("rpi-tmux-file-dir-{}", Uuid::now_v7()));
        std::fs::create_dir(&path).expect("dir");
        let error = remove_tmux_file(&path).expect_err("directory");
        assert!(error.contains("remove tmux file failed"), "{error}");
        std::fs::remove_dir(&path).expect("cleanup dir");
    }

    #[cfg(unix)]
    #[test]
    fn write_tmux_file_unlinks_when_write_fails() {
        let name = format!("notes-{}.txt", Uuid::now_v7());
        let accepted = accept_tmux_file(&name, b"hello").expect("accept");
        let _faults = arm_unix_persist_faults(true, false);
        let error = write_tmux_file(&accepted, b"hello").expect_err("write");
        assert!(error.contains("write tmux file failed"), "{error}");
        assert!(!error.contains("retry scheduled"), "{error}");
        let leftovers = leftover_named_tmux_files(&accepted.name);
        assert!(leftovers.is_empty(), "partial file must be unlinked: {leftovers:?}");
    }

    #[cfg(unix)]
    #[test]
    fn write_tmux_file_unlinks_when_sync_fails() {
        let name = format!("notes-{}.txt", Uuid::now_v7());
        let accepted = accept_tmux_file(&name, b"").expect("accept");
        let _faults = arm_unix_persist_faults(true, false);
        let error = write_tmux_file(&accepted, b"").expect_err("sync");
        assert!(error.contains("sync tmux file failed"), "{error}");
        assert!(!error.contains("retry scheduled"), "{error}");
        let leftovers = leftover_named_tmux_files(&accepted.name);
        assert!(leftovers.is_empty(), "partial file must be unlinked: {leftovers:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_fail_schedules_when_unlink_denied() {
        let dir = std::env::temp_dir().join(format!("rpi-tmux-file-write-ro-{}", Uuid::now_v7()));
        std::fs::create_dir(&dir).expect("dir");
        let path = dir.join("notes.txt");
        let original = std::fs::metadata(&dir).expect("meta").permissions();
        let _faults = arm_unix_persist_faults(true, true);
        let error = write_tmux_file_at(&path, b"hello").expect_err("write");
        assert!(error.contains("write tmux file failed"), "{error}");
        assert!(error.contains("remove tmux file failed"), "{error}");
        assert!(error.contains("retry scheduled"), "{error}");
        assert!(path.exists(), "immediate unlink denied so delayed cleanup owns the file");
        std::fs::set_permissions(&dir, original).expect("rw");
        std::fs::remove_file(&path).expect("file cleanup");
        std::fs::remove_dir(&dir).expect("dir cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn persist_schedules_when_sync_cleanup_fails() {
        let dir = std::env::temp_dir().join(format!("rpi-tmux-file-sync-ro-{}", Uuid::now_v7()));
        std::fs::create_dir(&dir).expect("dir");
        let path = dir.join("notes.txt");
        let original = std::fs::metadata(&dir).expect("meta").permissions();
        let _faults = arm_unix_persist_faults(true, true);
        let error = write_tmux_file_at(&path, b"").expect_err("sync");
        assert!(error.contains("sync tmux file failed"), "{error}");
        assert!(error.contains("remove tmux file failed"), "{error}");
        assert!(error.contains("retry scheduled"), "{error}");
        assert!(path.exists(), "immediate unlink denied so delayed cleanup owns the file");
        std::fs::set_permissions(&dir, original).expect("rw");
        std::fs::remove_file(&path).expect("file cleanup");
        std::fs::remove_dir(&dir).expect("dir cleanup");
    }

    #[test]
    fn write_create_new_does_not_clobber() {
        let path = std::env::temp_dir().join(format!("rpi-tmux-file-exists-{}", Uuid::now_v7()));
        std::fs::write(&path, b"old").expect("seed");
        let error = write_tmux_file_at(&path, b"new").expect_err("create_new");
        assert!(error.contains("create tmux file failed"), "{error}");
        assert_eq!(std::fs::read(&path).expect("read"), b"old");
        std::fs::remove_file(&path).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn write_creates_owner_only_mode() {
        use std::os::unix::fs::PermissionsExt;
        let accepted = accept_tmux_file("notes.txt", b"hi").expect("accept");
        let path = write_tmux_file(&accepted, b"hi").expect("write");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "mode={mode:#o}");
        remove_tmux_file(&path).expect("cleanup");
    }
}
