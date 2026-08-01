//! Bounded JSON repair for provider SSE chunk payloads.
//!
//! Source-verified composition of upstream helpers used when decoding a single
//! SSE `data:` payload (or similar model/tool JSON blob):
//!
//! - BOM strip (`\u{FEFF}`) — same leading-BOM handling as coding-agent edit
//!   paths and hashline input normalization.
//! - Markdown fenced JSON unwrap (```` ``` ```` / ```` ```json ````) — same
//!   shape as coding-agent `extractJsonObject` / title-generator fence strip.
//! - Surrounding prose isolation when exactly one complete JSON object or
//!   array can be scanned (brace/bracket depth with string awareness) —
//!   generalizes `splitLeadingJsonObject` / `tryParseLeadingJsonContainer`
//!   from pi-ai providers/validation.
//! - Trailing-comma truncation before `}` / `]` outside strings — from
//!   `packages/coding-agent/src/utils/json.ts` `stripJsonComments` (comma
//!   half only; line comments are not accepted).
//! - String-literal escape repair (raw controls, invalid backslash escapes) —
//!   port of `repairJson` / Go `repairJSON` in `ai/providers/json.go` and
//!   `packages/ai/src/utils/json-parse.ts`.
//!
//! This is deliberately **not** a permissive JSON5/RelaxedJson parser: bare
//! keys, single quotes, comments, and partial streaming close-out are out of
//! scope. Ambiguous or still-invalid input fails.

use serde_json::Value;

/// Parse `input` as JSON after applying bounded, source-verified repairs.
///
/// Returns the decoded [`Value`] on success. Valid JSON is accepted unchanged
/// (aside from a leading BOM). When strict parse fails, the helper tries, in
/// order: fence unwrap, isolation of one complete object/array from surrounding
/// prose, trailing-comma strip, and string-escape repair — each step only when
/// it produces a strict `serde_json` success. Inputs that remain ambiguous or
/// malformed after those steps return `None`.
pub fn parse_json_with_repair(input: &str) -> Option<Value> {
    let stripped = strip_bom(input);
    try_parse(stripped).or_else(|| repair_and_parse(stripped))
}

/// Apply the same bounded repairs as [`parse_json_with_repair`] and return the
/// repaired JSON text when it strict-parses. Useful when callers need the
/// canonical text rather than a decoded value.
pub fn repair_json_text(input: &str) -> Option<String> {
    let stripped = strip_bom(input);
    if serde_json::from_str::<Value>(stripped).is_ok() {
        return Some(stripped.to_string());
    }
    for candidate in repair_candidates(stripped) {
        if serde_json::from_str::<Value>(&candidate).is_ok() {
            return Some(candidate);
        }
    }
    None
}

fn try_parse(s: &str) -> Option<Value> {
    serde_json::from_str(s).ok()
}

fn repair_and_parse(s: &str) -> Option<Value> {
    for candidate in repair_candidates(s) {
        if let Ok(v) = serde_json::from_str(&candidate) {
            return Some(v);
        }
    }
    None
}

/// Ordered repair attempts. Each candidate is intended to be a full JSON
/// document; the caller strict-parses and keeps the first success.
fn repair_candidates(s: &str) -> Vec<String> {
    let mut out = Vec::new();

    // 1. Fence unwrap alone (and fence + trailing-comma / string repair).
    if let Some(unfenced) = strip_markdown_fence(s) {
        push_unique(&mut out, unfenced.to_string());
        push_repairs_of(&mut out, unfenced);
        // Isolation inside the fence body (prose wrapping the JSON inside fences).
        if let Some(isolated) = isolate_one_json_value(unfenced) {
            push_unique(&mut out, isolated.clone());
            push_repairs_of(&mut out, &isolated);
        }
    }

    // 2. Isolate one complete object/array from surrounding prose.
    if let Some(isolated) = isolate_one_json_value(s) {
        push_unique(&mut out, isolated.clone());
        push_repairs_of(&mut out, &isolated);
    }

    // 3. Whole-input trailing-comma / string-escape repairs (no isolation).
    push_repairs_of(&mut out, s);

    out
}

fn push_repairs_of(out: &mut Vec<String>, s: &str) {
    let trimmed_comma = strip_trailing_commas(s);
    if trimmed_comma != s {
        push_unique(out, trimmed_comma.clone());
        let repaired = repair_string_escapes(&trimmed_comma);
        if repaired != trimmed_comma {
            push_unique(out, repaired);
        }
    }
    let repaired = repair_string_escapes(s);
    if repaired != s {
        push_unique(out, repaired.clone());
        let both = strip_trailing_commas(&repaired);
        if both != repaired {
            push_unique(out, both);
        }
    }
}

fn push_unique(out: &mut Vec<String>, s: String) {
    if !out.iter().any(|e| e == &s) {
        out.push(s);
    }
}

fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{FEFF}').unwrap_or(s)
}

/// Unwrap a single markdown code fence wrapping the payload.
///
/// Accepts opening fences of the form ```` ``` ```` or ```` ```json ````
/// (language tag optional, case-insensitive `json`), with optional surrounding
/// whitespace. The closing fence must be a line whose first non-space run is
/// ```` ``` ````. Returns the interior when both fences are present; otherwise
/// `None` (unterminated or absent fences are not guessed at).
fn strip_markdown_fence(s: &str) -> Option<&str> {
    // Port of coding-agent `extractJsonObject` fence arm:
    //   /```(?:json)?\s*([\s\S]*?)```/i
    // First non-empty capture wins; unterminated fences are not guessed.
    let mut search_from = 0;
    while let Some(rel) = s[search_from..].find("```") {
        let open = search_from + rel;
        let mut pos = open + 3;
        if pos > s.len() {
            break;
        }
        // Optional `json` language tag (case-insensitive), then \s*.
        let rest = &s[pos..];
        if rest.len() >= 4 && rest[..4].eq_ignore_ascii_case("json") {
            // Only consume when it is exactly the tag (boundary is whitespace or end
            // or we simply consume as upstream's `(?:json)?` does — the next \s*
            // handles separation). Upstream applies `(?:json)?` then `\s*`, so
            // `json` is consumed only as that exact four-letter alternative.
            pos += 4;
        }
        // \s* after optional tag.
        while pos < s.len() && s.as_bytes()[pos].is_ascii_whitespace() {
            pos += 1;
        }
        let body_start = pos;
        let Some(close_rel) = s[body_start..].find("```") else {
            return None;
        };
        let close = body_start + close_rel;
        let interior = s[body_start..close].trim();
        if !interior.is_empty() {
            return Some(interior);
        }
        search_from = close + 3;
    }
    None
}

/// Isolate a single complete JSON object or array from `s` when the remainder
/// is only surrounding whitespace/prose and no second top-level value exists.
///
/// Scans for the first `{` or `[` and walks with string-aware depth tracking
/// (same rules as `splitLeadingJsonObject` / `tryParseLeadingJsonContainer`).
/// Succeeds only when that container closes and no further top-level JSON value
/// follows it.
fn isolate_one_json_value(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{' || b == b'[')?;

    let end = isolate_balanced_end(s, start)?;
    let candidate = &s[start..=end];

    // Reject a second top-level JSON value after the isolated span.
    let suffix = s[end + 1..].trim_start();
    if !suffix.is_empty() {
        if suffix.starts_with('{')
            || suffix.starts_with('[')
            || suffix.starts_with('"')
            || is_json_literal_prefix(suffix)
        {
            return None;
        }
        // Suffix is prose; also reject if the suffix alone is complete JSON.
        if serde_json::from_str::<Value>(suffix).is_ok() {
            return None;
        }
    }

    // Prefix before the container must not itself be a complete JSON value.
    let prefix = s[..start].trim();
    if !prefix.is_empty() && serde_json::from_str::<Value>(prefix).is_ok() {
        return None;
    }

    Some(candidate.to_string())
}

fn is_json_literal_prefix(s: &str) -> bool {
    s.starts_with("true")
        || s.starts_with("false")
        || s.starts_with("null")
        || s.as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_digit() || *b == b'-')
}

/// Byte index of the closing bracket matching the container that opens at
/// `start`, using a stack so nested `{}`/`[]` are handled. Returns `None` if
/// the container never closes, brackets mismatch, or strings are left open.
fn isolate_balanced_end(s: &str, start: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut stack: Vec<u8> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;

    for (i, &ch) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == b'\\' {
                escaped = true;
                continue;
            }
            if ch == b'"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            b'"' => in_string = true,
            b'{' => stack.push(b'}'),
            b'[' => stack.push(b']'),
            b'}' | b']' => {
                let expected = stack.pop()?;
                if ch != expected {
                    return None;
                }
                if stack.is_empty() {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Remove trailing commas before `}` / `]` outside of strings.
///
/// Port of the comma half of coding-agent `stripJsonComments`:
/// `/,(\s*[}\]])/` → `$1` while leaving string contents untouched. Implemented
/// with an explicit scan so string escapes are respected (regex replacement
/// alone is ambiguous under escaped quotes).
fn strip_trailing_commas(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i];
        if in_string {
            out.push(ch as char);
            if escaped {
                escaped = false;
            } else if ch == b'\\' {
                escaped = true;
            } else if ch == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if ch == b'"' {
            in_string = true;
            out.push('"');
            i += 1;
            continue;
        }
        if ch == b',' {
            // Look ahead over whitespace for `}` or `]`.
            let mut j = i + 1;
            while j < bytes.len() && matches!(bytes[j], b' ' | b'\t' | b'\n' | b'\r') {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'}' || bytes[j] == b']') {
                // Drop the comma; keep the whitespace and closer via later iters.
                i += 1;
                continue;
            }
        }
        out.push(ch as char);
        i += 1;
    }
    out
}

/// Escape raw control characters inside strings and double backslashes before
/// invalid escape characters. Port of upstream `repairJson` /
/// `ai/providers/json.go` `repairJSON`.
fn repair_string_escapes(json: &str) -> String {
    let bytes = json.as_bytes();
    let mut repaired = String::with_capacity(json.len() + 8);
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i];
        if !in_string {
            repaired.push(ch as char);
            if ch == b'"' {
                in_string = true;
            }
            i += 1;
            continue;
        }
        if ch == b'"' {
            repaired.push('"');
            in_string = false;
            i += 1;
            continue;
        }
        if ch == b'\\' {
            let next = bytes.get(i + 1).copied();
            match next {
                None => {
                    repaired.push_str("\\\\");
                    i += 1;
                    continue;
                }
                Some(b'u') => {
                    let hex = bytes.get(i + 2..i + 6);
                    if let Some(hex) = hex {
                        if hex.iter().all(|b| b.is_ascii_hexdigit()) {
                            repaired.push('\\');
                            repaired.push('u');
                            for b in hex {
                                repaired.push(*b as char);
                            }
                            i += 6;
                            continue;
                        }
                    }
                    // Invalid \u — double the backslash.
                    repaired.push_str("\\\\");
                    i += 1;
                    continue;
                }
                Some(n) if is_valid_json_escape(n) => {
                    repaired.push('\\');
                    repaired.push(n as char);
                    i += 2;
                    continue;
                }
                Some(_) => {
                    repaired.push_str("\\\\");
                    i += 1;
                    continue;
                }
            }
        }
        if ch <= 0x1f {
            repaired.push_str(&escape_control(ch));
            i += 1;
            continue;
        }
        // Multi-byte UTF-8: copy the full scalar so we don't split chars.
        let ch_str = json[i..].chars().next().unwrap();
        repaired.push(ch_str);
        i += ch_str.len_utf8();
    }
    repaired
}

fn is_valid_json_escape(b: u8) -> bool {
    matches!(
        b,
        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' | b'u'
    )
}

fn escape_control(ch: u8) -> String {
    match ch {
        b'\x08' => "\\b".to_string(),
        b'\x0c' => "\\f".to_string(),
        b'\n' => "\\n".to_string(),
        b'\r' => "\\r".to_string(),
        b'\t' => "\\t".to_string(),
        _ => format!("\\u{ch:04x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_json_unchanged() {
        let cases = [
            r#"{"a":1}"#,
            r#"[1,2,3]"#,
            r#"{"nested":{"x":"y"},"arr":[true,false,null]}"#,
            "  {\"a\": 1}  ",
        ];
        for c in cases {
            let v = parse_json_with_repair(c).expect(c);
            let direct = serde_json::from_str::<Value>(c).unwrap();
            assert_eq!(v, direct, "input={c:?}");
            let text = repair_json_text(c).expect(c);
            assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), direct);
        }
    }

    #[test]
    fn strips_leading_bom() {
        let input = "\u{FEFF}{\"ok\":true}";
        assert_eq!(parse_json_with_repair(input), Some(json!({"ok": true})));
        let text = repair_json_text(input).unwrap();
        assert!(!text.starts_with('\u{FEFF}'));
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap(),
            json!({"ok": true})
        );
    }

    #[test]
    fn unwraps_fenced_json() {
        let input = "```json\n{\"a\": 1}\n```";
        assert_eq!(parse_json_with_repair(input), Some(json!({"a": 1})));

        let input = "```\n[1, 2]\n```";
        assert_eq!(parse_json_with_repair(input), Some(json!([1, 2])));

        let input = "Here you go:\n```JSON\n{\"x\":true}\n```\nthanks";
        assert_eq!(parse_json_with_repair(input), Some(json!({"x": true})));
    }

    #[test]
    fn isolates_object_from_surrounding_prose() {
        let input = "Sure — here is the payload: {\"a\":1,\"b\":[2,3]} hope that helps.";
        assert_eq!(
            parse_json_with_repair(input),
            Some(json!({"a": 1, "b": [2, 3]}))
        );

        let input = "prefix [\"only\"] suffix";
        assert_eq!(parse_json_with_repair(input), Some(json!(["only"])));
    }

    #[test]
    fn strips_trailing_commas() {
        assert_eq!(parse_json_with_repair(r#"{"a":1,}"#), Some(json!({"a": 1})));
        assert_eq!(parse_json_with_repair(r#"[1,2,]"#), Some(json!([1, 2])));
        assert_eq!(
            parse_json_with_repair("{\"a\":1,\n}"),
            Some(json!({"a": 1}))
        );
        // Comma inside a string must not be stripped.
        assert_eq!(
            parse_json_with_repair(r#"{"a":"x,"}"#),
            Some(json!({"a": "x,"}))
        );
    }

    #[test]
    fn repairs_raw_controls_and_invalid_escapes_in_strings() {
        // Raw newline inside a string → \n
        let input = "{\"a\":\"line1\nline2\"}";
        assert_eq!(
            parse_json_with_repair(input),
            Some(json!({"a": "line1\nline2"}))
        );
        // Invalid escape \x → \\x
        let input = r#"{"a":"c:\x"}"#;
        let v = parse_json_with_repair(input).expect("repaired");
        assert_eq!(v, json!({"a": "c:\\x"}));
    }

    #[test]
    fn combined_fence_prose_trailing_comma() {
        let input = "Result:\n```json\n{\"items\":[1,2,],}\n```\n";
        assert_eq!(
            parse_json_with_repair(input),
            Some(json!({"items": [1, 2]}))
        );
    }

    #[test]
    fn ambiguous_or_malformed_fails() {
        assert_eq!(parse_json_with_repair(""), None);
        assert_eq!(parse_json_with_repair("   "), None);
        assert_eq!(parse_json_with_repair("not json at all"), None);
        assert_eq!(parse_json_with_repair("{"), None);
        assert_eq!(parse_json_with_repair("{\"a\":"), None);
        // Unclosed fence with incomplete JSON.
        assert_eq!(parse_json_with_repair("```json\n{\"a\":1\n"), None);
        // Two complete top-level values → ambiguous.
        assert_eq!(parse_json_with_repair(r#"{"a":1}{"b":2}"#), None);
        assert_eq!(parse_json_with_repair("[1][2]"), None);
        // Truncated without a completable trailing-comma form.
        assert_eq!(parse_json_with_repair(r#"{"a":1,"b""#), None);
    }

    #[test]
    fn nested_containers_isolate_correctly() {
        let input = r#"note: {"outer":{"inner":[1,{"k":"v"}]},"z":null} end"#;
        assert_eq!(
            parse_json_with_repair(input),
            Some(json!({"outer":{"inner":[1,{"k":"v"}]},"z":null}))
        );
    }

    #[test]
    fn braces_inside_strings_do_not_break_isolation() {
        let input = r#"say {"a":"use } carefully","b":1} please"#;
        assert_eq!(
            parse_json_with_repair(input),
            Some(json!({"a":"use } carefully","b":1}))
        );
    }
}
