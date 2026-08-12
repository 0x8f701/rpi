//! Secret-pattern redaction shared by tool error paths, store boundaries, and
//! retry/fallback diagnostics.
//!
//! [`redact_secrets`] replaces obvious credential shapes (private-key blocks,
//! `sk-…`, `ghp_…`/`github_pat_…`, `AKIA…`, `Bearer …` tokens,
//! `token=`/`access_token=` values, and the `name=value` credential forms
//! surfaced by retry diagnostics such as `api_key=…`, `password=…`, or
//! `AWS_SECRET_ACCESS_KEY=…`) with `[REDACTED]`; a few patterns keep the
//! variable name and redact only the value so diagnostics stay readable. It
//! is best-effort defense in depth, applied wherever server-controlled or
//! user-supplied text can surface in tool output or persisted state — stderr
//! tails embedded in MCP/LSP initialize-failure errors, GitHub API error
//! hints, memory entries, and aggregated retry errors. It is not a secrets
//! store: never rely on it for real credentials.

use std::sync::LazyLock;

use regex::Regex;

/// Credential-shaped patterns replaced by [`redact_secrets`], in application
/// order: `(pattern, replacement)`. The PEM block comes first so its
/// multi-line body is consumed whole before the narrower token patterns could
/// match inside it; the whole-match shapes come before the name-preserving
/// shapes so replacement text like `[REDACTED]` is never re-matched. This is
/// the single pattern set shared by every redaction path — including the
/// retry/fallback diagnostics that previously compiled their own regexes per
/// call. Compilation happens once at first use and panics on an invalid
/// pattern: a broken pattern set must fail the process, never silently skip
/// redaction.
static SECRET_PATTERNS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        (r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----", "[REDACTED]"),
        (r"\bsk-[A-Za-z0-9_-]{16,}", "[REDACTED]"),
        (r"\bgh[pousr]_[A-Za-z0-9]{20,}", "[REDACTED]"),
        (r"\bgithub_pat_[A-Za-z0-9_]{20,}", "[REDACTED]"),
        (r"\bAKIA[0-9A-Z]{16}", "[REDACTED]"),
        (r"Authorization:\s*Bearer\s+[A-Za-z0-9_.\-]+", "[REDACTED]"),
        (r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{16,}", "[REDACTED]"),
        (r"(?i)(token|access_token)=[A-Za-z0-9_.\-]+", "[REDACTED]"),
        // Retry/fallback diagnostic shapes (previously compiled per call in
        // retry_fallback::redact_retry_diagnostic). These keep the variable
        // name and redact only the value.
        (r#"(?i)\b(AWS_(?:ACCESS_KEY_ID|SECRET_ACCESS_KEY|SESSION_TOKEN))\s*[:=]\s*(?:"[^"]*"|'[^']*'|\S+)"#, "$1=[REDACTED]"),
        (r"(?i)(api[_-]?key|token|secret|password|authorization)\s*[:=]\s*(?:bearer\s+)?\S+", "$1=[REDACTED]"),
        (r"(?i)bearer\s+[a-z0-9._\-]+", "Bearer [REDACTED]"),
        (r"\bAKIA[0-9A-Z]{16}\b", "[REDACTED]"),
        (r"(?i)\b(?:sk|pk|rk|ghp|gho|ghu|ghs|ghr|xox[baprs])[-_][A-Za-z0-9\-_]{8,}\b", "[REDACTED]"),
    ]
    .into_iter()
    .map(|(pattern, replacement)| {
        (
            Regex::new(pattern).expect("secret pattern compiles"),
            replacement,
        )
    })
    .collect()
});

/// Replaces secret-looking patterns in `text` with `[REDACTED]`.
pub fn redact_secrets(input: &str) -> String {
    let mut redacted = input.to_owned();
    for (pattern, replacement) in SECRET_PATTERNS.iter() {
        redacted = pattern.replace_all(&redacted, *replacement).into_owned();
    }
    redacted
}

/// Redacts secrets, then truncates the result to `max_chars` characters (on
/// char boundaries). Redaction runs BEFORE truncation so a credential
/// straddling the truncation boundary is still scrubbed. Never emits a
/// PARTIAL `[REDACTED]` marker: when the cut lands inside a marker, the
/// result backs off to just before the marker so no truncated token
/// survives. Used wherever server-controlled or user-supplied text may
/// surface in an error (e.g. STT endpoint error bodies) so a hostile or
/// verbose body can never project unbounded text into an RPC response.
pub fn redact_bounded(input: &str, max_chars: usize) -> String {
    const MARKER: &str = "[REDACTED]";
    let redacted = redact_secrets(input);
    let chars: Vec<char> = redacted.chars().collect();
    let marker: Vec<char> = MARKER.chars().collect();
    let mut end = max_chars.min(chars.len());
    // A marker straddles the cut when its start is before `end` and its end
    // is beyond it. Redact_secrets only ever emits whole markers, so any
    // overlap is exactly a cut inside a token.
    let cut_inside_marker = |end: usize| {
        (0..end).any(|start| {
            start + marker.len() > end
                && start + marker.len() <= chars.len()
                && chars[start..start + marker.len()] == marker[..]
        })
    };
    while end > 0 && cut_inside_marker(end) {
        end -= 1;
    }
    chars[..end].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_credential_shapes() {
        let sk = ["s", "k-", "abcdefghijklmnop1234"].concat();
        let ghp = ["gh", "p_", "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij"].concat();
        let github_pat = ["github_", "pat_", "ABC_12345678901234567890"].concat();
        let aws_access = ["AK", "IA", "0123456789ABCDEF"].concat();
        let bearer_sk = ["s", "k-", "abcdefghijklmnop1234567890"].concat();
        let private_key_label = ["BEGIN RSA ", "PRIVATE KEY"].concat();
        let private_key_end_label = ["END RSA ", "PRIVATE KEY"].concat();
        let private_key = format!(
            "-----{private_key_label}-----\nZm9vYmFy\n-----{private_key_end_label}-----"
        );
        let text = format!(
            "token=abc123, {sk}, {ghp}, {github_pat}, {aws_access}, \
             Authorization: Bearer xyz789, Bearer {bearer_sk}, access_token=leak_me, \
             {private_key}"
        );
        let redacted = redact_secrets(&text);
        for leaked in [
            "abc123",
            sk.as_str(),
            ghp.as_str(),
            github_pat.as_str(),
            aws_access.as_str(),
            "xyz789",
            bearer_sk.as_str(),
            "leak_me",
            private_key_label.as_str(),
        ] {
            assert!(!redacted.contains(leaked), "{leaked:?} leaked: {redacted}");
        }
        assert!(redacted.contains("[REDACTED]"), "marker missing: {redacted}");
    }

    #[test]
    fn leaves_plain_text_untouched() {
        assert_eq!(
            redact_secrets("plain text, no secrets"),
            "plain text, no secrets"
        );
        assert_eq!(redact_secrets(""), "");
    }

    #[test]
    fn redact_bounded_truncates_after_redaction_on_char_boundary() {
        let sk = ["s", "k-", "abcdefghijklmnop1234"].concat();
        // A long body whose secret straddles the truncation boundary (starts
        // before char 100, ends after): redaction runs first, so the whole
        // match is scrubbed before truncation and no prefix leaks.
        let body = format!("{} {sk} {}", "x".repeat(90), "y".repeat(200));
        let bounded = redact_bounded(&body, 100);
        // Safety contract: never over budget, never a partial marker, never
        // the raw secret. Exact length / marker presence are NOT required —
        // the conservative backoff may drop the whole marker.
        assert!(bounded.chars().count() <= 100, "{bounded:?}");
        let stripped = bounded.replace("[REDACTED]", "");
        assert!(!stripped.contains("[REDACT"), "partial marker: {bounded:?}");
        assert!(!bounded.contains(&sk), "{bounded:?} leaked the key");
        assert!(!bounded.contains("abcdefghijklmnop1234"), "{bounded:?}");

        // Non-ASCII input truncates on char boundaries (never splits a
        // multi-byte char, never panics).
        let multibyte = "λ".repeat(50);
        assert_eq!(redact_bounded(&multibyte, 30).chars().count(), 30);

        // An empty or tiny input stays intact.
        assert_eq!(redact_bounded("", 10), "");
        assert_eq!(redact_bounded("abc", 10), "abc");
    }

    #[test]
    fn redact_bounded_never_leaves_partial_marker() {
        // Cut at EVERY position around a marker: the result must never be
        // over budget and never carry a PARTIAL marker like `[REDACT` or
        // `[R` — the whole token is either kept or dropped entirely.
        for cut in 0..=30 {
            let body = format!("{}[REDACTED]{}", "x".repeat(10), "y".repeat(30));
            let bounded = redact_bounded(&body, cut);
            assert!(
                bounded.chars().count() <= cut,
                "over budget at cut {cut}: {bounded}"
            );
            let stripped = bounded.replace("[REDACTED]", "");
            assert!(!stripped.contains("[REDACT"), "partial marker at cut {cut}: {bounded}");
        }
    }

    #[test]
    fn redact_bounded_keeps_literal_brackets() {
        // A literal "[" that is not part of a marker must survive truncation
        // when the cut lands before it (backoff only triggers on a real
        // marker straddling the cut).
        let body = format!("{}[", "x".repeat(50));
        assert_eq!(redact_bounded(&body, 20), "x".repeat(20));
        assert_eq!(redact_bounded(&body, 51), body);
        assert_eq!(redact_bounded(&body, 100), body);
        // A marker fully before the cut is kept whole; a cut inside or after
        // it never yields a partial token. `[REDACTED]` is 10 chars, so the
        // marker at chars 5..15 fits exactly at cut >= 15.
        let body = format!("{}[REDACTED]{}", "a".repeat(5), "b".repeat(40));
        assert_eq!(redact_bounded(&body, 15), "aaaaa[REDACTED]");
        for cut in 16..=40 {
            let bounded = redact_bounded(&body, cut);
            assert!(bounded.chars().count() <= cut, "{bounded}");
            let stripped = bounded.replace("[REDACTED]", "");
            assert!(!stripped.contains("[REDACT"), "partial marker at cut {cut}: {bounded}");
        }
    }
}
