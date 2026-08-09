//! Shared WebSocket transport auth for the control-plane listener (`listen`)
//! and the ACP WebSocket server (`acp`).
//!
//! Both transports use the same bearer-token and browser-Origin checks. Their
//! bind policies intentionally differ through an explicit pre-bind policy
//! value and a per-transport tokenless stance:
//!
//! - Loopback (127.0.0.0/8 or ::1) is always permitted. A token file is
//!   optional; the control-plane listener's tokenless mode accepts native
//!   clients and same-origin browsers whose request `Origin` is `http://`
//!   with an authority matching the request's `Host` — the address the
//!   user's browser actually used — rejecting ordinary unrelated
//!   cross-origin pages. No advertised origin is required for this check.
//! - `agent serve` always uses the strict loopback-only policy and rejects
//!   tokenless browsers outright: ACP's tokenless transport accepts only
//!   native clients without an `Origin` header.
//! - `rpi --listen` may opt into non-loopback plaintext HTTP/WebSocket only
//!   through its explicit insecure-remote flag; a token file is optional
//!   there too. The token authenticates clients but does not encrypt the
//!   bearer token or control traffic against passive network observers.
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
/// so the subprotocol channel grants nothing; the connection then stands or
/// falls on the caller's tokenless stance ([`authorized`]).
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

/// Canonical `scheme://host[:port]` form of an HTTP(S) origin for
/// comparison: lowercased host, explicit port preserved, no path, query, or
/// fragment. Returns `None` for malformed values and the literal `null`
/// origin (sandboxed/opaque contexts), which never match a listener.
pub(crate) fn normalize_origin(value: &str) -> Option<String> {
    let (scheme, rest) = value.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let (authority, tail) = match rest.find('/') {
        Some(index) => rest.split_at(index),
        None => (rest, ""),
    };
    if !tail.is_empty() || authority.is_empty() {
        return None;
    }
    if authority.contains(['@', '?', '#'])
        || authority.chars().any(char::is_whitespace)
    {
        return None;
    }
    Some(format!("{scheme}://{}", authority.to_lowercase()))
}

/// Canonical authority form of an HTTP `Host` header value for same-origin
/// comparison against an origin's authority: lowercased host, explicit port
/// preserved, IPv6 brackets kept. Returns `None` for malformed values
/// (empty, whitespace, credentials, path/query/fragment, unbracketed IPv6,
/// or an invalid/zero port).
pub(crate) fn normalize_host(value: &str) -> Option<String> {
    if value.is_empty()
        || value.chars().any(char::is_whitespace)
        || value.contains(['@', '/', '?', '#', ','])
    {
        return None;
    }
    let (host, port, bracketed) = if let Some(rest) = value.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = &rest[..end];
        let after = &rest[end + 1..];
        let port = match after {
            "" => None,
            rest => Some(rest.strip_prefix(':')?),
        };
        (host, port, true)
    } else {
        match value.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() => (host, Some(port), false),
            _ => (value, None, false),
        }
    };
    if host.is_empty() || host.chars().any(char::is_whitespace) {
        return None;
    }
    if !bracketed && host.contains(':') {
        return None;
    }
    let port = match port {
        Some(port) if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) => {
            return None;
        }
        Some(port) => {
            let value: u16 = port.parse().ok()?;
            if value == 0 {
                return None;
            }
            Some(port)
        }
        None => None,
    };
    let host = host.to_lowercase();
    Some(match (bracketed, port) {
        (true, Some(port)) => format!("[{host}]:{port}"),
        (true, None) => format!("[{host}]"),
        (false, Some(port)) => format!("{host}:{port}"),
        (false, None) => host,
    })
}

/// Whether the request carries a valid `Authorization: Bearer <token>`
/// header, or — when no token is configured — is permitted by the caller's
/// tokenless policy. Native clients (no `Origin` header) are always accepted
/// tokenless. A tokenless browser (browsers always send `Origin`) is accepted
/// only when the caller permits Origin-bearing requests
/// (`allow_tokenless_browser`, the control-plane listener) and the request
/// is same-origin: exactly one valid `Origin` of `http://` whose authority
/// equals the request's single valid `Host` — the address the user's browser
/// actually used, whatever LAN IP or hostname that is. Malformed or
/// duplicate `Origin` or `Host` headers, and scheme/authority mismatches,
/// reject the request. This is ordinary request same-origin, not
/// authentication: it only rejects unrelated cross-origin pages and is not
/// claimed as DNS-rebinding protection. With `allow_tokenless_browser` false
/// (ACP's strict loopback-only transport) every tokenless browser is
/// rejected. A configured token is enforced exactly, regardless of `Origin`
/// or `Host`.
pub(crate) fn authorized(
    headers: &HeaderMap,
    token: Option<&[u8]>,
    allow_tokenless_browser: bool,
) -> bool {
    let Some(token) = token else {
        let Some(origin) = headers.get(http::header::ORIGIN) else {
            return true;
        };
        if !allow_tokenless_browser {
            return false;
        }
        // Same-origin: exactly one valid Origin and one Host whose
        // authorities match.
        if headers.get_all(http::header::ORIGIN).iter().count() != 1
            || headers.get_all(http::header::HOST).iter().count() != 1
        {
            return false;
        }
        let Some(origin) = origin.to_str().ok().and_then(normalize_origin) else {
            return false;
        };
        let Some(host) = headers
            .get(http::header::HOST)
            .and_then(|value| value.to_str().ok())
            .and_then(normalize_host)
        else {
            return false;
        };
        // The transport is plaintext HTTP, so a same-origin browser page is
        // necessarily an `http://` origin whose authority matches the Host.
        return origin == format!("http://{host}");
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
    /// Permit non-loopback plaintext HTTP/WebSocket; a token file is
    /// optional (strongly recommended).
    AllowPlaintextRemote,
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
    // Non-loopback with the explicit plaintext opt-in: a token is optional.
    path.map(read_token_file).transpose()
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
                ListenAddressPolicy::AllowPlaintextRemote,
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
                ListenAddressPolicy::AllowPlaintextRemote,
            ] {
                for path in [None, Some(token.as_path())] {
                    let should_allow =
                        policy == ListenAddressPolicy::AllowPlaintextRemote;
                    let result = load_auth_token(address, path, "--listen", policy);
                    if should_allow {
                        assert_eq!(
                            result.unwrap(),
                            path.map(|_| b"fixture-value".to_vec()),
                            "{label}: explicit opt-in must pass pre-bind policy with or without a token"
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
        // The token path ignores the tokenless-browser stance entirely.
        for allow in [false, true] {
            assert!(authorized(&headers, Some(b"secret"), allow));
            assert!(!authorized(&headers, Some(b"wrong"), allow));
        }
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Basic secret"),
        );
        assert!(!authorized(&headers, Some(b"secret"), false));
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer secret"),
        );
        // A browser Origin must not bypass a configured token.
        headers.insert(
            http::header::ORIGIN,
            http::HeaderValue::from_static("https://app.example"),
        );
        assert!(authorized(&headers, Some(b"secret"), false));
        assert!(!authorized(&headers, Some(b"wrong"), true));
    }

    #[test]
    fn authorized_tokenless_strict_transport_rejects_all_browser_origins() {
        // ACP's strict loopback-only transport: `allow_tokenless_browser` is
        // false, so any Origin-bearing (browser) request is rejected — even
        // a same-origin one — while native clients (no Origin) are accepted.
        let mut headers = HeaderMap::new();
        assert!(authorized(&headers, None, false));
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer ignored-without-token-policy"),
        );
        assert!(authorized(&headers, None, false));
        headers.insert(
            http::header::ORIGIN,
            http::HeaderValue::from_static("https://example.test"),
        );
        assert!(!authorized(&headers, None, false));
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::HOST,
            http::HeaderValue::from_static("127.0.0.1:8765"),
        );
        headers.insert(
            http::header::ORIGIN,
            http::HeaderValue::from_static("http://127.0.0.1:8765"),
        );
        assert!(
            !authorized(&headers, None, false),
            "strict transport must reject even a same-origin tokenless browser"
        );
    }

    #[test]
    fn authorized_tokenless_listener_accepts_same_origin_browser_against_host() {
        // Native client: no Origin, always allowed (Host is irrelevant).
        let mut headers = HeaderMap::new();
        assert!(authorized(&headers, None, true));
        // Browser from the same origin as the request's Host: allowed.
        headers.insert(
            http::header::HOST,
            http::HeaderValue::from_static("127.0.0.1:8765"),
        );
        headers.insert(
            http::header::ORIGIN,
            http::HeaderValue::from_static("http://127.0.0.1:8765"),
        );
        assert!(authorized(&headers, None, true));
        // An unrelated cross-origin page (arbitrary Origin): rejected.
        headers.insert(
            http::header::ORIGIN,
            http::HeaderValue::from_static("https://evil.example"),
        );
        assert!(!authorized(&headers, None, true));
        // Malformed, `null`, and duplicate Origins: rejected.
        headers.insert(
            http::header::ORIGIN,
            http::HeaderValue::from_static("null"),
        );
        assert!(!authorized(&headers, None, true));
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::HOST,
            http::HeaderValue::from_static("127.0.0.1:8765"),
        );
        headers.append(http::header::ORIGIN, http::HeaderValue::from_static("http://127.0.0.1:8765"));
        headers.append(http::header::ORIGIN, http::HeaderValue::from_static("http://127.0.0.1:8765"));
        assert!(
            !authorized(&headers, None, true),
            "duplicate Origin headers must be rejected"
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::HOST,
            http::HeaderValue::from_static("127.0.0.1:8765"),
        );
        headers.insert(
            http::header::ORIGIN,
            http::HeaderValue::from_static("http://127.0.0.1:8765/path"),
        );
        assert!(!authorized(&headers, None, true));
    }

    #[test]
    fn authorized_tokenless_same_origin_matches_host_authority_case_insensitively() {
        // Hostname aliases and case: `localhost` and `127.0.0.1` are distinct
        // authorities, but host case never matters.
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::HOST,
            http::HeaderValue::from_static("LOCALHOST:8765"),
        );
        headers.insert(
            http::header::ORIGIN,
            http::HeaderValue::from_static("http://localhost:8765"),
        );
        assert!(authorized(&headers, None, true));
        // Default-port forms match when both sides omit the port.
        let mut headers = HeaderMap::new();
        headers.insert(http::header::HOST, http::HeaderValue::from_static("localhost"));
        headers.insert(
            http::header::ORIGIN,
            http::HeaderValue::from_static("http://localhost"),
        );
        assert!(authorized(&headers, None, true));
        // `localhost` and `127.0.0.1` are different authorities: a page
        // loaded from one cannot be same-origin with a request to the other.
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::HOST,
            http::HeaderValue::from_static("localhost:8765"),
        );
        headers.insert(
            http::header::ORIGIN,
            http::HeaderValue::from_static("http://127.0.0.1:8765"),
        );
        assert!(!authorized(&headers, None, true));
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::HOST,
            http::HeaderValue::from_static("127.0.0.1:8765"),
        );
        headers.insert(
            http::header::ORIGIN,
            http::HeaderValue::from_static("http://localhost:8765"),
        );
        assert!(!authorized(&headers, None, true));
        // A different port is a different authority.
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::HOST,
            http::HeaderValue::from_static("127.0.0.1:8765"),
        );
        headers.insert(
            http::header::ORIGIN,
            http::HeaderValue::from_static("http://127.0.0.1:9000"),
        );
        assert!(!authorized(&headers, None, true));
        // The transport is plaintext HTTP: the Origin scheme must be
        // `http://` even when the authority matches the Host.
        for bad_scheme in ["https://127.0.0.1:8765", "ftp://127.0.0.1:8765"] {
            let mut headers = HeaderMap::new();
            headers.insert(
                http::header::HOST,
                http::HeaderValue::from_static("127.0.0.1:8765"),
            );
            headers.insert(
                http::header::ORIGIN,
                http::HeaderValue::from_str(bad_scheme).expect("header value"),
            );
            assert!(
                !authorized(&headers, None, true),
                "origin {bad_scheme:?} must be rejected on the http listener"
            );
        }
    }

    #[test]
    fn authorized_tokenless_wildcard_listener_accepts_same_origin_browser_without_advertised_origin() {
        // A wildcard bind has no advertised origin, but the request's own
        // Host is the comparison target: any LAN address the browser
        // actually used is accepted, so no --listen-advertised-origin is
        // needed for browser auth.
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::HOST,
            http::HeaderValue::from_static("192.168.1.50:8765"),
        );
        headers.insert(
            http::header::ORIGIN,
            http::HeaderValue::from_static("http://192.168.1.50:8765"),
        );
        assert!(authorized(&headers, None, true));
        // A LAN hostname the user typed works the same way.
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::HOST,
            http::HeaderValue::from_static("mypi.lan:8765"),
        );
        headers.insert(
            http::header::ORIGIN,
            http::HeaderValue::from_static("http://mypi.lan:8765"),
        );
        assert!(authorized(&headers, None, true));
    }

    #[test]
    fn authorized_tokenless_rejects_missing_duplicate_or_malformed_host() {
        // A browser without a Host header has nothing to be same-origin with.
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::ORIGIN,
            http::HeaderValue::from_static("http://127.0.0.1:8765"),
        );
        assert!(!authorized(&headers, None, true));
        // Duplicate Host headers are rejected like duplicate Origins.
        let mut headers = HeaderMap::new();
        headers.append(http::header::HOST, http::HeaderValue::from_static("127.0.0.1:8765"));
        headers.append(http::header::HOST, http::HeaderValue::from_static("127.0.0.1:8765"));
        headers.insert(
            http::header::ORIGIN,
            http::HeaderValue::from_static("http://127.0.0.1:8765"),
        );
        assert!(!authorized(&headers, None, true));
        // Malformed Host values are rejected.
        for bad_host in [
            "127.0.0.1:",
            "127.0.0.1:abc",
            "127.0.0.1:99999",
            "127.0.0.1:0",
            "a:b:c",
            "ho st",
            "user@host",
            "host/path",
            "[::1",
            "[::1]extra",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(
                http::header::HOST,
                http::HeaderValue::from_str(bad_host).expect("header value"),
            );
            headers.insert(
                http::header::ORIGIN,
                http::HeaderValue::from_static("http://127.0.0.1:8765"),
            );
            assert!(
                !authorized(&headers, None, true),
                "host {bad_host:?} must be rejected"
            );
        }
        // A well-formed IPv6 Host matches its bracket-preserving authority.
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::HOST,
            http::HeaderValue::from_static("[2001:db8::1]:8765"),
        );
        headers.insert(
            http::header::ORIGIN,
            http::HeaderValue::from_static("http://[2001:db8::1]:8765"),
        );
        assert!(authorized(&headers, None, true));
    }

    #[test]
    fn normalize_origin_canonicalizes_and_rejects_non_origins() {
        for (input, expected) in [
            ("http://127.0.0.1:8765", "http://127.0.0.1:8765"),
            ("https://App.Example:8443", "https://app.example:8443"),
            ("http://[2001:DB8::1]:8765", "http://[2001:db8::1]:8765"),
            ("http://localhost", "http://localhost"),
        ] {
            assert_eq!(normalize_origin(input).as_deref(), Some(expected), "{input:?}");
        }
        for bad in [
            "null",
            "ftp://example.com",
            "http://",
            "http://host/path",
            "http://user@host",
            "http://host?q=1",
            "http://host#frag",
            "not-an-origin",
            "http://ho st",
        ] {
            assert_eq!(normalize_origin(bad), None, "{bad:?} must not normalize");
        }
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
