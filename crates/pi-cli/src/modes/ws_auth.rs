//! Shared WebSocket transport auth for the control-plane listener (`listen`)
//! and the ACP WebSocket server (`acp`).
//!
//! Both transports use the same bearer-token and browser-Origin checks. Their
//! bind policies intentionally differ only through an explicit pre-bind
//! policy value:
//!
//! - Loopback (127.0.0.0/8 or ::1) is always permitted. A token file is
//!   optional; tokenless loopback accepts native clients without `Origin` and
//!   rejects browsers.
//! - `agent serve` always uses the strict loopback-only policy.
//! - `rpi --listen` may opt into non-loopback plaintext HTTP/WebSocket only
//!   through its explicit insecure-remote flag and a valid token file. The
//!   token authenticates clients but does not encrypt the bearer token or
//!   control traffic against passive network observers.
//! - With a token configured, a client must present it either as an
//!   `Authorization: Bearer <token>` header or as the `rpi-auth.<token>`
//!   `Sec-WebSocket-Protocol` subprotocol. The matched subprotocol is echoed
//!   in the upgrade response. Both comparisons are constant-time.

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use http::HeaderMap;

/// `Sec-WebSocket-Protocol` prefix for browser WebSocket authentication.
const RPI_AUTH_PROTOCOL_PREFIX: &str = "rpi-auth.";

/// Cap on concurrent transport connection tasks (shared by listen and ACP);
/// connections beyond the cap are accepted and dropped. `pub` so `listen` can
/// keep re-exporting it (its integration tests reference it).
pub const MAX_CONNECTION_TASKS: usize = 64;

/// Token files are credentials; anything larger than this is a mistake.
const MAX_TOKEN_FILE_BYTES: u64 = 4096;

/// Extract the `rpi-auth.<token>` subprotocol a browser offered, when the
/// token matches the configured one. `Sec-WebSocket-Protocol` is a
/// comma-separated list of protocol names; the server picks the first
/// matching entry and returns its exact spelling so the upgrade response can
/// echo it. When no token is configured there is nothing to compare against,
/// so the subprotocol channel grants nothing (the tokenless-loopback policy
/// is unchanged: browsers are still rejected because they always send Origin).
pub(crate) fn websocket_subprotocol(headers: &HeaderMap, token: Option<&[u8]>) -> Option<String> {
    let token = token?;
    let value = headers
        .get(http::header::SEC_WEBSOCKET_PROTOCOL)?
        .to_str()
        .ok()?;
    value
        .split(',')
        .map(str::trim)
        .find(|protocol| {
            let Some(candidate) = protocol.strip_prefix(RPI_AUTH_PROTOCOL_PREFIX) else {
                return false;
            };
            !candidate.is_empty()
                && !candidate.bytes().any(|byte| byte.is_ascii_whitespace())
                && constant_work_eq(candidate.as_bytes(), token)
        })
        .map(str::to_owned)
}

/// Whether the request carries a valid `Authorization: Bearer <token>`
/// header, or (when no token is configured) comes from a native client:
/// loopback is a network boundary, not a browser-origin boundary, so
/// tokenless connections are accepted only without an `Origin` header.
pub(crate) fn authorized(headers: &HeaderMap, token: Option<&[u8]>) -> bool {
    let Some(token) = token else {
        return !headers.contains_key(http::header::ORIGIN);
    };
    let Some(value) = headers.get(http::header::AUTHORIZATION) else {
        return false;
    };
    let Some(value) = value.as_bytes().strip_prefix(b"Bearer ") else {
        return false;
    };
    !value.is_empty()
        && !value.iter().any(|byte| byte.is_ascii_whitespace())
        && constant_work_eq(value, token)
}

/// Constant-time byte comparison: the loop always walks the longer length so
/// timing does not reveal how much of the candidate matched.
pub(crate) fn constant_work_eq(candidate: &[u8], expected: &[u8]) -> bool {
    let mut different = candidate.len() ^ expected.len();
    let length = candidate.len().max(expected.len());
    for index in 0..length {
        let left = candidate.get(index).copied().unwrap_or(0);
        let right = expected.get(index).copied().unwrap_or(0);
        different |= usize::from(left ^ right);
    }
    different == 0
}

/// Explicit pre-bind policy for plaintext HTTP/WebSocket transports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ListenAddressPolicy {
    /// Permit only 127.0.0.0/8 and ::1.
    LoopbackOnly,
    /// Permit non-loopback only when a valid token file is also configured.
    AllowAuthenticatedPlaintextRemote,
}

/// Enforce the caller's address policy and load its authentication token.
/// This guard runs strictly before `TcpListener::bind` in both callers.
pub(crate) fn load_auth_token(
    address: IpAddr,
    path: Option<&Path>,
    address_name: &str,
    policy: ListenAddressPolicy,
) -> Result<Option<Vec<u8>>> {
    if address.is_loopback() {
        return path.map(read_token_file).transpose();
    }
    if policy == ListenAddressPolicy::LoopbackOnly {
        bail!(
            "{address_name} only binds loopback addresses (127.0.0.0/8 or ::1); \
             {address} is non-loopback. The transport is plaintext HTTP/WebSocket."
        );
    }
    let path = path.ok_or_else(|| {
        anyhow::anyhow!(
            "{address_name} requires a token file for non-loopback address {address}; \
             tokenless remote listening is forbidden"
        )
    })?;
    read_token_file(path).map(Some)
}

/// Read and validate a token file: a regular file, at most
/// [`MAX_TOKEN_FILE_BYTES`], whose trimmed contents are a single nonempty
/// whitespace-free token.
pub(crate) fn read_token_file(path: &Path) -> Result<Vec<u8>> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("reading token file metadata {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("token path must be a regular file");
    }
    if metadata.len() > MAX_TOKEN_FILE_BYTES {
        bail!("token file exceeds {MAX_TOKEN_FILE_BYTES} bytes");
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading token file {}", path.display()))?;
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    let token = bytes[start..end].to_vec();
    if token.is_empty() {
        bail!("token file must not be empty");
    }
    if token.iter().any(|byte| byte.is_ascii_whitespace()) {
        bail!("token must not contain whitespace");
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token_file_fixture(dir: &tempfile::TempDir) -> PathBuf {
        let path = dir.path().join("token-file");
        std::fs::write(&path, b"fixture-value").unwrap();
        path
    }

    #[test]
    fn load_auth_token_enforces_address_token_and_opt_in_matrix() {
        let dir = tempfile::tempdir().unwrap();
        let token = token_file_fixture(&dir);
        let loopbacks: [(IpAddr, &str); 2] = [
            ("127.0.0.1".parse().unwrap(), "IPv4 loopback"),
            ("::1".parse().unwrap(), "IPv6 loopback"),
        ];
        for (address, label) in loopbacks {
            for policy in [
                ListenAddressPolicy::LoopbackOnly,
                ListenAddressPolicy::AllowAuthenticatedPlaintextRemote,
            ] {
                assert!(
                    load_auth_token(address, None, "--listen", policy)
                        .unwrap()
                        .is_none(),
                    "{label}: tokenless loopback must remain available"
                );
                assert_eq!(
                    load_auth_token(address, Some(&token), "--listen", policy).unwrap(),
                    Some(b"fixture-value".to_vec()),
                    "{label}: token-authenticated loopback must remain available"
                );
            }
        }

        let remote: [(IpAddr, &str); 6] = [
            ("0.0.0.0".parse().unwrap(), "IPv4 wildcard"),
            ("::".parse().unwrap(), "IPv6 wildcard"),
            ("198.51.100.7".parse().unwrap(), "distinct non-loopback IPv4"),
            ("192.0.2.1".parse().unwrap(), "documentation IPv4"),
            ("8.8.8.8".parse().unwrap(), "public IPv4"),
            ("2001:db8::1".parse().unwrap(), "documentation IPv6"),
        ];
        for (address, label) in remote {
            for policy in [
                ListenAddressPolicy::LoopbackOnly,
                ListenAddressPolicy::AllowAuthenticatedPlaintextRemote,
            ] {
                for path in [None, Some(token.as_path())] {
                    let should_allow = policy
                        == ListenAddressPolicy::AllowAuthenticatedPlaintextRemote
                        && path.is_some();
                    let result = load_auth_token(address, path, "--listen", policy);
                    if should_allow {
                        assert_eq!(
                            result.unwrap(),
                            Some(b"fixture-value".to_vec()),
                            "{label}: authenticated explicit opt-in must pass pre-bind policy"
                        );
                    } else {
                        assert!(
                            result.is_err(),
                            "{label}: policy={policy:?} token={} must be refused",
                            path.is_some()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn token_file_must_be_regular_bounded_trimmed_and_nonempty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_token_file(dir.path()).is_err());
        let empty = dir.path().join("empty");
        std::fs::write(&empty, b" \n\t ").unwrap();
        assert!(read_token_file(&empty).is_err());
        let large = dir.path().join("large");
        std::fs::write(&large, vec![b'x'; 4097]).unwrap();
        assert!(read_token_file(&large).is_err());
        let valid = dir.path().join("valid");
        std::fs::write(&valid, b"  secret-value\n").unwrap();
        assert_eq!(read_token_file(&valid).unwrap(), b"secret-value");
    }

    #[test]
    fn constant_work_comparison_handles_lengths_and_bytes() {
        assert!(constant_work_eq(b"token", b"token"));
        assert!(!constant_work_eq(b"token", b"tokeN"));
        assert!(!constant_work_eq(b"token", b"token-long"));
        assert!(!constant_work_eq(b"", b"token"));
    }

    #[test]
    fn authorized_requires_bearer_token_when_configured() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer secret"),
        );
        assert!(authorized(&headers, Some(b"secret")));
        assert!(!authorized(&headers, Some(b"wrong")));
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Basic secret"),
        );
        assert!(!authorized(&headers, Some(b"secret")));
    }

    #[test]
    fn authorized_without_token_rejects_browser_origin_only() {
        let mut headers = HeaderMap::new();
        assert!(authorized(&headers, None));
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer ignored-without-token-policy"),
        );
        assert!(authorized(&headers, None));
        headers.insert(
            http::header::ORIGIN,
            http::HeaderValue::from_static("https://example.test"),
        );
        assert!(!authorized(&headers, None));
    }

    fn protocol_headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::SEC_WEBSOCKET_PROTOCOL, http::HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn websocket_subprotocol_matches_and_preserves_exact_spelling() {
        let headers = protocol_headers("chat, rpi-auth.secret");
        assert_eq!(
            websocket_subprotocol(&headers, Some(b"secret")).as_deref(),
            Some("rpi-auth.secret")
        );
        // The exact offered spelling is preserved so the server echoes it.
        let headers = protocol_headers("rpi-auth.secret");
        assert_eq!(
            websocket_subprotocol(&headers, Some(b"secret")).as_deref(),
            Some("rpi-auth.secret")
        );
        let headers = protocol_headers("chat, rpi-auth.secret");
        assert_eq!(
            websocket_subprotocol(&headers, Some(b"secret")).as_deref(),
            Some("rpi-auth.secret")
        );
    }

    #[test]
    fn websocket_subprotocol_rejects_wrong_empty_whitespace_and_missing() {
        assert_eq!(
            websocket_subprotocol(&protocol_headers("rpi-auth.wrong"), Some(b"secret")),
            None,
            "wrong token must not authenticate"
        );
        assert_eq!(
            websocket_subprotocol(&protocol_headers("rpi-auth."), Some(b"secret")),
            None,
            "empty candidate must not authenticate"
        );
        assert_eq!(
            websocket_subprotocol(&protocol_headers("rpi-auth.sec ret"), Some(b"secret")),
            None,
            "whitespace candidate must not authenticate"
        );
        assert_eq!(
            websocket_subprotocol(&protocol_headers("rpi-auth.secret"), None),
            None,
            "no configured token must not grant subprotocol auth"
        );
        let headers = HeaderMap::new();
        assert_eq!(
            websocket_subprotocol(&headers, Some(b"secret")),
            None,
            "missing header must not authenticate"
        );
        let headers = protocol_headers("chat");
        assert_eq!(
            websocket_subprotocol(&headers, Some(b"secret")),
            None,
            "unrelated subprotocol must not authenticate"
        );
        assert_eq!(
            websocket_subprotocol(&protocol_headers("rpi-auth.Secret"), Some(b"secret")),
            None,
            "token compare must be case-sensitive"
        );
        assert_eq!(
            websocket_subprotocol(&protocol_headers("rpi-auth.secret-long"), Some(b"secret")),
            None,
            "prefix match on a longer token must fail"
        );
    }
}
