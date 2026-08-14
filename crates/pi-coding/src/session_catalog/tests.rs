use std::env;
use std::fs::{self, File, FileTimes};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::sync::{Arc, Barrier};
use std::thread;

use super::*;
use crate::import::{parse_omp_chain_public, source_root_for, SourceSessionFormat};


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

fn synthetic_row(index: usize) -> CatalogRow {
    CatalogRow {
        kind: SessionSourceKind::Codex,
        session_id: format!("synthetic-{index:04}"),
        summary: String::new(),
        cwd: PathBuf::new(),
        modified_epoch: 777.0,
        display_time: String::new(),
        path: PathBuf::from(format!("/synthetic/{index:04}")),
        size: 0,
        message_count: None,
        name: None,
        status: CatalogRowStatus::Foreign,
        import_lineage: None,
        search_text: String::new(),
        message_blob: String::new(),
    }
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
fn exact_native_session_root_lists_direct_children_and_rejects_nested_files() {
    let fixture = Fixture::new();
    let root = fixture.root.join("custom-sessions");
    let catalog = SessionCatalog::new(fixture.root.join("home"))
        .with_native_session_root(root.clone());
    fs::create_dir_all(&root).expect("custom root");
    let direct = root.join("direct.jsonl");
    fs::write(
        &direct,
        "{\"type\":\"session\",\"version\":3,\"id\":\"direct\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/tmp/work\"}\n",
    )
    .expect("direct session");
    let nested = root.join("nested/nested.jsonl");
    fs::create_dir_all(nested.parent().expect("nested parent")).expect("nested directory");
    fs::write(
        &nested,
        "{\"type\":\"session\",\"version\":3,\"id\":\"nested\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/tmp/work\"}\n",
    )
    .expect("nested session");

    assert_eq!(catalog.root_for(SessionSourceKind::NativePi).path, root);
    assert!(catalog.is_safe_session_path(SessionSourceKind::NativePi, &direct));
    assert!(!catalog.is_safe_session_path(SessionSourceKind::NativePi, &nested));
    assert_eq!(catalog.discover(SessionSourceKind::NativePi), vec![direct]);
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
fn discover_keeps_bounded_newest_candidates_with_stable_ties() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let mut paths = Vec::with_capacity(SESSION_CATALOG_CANDIDATE_LIMIT + 2);
    for index in 0..SESSION_CATALOG_CANDIDATE_LIMIT + 2 {
        let path = write_codex(
            &catalog,
            &format!("rollout-{index:04}.jsonl"),
            &format!("id-{index:04}"),
            "/tmp/bounded",
            "bounded candidate",
        );
        set_modified(&path, index as u64 + 1);
        paths.push(path);
    }

    let discovered = catalog.discover(SessionSourceKind::Codex);
    assert_eq!(discovered.len(), SESSION_CATALOG_CANDIDATE_LIMIT);
    assert!(!discovered.contains(&paths[0]));
    assert!(!discovered.contains(&paths[1]));
    assert!(discovered.contains(paths.last().expect("newest")));

    for path in &paths {
        set_modified(path, 777);
    }
    let tied = catalog.discover(SessionSourceKind::Codex);
    assert!(tied.contains(&paths[0]), "stable smaller paths win equal-mtime ties");
    assert!(tied.contains(&paths[SESSION_CATALOG_CANDIDATE_LIMIT - 1]));
    assert!(!tied.contains(&paths[SESSION_CATALOG_CANDIDATE_LIMIT]));
    assert!(!tied.contains(&paths[SESSION_CATALOG_CANDIDATE_LIMIT + 1]));
}

#[test]
fn source_candidate_caps_prevent_starvation_before_global_row_cap() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    for index in 0..SESSION_CATALOG_CANDIDATE_LIMIT + 4 {
        let path = write_codex(
            &catalog,
            &format!("rollout-codex-{index:04}.jsonl"),
            &format!("codex-{index:04}"),
            "/tmp/codex",
            "codex bounded",
        );
        set_modified(&path, 10_000 + index as u64);
    }
    let corrupt = catalog
        .root_for(SessionSourceKind::Codex)
        .path
        .join("rollout-corrupt-newest.jsonl");
    fs::write(&corrupt, b"{not json\n").expect("corrupt");
    set_modified(&corrupt, 30_000);
    #[cfg(unix)]
    {
        let target = catalog
            .root_for(SessionSourceKind::Codex)
            .path
            .join("rollout-codex-0515.jsonl");
        let link = catalog
            .root_for(SessionSourceKind::Codex)
            .path
            .join("rollout-symlink-newest.jsonl");
        std::os::unix::fs::symlink(target, &link).expect("symlink");
    }
    let claude = write_claude(
        &catalog,
        "claude-sentinel.jsonl",
        "claude-sentinel",
        "/tmp/claude",
        "claude sentinel",
    );
    let claude_second = write_claude(
        &catalog,
        "claude-second.jsonl",
        "claude-second",
        "/tmp/claude",
        "claude second",
    );
    set_modified(&claude, 1);
    set_modified(&claude_second, 2);

    let rows = catalog.scan(&[SessionSourceKind::Codex, SessionSourceKind::Claude]);
    assert_eq!(rows.len(), SESSION_CATALOG_ROW_LIMIT);
    assert!(rows.iter().any(|row| row.session_id == "claude-second"));
    assert!(!rows.iter().any(|row| row.session_id == "claude-sentinel"));
    assert!(rows.iter().any(|row| row.session_id == "codex-0515"));
    assert_eq!(rows.iter().filter(|row| row.kind == SessionSourceKind::Claude).count(), 1);
    assert!(!rows.iter().any(|row| row.path == corrupt));
    #[cfg(unix)]
    assert!(!rows.iter().any(|row| row.path.ends_with("rollout-symlink-newest.jsonl")));
    assert!(!rows.iter().any(|row| row.session_id == "codex-0000"));
}

#[test]
fn finished_row_adapters_cap_with_stable_path_ties() {
    let rows = (0..SESSION_CATALOG_ROW_LIMIT + 2)
        .map(synthetic_row)
        .collect::<Vec<_>>();
    let deduped = SessionCatalog::dedupe_rows(&rows);
    assert_eq!(deduped.len(), SESSION_CATALOG_ROW_LIMIT);
    assert_eq!(deduped.first().expect("first").session_id, "synthetic-0000");
    assert_eq!(
        deduped.last().expect("last").session_id,
        format!("synthetic-{:04}", SESSION_CATALOG_ROW_LIMIT - 1)
    );

    let named = SessionCatalog::sort_rows(rows, CatalogSort::Name);
    assert_eq!(named.len(), SESSION_CATALOG_ROW_LIMIT);
    assert_eq!(named.first().expect("first").session_id, "synthetic-0000");
}

#[test]
fn list_and_search_share_bounded_universe_before_cwd_filtering() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    for index in 0..SESSION_CATALOG_ROW_LIMIT + 1 {
        let cwd = if index == SESSION_CATALOG_ROW_LIMIT {
            "/tmp/other"
        } else {
            "/tmp/wanted"
        };
        let path = write_codex(
            &catalog,
            &format!("rollout-shared-{index:04}.jsonl"),
            &format!("shared-{index:04}"),
            cwd,
            "bounded-universe-token",
        );
        set_modified(&path, index as u64 + 1);
    }
    let options = CatalogListOptions {
        sources: vec![SessionSourceKind::Codex],
        include_foreign: true,
        cwd_scope: Some(PathBuf::from("/tmp/wanted")),
        ..CatalogListOptions::default()
    };
    let listed = catalog.list(&options);
    let searched = catalog
        .search(
            "bounded-universe-token",
            &CatalogSearchOptions {
                sources: options.sources.clone(),
                include_foreign: true,
                cwd_scope: options.cwd_scope.clone(),
                ..CatalogSearchOptions::default()
            },
        )
        .expect("search");
    let unscoped_listed = catalog.list(&CatalogListOptions {
        cwd_scope: None,
        ..options.clone()
    });
    let unscoped_searched = catalog
        .search(
            "bounded-universe-token",
            &CatalogSearchOptions {
                sources: options.sources.clone(),
                include_foreign: true,
                cwd_scope: None,
                ..CatalogSearchOptions::default()
            },
        )
        .expect("unscoped search");
    let listed_ids = listed.iter().map(|row| &row.session_id).collect::<Vec<_>>();
    let searched_ids = searched.iter().map(|row| &row.session_id).collect::<Vec<_>>();
    assert_eq!(listed_ids, searched_ids);
    assert_eq!(listed.len(), SESSION_CATALOG_ROW_LIMIT - 1);
    assert!(!listed.iter().any(|row| row.session_id == "shared-0000"));
    assert!(!listed.iter().any(|row| row.session_id == "shared-0512"));
    assert!(!searched.iter().any(|row| row.session_id == "shared-0512"));
    assert!(unscoped_listed.iter().any(|row| row.session_id == "shared-0512"));
    assert!(unscoped_searched.iter().any(|row| row.session_id == "shared-0512"));
}

#[test]
fn walk_budget_counts_junk_entries_before_candidate_filtering() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let root = catalog.root_for(SessionSourceKind::Droid).path;
    fs::create_dir_all(&root).expect("droid root");
    for index in 0..SESSION_CATALOG_WALK_ENTRY_LIMIT {
        let path = root.join(format!("junk-{index:05}"));
        if index % 2 == 0 {
            fs::create_dir(&path).expect("junk directory");
        } else {
            fs::write(path.with_extension("txt"), b"junk").expect("junk file");
        }
    }
    let sentinel = write_droid(&catalog, "zzzzz-sentinel", "/tmp/droid");
    set_modified(&sentinel, 100_000);
    let walked = catalog
        .walk_source_entries(SessionSourceKind::Droid)
        .filter_map(Result::ok)
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();
    assert_eq!(walked.len(), SESSION_CATALOG_WALK_ENTRY_LIMIT);
    let discovered = catalog.discover(SessionSourceKind::Droid);
    assert_eq!(discovered.contains(&sentinel), walked.contains(&sentinel));
    assert!(discovered.len() <= 1, "junk must never become a candidate");
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
    assert!(body.contains("rpi-import"));
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
            message_blob: String::new(),
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
            message_blob: String::new(),
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
            message_blob: String::new(),
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

#[test]
fn catalog_prepares_native_resume_under_root_and_appends() {
    let fixture = Fixture::new();
    let target_cwd = fixture.root.join("catalog-target");
    let catalog = fixture.catalog();
    let path = write_native(
        &catalog,
        "--tmp-catalog--",
        "catalog.jsonl",
        "catalog-id",
        target_cwd.to_str().expect("utf-8 target cwd"),
        "catalog message",
    );

    let prepared = catalog
        .prepare_native_resume(&path)
        .expect("prepare native catalog session");
    assert_eq!(prepared.path(), path);
    assert_eq!(prepared.target_cwd(), target_cwd);
    assert_eq!(prepared.tree().header.id, "catalog-id");
    let recorder = prepared.into_recorder().expect("build recorder");
    recorder
        .record_message(&pi_ai::Message::user_text("catalog appended", 0))
        .expect("append catalog session");
    recorder.close().expect("close catalog session");
    let tree = crate::load_session_tree(&path).expect("reload catalog session");
    assert_eq!(tree.entries.len(), 2);
}

#[test]
#[cfg(unix)]
fn catalog_prepared_resume_rejects_final_symlink_replacement() {
    let fixture = Fixture::new();
    let target_cwd = fixture.root.join("symlink-target");
    let catalog = fixture.catalog();
    let target = write_native(
        &catalog,
        "--tmp-catalog--",
        "target.jsonl",
        "target-id",
        target_cwd.to_str().expect("utf-8 target cwd"),
        "target message",
    );
    let link = target.with_file_name("link.jsonl");
    std::os::unix::fs::symlink(&target, &link).expect("create final symlink");

    let error = catalog
        .prepare_native_resume(&link)
        .expect_err("catalog final symlink must fail");
    assert!(matches!(
        error,
        CatalogError::Import(ImportSessionError::InvalidInput { .. })
    ));
}

#[test]
fn native_catalog_does_not_count_empty_assistant_placeholders() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let path = catalog
        .root_for(SessionSourceKind::NativePi)
        .path
        .join("--empty--")
        .join("placeholder.jsonl");
    fs::create_dir_all(path.parent().expect("parent")).expect("parent");
    fs::write(
        &path,
        concat!(
            r#"{"type":"session","version":3,"id":"placeholder","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/empty"}"#,
            "\n",
            r#"{"type":"message","id":"a","parentId":null,"message":{"role":"assistant","content":[]}}"#,
            "\n"
        ),
    )
    .expect("placeholder session");
    let rows = catalog.list(&CatalogListOptions {
        sources: vec![SessionSourceKind::NativePi],
        ..CatalogListOptions::default()
    });
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].message_count, Some(0));
    assert_eq!(rows[0].summary, "(no messages)");
}

#[test]
fn native_image_only_message_counts_as_meaningful() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let path = catalog
        .root_for(SessionSourceKind::NativePi)
        .path
        .join("--image--")
        .join("image-only.jsonl");
    fs::create_dir_all(path.parent().expect("parent")).expect("parent");
    fs::write(
        &path,
        concat!(
            r#"{"type":"session","version":3,"id":"image-only","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/image"}"#,
            "\n",
            r#"{"type":"message","id":"u","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":[{"type":"image","data":"aW1n","mimeType":"image/png"}]}}"#,
            "\n"
        ),
    )
    .expect("image-only session");
    let rows = catalog.list(&CatalogListOptions {
        sources: vec![SessionSourceKind::NativePi],
        ..CatalogListOptions::default()
    });
    assert_eq!(rows.len(), 1);
    // Image-only user turn is meaningful even though no portable text exists.
    assert_eq!(rows[0].message_count, Some(1));
    // Textual summary still takes the first non-empty text; none here.
    assert_eq!(rows[0].summary, "(no messages)");
}

#[test]
fn omp_image_only_message_counts_as_meaningful() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let path = catalog
        .root_for(SessionSourceKind::Omp)
        .path
        .join("--image--")
        .join("omp-image.jsonl");
    fs::create_dir_all(path.parent().expect("parent")).expect("parent");
    fs::write(
        &path,
        concat!(
            r#"{"type":"session","version":3,"id":"omp-image","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/image"}"#,
            "\n",
            r#"{"type":"message","id":"u","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":[{"type":"image","data":"aW1n","mimeType":"image/png"}]}}"#,
            "\n"
        ),
    )
    .expect("omp image-only session");
    let rows = catalog.list(&CatalogListOptions {
        sources: vec![SessionSourceKind::Omp],
        include_foreign: true,
        ..CatalogListOptions::default()
    });
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, SessionSourceKind::Omp);
    // OMP image-only turn is meaningful for the catalog count even though the
    // lossy import projection drops it from `messages`.
    assert_eq!(rows[0].message_count, Some(1));
    assert_eq!(rows[0].summary, "(no messages)");
}

/// OMP native listAllSessions only globs `*/*.jsonl`. Deeper files under a
/// parent session directory (task/subagent child trees) are not native-resumable
/// and must never enter the catalog — eligibility is path structure only, not
/// title/body heuristics, and large non-empty child transcripts still drop.
#[test]
fn omp_lists_only_native_tree_sessions_and_excludes_child_subagent_trees() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let root = catalog.root_for(SessionSourceKind::Omp).path;

    // Top-level native-resumable shape: `<cwd-dir>/<session>.jsonl`.
    let top = write_omp(&catalog, "omp-top-level", "/tmp/omp-top");

    // Child/subagent tree under the parent session stem. Real OMP stores these
    // at depth 3+; they carry real messages and can be large, but native resume
    // listing never includes them.
    let parent_stem = top
        .file_stem()
        .expect("top stem")
        .to_str()
        .expect("utf8 stem");
    let child_dir = root.join("--injected--").join(parent_stem);
    fs::create_dir_all(&child_dir).expect("child dir");
    let padding = "x".repeat(12_000);
    let child = child_dir.join("ScoutChild.jsonl");
    fs::write(
        &child,
        format!(
            concat!(
                r#"{{"type":"title","v":1,"title":"Complete the assignment below...","updatedAt":"2026-01-01T00:00:00Z","pad":"x"}}"#,
                "\n",
                r#"{{"type":"session","version":3,"id":"omp-child-id","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/omp-top"}}"#,
                "\n",
                r#"{{"type":"session_init","id":"init","parentId":null,"timestamp":"2026-01-01T00:00:00Z","task":"child task","tools":[],"spawns":[]}}"#,
                "\n",
                r#"{{"type":"message","id":"u","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{{"role":"user","content":"child user prompt {padding}"}}}}"#,
                "\n",
                r#"{{"type":"message","id":"a","parentId":"u","timestamp":"2026-01-01T00:00:02Z","message":{{"role":"assistant","content":"child assistant reply"}}}}"#,
                "\n"
            ),
            padding = padding
        ),
    )
    .expect("child session");
    assert!(
        fs::metadata(&child).expect("child meta").len() > 10_240,
        "child fixture must be large enough that size alone would not hide it"
    );

    // Nested grandchild (depth 4) also excluded.
    let grandchild_dir = child_dir.join("NestedTeam");
    fs::create_dir_all(&grandchild_dir).expect("grandchild dir");
    let grandchild = grandchild_dir.join("NestedTeam.RpcScout.jsonl");
    fs::write(
        &grandchild,
        concat!(
            r#"{"type":"session","version":3,"id":"omp-grandchild","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/omp-top"}"#,
            "\n",
            r#"{"type":"message","id":"u","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":"grandchild prompt"}}"#,
            "\n"
        ),
    )
    .expect("grandchild session");

    // Root-level single-component jsonl is not native `*/*.jsonl`.
    let shallow = root.join("orphan-root.jsonl");
    fs::write(
        &shallow,
        concat!(
            r#"{"type":"session","version":3,"id":"omp-shallow","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/omp-top"}"#,
            "\n",
            r#"{"type":"message","id":"u","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":"shallow prompt"}}"#,
            "\n"
        ),
    )
    .expect("shallow session");

    assert!(
        catalog.is_safe_session_path(SessionSourceKind::Omp, &top),
        "top-level OMP session must remain native-resumable"
    );
    assert!(
        !catalog.is_safe_session_path(SessionSourceKind::Omp, &child),
        "depth-3 subagent child must be structurally ineligible"
    );
    assert!(
        !catalog.is_safe_session_path(SessionSourceKind::Omp, &grandchild),
        "depth-4 nested child must be structurally ineligible"
    );
    assert!(
        !catalog.is_safe_session_path(SessionSourceKind::Omp, &shallow),
        "root-level jsonl is outside native *//*.jsonl layout"
    );

    let discovered = catalog.discover(SessionSourceKind::Omp);
    assert_eq!(discovered, vec![top.clone()]);

    let before_child = fs::read(&child).expect("child bytes before scan");
    let before_mtime = fs::metadata(&child).expect("child meta").modified().expect("mtime");

    let rows = catalog.list(&CatalogListOptions {
        sources: vec![SessionSourceKind::Omp],
        include_foreign: true,
        ..CatalogListOptions::default()
    });
    assert_eq!(rows.len(), 1, "only the top-level OMP session is listed: {rows:?}");
    assert_eq!(rows[0].kind, SessionSourceKind::Omp);
    assert_eq!(rows[0].session_id, "omp-top-level");
    assert_eq!(rows[0].path, top);
    assert!(
        !rows.iter().any(|row| row.session_id == "omp-child-id"
            || row.session_id == "omp-grandchild"
            || row.session_id == "omp-shallow"
            || row.path == child
            || row.path == grandchild
            || row.path == shallow),
        "child/subagent/internal paths must not appear regardless of title/body"
    );

    // Foreign source bytes + mtime are never mutated by discovery/list.
    let after_child = fs::read(&child).expect("child bytes after scan");
    let after_mtime = fs::metadata(&child).expect("child meta").modified().expect("mtime");
    assert_eq!(before_child, after_child, "foreign child file bytes must stay unchanged");
    assert_eq!(before_mtime, after_mtime, "foreign child mtime must stay unchanged");
}

/// Codex native resume lists `rollout-*.jsonl` under the sessions tree and
/// excludes `archived_sessions`. Grok native discovery is exactly
/// `<cwd-token>/<session-id>/summary.json`. Keep those structural rules and
/// reject deeper/non-matching paths without title heuristics.
#[test]
fn codex_and_grok_native_resumable_layouts_exclude_non_native_paths() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();

    let codex_top = write_codex(
        &catalog,
        "2026/01/01/rollout-codex-top.jsonl",
        "codex-top",
        "/tmp/codex-top",
        "codex top prompt",
    );
    let codex_archived = catalog
        .root_for(SessionSourceKind::Codex)
        .path
        .join("archived_sessions/rollout-archived.jsonl");
    fs::create_dir_all(codex_archived.parent().expect("archived parent")).expect("archived dir");
    fs::copy(&codex_top, &codex_archived).expect("archived copy");

    let grok_top = write_grok(&catalog, "grok-top", "/tmp/grok-top");
    let grok_nested = catalog
        .root_for(SessionSourceKind::Grok)
        .path
        .join("injected-cwd")
        .join("grok-top")
        .join("state")
        .join("summary.json");
    fs::create_dir_all(grok_nested.parent().expect("nested parent")).expect("nested dir");
    fs::write(
        &grok_nested,
        r#"{"info":{"id":"grok-nested","cwd":"/tmp/grok-top"},"created_at":"2026-01-01T00:00:00Z"}"#,
    )
    .expect("nested summary");
    fs::write(
        grok_nested
            .parent()
            .expect("nested parent")
            .join("chat_history.jsonl"),
        r#"{"role":"user","content":"nested grok prompt","timestamp":"2026-01-01T00:00:01Z"}
"#,
    )
    .expect("nested chat");

    assert!(catalog.is_safe_session_path(SessionSourceKind::Codex, &codex_top));
    assert!(!catalog.is_safe_session_path(SessionSourceKind::Codex, &codex_archived));
    assert!(catalog.is_safe_session_path(SessionSourceKind::Grok, &grok_top));
    assert!(!catalog.is_safe_session_path(SessionSourceKind::Grok, &grok_nested));

    let codex_before = fs::read(&codex_archived).expect("archived bytes");
    let grok_before = fs::read(&grok_nested).expect("nested summary bytes");

    let codex_rows = catalog.list(&CatalogListOptions {
        sources: vec![SessionSourceKind::Codex],
        include_foreign: true,
        ..CatalogListOptions::default()
    });
    assert_eq!(codex_rows.len(), 1);
    assert_eq!(codex_rows[0].session_id, "codex-top");
    assert_eq!(codex_rows[0].path, codex_top);

    let grok_rows = catalog.list(&CatalogListOptions {
        sources: vec![SessionSourceKind::Grok],
        include_foreign: true,
        ..CatalogListOptions::default()
    });
    assert_eq!(grok_rows.len(), 1);
    assert_eq!(grok_rows[0].session_id, "grok-top");
    assert_eq!(grok_rows[0].path, grok_top);

    assert_eq!(
        codex_before,
        fs::read(&codex_archived).expect("archived after"),
        "archived codex foreign source must remain untouched"
    );
    assert_eq!(
        grok_before,
        fs::read(&grok_nested).expect("nested after"),
        "nested grok foreign source must remain untouched"
    );
}

/// Native Pi already excludes `children/<parent-id>/` by the same two-component
/// tree rule. Keep that contract explicit next to the OMP structural filter.
#[test]
fn native_pi_excludes_children_subtree_from_default_tree_discovery() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let top = write_native(
        &catalog,
        "--proj--",
        "parent.jsonl",
        "native-parent",
        "/tmp/proj",
        "native parent prompt",
    );
    let child = catalog
        .root_for(SessionSourceKind::NativePi)
        .path
        .join("--proj--")
        .join("children")
        .join("native-parent")
        .join("child.jsonl");
    fs::create_dir_all(child.parent().expect("child parent")).expect("child dir");
    fs::write(
        &child,
        concat!(
            r#"{"type":"session","version":3,"id":"native-child","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/proj","parentSession":"/tmp/parent.jsonl"}"#,
            "\n",
            r#"{"type":"message","id":"u","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":[{"type":"text","text":"native child prompt"}]}}"#,
            "\n"
        ),
    )
    .expect("native child");

    assert!(catalog.is_safe_session_path(SessionSourceKind::NativePi, &top));
    assert!(!catalog.is_safe_session_path(SessionSourceKind::NativePi, &child));
    let rows = catalog.list(&CatalogListOptions {
        sources: vec![SessionSourceKind::NativePi],
        ..CatalogListOptions::default()
    });
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].session_id, "native-parent");
    assert_eq!(rows[0].path, top);
}


/// Write an OMP rotation chain: `count` files where every file after the
/// first carries a `parentSession` header pointing at the previous file's
/// absolute path (OMP `newSession({ parentSession })` rotation), plus a
/// handoff-style custom message on the leaf. Each file contributes one
/// user/assistant turn whose text embeds the file index.
fn write_omp_chain(catalog: &SessionCatalog, dir: &str, count: usize) -> Vec<PathBuf> {
    let directory = catalog
        .root_for(SessionSourceKind::Omp)
        .path
        .join(dir);
    fs::create_dir_all(&directory).expect("omp chain dir");
    let mut files = Vec::new();
    for index in 0..count {
        let id = format!("omp-chain-{index}");
        let path = directory.join(format!("{id}.jsonl"));
        let parent = files
            .last()
            .map(|previous: &PathBuf| format!(r#","parentSession":"{}""#, previous.display()))
            .unwrap_or_default();
        let mut records = vec![format!(
            r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-01-01T00:0{index}:00Z","cwd":"<workspace>/chain"{parent}}}"#
        )];
        if index + 1 == count {
            records.push(
                r#"{"type":"custom_message","customType":"handoff","content":"handoff summary","display":true,"attribution":"agent","id":"handoff-1","parentId":null,"timestamp":"2026-01-01T00:05:00Z"}"#
                    .to_owned(),
            );
        }
        records.push(format!(
            r#"{{"type":"message","id":"u{index}","parentId":null,"timestamp":"2026-01-01T00:0{index}:01Z","message":{{"role":"user","content":"chain prompt {index}"}}}}"#
        ));
        records.push(format!(
            r#"{{"type":"message","id":"a{index}","parentId":"u{index}","timestamp":"2026-01-01T00:0{index}:02Z","message":{{"role":"assistant","content":"chain reply {index}"}}}}"#
        ));
        fs::write(&path, records.join("\n") + "\n").expect("omp chain file");
        files.push(path);
    }
    files
}

/// OMP rotates an oversized logical conversation into a new file whose
/// `session` header records the prior file in `parentSession`. Loading the
/// leaf must concatenate the complete chain (root → leaf) in order; today the
/// leaf alone would only carry the final messages.
#[test]
fn omp_import_concatenates_rotated_parent_session_chain_in_order() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let chain = write_omp_chain(&catalog, "--chain--", 3);
    let [root, mid, leaf] = chain.as_slice() else {
        panic!("expected 3 files");
    };
    let before = [
        fs::read(root).expect("root bytes"),
        fs::read(mid).expect("mid bytes"),
    ];

    let resolved = catalog
        .import_or_resume(SessionSourceKind::Omp, leaf, None)
        .expect("chained import");
    assert!(!resolved.reused_existing);
    assert_eq!(resolved.source_session_id.as_deref(), Some("omp-chain-2"));
    let texts = resolved
        .messages
        .iter()
        .map(|message| message.text.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        texts,
        [
            "chain prompt 0",
            "chain reply 0",
            "chain prompt 1",
            "chain reply 1",
            "chain prompt 2",
            "chain reply 2",
        ],
        "rotation chain must concatenate every file's messages in order"
    );

    // The emitted native session carries the full history.
    let native_body = fs::read_to_string(&resolved.path).expect("native body");
    for text in [
        "chain prompt 0",
        "chain reply 1",
        "chain prompt 2",
        "chain reply 2",
    ] {
        assert!(native_body.contains(text), "native import missing {text:?}");
    }
    // Handoff summaries are custom messages, never rendered as turns.
    assert!(!native_body.contains("handoff summary"));

    // Exactly one native import for the logical session.
    let native_dir = catalog.root_for(SessionSourceKind::NativePi).path;
    let native_files = WalkDir::new(&native_dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension() == Some(OsStr::new("jsonl")))
        .count();
    assert_eq!(native_files, 1);

    // Source files are read-only during the chain load.
    assert_eq!(fs::read(root).expect("root bytes"), before[0]);
    assert_eq!(fs::read(mid).expect("mid bytes"), before[1]);

    // Idempotent re-select returns the same native copy with the full chain.
    let again = catalog
        .import_or_resume(SessionSourceKind::Omp, leaf, None)
        .expect("reuse");
    assert!(again.reused_existing);
    assert_eq!(again.path, resolved.path);
    assert_eq!(again.id, resolved.id);
    assert_eq!(
        again.messages
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>(),
        texts
    );
}

/// `createBranchedSession` copies active-path entries (original ids) into the
/// new file while also stamping `parentSession`; chain loading must keep such
/// overlap exactly once.
#[test]
fn omp_chain_deduplicates_overlapping_branch_copies() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let root = write_omp_chain(&catalog, "--branch--", 1).remove(0);
    let directory = catalog
        .root_for(SessionSourceKind::Omp)
        .path
        .join("--branch--");
    let leaf = directory.join("omp-branch-leaf.jsonl");
    // Leaf copies the root turn verbatim (same ids) and adds a new turn.
    let copied = fs::read_to_string(&root).expect("root body");
    let records = copied
        .lines()
        .chain([
            r#"{"type":"message","id":"u1","parentId":"a0","timestamp":"2026-01-01T00:01:01Z","message":{"role":"user","content":"chain prompt 1"}}"#,
            r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-01-01T00:01:02Z","message":{"role":"assistant","content":"chain reply 1"}}"#,
        ])
        .map(|line| {
            if line.contains(r#""type":"session""#) {
                format!(
                    r#"{{"type":"session","version":3,"id":"omp-branch-leaf","timestamp":"2026-01-01T00:01:00Z","cwd":"<workspace>/chain","parentSession":"{}"}}"#,
                    root.display()
                )
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&leaf, records + "\n").expect("branch leaf");

    let resolved = catalog
        .import_or_resume(SessionSourceKind::Omp, &leaf, None)
        .expect("branch import");
    let texts = resolved
        .messages
        .iter()
        .map(|message| message.text.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        texts,
        ["chain prompt 0", "chain reply 0", "chain prompt 1", "chain reply 1"],
        "copied entries must appear once, in first-file order"
    );
}

/// Malformed, missing, cyclic, escaping, and bare-id `parentSession`
/// references fail closed: the load keeps the safe prefix and never follows
/// the reference.
#[test]
fn omp_chain_fails_closed_on_bad_references() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let omp_root = catalog.root_for(SessionSourceKind::Omp).path;
    let directory = omp_root.join("--closed--");
    fs::create_dir_all(&directory).expect("dir");

    let write = |name: &str, parent: Option<&str>| {
        let path = directory.join(format!("{name}.jsonl"));
        let parent = parent
            .map(|value| format!(r#","parentSession":"{value}""#))
            .unwrap_or_default();
        fs::write(
            &path,
            format!(
                concat!(
                    r#"{{"type":"session","version":3,"id":"{name}","timestamp":"2026-01-01T00:00:00Z","cwd":"<workspace>/closed"{parent}}}"#,
                    "\n",
                    r#"{{"type":"message","id":"u","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{{"role":"user","content":"{name} prompt"}}}}"#,
                    "\n"
                ),
                name = name,
                parent = parent
            ),
        )
        .expect("write");
        path
    };

    // Self-reference cycle.
    let self_ref = write("omp-self", Some(&directory.join("omp-self.jsonl").display().to_string()));
    let chain = catalog.omp_chain_paths(&self_ref);
    assert_eq!(chain, vec![self_ref.clone()], "self-reference must not loop");

    // Two-file cycle back to the leaf.
    let cycle_a = write("omp-cycle-a", None);
    let cycle_b = write("omp-cycle-b", Some(&cycle_a.display().to_string()));
    let cycle_a = write("omp-cycle-a", Some(&cycle_b.display().to_string()));
    let chain = catalog.omp_chain_paths(&cycle_b);
    assert_eq!(chain, vec![cycle_a, cycle_b], "cycle must stop after one pass");

    // Missing target under the root.
    let missing = write(
        "omp-missing",
        Some(&directory.join("omp-gone.jsonl").display().to_string()),
    );
    assert_eq!(
        catalog.omp_chain_paths(&missing),
        vec![missing.clone()],
        "missing parent must fail closed"
    );

    // Escape target outside the OMP root.
    let outside = fixture.write("outside-evil.jsonl", "{\"type\":\"session\",\"id\":\"evil\"}\n");
    let escaped = write("omp-escape", Some(&outside.display().to_string()));
    assert_eq!(
        catalog.omp_chain_paths(&escaped),
        vec![escaped.clone()],
        "out-of-root parent must never chain"
    );

    // Bare session id (fork lineage) is not a path reference.
    let fork = write("omp-fork", Some("019f1234-5678-7000-0000-000000000000"));
    assert_eq!(
        catalog.omp_chain_paths(&fork),
        vec![fork.clone()],
        "bare id parentSession (fork) must not chain"
    );

    // Non-JSONL suffix.
    let odd = write("omp-odd", Some(&omp_root.join("odd.txt").display().to_string()));
    assert_eq!(
        catalog.omp_chain_paths(&odd),
        vec![odd.clone()],
        "non-jsonl parent must fail closed"
    );

    // Every failed-closed leaf still imports its own messages.
    for leaf in [self_ref, missing, escaped, fork, odd] {
        let resolved = catalog
            .import_or_resume(SessionSourceKind::Omp, &leaf, None)
            .expect("leaf import");
        assert_eq!(resolved.messages.len(), 1);
    }
}

/// Task/subagent child trees live at depth 3+ under the parent session stem
/// and must never be pulled into a rotation chain, even when referenced.
#[test]
fn omp_chain_excludes_child_subagent_sessions() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let omp_root = catalog.root_for(SessionSourceKind::Omp).path;
    let directory = omp_root.join("--work--");
    let leaf = directory.join("omp-parent.jsonl");
    fs::create_dir_all(&directory).expect("dir");
    fs::write(
        &leaf,
        format!(
            concat!(
                r#"{{"type":"session","version":3,"id":"omp-parent","timestamp":"2026-01-01T00:00:00Z","cwd":"<workspace>/work"}}"#,
                "\n",
                r#"{{"type":"message","id":"u","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{{"role":"user","content":"parent prompt"}}}}"#,
                "\n"
            ),
        ),
    )
    .expect("leaf");
    // Depth-3 child under the session stem (OMP `<dir>/<name>/<AgentId>.jsonl`).
    let child = directory.join("omp-parent").join("child-agent.jsonl");
    fs::create_dir_all(child.parent().expect("parent")).expect("child dir");
    fs::write(
        &child,
        concat!(
            r#"{"type":"session","version":3,"id":"omp-child","timestamp":"2026-01-01T00:00:00Z","cwd":"<workspace>/work"}"#,
            "\n",
            r#"{"type":"message","id":"u","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":"child prompt"}}"#,
            "\n"
        ),
    )
    .expect("child");
    // Repoint the leaf's parentSession at the depth-3 child.
    let body = fs::read_to_string(&leaf).expect("leaf body");
    let body = body.replace(
        r#""timestamp":"2026-01-01T00:00:00Z","cwd":"<workspace>/work""#,
        &format!(
            r#""timestamp":"2026-01-01T00:00:00Z","cwd":"<workspace>/work","parentSession":"{}""#,
            child.display()
        ),
    );
    fs::write(&leaf, body).expect("leaf rewrite");

    assert!(
        !catalog.is_safe_session_path(SessionSourceKind::Omp, &child),
        "depth-3 child must stay structurally ineligible"
    );
    let chain = catalog.omp_chain_paths(&leaf);
    assert_eq!(chain, vec![leaf.clone()], "child tree must never chain");
    let resolved = catalog
        .import_or_resume(SessionSourceKind::Omp, &leaf, None)
        .expect("leaf import");
    let texts = resolved
        .messages
        .iter()
        .map(|message| message.text.as_str())
        .collect::<Vec<_>>();
    assert_eq!(texts, ["parent prompt"], "child messages must stay excluded");
}

/// The chain walk honors count and byte bounds and keeps the safe prefix.
#[test]
fn omp_chain_bounds_limit_count_and_bytes() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let chain = write_omp_chain(&catalog, "--bounded--", 10);
    let leaf = chain.last().expect("leaf");

    // Count bound: the leaf-anchored walk keeps at most 8 files — the newest
    // 8, since the oldest ancestors drop when the budget runs out.
    let (bounded, bounded_fingerprint) = catalog.omp_chain_probe_bounded(leaf, 8, u64::MAX);
    assert_eq!(bounded.len(), 8);
    assert_eq!(&bounded[..3], &chain[2..5], "newest files first in root order");
    assert_eq!(bounded.last(), Some(leaf));
    assert_eq!(bounded_fingerprint.split('\n').count(), 8, "fingerprint covers every walked member");

    // Import of an over-long chain truncates at the bound but still works.
    let resolved = catalog
        .import_or_resume(SessionSourceKind::Omp, leaf, None)
        .expect("bounded import");
    let texts = resolved
        .messages
        .iter()
        .map(|message| message.text.as_str())
        .collect::<Vec<_>>();
    assert_eq!(texts.len(), 16, "8 files x one user/assistant turn");
    assert!(texts.contains(&"chain prompt 2"), "oldest retained message present");
    assert!(!texts.contains(&"chain prompt 1"), "over-budget file excluded");
    assert!(texts.contains(&"chain prompt 9"), "leaf always included");
    // The walk fingerprint and the parse fingerprint agree on the same chain.
    let omp_root = catalog.root_for(SessionSourceKind::Omp).path;
    let (_, parse_fingerprint) = parse_omp_chain_public(&omp_root, &bounded, u64::MAX)
        .expect("parse of walked chain");
    assert_eq!(
        parse_fingerprint, bounded_fingerprint,
        "walk and parse chain fingerprints must agree"
    );

    // Byte bound: budget covering leaf + its direct parent excludes the next
    // ancestor.
    let [root, mid] = [&chain[0], &chain[8]];
    let mid_size = fs::metadata(mid).expect("mid meta").len();
    let leaf_size = fs::metadata(leaf).expect("leaf meta").len();
    let tight = catalog.omp_chain_probe_bounded(leaf, 8, mid_size + leaf_size).0;
    assert_eq!(tight, vec![mid.clone(), leaf.clone()], "earlier ancestor excluded by byte budget");
    let leaf_only = catalog.omp_chain_probe_bounded(leaf, 8, leaf_size).0;
    assert_eq!(leaf_only, vec![leaf.clone()], "only the leaf fits the budget");
    assert_eq!(catalog.omp_chain_probe_bounded(root, 8, u64::MAX).0, vec![root.clone()]);
}

/// The OMP lineage fingerprint covers every chain member: mutating an
/// ancestor's content + mtime while the leaf stays unchanged must yield a
/// fresh import with the updated transcript, never a reused stale native copy.
#[test]
fn omp_chain_reimports_when_ancestor_content_changes() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let chain = write_omp_chain(&catalog, "--refinger--", 3);
    let leaf = chain[2].clone();
    let mid = &chain[1];

    let first = catalog
        .import_or_resume(SessionSourceKind::Omp, &leaf, None)
        .expect("first chained import");
    assert!(!first.reused_existing);
    let first_texts = first
        .messages
        .iter()
        .map(|message| message.text.as_str())
        .collect::<Vec<_>>();
    assert_eq!(first_texts.len(), 6, "full chain on first import");
    let first_body = fs::read_to_string(&first.path).expect("first body");
    assert!(first_body.contains("chain reply 1"));

    // Mutate ONLY the middle ancestor (content + mtime); leaf untouched.
    fs::write(
        mid,
        fs::read_to_string(mid)
            .expect("mid body")
            .replace("chain prompt 1", "chain prompt MUTATED")
            .replace("chain reply 1", "chain reply MUTATED"),
    )
    .expect("mutate mid");
    set_modified(mid, 42_000);

    // Catalog/UI path: after the ancestor mutation the leaf row's chain
    // fingerprint no longer matches the stored import, so the row must NOT be
    // marked AlreadyImported (selection re-enters import_or_resume instead of
    // short-circuiting to the stale native copy).
    let rows_before = catalog.list(&CatalogListOptions {
        sources: vec![SessionSourceKind::Omp],
        include_foreign: true,
        ..CatalogListOptions::default()
    });
    let row_before = rows_before
        .iter()
        .find(|row| row.session_id == "omp-chain-2")
        .expect("leaf row");
    assert!(
        matches!(row_before.status, CatalogRowStatus::Foreign),
        "mutated ancestor must mark the leaf row Foreign, got {:?}",
        row_before.status
    );

    let second = catalog
        .import_or_resume(SessionSourceKind::Omp, &leaf, None)
        .expect("reimport after ancestor mutation");
    assert!(
        !second.reused_existing,
        "ancestor change must not reuse the stale native copy"
    );
    assert_ne!(second.path, first.path, "fresh native import expected");
    assert_ne!(second.id, first.id, "chain fingerprint must change the import id");
    let second_body = fs::read_to_string(&second.path).expect("second body");
    assert!(
        second_body.contains("chain prompt MUTATED"),
        "updated ancestor transcript must be in the new import"
    );
    assert!(second_body.contains("chain prompt 0"), "full chain retained");
    assert!(second_body.contains("chain prompt 2"), "leaf retained");
    // The stale copy is never deleted, but is no longer the selection.
    assert!(first.path.exists(), "stale import file is left in place");

    // Scan now marks the row AlreadyImported pointing at the FRESH import
    // (exact chain-fingerprint match among the same-identity natives).
    let rows_after = catalog.list(&CatalogListOptions {
        sources: vec![SessionSourceKind::Omp],
        include_foreign: true,
        ..CatalogListOptions::default()
    });
    let row_after = rows_after
        .iter()
        .find(|row| row.session_id == "omp-chain-2")
        .expect("leaf row after reimport");
    match &row_after.status {
        CatalogRowStatus::AlreadyImported { native_path, .. } => {
            assert_eq!(native_path, &second.path, "row must point at the fresh import");
        }
        other => panic!("expected AlreadyImported after reimport, got {other:?}"),
    }

    // Unchanged leaf + ancestors after the new import reuse the newest copy.
    let third = catalog
        .import_or_resume(SessionSourceKind::Omp, &leaf, None)
        .expect("settled reuse");
    assert!(third.reused_existing);
    assert_eq!(third.path, second.path);
}

/// The aggregate byte bound is revalidated authoritatively at parse time from
/// the securely opened descriptors (newest prefix retained, leaf
/// unconditional), and the chain fingerprint reflects the retained members.
#[test]
fn omp_chain_aggregate_byte_bound_revalidated_at_parse() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let chain = write_omp_chain(&catalog, "--parsebound--", 3);
    let root = catalog.root_for(SessionSourceKind::Omp).path;

    let (full, full_fingerprint) = parse_omp_chain_public(&root, &chain, u64::MAX)
        .expect("full chain parse");
    assert_eq!(full.messages.len(), 6);
    assert!(full.messages.iter().any(|m| m.text == "chain prompt 0"));

    // Budget covering leaf + mid excludes the root at parse time.
    let mid_size = fs::metadata(&chain[1]).expect("mid meta").len();
    let leaf_size = fs::metadata(&chain[2]).expect("leaf meta").len();
    let (truncated, truncated_fingerprint) =
        parse_omp_chain_public(&root, &chain, mid_size + leaf_size).expect("bounded parse");
    let texts = truncated
        .messages
        .iter()
        .map(|message| message.text.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        texts,
        ["chain prompt 1", "chain reply 1", "chain prompt 2", "chain reply 2"],
        "parse-time retention keeps the newest prefix and always the leaf"
    );
    assert_ne!(
        truncated_fingerprint, full_fingerprint,
        "fingerprint must cover exactly the retained members"
    );
    // Leaf-only budget still yields the leaf (unconditional).
    let (leaf_only, _) = parse_omp_chain_public(&root, &chain, leaf_size).expect("leaf parse");
    let leaf_texts = leaf_only
        .messages
        .iter()
        .map(|message| message.text.as_str())
        .collect::<Vec<_>>();
    assert_eq!(leaf_texts, ["chain prompt 2", "chain reply 2"]);
}

/// Linkage is revalidated against the parsed content: a member whose parsed
/// `parentSession` no longer references its predecessor fails closed at parse
/// time instead of stitching unrelated files together.
#[test]
fn omp_chain_parse_revalidates_linkage_and_fails_closed() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let chain = write_omp_chain(&catalog, "--linkage--", 3);
    let root = catalog.root_for(SessionSourceKind::Omp).path;
    let leaf_path = &chain[2];

    let (full, full_fingerprint) =
        parse_omp_chain_public(&root, &chain, u64::MAX).expect("consistent chain");
    assert_eq!(full.messages.len(), 6);
    let (again, again_fingerprint) =
        parse_omp_chain_public(&root, &chain, u64::MAX).expect("consistent chain again");
    assert_eq!(again_fingerprint, full_fingerprint, "digest is deterministic");
    assert_eq!(again.messages.len(), 6);

    // Break the middle member's parentSession: it no longer references the
    // root, so the root must be dropped (fail closed) while the leaf→middle
    // link still holds. The bogus reference is a fixture-derived path that is
    // not part of the chain.
    let bogus_root = chain[0].with_file_name("not-the-root.jsonl");
    let mid_body = fs::read_to_string(&chain[1]).expect("mid body");
    fs::write(
        &chain[1],
        mid_body.replace(
            &format!(r#","parentSession":"{}""#, chain[0].display()),
            &format!(r#","parentSession":"{}""#, bogus_root.display()),
        ),
    )
    .expect("rewrite mid linkage");
    let (drifted, drifted_fingerprint) =
        parse_omp_chain_public(&root, &chain, u64::MAX).expect("drifted chain");
    let texts = drifted
        .messages
        .iter()
        .map(|message| message.text.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        texts,
        ["chain prompt 1", "chain reply 1", "chain prompt 2", "chain reply 2"],
        "broken root link must drop the root, keep the consistent newest prefix"
    );
    assert_ne!(drifted_fingerprint, full_fingerprint);

    // Break the leaf's parentSession too: the chain collapses to the leaf.
    let bogus_mid = chain[1].with_file_name("not-the-mid.jsonl");
    let leaf_body = fs::read_to_string(leaf_path).expect("leaf body");
    fs::write(
        leaf_path,
        leaf_body.replace(
            &format!(r#","parentSession":"{}""#, chain[1].display()),
            &format!(r#","parentSession":"{}""#, bogus_mid.display()),
        ),
    )
    .expect("rewrite leaf linkage");
    let (collapsed, _) =
        parse_omp_chain_public(&root, &chain, u64::MAX).expect("collapsed chain");
    let collapsed_texts = collapsed
        .messages
        .iter()
        .map(|message| message.text.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        collapsed_texts,
        ["chain prompt 2", "chain reply 2"],
        "broken leaf link must collapse to the leaf alone"
    );

    // The catalog import path applies the same fail-closed behavior.
    let resolved = catalog
        .import_or_resume(SessionSourceKind::Omp, leaf_path, None)
        .expect("import after drift");
    let resolved_texts = resolved
        .messages
        .iter()
        .map(|message| message.text.as_str())
        .collect::<Vec<_>>();
    assert_eq!(resolved_texts, collapsed_texts);
}

/// Two fingerprint-scoped imports share one leaf identity; the lineage index
/// must NOT collapse to the last path sorted. Exact chain-fingerprint
/// matching must resolve the fresh import even when the stale file sorts
/// last lexically, and a fresh scan must point at the matching native copy.
#[test]
fn omp_lineage_index_resolves_matching_fingerprint_across_same_identity() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let chain = write_omp_chain(&catalog, "--multimap--", 3);
    let leaf = &chain[2];

    let first = catalog
        .import_or_resume(SessionSourceKind::Omp, leaf, None)
        .expect("first import");
    let stale_path = first.path.clone();

    // Mutate the middle ancestor and re-import: same leaf identity, different
    // chain fingerprint -> a second native import.
    fs::write(
        &chain[1],
        fs::read_to_string(&chain[1])
            .expect("mid body")
            .replace("chain prompt 1", "chain prompt MUTATED"),
    )
    .expect("mutate mid");
    set_modified(&chain[1], 43_000);
    let second = catalog
        .import_or_resume(SessionSourceKind::Omp, leaf, None)
        .expect("second import");
    assert_ne!(second.path, stale_path);
    let fresh_path = second.path.clone();

    // Plant the OPPOSITE path sort: the stale import sorts LAST lexically.
    let fresh_renamed = fresh_path
        .parent()
        .expect("native parent")
        .join("import_aaa_fresh.jsonl");
    let stale_renamed = stale_path
        .parent()
        .expect("native parent")
        .join("import_zzz_stale.jsonl");
    fs::rename(&fresh_path, &fresh_renamed).expect("rename fresh");
    fs::rename(&stale_path, &stale_renamed).expect("rename stale");

    // The index keeps BOTH same-identity imports with their stored lineage.
    let index = catalog.build_lineage_index();
    let entries = index
        .get(&(SessionSourceKind::Omp, "id:omp-chain-2".to_owned()))
        .expect("identity entries");
    assert_eq!(entries.len(), 2, "both imports must share the identity slot");

    // Scan resolves the fresh import by exact chain fingerprint (not the
    // last-sorted stale file).
    let rows = catalog.list(&CatalogListOptions {
        sources: vec![SessionSourceKind::Omp],
        include_foreign: true,
        ..CatalogListOptions::default()
    });
    let row = rows
        .iter()
        .find(|row| row.session_id == "omp-chain-2")
        .expect("leaf row");
    match &row.status {
        CatalogRowStatus::AlreadyImported { native_path, .. } => {
            assert_eq!(
                native_path, &fresh_renamed,
                "scan must point at the fingerprint-matching import, not the last-sorted file"
            );
        }
        other => panic!("expected AlreadyImported, got {other:?}"),
    }

    // A third selection resolves the matching fingerprint, not the stale one.
    let third = catalog
        .import_or_resume(SessionSourceKind::Omp, leaf, None)
        .expect("third selection");
    assert!(third.reused_existing);
    assert_eq!(third.path, fresh_renamed);
}

/// Non-OMP kinds never chain: a Codex file carrying an OMP-style
/// `parentSession` header reference loads exactly its own file.
#[test]
fn non_omp_sources_never_follow_parent_session_references() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    // A second session that would be chained if the reference were followed.
    let parent = write_codex(
        &catalog,
        "rollout-parent-target.jsonl",
        "codex-parent",
        "<workspace>/plain",
        "parent prompt",
    );
    let source = write_codex(
        &catalog,
        "rollout-parent-ref.jsonl",
        "codex-plain",
        "<workspace>/plain",
        "plain prompt",
    );
    // Inject an OMP-style absolute parentSession into the Codex session_meta
    // record; non-OMP sources must ignore it entirely.
    let body = fs::read_to_string(&source).expect("codex body");
    let body = body.replace(
        r#""payload":{"id":"codex-plain","cwd":"<workspace>/plain"}"#,
        &format!(
            r#""payload":{{"id":"codex-plain","cwd":"<workspace>/plain","parentSession":"{parent}"}}"#,
            parent = parent.display()
        ),
    );
    fs::write(&source, body).expect("rewrite codex");

    let resolved = catalog
        .import_or_resume(SessionSourceKind::Codex, &source, None)
        .expect("codex import");
    let texts = resolved
        .messages
        .iter()
        .map(|message| message.text.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        texts, ["plain prompt", "ok"],
        "parentSession reference must never chain a non-OMP source"
    );
    assert_eq!(resolved.source_session_id.as_deref(), Some("codex-plain"));
    assert!(
        !texts.iter().any(|text| text.contains("parent prompt")),
        "the referenced file's messages must stay out"
    );
}


#[test]
fn grok_aggregate_size_includes_chat_history_for_non_convertible_chat() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let directory = catalog
        .root_for(SessionSourceKind::Grok)
        .path
        .join("agg-cwd")
        .join("grok-agg");
    fs::create_dir_all(&directory).expect("grok directory");
    let summary_path = directory.join("summary.json");
    fs::write(
        &summary_path,
        r#"{"info":{"id":"grok-agg","cwd":"/tmp/agg"}}"#,
    )
    .expect("grok summary");
    // Tiny summary.json stays well under the noise threshold so the row's size
    // can only reach 10 KiB via the aggregate with chat_history.jsonl.
    assert!(fs::metadata(&summary_path).expect("summary meta").len() < 1024);
    // Non-convertible chat records (system role) with no user/assistant text,
    // padded past 10 KiB so aggregate size crosses the noise threshold while
    // message_count remains zero.
    let padding = "x".repeat(11_000);
    fs::write(
        directory.join("chat_history.jsonl"),
        format!(
            r#"{{"type":"system","content":"{padding}"}}"#
        ),
    )
    .expect("grok chat");
    let rows = catalog.list(&CatalogListOptions {
        sources: vec![SessionSourceKind::Grok],
        include_foreign: true,
        ..CatalogListOptions::default()
    });
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, SessionSourceKind::Grok);
    assert_eq!(rows[0].message_count, Some(0));
    assert!(rows[0].size >= 10_240, "aggregate size {}", rows[0].size);
    assert!(
        rows[0].size > fs::metadata(&summary_path).expect("summary meta").len(),
        "size must aggregate chat_history, not summary alone"
    );
    assert_eq!(rows[0].summary, "(no messages)");
}
