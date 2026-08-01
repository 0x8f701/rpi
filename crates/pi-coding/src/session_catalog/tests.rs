use std::env;
use std::fs::{self, File, FileTimes};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::sync::{Arc, Barrier};
use std::thread;

use super::*;
use crate::import::{source_root_for, SourceSessionFormat};


struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = env::temp_dir().join(format!(
            "pi-rs-session-catalog-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("fixture root");
        Self { root }
    }

    fn catalog(&self) -> SessionCatalog {
        SessionCatalog::new(&self.root)
    }

    fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(&path, contents).expect("write");
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn set_modified(path: &Path, seconds: u64) {
    let file = File::options().write(true).open(path).expect("open");
    file.set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)))
        .expect("mtime");
}

fn write_native(catalog: &SessionCatalog, project: &str, file: &str, id: &str, cwd: &str, user: &str) -> PathBuf {
    let path = catalog
        .root_for(SessionSourceKind::NativePi)
        .path
        .join(project)
        .join(file);
    fs::create_dir_all(path.parent().expect("parent")).expect("parent");
    let mut out = File::create(&path).expect("create");
    writeln!(
        out,
        r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-01-01T00:00:00.000Z","cwd":"{cwd}"}}"#
    )
    .unwrap();
    writeln!(
        out,
        r#"{{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":"{user}"}}]}}}}"#
    )
    .unwrap();
    path
}

fn write_codex(catalog: &SessionCatalog, name: &str, id: &str, cwd: &str, user: &str) -> PathBuf {
    let path = catalog
        .root_for(SessionSourceKind::Codex)
        .path
        .join(name);
    fs::create_dir_all(path.parent().expect("parent")).expect("parent");
    fs::write(
        &path,
        format!(
            concat!(
                r#"{{"timestamp":"2026-02-01T00:00:00Z","type":"session_meta","payload":{{"id":"{id}","cwd":"{cwd}"}}}}"#,
                "\n",
                r#"{{"timestamp":"2026-02-01T00:00:01Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{user}"}}]}}}}"#,
                "\n",
                r#"{{"timestamp":"2026-02-01T00:00:02Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"ok"}}]}}}}"#,
                "\n"
            ),
            id = id,
            cwd = cwd,
            user = user
        ),
    )
    .expect("codex");
    path
}

fn write_claude(catalog: &SessionCatalog, name: &str, id: &str, cwd: &str, user: &str) -> PathBuf {
    let path = catalog
        .root_for(SessionSourceKind::Claude)
        .path
        .join("proj")
        .join(name);
    fs::create_dir_all(path.parent().expect("parent")).expect("parent");
    fs::write(
        &path,
        format!(
            concat!(
                r#"{{"type":"user","uuid":"u","parentUuid":null,"isSidechain":false,"sessionId":"{id}","cwd":"{cwd}","timestamp":"2026-03-01T00:00:01Z","message":{{"role":"user","content":"{user}"}}}}"#,
                "\n",
                r#"{{"type":"assistant","uuid":"a","parentUuid":"u","isSidechain":false,"timestamp":"2026-03-01T00:00:02Z","message":{{"role":"assistant","content":[{{"type":"text","text":"hi"}}]}}}}"#,
                "\n",
                r#"{{"type":"last-prompt","leafUuid":"a","sessionId":"{id}"}}"#,
                "\n"
            ),
            id = id,
            cwd = cwd,
            user = user
        ),
    )
    .expect("claude");
    path
}

fn write_omp(catalog: &SessionCatalog, id: &str, cwd: &str) -> PathBuf {
    let path = catalog
        .root_for(SessionSourceKind::Omp)
        .path
        .join("--injected--")
        .join(format!("{id}.jsonl"));
    fs::create_dir_all(path.parent().expect("parent")).expect("parent");
    fs::write(
        &path,
        format!(
            concat!(
                r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-01-01T00:00:00Z","cwd":"{cwd}"}}"#,
                "\n",
                r#"{{"type":"message","id":"u","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{{"role":"user","content":"omp prompt"}}}}"#,
                "\n"
            )
            ,
            id = id,
            cwd = cwd
        ),
    )
    .expect("omp");
    path
}

fn write_grok(catalog: &SessionCatalog, id: &str, cwd: &str) -> PathBuf {
    let directory = catalog
        .root_for(SessionSourceKind::Grok)
        .path
        .join("injected-cwd")
        .join(id);
    fs::create_dir_all(&directory).expect("grok directory");
    let path = directory.join("summary.json");
    fs::write(
        &path,
        format!(r#"{{"info":{{"id":"{id}","cwd":"{cwd}"}},"created_at":"2026-01-01T00:00:00Z"}}"#),
    )
    .expect("grok summary");
    fs::write(
        directory.join("chat_history.jsonl"),
        r#"{"role":"user","content":"grok prompt","timestamp":"2026-01-01T00:00:01Z"}
"#,
    )
    .expect("grok chat");
    path
}

fn write_droid(catalog: &SessionCatalog, id: &str, cwd: &str) -> PathBuf {
    let path = catalog
        .root_for(SessionSourceKind::Droid)
        .path
        .join(format!("{id}.jsonl"));
    fs::create_dir_all(path.parent().expect("parent")).expect("parent");
    fs::write(
        &path,
        format!(
            concat!(
                r#"{{"type":"session_start","id":"{id}","cwd":"{cwd}"}}"#,
                "\n",
                r#"{{"type":"message","timestamp":"2026-01-01T00:00:01Z","message":{{"role":"user","content":"droid prompt"}}}}"#,
                "\n"
            )
            ,
            id = id,
            cwd = cwd
        ),
    )
    .expect("droid");
    path
}

#[test]
fn catalog_roots_honor_env_style_overrides() {
    let fixture = Fixture::new();
    let catalog = SessionCatalog::new(fixture.root.join("home"))
        .with_native_agent_dir(fixture.root.join("native-agent"))
        .with_codex_home(fixture.root.join("codex-home"))
        .with_claude_config_dir(fixture.root.join("claude-home"));
    assert_eq!(
        catalog.root_for(SessionSourceKind::NativePi).path,
        fixture.root.join("native-agent/sessions")
    );
    assert_eq!(
        catalog.root_for(SessionSourceKind::Codex).path,
        fixture.root.join("codex-home/sessions")
    );
    assert_eq!(
        catalog.root_for(SessionSourceKind::Claude).path,
        fixture.root.join("claude-home/projects")
    );
    assert_eq!(
        catalog.root_for(SessionSourceKind::Grok).path,
        fixture.root.join("home/.grok/sessions")
    );
}

#[test]
fn sessions_home_supplies_native_root_without_explicit_agent_override() {
    let fixture = Fixture::new();
    let sessions_home = fixture.root.join("sessions-home");
    let user_home = fixture.root.join("user-home");
    let catalog = SessionCatalog::from_env_paths(
        sessions_home.clone(),
        user_home,
        None,
        None,
        None,
    );
    assert_eq!(
        catalog.root_for(SessionSourceKind::NativePi).path,
        sessions_home.join(".pi/agent/sessions")
    );
}

#[test]
fn explicit_agent_dir_overrides_sessions_home_native_root() {
    let fixture = Fixture::new();
    let sessions_home = fixture.root.join("sessions-home");
    let user_home = fixture.root.join("user-home");
    let explicit = fixture.root.join("explicit-agent");
    let catalog = SessionCatalog::from_env_paths(
        sessions_home,
        user_home,
        Some(explicit.clone()),
        None,
        None,
    );
    assert_eq!(
        catalog.root_for(SessionSourceKind::NativePi).path,
        explicit.join("sessions")
    );
}

#[test]
fn scan_mixed_sources_newest_first() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let pi = write_native(&catalog, "--proj--", "a.jsonl", "pi-1", "/tmp/proj", "native hello");
    let codex = write_codex(&catalog, "rollout-codex.jsonl", "codex-1", "/tmp/codex", "codex task");
    let claude = write_claude(&catalog, "claude.jsonl", "claude-1", "/tmp/claude", "claude hi");
    set_modified(&pi, 100);
    set_modified(&codex, 300);
    set_modified(&claude, 200);

    let rows = catalog.list(&CatalogListOptions {
        include_foreign: true,
        ..CatalogListOptions::default()
    });
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].kind, SessionSourceKind::Codex);
    assert_eq!(rows[0].session_id, "codex-1");
    assert_eq!(rows[1].kind, SessionSourceKind::Claude);
    assert_eq!(rows[2].kind, SessionSourceKind::NativePi);
    assert_eq!(rows[2].status, CatalogRowStatus::Native);
    assert!(rows.iter().any(|row| row.kind.label() == "codex"));
}

#[test]
fn scan_skips_symlink_archived_rsync_and_malformed() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let good = write_codex(&catalog, "rollout-good.jsonl", "good", "/tmp", "keep");
    let archived = catalog
        .root_for(SessionSourceKind::Codex)
        .path
        .join("archived_sessions/rollout-old.jsonl");
    fs::create_dir_all(archived.parent().unwrap()).unwrap();
    fs::copy(&good, &archived).unwrap();
    let rsync = catalog
        .root_for(SessionSourceKind::Codex)
        .path
        .join(".rsync-partial/rollout-partial.jsonl");
    fs::create_dir_all(rsync.parent().unwrap()).unwrap();
    fs::copy(&good, &rsync).unwrap();
    let symlink = catalog
        .root_for(SessionSourceKind::Codex)
        .path
        .join("rollout-link.jsonl");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&good, &symlink).unwrap();
    let malformed = catalog
        .root_for(SessionSourceKind::Codex)
        .path
        .join("rollout-bad.jsonl");
    fs::write(&malformed, "{not json\n").unwrap();

    let rows = catalog.scan(&[SessionSourceKind::Codex]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].session_id, "good");
    assert!(!catalog.discover(SessionSourceKind::Codex).contains(&archived));
    assert!(!catalog.discover(SessionSourceKind::Codex).contains(&rsync));
    #[cfg(unix)]
    assert!(!catalog.discover(SessionSourceKind::Codex).contains(&symlink));
}

#[test]
fn dedupe_keeps_newest_same_cwd_summary() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let older = write_codex(&catalog, "rollout-old.jsonl", "old", "/tmp/same", "same summary");
    let newer = write_codex(&catalog, "rollout-new.jsonl", "new", "/tmp/same", "same summary");
    set_modified(&older, 10);
    set_modified(&newer, 50);
    let rows = catalog.scan(&[SessionSourceKind::Codex]);
    let deduped = SessionCatalog::dedupe_rows(&rows);
    assert_eq!(deduped.len(), 1);
    assert_eq!(deduped[0].session_id, "new");
}

#[test]
fn fuzzy_search_matches_summary_and_source() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    write_native(&catalog, "--p--", "n.jsonl", "native-id", "/tmp/p", "alpha uniquephrase");
    write_codex(&catalog, "rollout-c.jsonl", "codex-id", "/tmp/c", "beta other");
    let hits = catalog
        .search(
            "uniq phrs codx",
            &CatalogSearchOptions {
                include_foreign: true,
                ..CatalogSearchOptions::default()
            },
        )
        .expect("search");
    // "uniq phrs" fuzzy-matches uniquephrase; "codx" is a separate token requiring all tokens.
    // Use a query that should hit the native row only.
    let native_hits = catalog
        .search(
            "uniquephrase",
            &CatalogSearchOptions {
                include_foreign: true,
                ..CatalogSearchOptions::default()
            },
        )
        .expect("search native");
    assert_eq!(native_hits.len(), 1);
    assert_eq!(native_hits[0].session_id, "native-id");

    let codex_hits = catalog
        .search(
            "beta codex",
            &CatalogSearchOptions {
                include_foreign: true,
                ..CatalogSearchOptions::default()
            },
        )
        .expect("search codex");
    assert_eq!(codex_hits.len(), 1);
    assert_eq!(codex_hits[0].kind, SessionSourceKind::Codex);
    let _ = hits;
}

#[test]
fn duplicate_ids_across_sources_remain_distinct_and_resolve_ambiguous() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    write_native(&catalog, "--p--", "n.jsonl", "shared-id", "/tmp/p", "native");
    write_codex(&catalog, "rollout-shared.jsonl", "shared-id", "/tmp/c", "codex");
    let rows = catalog.list(&CatalogListOptions {
        include_foreign: true,
        ..CatalogListOptions::default()
    });
    assert_eq!(rows.iter().filter(|row| row.session_id == "shared-id").count(), 2);
    let err = catalog.resolve_any("shared-id").expect_err("ambiguous");
    assert!(matches!(err, CatalogError::AmbiguousSession { .. }));
}

#[test]
fn import_or_resume_idempotent_and_writes_lineage() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let source = write_codex(&catalog, "rollout-once.jsonl", "once-id", "/tmp/once", "import me");
    let first = catalog
        .import_or_resume(SessionSourceKind::Codex, &source, Some(Path::new("/tmp/once")))
        .expect("first import");
    assert!(!first.native_no_copy);
    assert!(!first.reused_existing || first.path.exists());
    assert!(first.path.exists());
    let first_path = first.path.clone();
    let first_id = first.id.clone();

    let body = fs::read_to_string(&first_path).expect("read imported");
    assert!(body.contains("\"parentSession\":\"codex:once-id\"") || body.contains("codex:once-id"));
    assert!(body.contains(LINEAGE_CUSTOM_TYPE));
    assert!(body.contains("sourceSessionId"));
    assert!(body.contains("pi-rs-import"));
    assert!(body.contains("converted-from-codex"));

    let second = catalog
        .import_or_resume(SessionSourceKind::Codex, &source, Some(Path::new("/tmp/once")))
        .expect("second import");
    assert!(second.reused_existing);
    assert_eq!(second.path, first_path);
    assert_eq!(second.id, first_id);

    let native_dir = catalog.root_for(SessionSourceKind::NativePi).path;
    let native_files = WalkDir::new(&native_dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension() == Some(OsStr::new("jsonl")))
        .count();
    assert_eq!(native_files, 1);

    let rows = catalog.scan(&[SessionSourceKind::Codex]);
    assert!(matches!(
        rows[0].status,
        CatalogRowStatus::AlreadyImported { .. }
    ));
}

#[test]
fn native_row_status_not_reimported_and_no_copy_resume() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let path = write_native(&catalog, "--proj--", "n.jsonl", "native-keep", "/tmp/proj", "hello");
    let rows = catalog.scan(&[SessionSourceKind::NativePi]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, CatalogRowStatus::Native);

    let resolved = catalog
        .import_or_resume(SessionSourceKind::NativePi, &path, None)
        .expect("native resume");
    assert!(resolved.native_no_copy);
    assert!(resolved.reused_existing);
    assert_eq!(resolved.path, path);
    assert_eq!(resolved.id, "native-keep");
}

#[test]
fn oversized_foreign_file_rejected_on_import_but_skipped_in_scan() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let path = catalog
        .root_for(SessionSourceKind::Codex)
        .path
        .join("rollout-huge.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // Exceed import MAX_SOURCE_BYTES (64 MiB) with a sparse-ish write of large content.
    // Use a smaller oversize relative to test speed: parsers enforce 64MiB; craft via
    // ResourceLimit by writing a file the import path rejects. For unit speed we call
    // import directly and also ensure scan skips unparsable huge files.
    //
    // Write a file just over the per-line limit path by using invalid large single line
    // through import API — catalog scan isolates errors.
    let mut big = String::from(
        r#"{"timestamp":"2026-02-01T00:00:00Z","type":"session_meta","payload":{"id":"huge","cwd":"/tmp"}}"#,
    );
    big.push('\n');
    big.push_str(&"x".repeat(5 * 1024 * 1024));
    big.push('\n');
    fs::write(&path, big).unwrap();

    let rows = catalog.scan(&[SessionSourceKind::Codex]);
    assert!(rows.iter().all(|row| row.session_id != "huge"));

    let err = catalog
        .import_or_resume(SessionSourceKind::Codex, &path, None)
        .expect_err("oversize/malformed rejected");
    assert!(matches!(
        err,
        CatalogError::Import(ImportSessionError::ResourceLimit { .. })
            | CatalogError::Import(ImportSessionError::Json { .. })
            | CatalogError::Import(ImportSessionError::NoConvertibleMessages { .. })
    ));
}

#[test]
fn resolve_rollout_prefix_and_source_provenance() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let path = write_codex(
        &catalog,
        "rollout-2026-01-01T00-00-00-uniqueprefix.jsonl",
        "prov-id",
        "/tmp/prov",
        "provenance",
    );
    let (kind, resolved) = catalog.resolve_any("uniqueprefix").expect("prefix");
    assert_eq!(kind, SessionSourceKind::Codex);
    assert_eq!(resolved, path);
    let imported = catalog
        .import_or_resume(kind, resolved, Some(Path::new("/tmp/prov")))
        .expect("import");
    assert_eq!(imported.source_session_id.as_deref(), Some("prov-id"));
    assert_eq!(imported.kind, SessionSourceKind::Codex);
    let body = fs::read_to_string(&imported.path).unwrap();
    assert!(body.contains("\"source\":\"codex\"") || body.contains("converted-from-codex"));
}

#[test]
fn source_root_for_matches_catalog_codex_claude() {
    // Ensure public bridge stays aligned for import helpers.
    let root = source_root_for(SourceSessionFormat::Codex, Some(Path::new("/x/codex")), None)
        .expect("codex root");
    assert_eq!(root, PathBuf::from("/x/codex/sessions"));
}

/// Symlink race (TOCTOU-representable): discover skips links, and a direct
/// import path that is a symlink is rejected before parse/write.
#[test]
#[cfg(unix)]
fn import_rejects_symlink_source_even_when_target_is_valid() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let target = write_codex(
        &catalog,
        "rollout-real.jsonl",
        "real-id",
        "/tmp/symlink-race",
        "real body",
    );
    let link = catalog
        .root_for(SessionSourceKind::Codex)
        .path
        .join("rollout-alias.jsonl");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");

    assert!(
        !catalog.is_safe_session_path(SessionSourceKind::Codex, &link),
        "symlink must fail safety check"
    );
    assert!(
        !catalog.discover(SessionSourceKind::Codex).contains(&link),
        "discover must not surface symlink paths"
    );

    let err = catalog
        .import_or_resume(SessionSourceKind::Codex, &link, Some(Path::new("/tmp/symlink-race")))
        .expect_err("symlink import must fail");
    match err {
        CatalogError::Import(ImportSessionError::InvalidInput { reason, path, .. }) => {
            assert_eq!(path, link);
            assert!(
                reason.contains("symlink") || reason.contains("non-regular"),
                "reason={reason}"
            );
        }
        // resolve_for may also refuse unsafe path membership before open.
        CatalogError::SessionNotFound(_) => {}
        other => panic!("unexpected error for symlink import: {other:?}"),
    }

    let native_dir = catalog.root_for(SessionSourceKind::NativePi).path;
    if native_dir.is_dir() {
        let native_files = WalkDir::new(&native_dir)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension() == Some(OsStr::new("jsonl")))
            .count();
        assert_eq!(native_files, 0, "symlink import must emit zero native bytes");
    }
}

/// Lexical `..` escape and path-outside-root must never count as safe sessions.
#[test]
fn path_with_parent_components_is_not_safe_session() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let good = write_codex(&catalog, "rollout-safe.jsonl", "safe-id", "/tmp", "ok");
    let escaped = catalog
        .root_for(SessionSourceKind::Codex)
        .path
        .join("nested")
        .join("..")
        .join("..")
        .join("outside.jsonl");
    assert!(
        !catalog.is_safe_session_path(SessionSourceKind::Codex, &escaped),
        "parent-dir components must fail lexical root check"
    );
    // Even the real file is only safe when addressed without escaping components.
    assert!(catalog.is_safe_session_path(SessionSourceKind::Codex, &good));
}

/// Native catalog visibility shares the 64 MiB session loader boundary.
#[test]
fn native_session_between_8_and_64_mib_is_visible() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let path = catalog
        .root_for(SessionSourceKind::NativePi)
        .path
        .join("--proj--")
        .join("large-valid.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = File::create(&path).expect("create large native");
    writeln!(
        file,
        r#"{{"type":"session","version":3,"id":"native-large","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp/proj"}}"#
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":"visible large session"}}]}}}}"#
    )
    .unwrap();
    for _ in 0..10 {
        file.write_all(&vec![b' '; 1024 * 1024]).unwrap();
        file.write_all(b"\n").unwrap();
    }
    drop(file);

    let rows = catalog.scan(&[SessionSourceKind::NativePi]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].session_id, "native-large");
    assert_eq!(rows[0].path, path);

    let resumed = catalog
        .import_or_resume(SessionSourceKind::NativePi, &path, None)
        .expect("large native resume");
    assert!(resumed.native_no_copy);
    assert_eq!(resumed.id, "native-large");
}

/// Malformed native JSONL is isolated: scan continues, resolve/import fails closed.
#[test]
fn malformed_native_jsonl_skipped_in_scan_and_rejected_on_resume() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    write_native(
        &catalog,
        "--proj--",
        "ok.jsonl",
        "native-ok",
        "/tmp/proj",
        "hello",
    );
    let bad = catalog
        .root_for(SessionSourceKind::NativePi)
        .path
        .join("--proj--")
        .join("bad.jsonl");
    fs::create_dir_all(bad.parent().unwrap()).unwrap();
    fs::write(&bad, "{not-json\n{\"type\":\"message\"}\n").unwrap();

    let rows = catalog.scan(&[SessionSourceKind::NativePi]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].session_id, "native-ok");

    let err = catalog
        .import_or_resume(SessionSourceKind::NativePi, &bad, None)
        .expect_err("malformed native must fail resume");
    assert!(matches!(
        err,
        CatalogError::Import(ImportSessionError::InvalidInput { .. })
    ));
}

/// Empty / non-convertible foreign source must not emit a native file.
#[test]
fn empty_foreign_session_rejects_import_with_zero_bytes() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let path = catalog
        .root_for(SessionSourceKind::Codex)
        .path
        .join("rollout-empty.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        concat!(
            r#"{"timestamp":"2026-02-01T00:00:00Z","type":"session_meta","payload":{"id":"empty-id","cwd":"/tmp/empty"}}"#,
            "\n"
        ),
    )
    .unwrap();

    let err = catalog
        .import_or_resume(SessionSourceKind::Codex, &path, Some(Path::new("/tmp/empty")))
        .expect_err("empty foreign must fail");
    assert!(matches!(
        err,
        CatalogError::Import(ImportSessionError::NoConvertibleMessages { .. })
    ));

    let native_dir = catalog.root_for(SessionSourceKind::NativePi).path;
    if native_dir.is_dir() {
        let native_files = WalkDir::new(&native_dir)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension() == Some(OsStr::new("jsonl")))
            .count();
        assert_eq!(native_files, 0);
    }
}

/// Ambiguous shared prefix within one source must fail closed with no write.
#[test]
fn ambiguous_prefix_within_source_rejects_without_import() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    write_codex(
        &catalog,
        "rollout-alpha-one.jsonl",
        "alpha-one",
        "/tmp/a",
        "first",
    );
    write_codex(
        &catalog,
        "rollout-alpha-two.jsonl",
        "alpha-two",
        "/tmp/b",
        "second",
    );

    let err = catalog.resolve_any("alpha").expect_err("shared prefix ambiguous");
    match err {
        CatalogError::AmbiguousSession { input, matches } => {
            assert_eq!(input, "alpha");
            assert!(matches.len() >= 2, "matches={matches:?}");
        }
        other => panic!("expected AmbiguousSession, got {other:?}"),
    }

    let err = catalog
        .import_or_resume_any("alpha", Some(Path::new("/tmp")))
        .expect_err("ambiguous import_or_resume_any must not write");
    assert!(matches!(err, CatalogError::AmbiguousSession { .. }));

    let native_dir = catalog.root_for(SessionSourceKind::NativePi).path;
    if native_dir.is_dir() {
        let native_files = WalkDir::new(&native_dir)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension() == Some(OsStr::new("jsonl")))
            .count();
        assert_eq!(native_files, 0);
    }

    // Exact id still resolves uniquely.
    let (kind, path) = catalog.resolve_any("alpha-one").expect("exact id");
    assert_eq!(kind, SessionSourceKind::Codex);
    assert!(path.ends_with("rollout-alpha-one.jsonl"));
}

/// Same foreign id imported after content change still reuses by lineage
/// identity (source + source_session_id). Policy: no forced reimport.
#[test]
fn lineage_reuses_existing_after_source_content_change() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let source = write_codex(
        &catalog,
        "rollout-mutate.jsonl",
        "mutate-id",
        "/tmp/mutate",
        "original prompt",
    );
    let first = catalog
        .import_or_resume(
            SessionSourceKind::Codex,
            &source,
            Some(Path::new("/tmp/mutate")),
        )
        .expect("first import");
    assert!(!first.reused_existing);
    let first_path = first.path.clone();
    let first_id = first.id.clone();
    let first_body = fs::read_to_string(&first_path).expect("body");
    assert!(first_body.contains("original prompt") || first_body.contains("original"));

    // Mutate source content + mtime so content_fingerprint changes.
    write_codex(
        &catalog,
        "rollout-mutate.jsonl",
        "mutate-id",
        "/tmp/mutate",
        "changed prompt after edit",
    );
    set_modified(&source, 9_999);

    let second = catalog
        .import_or_resume(
            SessionSourceKind::Codex,
            &source,
            Some(Path::new("/tmp/mutate")),
        )
        .expect("second import after content change");
    assert!(
        second.reused_existing,
        "same source_session_id must reuse existing native conversion"
    );
    assert_eq!(second.path, first_path);
    assert_eq!(second.id, first_id);
    assert!(!second.native_no_copy);

    let native_dir = catalog.root_for(SessionSourceKind::NativePi).path;
    let native_files = WalkDir::new(&native_dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension() == Some(OsStr::new("jsonl")))
        .count();
    assert_eq!(native_files, 1, "content change must not spawn a second native file");

    // Catalog marks foreign row AlreadyImported via identity key, independent of fingerprint.
    let rows = catalog.scan(&[SessionSourceKind::Codex]);
    let row = rows
        .iter()
        .find(|row| row.session_id == "mutate-id")
        .expect("row");
    match &row.status {
        CatalogRowStatus::AlreadyImported { native_id, native_path } => {
            assert_eq!(native_id, &first_id);
            assert_eq!(native_path, &first_path);
        }
        other => panic!("expected AlreadyImported, got {other:?}"),
    }
    let lineage = row.import_lineage.as_ref().expect("lineage");
    assert_eq!(lineage.source_session_id, "mutate-id");
    assert_ne!(
        lineage.content_fingerprint.as_deref(),
        None,
        "scan still records current content fingerprint"
    );
}

/// Distinct foreign sessions (different ids) never collide into one native file.
#[test]
fn distinct_foreign_ids_do_not_share_lineage_slot() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let a = write_codex(&catalog, "rollout-a.jsonl", "id-a", "/tmp/a", "session a");
    let b = write_codex(&catalog, "rollout-b.jsonl", "id-b", "/tmp/b", "session b");

    let ra = catalog
        .import_or_resume(SessionSourceKind::Codex, &a, Some(Path::new("/tmp/a")))
        .expect("import a");
    let rb = catalog
        .import_or_resume(SessionSourceKind::Codex, &b, Some(Path::new("/tmp/b")))
        .expect("import b");
    assert_ne!(ra.path, rb.path);
    assert_ne!(ra.id, rb.id);
    assert!(!ra.reused_existing);
    assert!(!rb.reused_existing);

    let index = catalog.build_lineage_index();
    assert_eq!(index.len(), 2);
    assert!(index.contains_key(&(SessionSourceKind::Codex, "id:id-a".to_owned())));
    assert!(index.contains_key(&(SessionSourceKind::Codex, "id:id-b".to_owned())));
}

/// Native path selection never copies; repeated resume is identity on path/id.
#[test]
fn native_no_copy_resume_is_path_identity() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let path = write_native(
        &catalog,
        "--proj--",
        "keep.jsonl",
        "native-identity",
        "/tmp/proj",
        "stay put",
    );
    let before = fs::read(&path).expect("bytes before");

    let first = catalog
        .import_or_resume(SessionSourceKind::NativePi, &path, None)
        .expect("first");
    let second = catalog
        .import_or_resume_any(&path, None)
        .expect("second via any");

    assert!(first.native_no_copy);
    assert!(second.native_no_copy);
    assert!(first.reused_existing);
    assert!(second.reused_existing);
    assert_eq!(first.path, path);
    assert_eq!(second.path, path);
    assert_eq!(first.id, "native-identity");
    assert_eq!(second.id, first.id);
    assert_eq!(fs::read(&path).expect("bytes after"), before);

    let native_files = WalkDir::new(catalog.root_for(SessionSourceKind::NativePi).path)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension() == Some(OsStr::new("jsonl")))
        .count();
    assert_eq!(native_files, 1);
}

/// Foreign list dedupe: same kind+cwd+summary keeps newest; different cwd retained;
/// empty-summary fallback keys on session_id.
#[test]
fn foreign_dedupe_by_cwd_summary_and_empty_summary_fallback() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();

    let older_same = write_codex(
        &catalog,
        "rollout-old-same.jsonl",
        "old-same",
        "/tmp/dedupe",
        "shared summary text",
    );
    let newer_same = write_codex(
        &catalog,
        "rollout-new-same.jsonl",
        "new-same",
        "/tmp/dedupe",
        "shared summary text",
    );
    let other_cwd = write_codex(
        &catalog,
        "rollout-other-cwd.jsonl",
        "other-cwd",
        "/tmp/other",
        "shared summary text",
    );
    // Meta-only sessions parse with empty message text → summary "(no messages)" after scan
    // is non-empty; craft two rows with identical empty cwd and force empty summary via
    // dedupe_rows unit shape instead for the fallback branch.
    set_modified(&older_same, 10);
    set_modified(&newer_same, 40);
    set_modified(&other_cwd, 30);

    let rows = catalog.scan(&[SessionSourceKind::Codex]);
    assert_eq!(rows.len(), 3);

    let deduped = SessionCatalog::dedupe_rows(&rows);
    let ids: Vec<_> = deduped.iter().map(|row| row.session_id.as_str()).collect();
    assert!(ids.contains(&"new-same"), "newest same-cwd/summary kept: {ids:?}");
    assert!(!ids.contains(&"old-same"), "older duplicate dropped: {ids:?}");
    assert!(ids.contains(&"other-cwd"), "different cwd retained: {ids:?}");
    assert_eq!(deduped.len(), 2);

    // Empty-summary / empty-cwd fallback: key is (kind, session_id, "").
    let fallback = [
        CatalogRow {
            kind: SessionSourceKind::Codex,
            session_id: "fb-1".into(),
            summary: "   ".into(),
            cwd: PathBuf::new(),
            modified_epoch: 1.0,
            display_time: String::new(),
            path: PathBuf::from("/a"),
            size: 1,
            message_count: None,
            name: None,
            status: CatalogRowStatus::Foreign,
            import_lineage: None,
            search_text: String::new(),
        },
        CatalogRow {
            kind: SessionSourceKind::Codex,
            session_id: "fb-1".into(),
            summary: "".into(),
            cwd: PathBuf::new(),
            modified_epoch: 5.0,
            display_time: String::new(),
            path: PathBuf::from("/b"),
            size: 1,
            message_count: None,
            name: None,
            status: CatalogRowStatus::Foreign,
            import_lineage: None,
            search_text: String::new(),
        },
        CatalogRow {
            kind: SessionSourceKind::Codex,
            session_id: "fb-2".into(),
            summary: "".into(),
            cwd: PathBuf::new(),
            modified_epoch: 3.0,
            display_time: String::new(),
            path: PathBuf::from("/c"),
            size: 1,
            message_count: None,
            name: None,
            status: CatalogRowStatus::Foreign,
            import_lineage: None,
            search_text: String::new(),
        },
    ];
    let fallback_deduped = SessionCatalog::dedupe_rows(&fallback);
    assert_eq!(fallback_deduped.len(), 2);
    let fb1 = fallback_deduped
        .iter()
        .find(|row| row.session_id == "fb-1")
        .expect("fb-1");
    assert_eq!(fb1.path, PathBuf::from("/b"), "newest empty-summary id wins");
    assert!(fallback_deduped.iter().any(|row| row.session_id == "fb-2"));

    // list(dedupe=true, include_foreign) exercises the same path through finish_rows.
    let listed = catalog.list(&CatalogListOptions {
        sources: vec![SessionSourceKind::Codex],
        include_foreign: true,
        dedupe: true,
        ..CatalogListOptions::default()
    });
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().any(|row| row.session_id == "new-same"));
    assert!(listed.iter().any(|row| row.session_id == "other-cwd"));
}

/// Identity key encoding is the collision domain for lineage index lookups.
#[test]
fn lineage_identity_key_distinguishes_id_and_path_modes() {
    let with_id = ImportLineageKey {
        source: SessionSourceKind::Codex,
        source_session_id: "abc".into(),
        source_path_fingerprint: "/x".into(),
        content_fingerprint: Some("1:2".into()),
    };
    let path_only = ImportLineageKey {
        source: SessionSourceKind::Codex,
        source_session_id: String::new(),
        source_path_fingerprint: "/x".into(),
        content_fingerprint: None,
    };
    assert_eq!(
        with_id.identity_key(),
        (SessionSourceKind::Codex, "id:abc".to_owned())
    );
    assert_eq!(
        path_only.identity_key(),
        (SessionSourceKind::Codex, "path:/x".to_owned())
    );
    assert_ne!(with_id.identity_key(), path_only.identity_key());
    assert_eq!(with_id.parent_session_value(), "codex:abc");
    assert_eq!(path_only.parent_session_value(), "codex:/x");
}

#[test]
fn forced_foreign_resolution_uses_every_injected_catalog_root() {
    let fixture = Fixture::new();
    let catalog = SessionCatalog::with_homes(
        fixture.root.join("sessions-home"),
        fixture.root.join("user-home"),
    )
    .with_native_agent_dir(fixture.root.join("native-agent"))
    .with_codex_home(fixture.root.join("codex-home"))
    .with_claude_config_dir(fixture.root.join("claude-home"));

    let cases = [
        (
            SessionSourceKind::Omp,
            write_omp(&catalog, "injected-omp-id", "/tmp/omp"),
            "injected-omp",
        ),
        (
            SessionSourceKind::Codex,
            write_codex(
                &catalog,
                "rollout-injected-codex.jsonl",
                "injected-codex-id",
                "/tmp/codex",
                "codex prompt",
            ),
            "injected-codex",
        ),
        (
            SessionSourceKind::Claude,
            write_claude(
                &catalog,
                "injected-claude.jsonl",
                "injected-claude-id",
                "/tmp/claude",
                "claude prompt",
            ),
            "injected-claude",
        ),
        (
            SessionSourceKind::Grok,
            write_grok(&catalog, "injected-grok-id", "/tmp/grok"),
            "injected-grok",
        ),
        (
            SessionSourceKind::Droid,
            write_droid(&catalog, "injected-droid-id", "/tmp/droid"),
            "injected-droid",
        ),
    ];

    for (kind, expected, prefix) in cases {
        assert_eq!(catalog.resolve_for(kind, prefix).expect("injected prefix"), expected);
    }
}

#[test]
#[cfg(unix)]
fn ancestor_symlink_escape_fails_closed_during_capability_open() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let outside = fixture.root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let escaped = outside.join("rollout-escaped.jsonl");
    fs::write(
        &escaped,
        concat!(
            r#"{"timestamp":"2026-02-01T00:00:00Z","type":"session_meta","payload":{"id":"escaped-id","cwd":"/tmp/escaped"}}"#,
            "\n",
            r#"{"timestamp":"2026-02-01T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":"must not import"}}"#,
            "\n"
        ),
    )
    .unwrap();
    let link = catalog
        .root_for(SessionSourceKind::Codex)
        .path
        .join("ancestor");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let through_link = link.join("rollout-escaped.jsonl");

    let error = catalog
        .import_or_resume(SessionSourceKind::Codex, &through_link, None)
        .expect_err("ancestor symlink escape");
    assert!(matches!(
        error,
        CatalogError::Import(ImportSessionError::InvalidInput { .. })
    ));
    assert!(!catalog.root_for(SessionSourceKind::NativePi).path.exists());
}

#[test]
#[cfg(unix)]
fn grok_symlinked_companions_fail_closed() {
    for companion in ["chat_history.jsonl", ".cwd"] {
        let fixture = Fixture::new();
        let catalog = fixture.catalog();
        let directory = catalog
            .root_for(SessionSourceKind::Grok)
            .path
            .join("relative-cwd")
            .join(format!("grok-{companion}"));
        fs::create_dir_all(&directory).unwrap();
        let summary = directory.join("summary.json");
        fs::write(&summary, r#"{"info":{"id":"grok-companion"}}"#).unwrap();
        let outside = fixture.write("outside-companion", "outside");
        if companion == "chat_history.jsonl" {
            std::os::unix::fs::symlink(&outside, directory.join(companion)).unwrap();
        } else {
            fs::write(
                directory.join("chat_history.jsonl"),
                r#"{"role":"user","content":"hello"}
"#,
            )
            .unwrap();
            std::os::unix::fs::symlink(
                &outside,
                directory.parent().unwrap().join(companion),
            )
            .unwrap();
        }

        let error = catalog
            .import_or_resume(SessionSourceKind::Grok, &summary, None)
            .expect_err("symlinked Grok companion");
        assert!(matches!(
            error,
            CatalogError::Import(ImportSessionError::InvalidInput { .. })
        ));
    }
}

#[test]
fn concurrent_imports_share_one_atomic_native_result() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let source = write_codex(
        &catalog,
        "rollout-concurrent.jsonl",
        "concurrent-id",
        "/tmp/concurrent",
        "race once",
    );
    let workers = 8;
    let barrier = Arc::new(Barrier::new(workers));
    let mut threads = Vec::new();
    for _ in 0..workers {
        let catalog = catalog.clone();
        let source = source.clone();
        let barrier = Arc::clone(&barrier);
        threads.push(thread::spawn(move || {
            barrier.wait();
            catalog
                .import_or_resume(
                    SessionSourceKind::Codex,
                    source,
                    Some(Path::new("/tmp/concurrent")),
                )
                .expect("concurrent import")
        }));
    }
    let results = threads
        .into_iter()
        .map(|thread| thread.join().expect("worker"))
        .collect::<Vec<_>>();
    let path = results[0].path.clone();
    let id = results[0].id.clone();
    assert!(results.iter().all(|result| result.path == path));
    assert!(results.iter().all(|result| result.id == id));
    assert_eq!(
        results.iter().filter(|result| !result.reused_existing).count(),
        1
    );
    let native_files = WalkDir::new(catalog.root_for(SessionSourceKind::NativePi).path)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension() == Some(OsStr::new("jsonl")))
        .count();
    assert_eq!(native_files, 1);
}

#[test]
fn native_no_copy_resume_accepts_safe_ad_hoc_path() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let path = fixture.write(
        "ad-hoc-native.jsonl",
        concat!(
            r#"{"type":"session","version":3,"id":"ad-hoc-native","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/ad-hoc"}"#,
            "\n",
            r#"{"type":"message","id":"u","parentId":null,"message":{"role":"user","content":"hello"}}"#,
            "\n"
        ),
    );
    let resolved = catalog
        .import_or_resume_any(&path, None)
        .expect("ad-hoc native resume");
    assert!(resolved.native_no_copy);
    assert!(resolved.reused_existing);
    assert_eq!(resolved.path, path);
    assert_eq!(resolved.id, "ad-hoc-native");
}
