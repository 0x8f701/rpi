//! Unit coverage for the `@`-file completion surface
//! (`crates/pi-cli/src/file_search.rs` — 0% coverage before this file).
//!
//! `current_at_prefix` is the pure parser the TUI/REPL use to find the live
//! `@path` token on an editor line (quoted/escaped paths, cursor bounds, and
//! token boundaries); `search` walks the working directory without following
//! symlinks, honors hidden-file rules, and bounds/quotes results. Both are
//! driven through their public API only.

use std::fs;

use pi_cli::file_search::{AtPrefix, current_at_prefix, search};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

fn prefix(line: &str, cursor: usize) -> Option<AtPrefix> {
    current_at_prefix(line, cursor)
}

#[test]
fn plain_at_token_at_line_start() {
    let found = prefix("@src/main.rs", 12).expect("prefix");
    assert_eq!(found.start, 0);
    assert_eq!(found.end, 12);
    assert_eq!(found.query, "src/main.rs");
}

#[test]
fn at_token_after_whitespace_or_punctuation() {
    // Whitespace boundary: the cursor sits at the end of the @ token, and the
    // trailing text belongs to the next token (no whitespace inside the query).
    let line = "look at @docs/readme.md now";
    let token_end = line.find('@').expect("at") + "@docs/readme.md".len();
    let found = prefix(line, token_end).expect("prefix");
    assert_eq!(found.query, "docs/readme.md");
    // A cursor past the token (inside the trailing word) sees no prefix.
    assert!(prefix(line, line.len()).is_none());
    // Punctuation boundaries: ( [ { = , :
    for boundary in ["(", "[", "{", "=", ",", ":"] {
        let line = format!("value {boundary}@path/to.rs tail");
        let at = line.find('@').expect("at");
        let found = prefix(&line, at + "@path/to.rs".len()).expect("punctuation boundary");
        assert_eq!(found.query, "path/to.rs", "boundary {boundary:?}");
    }
}

#[test]
fn at_mid_word_is_not_a_token() {
    // `@` glued to a word character is not a completion trigger.
    assert!(prefix("abc@def", 7).is_none());
    assert!(prefix("user@host", 9).is_none());
    // But at the very start of the line it is, even with no boundary before.
    assert!(prefix("@", 1).is_some());
}

#[test]
fn cursor_position_selects_the_query_text() {
    let line = "@src/main.rs";
    // Cursor before the `@` sees no prefix.
    assert!(prefix(line, 0).is_none());
    // Cursor inside the query truncates it.
    let mid = prefix(line, 6).expect("mid cursor");
    assert_eq!(mid.query, "src/m");
    assert_eq!(mid.end, 6);
    // Cursor beyond the end is clamped to the token end.
    let clamped = prefix(line, 100).expect("clamped");
    assert_eq!(clamped.query, "src/main.rs");
}

#[test]
fn quoted_at_token_unescapes_and_keeps_spaces() {
    let line = r#"open @"my docs/file name.txt" now"#;
    // The cursor sits ON the closing quote: `before` excludes it, so the
    // quoted token is complete and unescapes to the spaced path.
    let closing = line.rfind('"').expect("closing quote");
    let found = prefix(line, closing).expect("quoted prefix");
    assert_eq!(found.query, "my docs/file name.txt");
    assert_eq!(found.start, line.find('@').expect("at"));
    // A cursor past the closing quote terminates the token (no prefix).
    assert!(prefix(line, closing + 1).is_none());
    // Escaped quote inside the quoted path: the token is unterminated at the
    // input end, so the full line is the query and `\"` unescapes to `"`.
    let escaped = r#"@"a \"b\""#;
    let found = prefix(escaped, escaped.len()).expect("escaped prefix");
    assert_eq!(found.query, "a \"b\"");
}

#[test]
fn unquoted_token_with_whitespace_or_quotes_is_rejected() {
    // Unquoted whitespace means the token ended.
    assert!(prefix("@two words", 10).is_none());
    // A bare quote inside the token is rejected (must use @"..." form).
    assert!(prefix("@two\"words", 11).is_none());
}

#[test]
fn absolute_and_parent_queries_are_rejected() {
    assert!(prefix("@/etc/passwd", 11).is_none());
    assert!(prefix("@../escape", 10).is_none());
    assert!(prefix("..@../escape", 12).is_none());
}

#[tokio::test]
async fn search_lists_files_directories_and_quotes_spaces() {
    let dir = TempDir::new().expect("cwd");
    fs::create_dir_all(dir.path().join("src/nested")).expect("dirs");
    fs::write(dir.path().join("src/main.rs"), "fn main() {}").expect("file");
    fs::write(dir.path().join("src/lib.rs"), "").expect("file");
    fs::write(dir.path().join("README.md"), "").expect("file");
    fs::write(dir.path().join("my file.txt"), "").expect("file with space");
    fs::create_dir_all(dir.path().join(".hidden")).expect("hidden dir");

    let matches = search(
        dir.path().to_path_buf(),
        "".to_owned(),
        50,
        CancellationToken::new(),
    )
    .await
    .expect("search");

    let labels: Vec<&str> = matches.iter().map(|m| m.label.as_str()).collect();
    assert!(
        labels.iter().any(|label| *label == "@src/"),
        "directory match carries a trailing slash: {labels:?}"
    );
    assert!(
        labels.iter().any(|label| *label == "@README.md"),
        "root file: {labels:?}"
    );
    // An empty query lists only top-level entries (nested files need a query).
    assert!(
        !labels.iter().any(|label| *label == "@src/main.rs"),
        "empty query is root-scoped: {labels:?}"
    );
    // A `src` query reaches the nested files.
    let nested = search(
        dir.path().to_path_buf(),
        "src".to_owned(),
        50,
        CancellationToken::new(),
    )
    .await
    .expect("src search");
    let nested_labels: Vec<&str> = nested.iter().map(|m| m.label.as_str()).collect();
    assert!(
        nested_labels.iter().any(|label| *label == "@src/main.rs")
            && nested_labels.iter().any(|label| *label == "@src/lib.rs"),
        "query reaches nested files: {nested_labels:?}"
    );
    // Spaces force the quoted form.
    let spaced = matches
        .iter()
        .find(|m| m.label.contains("my file"))
        .expect("spaced file listed");
    assert_eq!(spaced.value, "@\"my file.txt\"");
    // Hidden entries are excluded for an empty query.
    assert!(
        !labels.iter().any(|label| label.contains(".hidden")),
        "hidden entries hidden: {labels:?}"
    );
}

#[tokio::test]
async fn search_respects_query_prefix_hidden_rule_and_limit() {
    let dir = TempDir::new().expect("cwd");
    fs::write(dir.path().join("alpha.rs"), "").expect("alpha");
    fs::write(dir.path().join("beta.rs"), "").expect("beta");
    fs::write(dir.path().join(".env"), "").expect("dotenv");

    // Query filters to matching basenames.
    let matches = search(
        dir.path().to_path_buf(),
        "alp".to_owned(),
        50,
        CancellationToken::new(),
    )
    .await
    .expect("search");
    assert_eq!(matches.len(), 1, "{matches:?}");
    assert!(matches[0].label.ends_with("alpha.rs"));

    // A dot-leading query exposes hidden entries.
    let matches = search(
        dir.path().to_path_buf(),
        ".e".to_owned(),
        50,
        CancellationToken::new(),
    )
    .await
    .expect("hidden search");
    assert!(
        matches.iter().any(|m| m.label.contains(".env")),
        "dot query sees hidden files: {matches:?}"
    );

    // The limit bounds the result set.
    let matches = search(
        dir.path().to_path_buf(),
        "".to_owned(),
        1,
        CancellationToken::new(),
    )
    .await
    .expect("limited search");
    assert_eq!(matches.len(), 1, "limit enforced: {matches:?}");
}

#[tokio::test]
async fn search_rejects_escapes_and_honors_cancellation() {
    let dir = TempDir::new().expect("cwd");
    fs::write(dir.path().join("file.txt"), "").expect("file");

    // Parent traversal is refused.
    let err = search(
        dir.path().to_path_buf(),
        "../up".to_owned(),
        50,
        CancellationToken::new(),
    )
    .await
    .expect_err("parent query must fail");
    assert!(err.to_string().contains("within the working directory"), "{err}");

    // Absolute queries are refused.
    let err = search(
        dir.path().to_path_buf(),
        "/etc".to_owned(),
        50,
        CancellationToken::new(),
    )
    .await
    .expect_err("absolute query must fail");
    assert!(err.to_string().contains("within the working directory"), "{err}");

    // Pre-cancelled search returns an empty result set.
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let matches = search(
        dir.path().to_path_buf(),
        "".to_owned(),
        50,
        cancelled,
    )
    .await
    .expect("cancelled search");
    assert!(matches.is_empty());
}

#[tokio::test]
async fn search_does_not_follow_symlinks() {
    let dir = TempDir::new().expect("cwd");
    let outside = TempDir::new().expect("outside");
    fs::write(outside.path().join("secret.txt"), "s3cr3t").expect("secret");
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).expect("symlink");

    let matches = search(
        dir.path().to_path_buf(),
        "".to_owned(),
        50,
        CancellationToken::new(),
    )
    .await
    .expect("search");
    assert!(
        !matches.iter().any(|m| m.label.contains("secret.txt")),
        "symlink target must not be walked: {matches:?}"
    );
    assert!(
        !matches.iter().any(|m| m.label.contains("link")),
        "symlink entries themselves are skipped: {matches:?}"
    );
}
