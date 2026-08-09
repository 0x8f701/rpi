//! Shared `Content-Length` framing for the JSON-RPC-over-child-stdio clients
//! in this crate (LSP in `lsp_client.rs`, MCP in `crate::mcp`, DAP in
//! `debug.rs`).
//!
//! All three protocols speak the same wire format: `Content-Length: <N>\r\n\r\n`
//! followed by the N-byte JSON body. [`encode_message`] writes a frame and
//! [`read_message`] reads one; the `protocol` label only selects the wording of
//! framing errors so a DAP error says "DAP message …" instead of "LSP message …".
//!
//! The clients keep their own thin `read_message(reader)` wrappers so call
//! sites and tests stay unchanged; the MCP/LSP wording is preserved there.
//!
//! Every reader passes an explicit per-protocol body cap ([`read_message`]'s
//! `max_body_bytes`): a frame whose declared `Content-Length` exceeds the cap
//! is rejected before any body bytes are read or allocated, so a hostile or
//! corrupted peer can never drive a multi-gigabyte allocation.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

/// Cap on non-header junk lines scanned before the Content-Length header
/// (mirrors OMP's framing resync: embedded tooling noise must not wedge the
/// client).
pub const MAX_JUNK_HEADER_LINES: usize = 64;

/// Default inbound body cap for the crate's LSP/MCP/DAP child-process clients
/// (16 MiB, matching the ACP framing budget in `pi-cli`). Generous enough for
/// any legitimate message, yet it bounds what a hostile or corrupted peer can
/// make the client allocate.
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// Encodes one JSON-RPC message as a `Content-Length` framed byte payload:
/// `Content-Length: <N>\r\n\r\n` followed by the N-byte JSON body.
pub fn encode_message(body: &Value) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(body)?;
    let mut out = Vec::with_capacity(json.len() + 64);
    out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", json.len()).as_bytes());
    out.extend_from_slice(&json);
    Ok(out)
}

/// Reads one `Content-Length` framed JSON-RPC message from `reader`.
///
/// Headers are parsed case-insensitively and unknown headers are ignored.
/// Junk lines before the header block (blank lines or lines without a valid
/// header) are tolerated and skipped, bounded by [`MAX_JUNK_HEADER_LINES`].
/// `protocol` names the peer in error messages ("LSP", "MCP", "DAP", "ACP").
/// `max_body_bytes` is the caller's per-protocol cap on the declared body
/// size: a frame whose `Content-Length` exceeds it is rejected before any
/// body bytes are read or allocated.
pub async fn read_message(
    protocol: &str,
    reader: &mut (impl AsyncBufRead + Unpin),
    max_body_bytes: usize,
) -> Result<Value> {
    let mut header = String::new();
    let mut content_length: Option<usize> = None;
    let mut junk_lines = 0usize;
    loop {
        header.clear();
        let read = reader
            .read_line(&mut header)
            .await
            .with_context(|| format!("reading {protocol} message header"))?;
        if read == 0 {
            bail!("{protocol} peer closed stdout while reading message headers");
        }
        if header == "\r\n" || header == "\n" {
            if content_length.is_some() {
                break;
            }
            junk_lines += 1;
            if junk_lines > MAX_JUNK_HEADER_LINES {
                bail!("{protocol} message missing Content-Length header");
            }
            continue;
        }
        let Some((name, value)) = header.trim_end().split_once(':') else {
            junk_lines += 1;
            if junk_lines > MAX_JUNK_HEADER_LINES {
                bail!("{protocol} message missing Content-Length header");
            }
            continue; // malformed header line — ignore and keep scanning
        };
        if name.trim().eq_ignore_ascii_case("Content-Length") {
            content_length = Some(value.trim().parse().with_context(|| {
                format!("invalid Content-Length header value: {:?}", value.trim())
            })?);
        }
    }
    let length = content_length
        .ok_or_else(|| anyhow!("{protocol} message missing Content-Length header"))?;
    if length > max_body_bytes {
        bail!(
            "{protocol} message body of {length} bytes exceeds the {max_body_bytes}-byte limit"
        );
    }
    let mut body = vec![0u8; length];
    reader
        .read_exact(&mut body)
        .await
        .with_context(|| format!("reading {protocol} message body"))?;
    serde_json::from_slice(&body).with_context(|| format!("parsing {protocol} message body"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::BufReader;

    #[test]
    fn encode_message_uses_content_length_framing() {
        let body = json!({ "seq": 1, "type": "request", "command": "initialize", "arguments": {} });
        let bytes = encode_message(&body).unwrap();
        let serialized = serde_json::to_vec(&body).unwrap();
        let header = format!("Content-Length: {}\r\n\r\n", serialized.len());
        assert!(
            bytes.starts_with(header.as_bytes()),
            "expected header prefix {header:?} in {bytes:?}"
        );
        assert_eq!(&bytes[header.len()..], &serialized[..]);
    }

    #[test]
    fn encode_message_counts_bytes_not_chars() {
        let body = json!({ "text": "héllo 世界 — ünïcode" });
        let bytes = encode_message(&body).unwrap();
        let serialized = serde_json::to_vec(&body).unwrap();
        let header = format!("Content-Length: {}\r\n\r\n", serialized.len());
        assert!(bytes.starts_with(header.as_bytes()));
        let text_chars = body["text"].as_str().unwrap().chars().count();
        assert!(
            serialized.len() > text_chars,
            "bytes {} vs chars {text_chars}",
            serialized.len()
        );
        assert_eq!(&bytes[header.len()..], &serialized[..]);
    }

    #[tokio::test]
    async fn read_message_decodes_multiple_framed_messages() {
        let mut payload = Vec::new();
        payload.extend(encode_message(&json!({"seq":1,"type":"request","command":"a"})).unwrap());
        payload.extend(encode_message(&json!({"seq":2,"type":"request","command":"b"})).unwrap());
        let mut reader = BufReader::new(&payload[..]);
        let first = read_message("DAP", &mut reader, DEFAULT_MAX_MESSAGE_BYTES).await.unwrap();
        let second = read_message("DAP", &mut reader, DEFAULT_MAX_MESSAGE_BYTES).await.unwrap();
        assert_eq!(first["seq"], 1);
        assert_eq!(second["seq"], 2);
        assert_eq!(first["command"], "a");
        assert_eq!(second["command"], "b");
    }

    #[tokio::test]
    async fn read_message_accepts_any_header_case_and_order() {
        let body = r#"{"seq":9,"type":"response","success":true}"#;
        let payload = format!(
            "x-custom: ignored\r\ncontent-length: {}\r\nContent-Type: application/json\r\n\r\n{}",
            body.len(),
            body
        );
        let mut reader = BufReader::new(payload.as_bytes());
        let message = read_message("DAP", &mut reader, DEFAULT_MAX_MESSAGE_BYTES).await.unwrap();
        assert_eq!(message["seq"], 9);
        assert_eq!(message["success"], true);
    }

    #[tokio::test]
    async fn read_message_rejects_missing_content_length() {
        // The protocol label is threaded into error wording.
        let payload = format!("{}\r\n\r\n{{}}", "\n".repeat(MAX_JUNK_HEADER_LINES + 1));
        let mut reader = BufReader::new(payload.as_bytes());
        let err = read_message("DAP", &mut reader, DEFAULT_MAX_MESSAGE_BYTES).await.unwrap_err().to_string();
        assert!(err.contains("Content-Length"), "{err}");
        let mut reader = BufReader::new(payload.as_bytes());
        let err = read_message("MCP", &mut reader, DEFAULT_MAX_MESSAGE_BYTES).await.unwrap_err().to_string();
        assert!(err.contains("MCP message"), "{err}");
    }

    #[tokio::test]
    async fn read_message_skips_junk_lines_before_headers() {
        let body = r#"{"seq":7,"type":"event","event":"stopped"}"#;
        let payload = format!(
            "\n\nrunning 1 test\n\ntest debug::x ... ok\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let mut reader = BufReader::new(payload.as_bytes());
        let message = read_message("DAP", &mut reader, DEFAULT_MAX_MESSAGE_BYTES).await.unwrap();
        assert_eq!(message["seq"], 7);
        assert_eq!(message["event"], "stopped");
    }

    #[tokio::test]
    async fn read_message_reports_eof() {
        let mut reader = BufReader::new(&b""[..]);
        let err = read_message("DAP", &mut reader, DEFAULT_MAX_MESSAGE_BYTES).await.unwrap_err().to_string();
        assert!(err.contains("closed"), "{err}");
    }

    #[tokio::test]
    async fn read_message_handles_unicode_bodies() {
        let body = r#"{"seq":1,"type":"event","event":"output","body":{"output":"héllo 世界"}}"#;
        let payload = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let mut reader = BufReader::new(payload.as_bytes());
        let message = read_message("DAP", &mut reader, DEFAULT_MAX_MESSAGE_BYTES).await.unwrap();
        assert_eq!(message["body"]["output"], "héllo 世界");
    }

    /// An `AsyncRead` that yields at most `chunk` bytes per poll, simulating a
    /// peer that dribbles bytes across many small packets (the real stdio
    /// pipes deliver data in OS-sized chunks, not whole frames).
    struct ChunkedReader {
        data: std::io::Cursor<Vec<u8>>,
        chunk: usize,
    }

    impl tokio::io::AsyncRead for ChunkedReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let pos = self.data.position() as usize;
            let data = self.data.get_ref();
            let available = data.len().saturating_sub(pos);
            let take = buf.remaining().min(self.chunk).min(available);
            buf.put_slice(&data[pos..pos + take]);
            self.data.set_position((pos + take) as u64);
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn read_message_reads_body_across_partial_reads() {
        // A frame whose body arrives in 3-byte dribbles must still decode:
        // `read_exact` on the body is the contract that survives partial
        // pipe writes (the MCP/LSP/DAP/ACP clients all rely on it).
        let mut payload = Vec::new();
        payload.extend(encode_message(&json!({"seq":1,"type":"request","command":"a"})).unwrap());
        payload.extend(encode_message(&json!({"seq":2,"type":"request","command":"b"})).unwrap());
        let mut reader = BufReader::new(ChunkedReader {
            data: std::io::Cursor::new(payload),
            chunk: 3,
        });
        let first = read_message("LSP", &mut reader, DEFAULT_MAX_MESSAGE_BYTES).await.unwrap();
        let second = read_message("LSP", &mut reader, DEFAULT_MAX_MESSAGE_BYTES).await.unwrap();
        assert_eq!(first["seq"], 1);
        assert_eq!(first["command"], "a");
        assert_eq!(second["seq"], 2);
        assert_eq!(second["command"], "b");
    }

    #[tokio::test]
    async fn read_message_rejects_invalid_content_length_values() {
        // A header value that cannot parse as a byte count is a framing
        // error, not a hang or an allocation attempt.
        for value in ["nope", "-5", "1.5", ""] {
            let payload = format!("Content-Length: {value}\r\n\r\n{{}}");
            let mut reader = BufReader::new(payload.as_bytes());
            let err = read_message("MCP", &mut reader, DEFAULT_MAX_MESSAGE_BYTES).await.unwrap_err().to_string();
            assert!(
                err.contains("invalid Content-Length header value"),
                "value {value:?}: {err}"
            );
        }
    }

    #[tokio::test]
    async fn read_message_reports_truncated_body() {
        // A declared length that exceeds the bytes actually available (an
        // oversized or cut-off frame) must fail cleanly instead of hanging
        // forever on the missing tail.
        let body = r#"{"seq":1,"type":"event","event":"stopped"}"#;
        let payload = format!("Content-Length: 1000\r\n\r\n{}", body);
        let mut reader = BufReader::new(payload.as_bytes());
        let err = read_message("DAP", &mut reader, DEFAULT_MAX_MESSAGE_BYTES).await.unwrap_err().to_string();
        assert!(err.contains("reading DAP message body"), "{err}");
    }

    #[tokio::test]
    async fn read_message_rejects_oversized_content_length_before_body_read() {
        // A declared length above the caller's cap must be rejected before
        // any body bytes are consumed or allocated: the payload here carries
        // no body at all, so a truncated-body error would prove the reader
        // tried to read the body; the limit error proves it did not.
        let payload = b"Content-Length: 1025\r\n\r\n";
        let mut reader = BufReader::new(&payload[..]);
        let err = read_message("ACP", &mut reader, 1024)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("exceeds the 1024-byte limit"), "{err}");
        assert!(err.contains("ACP message"), "{err}");
    }

    #[tokio::test]
    async fn read_message_accepts_exact_limit_body() {
        // The cap is exclusive: a frame whose declared length equals it is
        // accepted and parsed normally.
        let body = r#"{"seq":1,"type":"request","command":"initialize"}"#;
        let limit = body.len();
        let payload = format!("Content-Length: {limit}\r\n\r\n{body}");
        let mut reader = BufReader::new(payload.as_bytes());
        let message = read_message("DAP", &mut reader, limit).await.unwrap();
        assert_eq!(message["command"], "initialize");
    }

    #[tokio::test]
    async fn read_message_rejects_body_over_default_cap() {
        // The default cap used by the LSP/MCP/DAP clients is enforced too:
        // one byte over the cap fails fast with the contextual error.
        let payload = format!(
            "Content-Length: {}\r\n\r\n",
            DEFAULT_MAX_MESSAGE_BYTES + 1
        );
        let mut reader = BufReader::new(payload.as_bytes());
        let err = read_message("MCP", &mut reader, DEFAULT_MAX_MESSAGE_BYTES)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&format!(
                "exceeds the {DEFAULT_MAX_MESSAGE_BYTES}-byte limit"
            )),
            "{err}"
        );
        assert!(err.contains("MCP message"), "{err}");
    }
}
