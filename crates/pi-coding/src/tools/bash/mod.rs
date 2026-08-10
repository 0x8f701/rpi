//! Bash output accumulator (port of pi's `output-accumulator.ts`).
//!
//! Incrementally tracks streaming output with bounded memory: it keeps only a
//! rolling tail (≤ 2× max_rolling_bytes) for display snapshots and streams raw
//! chunks to a temp file once the output exceeds the limits. The throttled
//! updater and child-process running live in `tools.rs` (they depend on the
//! agent runtime types); this module is pure and unit-testable.
//!
//! The [`brush`] submodule implements the embedded brush-shell execution used
//! by the bash tool's default (unsandboxed) path.

pub(crate) mod brush;

/// Opt-in PTY execution for the bash tool (`pty: true`): spawns the command
/// in a pseudo-terminal so interactive programs like `sudo` can prompt.
pub(crate) mod pty;

use crate::truncate::{truncate_tail, TruncationResult, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::fs::File;
use std::io::Write;
use std::sync::LazyLock;

/// Contained spill root under the process temp dir. Each process uses a
/// private PID subdirectory so concurrent rpi processes never share files.
pub(crate) const BASH_SPILL_DIR_NAME: &str = "rpi-bash";

/// Process-wide registry of detached (success-path) spill files for THIS
/// process only. Detached paths are registered on take and removed on
/// explicit cleanup or [`cleanup_all_bash_spills`].
///
/// Multi-session note: this registry is process-scoped. A Session Drop must
/// NOT call [`cleanup_all_bash_spills`] (that would delete other live
/// sessions' paths). Session should track the paths it published and call
/// [`cleanup_full_output_path`] per path; Application/process exit calls
/// [`cleanup_all_bash_spills`].
static SPILL_REGISTRY: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

fn register_spill_path(path: &str) {
    if !path.is_empty() {
        SPILL_REGISTRY.lock().insert(path.to_owned());
    }
}

fn unregister_spill_path(path: &str) {
    if !path.is_empty() {
        SPILL_REGISTRY.lock().remove(path);
    }
}

/// Absolute path of this process's private spill directory
/// (`$TMPDIR/rpi-bash/<pid>/`). Concurrent rpi processes never share files.
pub fn bash_spill_dir() -> std::path::PathBuf {
    std::env::temp_dir()
        .join(BASH_SPILL_DIR_NAME)
        .join(std::process::id().to_string())
}

/// Removes one detached spill file and unregisters it. Idempotent.
/// Safe for per-Session cleanup of paths that Session published.
pub fn cleanup_full_output_path(path: &str) {
    if path.is_empty() {
        return;
    }
    unregister_spill_path(path);
    let _ = std::fs::remove_file(path);
}

/// Drains every spill path registered by THIS process. Does NOT sweep the
/// spill directory on disk (that would delete other live rpi processes' files
/// under a shared parent, and other live Sessions' files under this PID).
///
/// Call from Application/process exit only — not from per-Session Drop.
/// Session Drop should call [`cleanup_full_output_path`] for paths it owns.
pub fn cleanup_all_bash_spills() {
    let paths: Vec<String> = {
        let mut reg = SPILL_REGISTRY.lock();
        reg.drain().collect()
    };
    for path in paths {
        let _ = std::fs::remove_file(&path);
    }
}

/// Hard cap on the bytes written to the full-output temp file. Bounds per-command
/// disk usage so a runaway command cannot exhaust the disk; the in-memory
/// rolling tail is unaffected. Chosen generously (10 MiB) so the agent can still
/// `read` the full output of typical truncated commands.
pub(crate) const MAX_FULL_OUTPUT_DISK_BYTES: usize = 10 * 1024 * 1024;

/// The (partial or final) output snapshot.
#[derive(Debug, Clone)]
pub(crate) struct OutputSnapshot {
    pub content: String,
    pub truncation: TruncationResult,
    pub full_output_path: String,
    /// True once the spilled temp file hit `MAX_FULL_OUTPUT_DISK_BYTES`; the
    /// on-disk full output is then capped (the in-memory tail is unaffected).
    pub disk_truncated: bool,
}

/// Incrementally tracks streaming output with bounded memory.
pub(crate) struct OutputAccumulator {
    max_lines: usize,
    max_bytes: usize,
    max_rolling_bytes: usize,
    prefix: String,

    raw_chunks: Vec<Vec<u8>>,
    tail: Vec<u8>,
    tail_starts_at_line_bound: bool,
    total_bytes: usize,
    completed_lines: usize,
    total_lines: usize,
    current_line_bytes: usize,
    has_open_line: bool,
    finished: bool,

    temp_file_path: String,
    temp_file: Option<File>,
    max_disk_bytes: usize,
    disk_bytes_written: usize,
    disk_truncated: bool,
}

impl OutputAccumulator {
    pub(crate) fn new(max_lines: usize, max_bytes: usize, prefix: &str) -> Self {
        let max_lines = if max_lines == 0 { DEFAULT_MAX_LINES } else { max_lines };
        let max_bytes = if max_bytes == 0 { DEFAULT_MAX_BYTES } else { max_bytes };
        let rolling = max_bytes.saturating_mul(2).max(1);
        let prefix = if prefix.is_empty() { "pi-output".to_string() } else { prefix.to_string() };
        Self {
            max_lines,
            max_bytes,
            max_rolling_bytes: rolling,
            prefix,
            raw_chunks: Vec::new(),
            tail: Vec::new(),
            tail_starts_at_line_bound: true,
            total_bytes: 0,
            completed_lines: 0,
            total_lines: 0,
            current_line_bytes: 0,
            has_open_line: false,
            finished: false,
            temp_file_path: String::new(),
            temp_file: None,
            max_disk_bytes: MAX_FULL_OUTPUT_DISK_BYTES,
            disk_bytes_written: 0,
            disk_truncated: false,
        }
    }

    pub(crate) fn append(&mut self, data: &[u8]) {
        if self.finished || data.is_empty() {
            return;
        }
        self.append_text(data);
        if self.temp_file.is_some() || self.should_use_temp_file() {
            self.ensure_temp_file();
            if let Some(f) = &mut self.temp_file {
                Self::write_capped(
                    f,
                    self.max_disk_bytes,
                    &mut self.disk_bytes_written,
                    &mut self.disk_truncated,
                    data,
                );
            }
        } else {
            // Copy: the caller may reuse the write buffer.
            self.raw_chunks.push(data.to_vec());
        }
    }

    /// Writes `data` to the temp file, hard-capped at `max_disk_bytes`. Once the
    /// cap is hit, further bytes are dropped and `disk_truncated` is set (the
    /// in-memory rolling tail still reflects all output for display). Takes the
    /// disk-tracking fields by reference so it can be called while the file
    /// handle is borrowed from `self.temp_file` (disjoint field borrows).
    fn write_capped(
        f: &mut File,
        max_disk_bytes: usize,
        disk_bytes_written: &mut usize,
        disk_truncated: &mut bool,
        data: &[u8],
    ) {
        if *disk_truncated {
            return;
        }
        let remaining = max_disk_bytes.saturating_sub(*disk_bytes_written);
        if remaining == 0 {
            *disk_truncated = true;
            return;
        }
        let n = data.len().min(remaining);
        let _ = f.write_all(&data[..n]);
        *disk_bytes_written += n;
        if n < data.len() {
            *disk_truncated = true;
        }
    }

    pub(crate) fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        if self.should_use_temp_file() {
            self.ensure_temp_file();
        }
    }

    fn append_text(&mut self, data: &[u8]) {
        self.total_bytes += data.len();
        self.tail.extend_from_slice(data);
        if self.tail.len() > self.max_rolling_bytes * 2 {
            self.trim_tail();
        }

        let newlines = data.iter().filter(|&&b| b == b'\n').count();
        if newlines == 0 {
            self.current_line_bytes += data.len();
            self.has_open_line = true;
        } else {
            self.completed_lines += newlines;
            let last_nl = data.iter().rposition(|&b| b == b'\n').unwrap();
            let tail_len = data.len() - last_nl - 1;
            self.current_line_bytes = tail_len;
            self.has_open_line = tail_len > 0;
        }
        self.total_lines = self.completed_lines;
        if self.has_open_line {
            self.total_lines += 1;
        }
    }

    fn trim_tail(&mut self) {
        if self.tail.len() <= self.max_rolling_bytes {
            return;
        }
        let mut start = self.tail.len() - self.max_rolling_bytes;
        while start < self.tail.len() && (self.tail[start] & 0xc0) == 0x80 {
            start += 1;
        }
        if start > 0 {
            self.tail_starts_at_line_bound = self.tail[start - 1] == b'\n';
        }
        self.tail = self.tail[start..].to_vec();
    }

    fn snapshot_text(&self) -> String {
        if self.tail_starts_at_line_bound {
            return String::from_utf8_lossy(&self.tail).into_owned();
        }
        if let Some(i) = self.tail.iter().position(|&b| b == b'\n') {
            return String::from_utf8_lossy(&self.tail[i + 1..]).into_owned();
        }
        String::from_utf8_lossy(&self.tail).into_owned()
    }

    pub(crate) fn snapshot(&mut self, persist_if_truncated: bool) -> OutputSnapshot {
        let mut tr = truncate_tail(&self.snapshot_text(), self.max_lines, self.max_bytes);
        let truncated = self.total_lines > self.max_lines || self.total_bytes > self.max_bytes;
        let mut truncated_by = String::new();
        if truncated {
            truncated_by = tr.truncated_by.clone();
            if truncated_by.is_empty() {
                if self.total_bytes > self.max_bytes {
                    truncated_by = "bytes".to_string();
                } else {
                    truncated_by = "lines".to_string();
                }
            }
        }
        tr.truncated = truncated;
        tr.truncated_by = truncated_by;
        tr.total_lines = self.total_lines;
        tr.total_bytes = self.total_bytes;
        tr.max_lines = self.max_lines;
        tr.max_bytes = self.max_bytes;

        if persist_if_truncated && truncated {
            self.ensure_temp_file();
        }
        let content = tr.content.clone();
        OutputSnapshot {
            content,
            truncation: tr,
            full_output_path: self.temp_file_path.clone(),
            disk_truncated: self.disk_truncated,
        }
    }

    pub(crate) fn get_last_line_bytes(&self) -> usize {
        self.current_line_bytes
    }

    fn should_use_temp_file(&self) -> bool {
        self.total_bytes > self.max_bytes || self.total_lines > self.max_lines
    }

    fn ensure_temp_file(&mut self) {
        if !self.temp_file_path.is_empty() {
            return;
        }
        // Contained, pi-rs-owned temp dir so spill files are isolated and the
        // application can clean the whole dir on shutdown. Create lazily; if the
        // dir is unavailable we degrade to the bounded tail (no full-output path).
        let dir = bash_spill_dir();
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        // Atomic, unique, no-symlink-overwrite creation: `create_new` (O_CREAT |
        // O_EXCL) fails if the path already exists — including via a symlink — so
        // neither a UUID collision nor a symlink substitution can overwrite an
        // unrelated file. Retry a few times on the vanishingly-unlikely collision.
        let buffered = std::mem::take(&mut self.raw_chunks);
        for _ in 0..8 {
            let id = uuid::Uuid::new_v4().simple().to_string();
            let path = dir.join(format!("{}-{}.log", self.prefix, &id[..16]));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    for chunk in &buffered {
                        Self::write_capped(
                            &mut f,
                            self.max_disk_bytes,
                            &mut self.disk_bytes_written,
                            &mut self.disk_truncated,
                            chunk,
                        );
                    }
                    let _ = f.sync_all();
                    self.temp_file = Some(f);
                    // Publish the path only after the file is successfully and
                    // securely opened, so the returned path is never dangling.
                    self.temp_file_path = path.to_string_lossy().into_owned();
                    return;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => break,
            }
        }
        // All attempts failed (or create unavailable): restore the buffered
        // chunks so a later successful open can flush them (the tail already
        // holds this data for display). Never publish a path when no file was
        // created.
        self.raw_chunks = buffered;
    }
    /// Detaches the temp file: drops the file handle and yields the path, so the
    /// accumulator's `Drop` will NOT delete it. The caller owns the file and is
    /// responsible for cleanup (see `tools::cleanup_full_output_path`).
    pub(crate) fn take_temp_file(&mut self) -> Option<String> {
        self.temp_file = None;
        let path = std::mem::take(&mut self.temp_file_path);
        if path.is_empty() {
            None
        } else {
            // Register so Application/Session (or cleanup_all_bash_spills) can
            // drain success-path leaks on shutdown.
            register_spill_path(&path);
            Some(path)
        }
    }
}

impl Drop for OutputAccumulator {
    fn drop(&mut self) {
        // Safety net: delete any still-owned temp file so failed/aborted commands
        // never leak. Files detached via `take_temp_file` are left for the caller.
        if !self.temp_file_path.is_empty() {
            // Drop the open handle BEFORE unlinking: Windows refuses to delete a
            // file that is still open.
            self.temp_file = None;
            let _ = std::fs::remove_file(&self.temp_file_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accumulator_small_output_passthrough() {
        let mut acc = OutputAccumulator::new(0, 0, "pi-test");
        acc.append(b"hello\nworld\n");
        let snap = acc.snapshot(false);
        assert!(!snap.truncation.truncated);
        // The trailing newline is preserved (tail starts at a line boundary).
        assert_eq!(snap.content, "hello\nworld\n");
        assert_eq!(snap.truncation.total_lines, 2);
    }

    #[test]
    fn accumulator_keeps_rolling_tail() {
        let mut acc = OutputAccumulator::new(3, 0, "pi-test");
        for i in 0..5 {
            acc.append(format!("line{i}\n").as_bytes());
        }
        acc.finish();
        let snap = acc.snapshot(false);
        assert!(snap.truncation.truncated);
        assert_eq!(snap.content, "line2\nline3\nline4");
        assert_eq!(snap.truncation.total_lines, 5);
    }

    #[test]
    fn accumulator_byte_limit_triggers_temp_file() {
        let mut acc = OutputAccumulator::new(0, 10, "pi-test");
        acc.append(b"12345678901234567890\n"); // 21 bytes > 10
        let snap = acc.snapshot(true);
        assert!(snap.truncation.truncated);
        assert!(!snap.full_output_path.is_empty());
        let _ = std::fs::remove_file(&snap.full_output_path);
    }

    #[test]
    fn accumulator_disk_cap_truncates_spill() {
        let mut acc = OutputAccumulator::new(0, 10, "pi-test-cap");
        // Push well past the 10 MiB disk cap in 64 KiB chunks (32 MiB total),
        // matching the audit repro: assert no disk growth beyond the cap.
        let chunk = vec![b'x'; 64 * 1024];
        for _ in 0..512 {
            acc.append(&chunk);
        }
        acc.finish();
        let snap = acc.snapshot(true);
        assert!(snap.disk_truncated, "expected disk_truncated after cap");
        let path = snap.full_output_path.clone();
        assert!(!path.is_empty());
        let size = std::fs::metadata(&path).map(|m| m.len() as usize).unwrap_or(usize::MAX);
        assert!(
            size <= MAX_FULL_OUTPUT_DISK_BYTES,
            "spill file {size} bytes exceeds cap {}",
            MAX_FULL_OUTPUT_DISK_BYTES
        );
        // The in-memory tail accounting is unaffected by the disk cap.
        assert!(acc.total_bytes > MAX_FULL_OUTPUT_DISK_BYTES);
        drop(acc);
        assert!(!std::path::Path::new(&path).exists(), "Drop should remove the spill file");
    }

    #[test]
    fn accumulator_drop_removes_owned_temp_file() {
        let mut acc = OutputAccumulator::new(0, 10, "pi-test-drop");
        acc.append(b"12345678901234567890\n"); // 21 bytes > 10 → spill
        let snap = acc.snapshot(true);
        let path = snap.full_output_path.clone();
        assert!(std::path::Path::new(&path).exists());
        drop(acc);
        assert!(!std::path::Path::new(&path).exists(), "Drop must remove the owned temp file");
    }

    #[test]
    fn accumulator_take_temp_file_survives_drop() {
        let mut acc = OutputAccumulator::new(0, 10, "pi-test-take");
        acc.append(b"12345678901234567890\n");
        let snap = acc.snapshot(true);
        let path = snap.full_output_path.clone();
        let taken = acc.take_temp_file();
        assert_eq!(taken.as_deref(), Some(path.as_str()));
        drop(acc);
        assert!(std::path::Path::new(&path).exists(), "detached file must survive Drop");
        // Detached path is registered; explicit cleanup removes + unregisters.
        cleanup_full_output_path(&path);
        assert!(!std::path::Path::new(&path).exists());
    }

    #[test]
    fn cleanup_all_bash_spills_drains_registry() {
        let mut a = OutputAccumulator::new(0, 10, "pi-test-reg-a");
        let mut b = OutputAccumulator::new(0, 10, "pi-test-reg-b");
        a.append(b"12345678901234567890\n");
        b.append(b"12345678901234567890\n");
        let pa = a.snapshot(true).full_output_path;
        let pb = b.snapshot(true).full_output_path;
        assert!(!pa.is_empty() && !pb.is_empty());
        let _ = a.take_temp_file();
        let _ = b.take_temp_file();
        drop(a);
        drop(b);
        assert!(std::path::Path::new(&pa).exists());
        assert!(std::path::Path::new(&pb).exists());
        cleanup_all_bash_spills();
        assert!(!std::path::Path::new(&pa).exists(), "cleanup_all must drain registered path a");
        assert!(!std::path::Path::new(&pb).exists(), "cleanup_all must drain registered path b");
    }

    #[test]
    fn accumulator_last_line_bytes_tracking() {
        let mut acc = OutputAccumulator::new(0, 0, "pi-test");
        acc.append(b"ab\ncdef"); // "ab\n" completes a line; "cdef" is open.
        assert_eq!(acc.get_last_line_bytes(), 4);
        assert!(acc.total_lines >= 2);
    }
}