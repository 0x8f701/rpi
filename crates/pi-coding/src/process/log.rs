use std::collections::VecDeque;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use super::ProcessStream;

#[derive(Clone, Debug)]
struct StoredChunk {
    start_cursor: u64,
    stream: ProcessStream,
    bytes: Vec<u8>,
}

#[derive(Debug)]
pub(super) struct ProcessLog {
    capacity: usize,
    retained_bytes: usize,
    start_cursor: u64,
    cursor: u64,
    chunks: VecDeque<StoredChunk>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessLogChunk {
    pub start_cursor: u64,
    pub cursor: u64,
    pub stream: ProcessStream,
    pub data_base64: String,
}

impl ProcessLogChunk {
    #[must_use]
    pub fn bytes(&self) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(&self.data_base64)
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessLogs {
    pub requested_cursor: u64,
    pub start_cursor: u64,
    pub cursor: u64,
    pub lost: bool,
    pub lost_bytes: u64,
    pub chunks: Vec<ProcessLogChunk>,
    pub eof: bool,
}

impl ProcessLog {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            retained_bytes: 0,
            start_cursor: 0,
            cursor: 0,
            chunks: VecDeque::new(),
        }
    }

    pub(super) fn append(&mut self, stream: ProcessStream, bytes: &[u8]) -> (u64, u64) {
        let start_cursor = self.cursor;
        self.cursor = self.cursor.saturating_add(bytes.len() as u64);
        if self.capacity == 0 || bytes.is_empty() {
            self.start_cursor = self.cursor;
            self.chunks.clear();
            self.retained_bytes = 0;
            return (start_cursor, self.cursor);
        }

        let retained = if bytes.len() > self.capacity {
            &bytes[bytes.len() - self.capacity..]
        } else {
            bytes
        };
        let retained_start = self.cursor - retained.len() as u64;
        self.chunks.push_back(StoredChunk {
            start_cursor: retained_start,
            stream,
            bytes: retained.to_vec(),
        });
        self.retained_bytes = self.retained_bytes.saturating_add(retained.len());
        self.trim();
        (start_cursor, self.cursor)
    }

    fn trim(&mut self) {
        while self.retained_bytes > self.capacity {
            let remove = self.retained_bytes - self.capacity;
            let Some(front) = self.chunks.front_mut() else {
                break;
            };
            if remove >= front.bytes.len() {
                self.retained_bytes -= front.bytes.len();
                self.chunks.pop_front();
            } else {
                front.bytes.drain(..remove);
                front.start_cursor = front.start_cursor.saturating_add(remove as u64);
                self.retained_bytes -= remove;
            }
        }
        self.start_cursor = self
            .chunks
            .front()
            .map_or(self.cursor, |chunk| chunk.start_cursor);
    }
    pub(super) fn bounds(&self) -> (u64, u64) {
        (self.start_cursor, self.cursor)
    }


    pub(super) fn read(&self, requested_cursor: u64, max_bytes: usize, eof: bool) -> ProcessLogs {
        let lost_bytes = self.start_cursor.saturating_sub(requested_cursor);
        let effective_cursor = requested_cursor.max(self.start_cursor).min(self.cursor);
        let mut remaining = max_bytes;
        let mut chunks = Vec::new();
        let mut result_cursor = effective_cursor;

        for stored in &self.chunks {
            if remaining == 0 {
                break;
            }
            let stored_end = stored.start_cursor + stored.bytes.len() as u64;
            if stored_end <= effective_cursor {
                continue;
            }
            let offset = effective_cursor.saturating_sub(stored.start_cursor) as usize;
            let available = &stored.bytes[offset..];
            let take = available.len().min(remaining);
            if take == 0 {
                continue;
            }
            let start_cursor = stored.start_cursor + offset as u64;
            let cursor = start_cursor + take as u64;
            chunks.push(ProcessLogChunk {
                start_cursor,
                cursor,
                stream: stored.stream,
                data_base64: base64::engine::general_purpose::STANDARD.encode(&available[..take]),
            });
            result_cursor = cursor;
            remaining -= take;
        }

        ProcessLogs {
            requested_cursor,
            start_cursor: self.start_cursor,
            cursor: result_cursor,
            lost: lost_bytes != 0,
            lost_bytes,
            chunks,
            eof: eof && result_cursor == self.cursor,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_is_monotonic_and_reports_loss() {
        let mut log = ProcessLog::new(5);
        assert_eq!(log.append(ProcessStream::Combined, b"abc"), (0, 3));
        assert_eq!(log.append(ProcessStream::Combined, b"defg"), (3, 7));

        let read = log.read(0, usize::MAX, false);
        assert_eq!(read.start_cursor, 2);
        assert_eq!(read.cursor, 7);
        assert!(read.lost);
        assert_eq!(read.lost_bytes, 2);
        assert_eq!(
            read.chunks
                .iter()
                .flat_map(ProcessLogChunk::bytes)
                .collect::<Vec<_>>(),
            b"cdefg"
        );
    }

    #[test]
    fn reads_from_cursor_and_clips_to_limit() {
        let mut log = ProcessLog::new(32);
        log.append(ProcessStream::Stdout, b"hello");
        log.append(ProcessStream::Stderr, b"world");

        let first = log.read(3, 4, false);
        assert_eq!(first.cursor, 7);
        assert_eq!(first.chunks[0].bytes(), b"lo");
        assert_eq!(first.chunks[1].bytes(), b"wo");
        let second = log.read(first.cursor, 32, true);
        assert_eq!(second.chunks[0].bytes(), b"rld");
        assert!(second.eof);
    }
}
