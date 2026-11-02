//! Bounded Bash output accumulation backed by retained session scratch authority.
//!
//! Streaming keeps a bounded rolling tail in memory and spills full output only
//! through the generation-captured scratch capability supplied by the caller.
//! Detached spills retain their exact file identity for capability-relative
//! cleanup; display paths are diagnostic handles, never filesystem authority.

pub(crate) mod brush;

/// Governed PTY execution for `pty: true`.
pub(crate) mod pty;

/// Governed merged-pipe execution for every confined non-PTY command and for
/// danger-mode commands the embedded shell cannot parse.
pub(crate) mod foreground;

use crate::fs::{FileIdentity, FileMode, PolicyFile, PolicyScratch, PolicyWritableFile};
use crate::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncationResult, truncate_tail};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

#[derive(Clone)]
pub(crate) struct BashSpillAuthority {
    scratch: Arc<PolicyScratch>,
}

impl BashSpillAuthority {
    pub(crate) fn new(scratch: PolicyScratch) -> Self {
        Self {
            scratch: Arc::new(scratch),
        }
    }
}

struct DetachedSpill {
    scratch: Arc<PolicyScratch>,
    relative: PathBuf,
    identity: FileIdentity,
    file: PolicyFile,
}

static SPILL_REGISTRY: LazyLock<Mutex<HashMap<String, DetachedSpill>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn register_spill(path: String, spill: DetachedSpill) {
    SPILL_REGISTRY.lock().insert(path, spill);
}

pub fn cleanup_full_output_path(path: &str) {
    let Some(spill) = SPILL_REGISTRY.lock().remove(path) else {
        return;
    };
    let _ = spill
        .scratch
        .remove_file_expected(&spill.relative, spill.identity);
}

pub fn cleanup_all_bash_spills() {
    let spills = SPILL_REGISTRY
        .lock()
        .drain()
        .map(|(_, spill)| spill)
        .collect::<Vec<_>>();
    for spill in spills {
        let _ = spill
            .scratch
            .remove_file_expected(&spill.relative, spill.identity);
    }
}

pub(crate) fn read_full_output_path(
    path: &str,
    max_bytes: u64,
) -> Option<Result<Vec<u8>, crate::fs::FsError>> {
    let registry = SPILL_REGISTRY.lock();
    Some(registry.get(path)?.file.read_bounded(max_bytes))
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
    spill_authority: BashSpillAuthority,

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
    temp_file_relative: Option<PathBuf>,
    temp_file: Option<PolicyWritableFile>,
    max_disk_bytes: usize,
    disk_bytes_written: usize,
    disk_truncated: bool,
}

impl OutputAccumulator {
    pub(crate) fn new(
        spill_authority: BashSpillAuthority,
        max_lines: usize,
        max_bytes: usize,
        prefix: &str,
    ) -> Self {
        let max_lines = if max_lines == 0 {
            DEFAULT_MAX_LINES
        } else {
            max_lines
        };
        let max_bytes = if max_bytes == 0 {
            DEFAULT_MAX_BYTES
        } else {
            max_bytes
        };
        let rolling = max_bytes.saturating_mul(2).max(1);
        let prefix = if prefix.is_empty() {
            "pi-output".to_string()
        } else {
            prefix.to_string()
        };
        Self {
            max_lines,
            max_bytes,
            max_rolling_bytes: rolling,
            prefix,
            spill_authority,
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
            temp_file_relative: None,
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
        file: &mut PolicyWritableFile,
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
        let count = data.len().min(remaining);
        if file.write_all(&data[..count]).is_err() {
            *disk_truncated = true;
            return;
        }
        *disk_bytes_written += count;
        if count < data.len() {
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
        let buffered = std::mem::take(&mut self.raw_chunks);
        for _ in 0..8 {
            let id = uuid::Uuid::new_v4().simple().to_string();
            let relative = PathBuf::from(format!("{}-{}.log", self.prefix, &id[..16]));
            match self
                .spill_authority
                .scratch
                .create_new(&relative, FileMode::Private)
            {
                Ok(mut file) => {
                    for chunk in &buffered {
                        Self::write_capped(
                            &mut file,
                            self.max_disk_bytes,
                            &mut self.disk_bytes_written,
                            &mut self.disk_truncated,
                            chunk,
                        );
                    }
                    if let Ok(path) = self.spill_authority.scratch.display_path(&relative) {
                        self.temp_file = Some(file);
                        self.temp_file_relative = Some(relative);
                        self.temp_file_path = path.to_string_lossy().into_owned();
                        return;
                    }
                    break;
                }
                Err(crate::fs::FsError::System {
                    kind: std::io::ErrorKind::AlreadyExists,
                    ..
                }) => continue,
                Err(_) => break,
            }
        }
        self.raw_chunks = buffered;
    }
    pub(crate) fn take_temp_file(&mut self) -> Option<String> {
        let path = std::mem::take(&mut self.temp_file_path);
        let relative = self.temp_file_relative.take()?;
        let file = self.temp_file.take()?.finish().ok()?;
        if path.is_empty() {
            return None;
        }
        let identity = file.identity();
        register_spill(
            path.clone(),
            DetachedSpill {
                scratch: self.spill_authority.scratch.clone(),
                relative,
                identity,
                file,
            },
        );
        Some(path)
    }
}

impl Drop for OutputAccumulator {
    fn drop(&mut self) {
        self.temp_file.take();
        self.temp_file_relative.take();
        self.temp_file_path.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accumulator(max_lines: usize, max_bytes: usize, prefix: &str) -> OutputAccumulator {
        let root = tempfile::tempdir().expect("scratch root");
        let capabilities = crate::fs::CapabilityFs::new().expect("capabilities");
        let scratch_root = crate::fs::PolicyScratchRoot::acquire(
            pi_sandbox::PolicyRevision::new(1).expect("revision"),
            &capabilities,
            pi_sandbox::AbsolutePath::parse(root.keep()).expect("host scratch"),
            pi_sandbox::AbsolutePath::parse(PathBuf::from("/run/pi/scratch"))
                .expect("sandbox scratch"),
        )
        .expect("scratch authority");
        let scratch = scratch_root
            .create_random(crate::fs::ScratchPurpose::BashOutput)
            .expect("bash scratch");
        OutputAccumulator::new(
            BashSpillAuthority::new(scratch),
            max_lines,
            max_bytes,
            prefix,
        )
    }

    #[test]
    fn accumulator_small_output_passthrough() {
        let mut acc = accumulator(0, 0, "pi-test");
        acc.append(b"hello\nworld\n");
        let snap = acc.snapshot(false);
        assert!(!snap.truncation.truncated);
        assert_eq!(snap.content, "hello\nworld\n");
        assert_eq!(snap.truncation.total_lines, 2);
    }

    #[test]
    fn accumulator_keeps_rolling_tail() {
        let mut acc = accumulator(3, 0, "pi-test");
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
    fn accumulator_last_line_bytes_tracking() {
        let mut acc = accumulator(0, 0, "pi-test");
        acc.append(b"ab\ncdef");
        assert_eq!(acc.get_last_line_bytes(), 4);
        assert!(acc.total_lines >= 2);
    }
}
