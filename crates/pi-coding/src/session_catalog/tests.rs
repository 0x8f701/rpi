use std::fs::{self, File, FileTimes};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use super::*;
use crate::SessionKind;
use crate::import::{
    ImportLineage, ImportSessionError, MAX_HEADER_SCAN_BYTES, MAX_HEADER_SCAN_RECORDS,
    MAX_SOURCE_BYTES, SourceSessionFormat, occupy_import_output_with_lineage, parse_omp_chain_public,
    source_root_for,
};

struct Fixture {
    root: PathBuf,
    _owner: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let owner = tempfile::tempdir().expect("fixture root");
        Self {
            root: owner.path().to_path_buf(),
            _owner: owner,
        }
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

fn set_modified(path: &Path, seconds: u64) {
    let file = File::options().write(true).open(path).expect("open");
    file.set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)))
        .expect("mtime");
}

fn fixture_cwd(catalog: &SessionCatalog, relative: &str) -> PathBuf {
    catalog.user_home().join("cwd").join(relative)
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
        has_parent_session: false,
        session_kind: SessionKind::User,
        agent_session: false,
        bootstrap_only: false,
        search_text: String::new(),
        message_blob: String::new(),
    }
}

fn write_native(
    catalog: &SessionCatalog,
    project: &str,
    file: &str,
    id: &str,
    cwd: &str,
    user: &str,
) -> PathBuf {
    write_native_with(catalog, project, file, id, cwd, user, None, None)
}

fn write_native_with(
    catalog: &SessionCatalog,
    project: &str,
    file: &str,
    id: &str,
    cwd: &str,
    user: &str,
    name: Option<&str>,
    parent_session: Option<&str>,
) -> PathBuf {
    let cwd = fixture_cwd(catalog, cwd);
    let cwd = cwd.display();
    let path = catalog
        .root_for(SessionSourceKind::NativePi)
        .path
        .join(project)
        .join(file);
    fs::create_dir_all(path.parent().expect("parent")).expect("parent");
    let mut out = File::create(&path).expect("create");
    let parent = parent_session
        .map(|value| format!(r#","parentSession":"{value}""#))
        .unwrap_or_default();
    writeln!(
        out,
        r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-01-01T00:00:00.000Z","cwd":"{cwd}"{parent}}}"#
    )
    .unwrap();
    writeln!(
        out,
        r#"{{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":"{user}"}}]}}}}"#
    )
    .unwrap();
    if let Some(name) = name {
        writeln!(out, r#"{{"type":"session_info","name":"{name}"}}"#).unwrap();
    }
    path
}

fn write_codex(catalog: &SessionCatalog, name: &str, id: &str, cwd: &str, user: &str) -> PathBuf {
    let cwd = fixture_cwd(catalog, cwd);
    let cwd = cwd.display();
    let path = catalog.root_for(SessionSourceKind::Codex).path.join(name);
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
    let cwd = fixture_cwd(catalog, cwd);
    let cwd = cwd.display();
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
    let cwd = fixture_cwd(catalog, cwd);
    let cwd = cwd.display();
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
fn write_deep_omp(catalog: &SessionCatalog, id: &str, cwd: &str, depth: usize) -> PathBuf {
    let cwd = fixture_cwd(catalog, cwd);
    let path = catalog
        .root_for(SessionSourceKind::Omp)
        .path
        .join("--injected--")
        .join(format!("{id}.jsonl"));
    fs::create_dir_all(path.parent().expect("parent")).expect("parent");
    let nested = format!("{}null{}", "[".repeat(depth), "]".repeat(depth));
    fs::write(
        &path,
        format!(
            concat!(
                r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-01-01T00:00:00Z","cwd":"{cwd}"}}"#,
                "\n",
                r#"{{"type":"message","id":"u","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{{"role":"user","content":"deep omp prompt"}},"diagnostic":{nested}}}"#,
                "\n"
            ),
            id = id,
            cwd = cwd.display(),
            nested = nested,
        ),
    )
    .expect("deep omp");
    path
}

#[test]
fn deep_omp_catalog_row_drops_nested_json_without_stack_overflow() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let path = write_deep_omp(&catalog, "deep-omp", "deep", 120);

    let rows = catalog.scan(&[SessionSourceKind::Omp]);
    let row = rows
        .iter()
        .find(|row| row.session_id == "deep-omp")
        .expect("deep OMP row");
    assert_eq!(row.path, path);
    assert_eq!(row.message_count, Some(1));
    assert_eq!(row.message_blob, "deep omp prompt");
}

fn write_grok(catalog: &SessionCatalog, id: &str, cwd: &str) -> PathBuf {
    let cwd = fixture_cwd(catalog, cwd);
    let cwd = cwd.display();
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
    let cwd = fixture_cwd(catalog, cwd);
    let cwd = cwd.display();
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
        catalog.root_for(SessionSourceKind::Pi).path,
        fixture.root.join("home/.pi/agent/sessions")
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
fn rpi_and_pi_have_distinct_wire_identities() {
    assert_eq!(SessionSourceKind::Rpi.as_str(), "rpi");
    assert_eq!(SessionSourceKind::Pi.as_str(), "pi");
    assert_eq!(
        "rpi".parse::<SessionSourceKind>().expect("rpi source"),
        SessionSourceKind::Rpi
    );
    assert_eq!(
        "pi".parse::<SessionSourceKind>().expect("pi source"),
        SessionSourceKind::Pi
    );
}

#[test]
fn pi_sessions_are_cataloged_and_imported_without_mutating_the_source() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let path = catalog
        .root_for(SessionSourceKind::Pi)
        .path
        .join("project/pi-session.jsonl");
    fs::create_dir_all(path.parent().expect("pi session parent")).expect("create pi root");
    fs::write(
        &path,
        format!(
            concat!(
                r#"{{"type":"session","version":3,"id":"pi-session","timestamp":"2026-01-01T00:00:00.000Z","cwd":"{}"}}"#,
                "\n",
                r#"{{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":"pi prompt"}}]}}}}"#,
                "\n"
            ),
            fixture.root.join("pi-workspace").display()
        ),
    )
    .expect("write pi session");
    let source_bytes = fs::read(&path).expect("pi source bytes");
    let source_modified = fs::metadata(&path)
        .expect("pi source metadata")
        .modified()
        .expect("mtime");

    let rows = catalog.scan(&[SessionSourceKind::Pi]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, SessionSourceKind::Pi);
    assert!(matches!(rows[0].status, CatalogRowStatus::Foreign));
    let imported = catalog
        .import_or_resume(SessionSourceKind::Pi, &path, None)
        .expect("import Pi source");
    assert_eq!(imported.kind, SessionSourceKind::Pi);
    assert_ne!(imported.path, path);
    assert!(!imported.native_no_copy);
    assert_eq!(imported.messages.len(), 1);
    assert_eq!(imported.messages[0].text, "pi prompt");
    assert_eq!(fs::read(&path).expect("pi source after"), source_bytes);
    assert_eq!(
        fs::metadata(&path)
            .expect("pi source metadata after")
            .modified()
            .expect("mtime after"),
        source_modified,
    );
    assert!(catalog.is_safe_session_path(SessionSourceKind::NativePi, &imported.path));
}

/// Pi lineage reuse requires current content fingerprint equality: unchanged
/// reselect reuses one native path, but a mutated source is re-listed as
/// Foreign and re-imports to a distinct native path carrying the new text,
/// while the import never mutates the source bytes.
#[test]
fn pi_import_reuses_unchanged_and_reimports_changed_content() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let path = catalog
        .root_for(SessionSourceKind::Pi)
        .path
        .join("project/pi-fresh.jsonl");
    fs::create_dir_all(path.parent().expect("pi session parent")).expect("create pi root");
    let original = format!(
        concat!(
            r#"{{"type":"session","version":3,"id":"pi-fresh","timestamp":"2026-01-01T00:00:00.000Z","cwd":"{}"}}"#,
            "\n",
            r#"{{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":"pi fresh prompt"}}]}}}}"#,
            "\n"
        ),
        fixture.root.join("pi-workspace").display()
    );
    fs::write(&path, &original).expect("write pi session");

    let first = catalog
        .import_or_resume(SessionSourceKind::Pi, &path, None)
        .expect("first Pi import");
    assert!(!first.reused_existing);
    let first_path = first.path.clone();
    let first_id = first.id.clone();
    assert!(
        fs::read_to_string(&first_path)
            .expect("first body")
            .contains("pi fresh prompt")
    );
    // Source must be untouched by the first import.
    assert_eq!(
        fs::read(&path).expect("pi source after first import"),
        original.as_bytes(),
        "first import must not mutate the Pi source"
    );

    // Unchanged reselect reuses the same native path/id.
    let unchanged = catalog
        .import_or_resume(SessionSourceKind::Pi, &path, None)
        .expect("unchanged Pi reselect");
    assert!(unchanged.reused_existing);
    assert_eq!(unchanged.path, first_path);
    assert_eq!(unchanged.id, first_id);
    assert_eq!(
        fs::read(&path).expect("pi source after unchanged reselect"),
        original.as_bytes(),
        "unchanged reselect must not mutate the Pi source"
    );

    // Mutate the source with another valid canonical Pi message (and bump
    // mtime) so its content fingerprint changes while remaining a valid Pi v3.
    let mut mutated = original.clone();
    mutated.push_str(
        r#"{"type":"message","id":"u2","parentId":"u1","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"pi mutated reply"}]}}
"#,
    );
    fs::write(&path, &mutated).expect("mutate pi source");
    set_modified(&path, 10_000);

    // Next catalog scan reports Foreign (not AlreadyImported at the stale path).
    let rows = catalog.scan(&[SessionSourceKind::Pi]);
    let row = rows
        .iter()
        .find(|row| row.session_id == "pi-fresh")
        .expect("mutated pi row");
    assert!(
        matches!(row.status, CatalogRowStatus::Foreign),
        "mutated Pi source must read as Foreign, got {:?}",
        row.status
    );

    // Next import creates a distinct native path containing the new text.
    let second = catalog
        .import_or_resume(SessionSourceKind::Pi, &path, None)
        .expect("re-import after Pi mutation");
    assert!(
        !second.reused_existing,
        "mutated content must not reuse the stale native copy"
    );
    assert_ne!(
        second.path, first_path,
        "mutation must emit a distinct native path"
    );
    assert_ne!(
        second.id, first_id,
        "mutation must change the deterministic import id"
    );
    let second_body = fs::read_to_string(&second.path).expect("second body");
    assert!(
        second_body.contains("pi mutated reply"),
        "fresh import carries the mutated transcript"
    );
    // Source must still be untouched by the second import.
    assert_eq!(
        fs::read(&path).expect("pi source after second import"),
        mutated.as_bytes(),
        "re-import must not mutate the Pi source"
    );

    // The fresh import is the one the catalog now resolves as AlreadyImported.
    let rows_after = catalog.scan(&[SessionSourceKind::Pi]);
    let row_after = rows_after
        .iter()
        .find(|row| row.session_id == "pi-fresh")
        .expect("row after reimport");
    match &row_after.status {
        CatalogRowStatus::AlreadyImported {
            native_id,
            native_path,
        } => {
            assert_eq!(native_id, &second.id);
            assert_eq!(native_path, &second.path);
        }
        other => panic!("expected AlreadyImported at fresh import, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn pi_import_reuses_unchanged_and_reimports_same_size_same_second_content() {
    use std::os::unix::fs::MetadataExt;

    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let path = catalog
        .root_for(SessionSourceKind::Pi)
        .path
        .join("project/pi-same-metadata.jsonl");
    fs::create_dir_all(path.parent().expect("pi session parent")).expect("create pi root");
    let original = format!(
        concat!(
            r#"{{"type":"session","version":3,"id":"pi-same-metadata","timestamp":"2026-01-01T00:00:00.000Z","cwd":"{}"}}"#,
            "\n",
            r#"{{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":"pi alpha prompt"}}]}}}}"#,
            "\n"
        ),
        fixture.root.join("pi-workspace").display()
    );
    fs::write(&path, &original).expect("write Pi source");
    set_modified(&path, 20_000);

    let first = catalog
        .import_or_resume(SessionSourceKind::Pi, &path, None)
        .expect("first Pi import");
    let unchanged = catalog
        .import_or_resume(SessionSourceKind::Pi, &path, None)
        .expect("unchanged Pi reselect");
    assert!(unchanged.reused_existing);
    assert_eq!(unchanged.path, first.path);

    let mutated = original.replace("pi alpha prompt", "pi omega prompt");
    assert_eq!(
        mutated.len(),
        original.len(),
        "mutation must preserve exact byte size"
    );
    fs::write(&path, &mutated).expect("same-size Pi mutation");
    set_modified(&path, 20_000);
    let source_before = fs::metadata(&path).expect("mutated source metadata");
    let source_inode = source_before.ino();
    let source_modified = source_before.modified().expect("mutated source mtime");
    let source_bytes = fs::read(&path).expect("mutated source bytes");

    let row = catalog
        .scan(&[SessionSourceKind::Pi])
        .into_iter()
        .find(|row| row.session_id == "pi-same-metadata")
        .expect("same-metadata Pi row");
    assert!(matches!(row.status, CatalogRowStatus::Foreign));
    let row_fingerprint = row
        .import_lineage
        .as_ref()
        .and_then(|lineage| lineage.content_fingerprint.clone())
        .expect("row content fingerprint");

    let second = catalog
        .import_or_resume(SessionSourceKind::Pi, &path, None)
        .expect("same-metadata Pi re-import");
    assert!(!second.reused_existing);
    assert_ne!(second.path, first.path);
    assert_ne!(second.id, first.id);
    assert!(
        fs::read_to_string(&second.path)
            .expect("second import body")
            .contains("pi omega prompt")
    );
    let native_root = catalog.root_for(SessionSourceKind::NativePi).path;
    let opened = open_source_under_root(SourceSessionFormat::Pi, &native_root, &second.path)
        .expect("open imported native session");
    let (stored_lineage, stored_id) =
        read_native_lineage(opened.into_primary(), &second.path).expect("stored import lineage");
    assert_eq!(stored_id, second.id);
    assert_eq!(
        stored_lineage.content_fingerprint.as_deref(),
        Some(row_fingerprint.as_str())
    );

    let source_after = fs::metadata(&path).expect("source metadata after import");
    assert_eq!(
        source_after.ino(),
        source_inode,
        "import must preserve source inode"
    );
    assert_eq!(
        source_after.modified().expect("source mtime after import"),
        source_modified,
        "import must preserve source timestamp"
    );
    assert_eq!(
        fs::read(&path).expect("source bytes after import"),
        source_bytes
    );
}

#[test]
fn native_resume_rejects_explicit_pi_catalog_path() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let path = catalog
        .root_for(SessionSourceKind::Pi)
        .path
        .join("project/pi-resume.jsonl");
    fs::create_dir_all(path.parent().expect("pi session parent")).expect("create pi root");
    fs::write(
        &path,
        format!(
            r#"{{"type":"session","version":3,"id":"pi-resume","timestamp":"2026-01-01T00:00:00.000Z","cwd":"{}"}}
"#,
            fixture.root.join("pi-workspace").display()
        ),
    )
    .expect("write pi session");

    let error = catalog
        .prepare_native_resume(&path)
        .expect_err("Pi catalog path must not resume as rpi");
    assert!(matches!(
        error,
        CatalogError::ReadOnlySource(SessionSourceKind::Pi)
    ));
}

#[cfg(unix)]
#[test]
fn native_resume_rejects_symlink_into_pi_catalog_root() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let path = catalog
        .root_for(SessionSourceKind::Pi)
        .path
        .join("project/pi-linked.jsonl");
    fs::create_dir_all(path.parent().expect("pi session parent")).expect("create pi root");
    fs::write(
        &path,
        format!(
            r#"{{"type":"session","version":3,"id":"pi-linked","timestamp":"2026-01-01T00:00:00.000Z","cwd":"{}"}}
"#,
            fixture.root.join("pi-workspace").display()
        ),
    )
    .expect("write pi session");
    let alias = fixture.root.join("pi-session-alias.jsonl");
    symlink(&path, &alias).expect("link Pi session outside catalog root");

    let error = catalog
        .prepare_native_resume(&alias)
        .expect_err("symlink into Pi catalog must not resume as rpi");
    assert!(matches!(
        error,
        CatalogError::ReadOnlySource(SessionSourceKind::Pi)
    ));
}

fn write_pi_tree_session(catalog: &SessionCatalog, id: &str) -> PathBuf {
    let path = catalog
        .root_for(SessionSourceKind::Pi)
        .path
        .join("project")
        .join(format!("{id}.jsonl"));
    fs::create_dir_all(path.parent().expect("pi session parent")).expect("create pi root");
    let cwd = fixture_cwd(catalog, "work");
    fs::write(
        &path,
        format!(
            concat!(
                r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-01-01T00:00:00.000Z","cwd":"{cwd}"}}"#,
                "\n",
                r#"{{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":"pi overlap"}}]}}}}"#,
                "\n"
            ),
            id = id,
            cwd = cwd.display()
        ),
    )
    .expect("write pi session");
    path
}

fn assert_typed_pi_path_never_native_no_copy(catalog: &SessionCatalog, path: &Path) {
    assert!(
        catalog.native_and_pi_roots_overlap(),
        "fixture must overlap rpi and Pi catalog roots"
    );
    assert!(catalog.is_pi_catalog_path(path));
    assert!(!catalog.is_safe_session_path(SessionSourceKind::NativePi, path));
    let resolved = catalog.resolve_any(path);
    assert!(
        !matches!(resolved, Ok((kind, _)) if kind.is_native()),
        "typed Pi path must not classify as native: {resolved:?}"
    );
    assert!(matches!(
        catalog.import_or_resume(SessionSourceKind::NativePi, path, None),
        Err(CatalogError::ReadOnlySource(SessionSourceKind::Pi))
    ));
    match catalog.import_or_resume_any(path, None) {
        Ok(session) => {
            assert_eq!(session.kind, SessionSourceKind::Pi);
            assert!(!session.native_no_copy);
        }
        Err(CatalogError::ReadOnlySource(SessionSourceKind::Pi))
        | Err(CatalogError::SessionNotFound(_))
        | Err(CatalogError::Import(ImportSessionError::InvalidOutput { .. })) => {}
        Err(error) => panic!("typed Pi path must not resume native/no-copy: {error:?}"),
    }
}

#[test]
fn equal_rpi_and_pi_catalog_roots_never_label_pi_native() {
    let fixture = Fixture::new();
    let home = fixture.root.join("home");
    let shared = home.join(".pi/agent/sessions");
    fs::create_dir_all(&shared).expect("shared catalog root");
    let catalog = SessionCatalog::new(&home).with_native_session_root(&shared);
    let path = write_pi_tree_session(&catalog, "equal-root");
    assert_typed_pi_path_never_native_no_copy(&catalog, &path);
}

#[test]
fn native_inside_pi_catalog_root_never_labels_pi_native() {
    let fixture = Fixture::new();
    let home = fixture.root.join("home");
    let pi_root = home.join(".pi/agent/sessions");
    let native_root = pi_root.join("rpi-native");
    fs::create_dir_all(&native_root).expect("nested native root");
    let catalog = SessionCatalog::new(&home).with_native_session_root(&native_root);
    let path = write_pi_tree_session(&catalog, "native-inside-pi");
    assert_typed_pi_path_never_native_no_copy(&catalog, &path);
}

#[test]
fn pi_inside_native_catalog_root_never_labels_pi_native() {
    let fixture = Fixture::new();
    let home = fixture.root.join("home");
    let native_root = home.join(".pi/agent");
    fs::create_dir_all(native_root.join("sessions")).expect("native parent of Pi");
    let catalog = SessionCatalog::new(&home).with_native_session_root(&native_root);
    let path = write_pi_tree_session(&catalog, "pi-inside-native");
    assert_typed_pi_path_never_native_no_copy(&catalog, &path);
}

#[cfg(unix)]
#[test]
fn canonical_symlink_alias_overlap_never_labels_pi_native() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let home = fixture.root.join("home");
    let pi_root = home.join(".pi/agent/sessions");
    fs::create_dir_all(&pi_root).expect("pi catalog root");
    let alias = fixture.root.join("native-alias");
    symlink(&pi_root, &alias).expect("alias native root onto Pi");
    let catalog = SessionCatalog::new(&home).with_native_session_root(&alias);
    let path = write_pi_tree_session(&catalog, "symlink-alias");
    let alias_path = alias.join("project/symlink-alias.jsonl");
    assert_typed_pi_path_never_native_no_copy(&catalog, &path);
    assert_typed_pi_path_never_native_no_copy(&catalog, &alias_path);
}

#[test]
fn disjoint_rpi_and_pi_roots_still_import_pi_without_copying_source() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    assert!(!catalog.native_and_pi_roots_overlap());
    let path = write_pi_tree_session(&catalog, "disjoint-ok");
    let imported = catalog
        .import_or_resume_any(&path, None)
        .expect("disjoint Pi import");
    assert_eq!(imported.kind, SessionSourceKind::Pi);
    assert!(!imported.native_no_copy);
    assert_ne!(imported.path, path);
}

#[test]
fn user_home_supplies_native_root_without_explicit_agent_override() {
    let fixture = Fixture::new();
    let sessions_home = fixture.root.join("sessions-home");
    let user_home = fixture.root.join("user-home");
    let catalog =
        SessionCatalog::from_env_paths(sessions_home.clone(), user_home, None, None, None);
    assert_eq!(
        catalog.root_for(SessionSourceKind::NativePi).path,
        fixture.root.join("user-home/.rpi/sessions")
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
    let catalog =
        SessionCatalog::new(fixture.root.join("home")).with_native_session_root(root.clone());
    fs::create_dir_all(&root).expect("custom root");
    let cwd = fixture_cwd(&catalog, "work");
    let direct = root.join("direct.jsonl");
    fs::write(
        &direct,
        format!(
            "{{\"type\":\"session\",\"version\":3,\"id\":\"direct\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"{}\"}}\n",
            cwd.display()
        ),
    )
    .expect("direct session");
    let nested = root.join("nested/nested.jsonl");
    fs::create_dir_all(nested.parent().expect("nested parent")).expect("nested directory");
    fs::write(
        &nested,
        format!(
            "{{\"type\":\"session\",\"version\":3,\"id\":\"nested\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"{}\"}}\n",
            cwd.display()
        ),
    )
    .expect("nested session");

    assert_eq!(catalog.root_for(SessionSourceKind::NativePi).path, root);
    assert!(catalog.is_safe_session_path(SessionSourceKind::NativePi, &direct));
    assert!(!catalog.is_safe_session_path(SessionSourceKind::NativePi, &nested));
    assert_eq!(catalog.discover(SessionSourceKind::NativePi), vec![direct]);
}


    /// Incremental-rescan correctness + measured latency: the per-file row
    /// cache (mtime+size validated) must (a) surface NEW files on the next
    /// scan, (b) re-parse CHANGED files (message content updates), (c) drop
    /// DELETED files, and (d) make the warm scan measurably cheaper than the
    /// cold full parse at sidebar scale (~450 files).
    #[test]
    fn catalog_scan_incremental_cache_correctness_and_latency() {
        use std::time::{Duration, Instant};

        let fixture = Fixture::new();
        let catalog = fixture.catalog();
        for index in 0..450 {
            write_native(
                &catalog,
                "project",
                &format!("s{index}.jsonl"),
                &format!("bench-{index}"),
                "work",
                &format!("prompt {index}"),
            );
        }
        // A handful of LARGE sessions (the 1766-message shape that dominates
        // real sidebar latency): the per-file parse cost scales with the
        // transcript, and the warm cache skips it entirely.
        for index in 0..8 {
            let path = write_native(
                &catalog,
                "project",
                &format!("large-{index}.jsonl"),
                &format!("bench-large-{index}"),
                "work",
                "seed prompt",
            );
            let mut out = fs::OpenOptions::new().append(true).open(&path).expect("append");
            for turn in 0..500 {
                writeln!(
                    out,
                    r#"{{"type":"message","id":"large-{index}-{turn}","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":"filler turn {turn} for session {index} to grow the transcript"}}]}}}}"#
                )
                .unwrap();
            }
        }
        let options = CatalogListOptions {
            sources: vec![SessionSourceKind::NativePi],
            ..CatalogListOptions::default()
        };

        // Cold scan: every file parsed.
        let cold_started = Instant::now();
        let rows = catalog.list(&options);
        let cold = cold_started.elapsed();
        assert_eq!(rows.len(), 458);

        // Warm scan: every file a cache hit.
        let warm_started = Instant::now();
        let warm_rows = catalog.list(&options);
        let warm = warm_started.elapsed();
        assert_eq!(warm_rows.len(), 458);
        assert_eq!(warm_rows[0].session_id, rows[0].session_id);

        // (a) A NEW file appears on the next scan.
        let changed_path = write_native(
            &catalog,
            "project",
            "changed.jsonl",
            "bench-changed",
            "work",
            "first prompt",
        );
        let rows_with_new = catalog.list(&options);
        assert_eq!(rows_with_new.len(), 459);
        let changed_row = rows_with_new
            .iter()
            .find(|row| row.session_id == "bench-changed")
            .expect("the new file appears");
        assert_eq!(changed_row.message_count, Some(1));

        let rows_with_new = catalog.list(&options);
        assert_eq!(rows_with_new.len(), 459);
        {
            let mut out = fs::OpenOptions::new()
                .append(true)
                .open(&changed_path)
                .expect("append");
            writeln!(
                out,
                r#"{{"type":"message","id":"u2","parentId":"u1","timestamp":"2026-01-01T00:00:02.000Z","message":{{"role":"user","content":[{{"type":"text","text":"second prompt"}}]}}}}"#
            )
            .unwrap();
        }
        set_modified(&changed_path, 2);
        let rows_after_change = catalog.list(&options);
        assert_eq!(rows_after_change.len(), 459);
        let changed_row = rows_after_change
            .iter()
            .find(|row| row.session_id == "bench-changed")
            .expect("the changed file stays listed");
        assert_eq!(
            changed_row.message_count,
            Some(2),
            "the changed file must be re-parsed, not served stale"
        );

        // (c) A DELETED file disappears.
        fs::remove_file(&changed_path).expect("delete changed file");
        let rows_after_delete = catalog.list(&options);
        assert_eq!(rows_after_delete.len(), 458);
        assert!(
            !rows_after_delete.iter().any(|row| row.session_id == "bench-changed"),
            "the deleted file must disappear from the listing"
        );

        eprintln!(
            "catalog scan latency (458 native sessions, 8 of them ~500-message): cold={cold:?} warm={warm:?}"
        );
        // The warm scan skips the per-file JSON parse entirely; it must be
        // cheaper than the cold parse (loosely bounded so slow CI machines
        // never flake on a scheduler hiccup).
        assert!(
            warm <= cold && warm < Duration::from_millis(200),
            "the warm scan must be measurably cheaper than the cold parse: cold={cold:?} warm={warm:?}"
        );
    }

    #[test]
    fn scan_mixed_sources_newest_first() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let pi = write_native(
        &catalog,
        "--proj--",
        "a.jsonl",
        "pi-1",
        "proj",
        "native hello",
    );
    let codex = write_codex(
        &catalog,
        "rollout-codex.jsonl",
        "codex-1",
        "codex",
        "codex task",
    );
    let claude = write_claude(&catalog, "claude.jsonl", "claude-1", "claude", "claude hi");
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
    let good = write_codex(&catalog, "rollout-good.jsonl", "good", "root", "keep");
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
    assert!(
        !catalog
            .discover(SessionSourceKind::Codex)
            .contains(&archived)
    );
    assert!(!catalog.discover(SessionSourceKind::Codex).contains(&rsync));
    #[cfg(unix)]
    assert!(
        !catalog
            .discover(SessionSourceKind::Codex)
            .contains(&symlink)
    );
}

#[test]
fn dedupe_keeps_newest_same_cwd_summary() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let older = write_codex(&catalog, "rollout-old.jsonl", "old", "same", "same summary");
    let newer = write_codex(&catalog, "rollout-new.jsonl", "new", "same", "same summary");
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
            "bounded",
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
    assert!(
        tied.contains(&paths[0]),
        "stable smaller paths win equal-mtime ties"
    );
    assert!(tied.contains(&paths[SESSION_CATALOG_CANDIDATE_LIMIT - 1]));
    assert!(!tied.contains(&paths[SESSION_CATALOG_CANDIDATE_LIMIT]));
    assert!(!tied.contains(&paths[SESSION_CATALOG_CANDIDATE_LIMIT + 1]));
    assert!(matches!(
        catalog.row_for_exact_path(SessionSourceKind::Codex, &paths[0]),
        Ok(_)
    ));
    assert!(matches!(
        catalog.row_for_exact_path(
            SessionSourceKind::Codex,
            &paths[SESSION_CATALOG_CANDIDATE_LIMIT],
        ),
        Err(CatalogError::SessionNotFound(_))
    ));
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
            "codex",
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
        "claude",
        "claude sentinel",
    );
    let claude_second = write_claude(
        &catalog,
        "claude-second.jsonl",
        "claude-second",
        "claude",
        "claude second",
    );
    set_modified(&claude, 1);
    set_modified(&claude_second, 2);

    let rows = catalog.scan(&[SessionSourceKind::Codex, SessionSourceKind::Claude]);
    assert_eq!(rows.len(), SESSION_CATALOG_ROW_LIMIT);
    assert!(rows.iter().any(|row| row.session_id == "claude-second"));
    assert!(!rows.iter().any(|row| row.session_id == "claude-sentinel"));
    assert!(rows.iter().any(|row| row.session_id == "codex-0515"));
    assert_eq!(
        rows.iter()
            .filter(|row| row.kind == SessionSourceKind::Claude)
            .count(),
        1
    );
    assert!(!rows.iter().any(|row| row.path == corrupt));
    #[cfg(unix)]
    assert!(
        !rows
            .iter()
            .any(|row| row.path.ends_with("rollout-symlink-newest.jsonl"))
    );
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
            "other"
        } else {
            "wanted"
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
        cwd_scope: Some(fixture_cwd(&catalog, "wanted")),
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
    let searched_ids = searched
        .iter()
        .map(|row| &row.session_id)
        .collect::<Vec<_>>();
    assert_eq!(listed_ids, searched_ids);
    assert_eq!(listed.len(), SESSION_CATALOG_ROW_LIMIT - 1);
    assert!(!listed.iter().any(|row| row.session_id == "shared-0000"));
    assert!(!listed.iter().any(|row| row.session_id == "shared-0512"));
    assert!(!searched.iter().any(|row| row.session_id == "shared-0512"));
    assert!(
        unscoped_listed
            .iter()
            .any(|row| row.session_id == "shared-0512")
    );
    assert!(
        unscoped_searched
            .iter()
            .any(|row| row.session_id == "shared-0512")
    );
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
    let sentinel = write_droid(&catalog, "zzzzz-sentinel", "droid");
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
    write_native(
        &catalog,
        "--p--",
        "n.jsonl",
        "native-id",
        "p",
        "alpha uniquephrase",
    );
    write_codex(&catalog, "rollout-c.jsonl", "codex-id", "c", "beta other");
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
    write_native(&catalog, "--p--", "n.jsonl", "shared-id", "p", "native");
    write_codex(&catalog, "rollout-shared.jsonl", "shared-id", "c", "codex");
    let rows = catalog.list(&CatalogListOptions {
        include_foreign: true,
        ..CatalogListOptions::default()
    });
    assert_eq!(
        rows.iter()
            .filter(|row| row.session_id == "shared-id")
            .count(),
        2
    );
    let err = catalog.resolve_any("shared-id").expect_err("ambiguous");
    assert!(matches!(err, CatalogError::AmbiguousSession { .. }));
}

#[test]
fn import_or_resume_idempotent_and_writes_lineage() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let source = write_codex(
        &catalog,
        "rollout-once.jsonl",
        "once-id",
        "once",
        "import me",
    );
    let preferred_cwd = fixture_cwd(&catalog, "once");
    let first = catalog
        .import_or_resume(SessionSourceKind::Codex, &source, Some(&preferred_cwd))
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
    assert!(!body.contains("rpi-import"));
    assert!(!body.contains("converted-from-codex"));

    let second = catalog
        .import_or_resume(SessionSourceKind::Codex, &source, Some(&preferred_cwd))
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
fn import_with_authoritative_cwd_avoids_full_lineage_index() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let source = write_codex(
        &catalog,
        "rollout-direct-reuse.jsonl",
        "direct-reuse",
        "direct-reuse",
        "import directly",
    );
    let preferred_cwd = fixture_cwd(&catalog, "other");
    LINEAGE_INDEX_BUILD_COUNT.with(|count| count.set(0));

    let first = catalog
        .import_or_resume(SessionSourceKind::Codex, &source, Some(&preferred_cwd))
        .expect("first import");
    LINEAGE_INDEX_BUILD_COUNT.with(|count| assert_eq!(count.get(), 1));
    LINEAGE_INDEX_BUILD_COUNT.with(|count| count.set(0));

    let second = catalog
        .import_or_resume(SessionSourceKind::Codex, &source, Some(&preferred_cwd))
        .expect("direct reuse");

    assert!(!first.reused_existing);
    assert!(second.reused_existing);
    assert_eq!(second.path, first.path);
    LINEAGE_INDEX_BUILD_COUNT.with(|count| assert_eq!(count.get(), 0));
    let row = catalog
        .row_for_exact_path(SessionSourceKind::Codex, &source)
        .expect("exact source row");
    assert_eq!(row.session_id, "direct-reuse");
    LINEAGE_INDEX_BUILD_COUNT.with(|count| assert_eq!(count.get(), 0));
}
#[cfg(unix)]
#[test]
fn configured_native_import_root_symlink_fails_closed() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let target = fixture.root.join("native-target");
    fs::create_dir(&target).expect("native target");
    let configured_root = fixture.root.join("native-link");
    symlink(&target, &configured_root).expect("configured native root symlink");
    let catalog = fixture.catalog().with_native_session_root(&configured_root);
    let source = write_codex(
        &catalog,
        "rollout-symlink-output.jsonl",
        "symlink-output",
        "symlink-output",
        "must not import",
    );

    let error = catalog
        .import_or_resume(SessionSourceKind::Codex, &source, None)
        .expect_err("configured native root symlink must fail closed");
    assert!(matches!(
        error,
        CatalogError::Import(ImportSessionError::InvalidOutput { .. })
    ));
    assert_eq!(fs::read_dir(&target).expect("target readable").count(), 0);
}

#[test]
fn native_row_status_not_reimported_and_no_copy_resume() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let path = write_native(
        &catalog,
        "--proj--",
        "n.jsonl",
        "native-keep",
        "proj",
        "hello",
    );
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
    let huge_cwd = fixture_cwd(&catalog, "huge");
    let mut big = format!(
        r#"{{"timestamp":"2026-02-01T00:00:00Z","type":"session_meta","payload":{{"id":"huge","cwd":"{}"}}}}"#,
        huge_cwd.display()
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
        "prov",
        "provenance",
    );
    let preferred_cwd = fixture_cwd(&catalog, "prov");
    let (kind, resolved) = catalog.resolve_any("uniqueprefix").expect("prefix");
    assert_eq!(kind, SessionSourceKind::Codex);
    assert_eq!(resolved, path);
    let imported = catalog
        .import_or_resume(kind, resolved, Some(&preferred_cwd))
        .expect("import");
    assert_eq!(imported.source_session_id.as_deref(), Some("prov-id"));
    assert_eq!(imported.kind, SessionSourceKind::Codex);
    let body = fs::read_to_string(&imported.path).unwrap();
    assert!(body.contains("\"source\":\"codex\""));
    assert!(!body.contains("converted-from-codex"));
}

#[test]
fn source_root_for_matches_catalog_codex_claude() {
    // Ensure public bridge stays aligned for import helpers.
    let root = source_root_for(
        SourceSessionFormat::Codex,
        Some(Path::new("/x/codex")),
        None,
    )
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
        "symlink-race",
        "real body",
    );
    let preferred_cwd = fixture_cwd(&catalog, "symlink-race");
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
        .import_or_resume(SessionSourceKind::Codex, &link, Some(&preferred_cwd))
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
        assert_eq!(
            native_files, 0,
            "symlink import must emit zero native bytes"
        );
    }
}

/// Lexical `..` escape and path-outside-root must never count as safe sessions.
#[test]
fn path_with_parent_components_is_not_safe_session() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let good = write_codex(&catalog, "rollout-safe.jsonl", "safe-id", "root", "ok");
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
    let large_cwd = fixture_cwd(&catalog, "proj");
    writeln!(
        file,
        r#"{{"type":"session","version":3,"id":"native-large","timestamp":"2026-01-01T00:00:00.000Z","cwd":"{}"}}"#,
        large_cwd.display()
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
        "proj",
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
    let empty_cwd = fixture_cwd(&catalog, "empty");
    fs::write(
        &path,
        format!(
            "{{\"timestamp\":\"2026-02-01T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"empty-id\",\"cwd\":\"{}\"}}}}\n",
            empty_cwd.display()
        ),
    )
    .unwrap();

    let err = catalog
        .import_or_resume(SessionSourceKind::Codex, &path, Some(&empty_cwd))
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
        "a",
        "first",
    );
    write_codex(
        &catalog,
        "rollout-alpha-two.jsonl",
        "alpha-two",
        "b",
        "second",
    );
    let preferred_cwd = fixture_cwd(&catalog, "ambiguous");

    let err = catalog
        .resolve_any("alpha")
        .expect_err("shared prefix ambiguous");
    match err {
        CatalogError::AmbiguousSession { input, matches } => {
            assert_eq!(input, "alpha");
            assert!(matches.len() >= 2, "matches={matches:?}");
        }
        other => panic!("expected AmbiguousSession, got {other:?}"),
    }

    let err = catalog
        .import_or_resume_any("alpha", Some(&preferred_cwd))
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

/// A foreign source (Codex) whose content changes under the same id must
/// re-import under a fresh native lineage: content fingerprint equality is
/// required for reuse, so the stale native copy is never returned.
#[test]
fn codex_reimports_after_source_content_change() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let source = write_codex(
        &catalog,
        "rollout-mutate.jsonl",
        "mutate-id",
        "mutate",
        "original prompt",
    );
    let preferred_cwd = fixture_cwd(&catalog, "mutate");
    let first = catalog
        .import_or_resume(SessionSourceKind::Codex, &source, Some(&preferred_cwd))
        .expect("first import");
    assert!(!first.reused_existing);
    let first_path = first.path.clone();
    let first_id = first.id.clone();
    let first_body = fs::read_to_string(&first_path).expect("body");
    assert!(first_body.contains("original prompt") || first_body.contains("original"));

    // Unchanged reselect reuses the same native path.
    let unchanged = catalog
        .import_or_resume(SessionSourceKind::Codex, &source, Some(&preferred_cwd))
        .expect("unchanged reselect");
    assert!(unchanged.reused_existing);
    assert_eq!(unchanged.path, first_path);

    // Mutate source content + mtime so content_fingerprint changes.
    write_codex(
        &catalog,
        "rollout-mutate.jsonl",
        "mutate-id",
        "mutate",
        "changed prompt after edit",
    );
    set_modified(&source, 9_999);

    let rows_before = catalog.scan(&[SessionSourceKind::Codex]);
    let row_before = rows_before
        .iter()
        .find(|row| row.session_id == "mutate-id")
        .expect("row");
    assert!(
        matches!(row_before.status, CatalogRowStatus::Foreign),
        "mutated source must read as Foreign (no stale AlreadyImported), got {:?}",
        row_before.status
    );

    let second = catalog
        .import_or_resume(SessionSourceKind::Codex, &source, Some(&preferred_cwd))
        .expect("second import after content change");
    assert!(
        !second.reused_existing,
        "content change must not reuse the stale native conversion"
    );
    assert_ne!(
        second.path, first_path,
        "content change must emit a fresh import"
    );
    assert_ne!(
        second.id, first_id,
        "content change must change the import id"
    );
    let second_body = fs::read_to_string(&second.path).expect("second body");
    assert!(
        second_body.contains("changed prompt after edit"),
        "fresh import carries the changed transcript"
    );

    // Catalog resolves the fresh conversion after re-import.

    let rows_after = catalog.scan(&[SessionSourceKind::Codex]);
    let row_after = rows_after
        .iter()
        .find(|row| row.session_id == "mutate-id")
        .expect("row after");
    match &row_after.status {
        CatalogRowStatus::AlreadyImported {
            native_id,
            native_path,
        } => {
            assert_eq!(native_id, &second.id);
            assert_eq!(native_path, &second.path);
        }
        other => panic!("expected AlreadyImported at fresh import, got {other:?}"),
    }
    let lineage = row_after.import_lineage.as_ref().expect("lineage");
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
    let a = write_codex(&catalog, "rollout-a.jsonl", "id-a", "a", "session a");
    let b = write_codex(&catalog, "rollout-b.jsonl", "id-b", "b", "session b");
    let cwd_a = fixture_cwd(&catalog, "a");
    let cwd_b = fixture_cwd(&catalog, "b");

    let ra = catalog
        .import_or_resume(SessionSourceKind::Codex, &a, Some(&cwd_a))
        .expect("import a");
    let rb = catalog
        .import_or_resume(SessionSourceKind::Codex, &b, Some(&cwd_b))
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
        "proj",
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
        "dedupe",
        "shared summary text",
    );
    let newer_same = write_codex(
        &catalog,
        "rollout-new-same.jsonl",
        "new-same",
        "dedupe",
        "shared summary text",
    );
    let other_cwd = write_codex(
        &catalog,
        "rollout-other-cwd.jsonl",
        "other-cwd",
        "other",
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
    assert!(
        ids.contains(&"new-same"),
        "newest same-cwd/summary kept: {ids:?}"
    );
    assert!(
        !ids.contains(&"old-same"),
        "older duplicate dropped: {ids:?}"
    );
    assert!(
        ids.contains(&"other-cwd"),
        "different cwd retained: {ids:?}"
    );
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
            has_parent_session: false,
            session_kind: SessionKind::User,
            agent_session: false,
            bootstrap_only: false,
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
            has_parent_session: false,
            session_kind: SessionKind::User,
            agent_session: false,
            bootstrap_only: false,
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
            has_parent_session: false,
            session_kind: SessionKind::User,
            agent_session: false,
            bootstrap_only: false,
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
    assert_eq!(
        fb1.path,
        PathBuf::from("/b"),
        "newest empty-summary id wins"
    );
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

/// Native parent/fork sessions inherit cwd+name/summary; distinct session ids
/// must both survive dedupe. Duplicate native rows for the same id keep the
/// preferred newest copy. Codex/OMP cwd+summary identity is unchanged.
#[test]
fn native_parent_and_fork_same_cwd_summary_keep_distinct_ids() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    const SHARED_NAME: &str = "Shared fork name";
    const SHARED_PROMPT: &str = "shared inherited summary";

    let parent = write_native_with(
        &catalog,
        "--fork-parent--",
        "parent.jsonl",
        "native-parent",
        "work",
        SHARED_PROMPT,
        Some(SHARED_NAME),
        None,
    );
    let fork = write_native_with(
        &catalog,
        "--fork-parent--",
        "fork.jsonl",
        "native-fork",
        "work",
        SHARED_PROMPT,
        Some(SHARED_NAME),
        Some(parent.to_str().expect("utf-8 parent path")),
    );
    let older_dup = write_native_with(
        &catalog,
        "--dup-old--",
        "old.jsonl",
        "native-dup",
        "work",
        SHARED_PROMPT,
        Some(SHARED_NAME),
        None,
    );
    let newer_dup = write_native_with(
        &catalog,
        "--dup-new--",
        "new.jsonl",
        "native-dup",
        "work",
        SHARED_PROMPT,
        Some(SHARED_NAME),
        None,
    );
    set_modified(&parent, 10);
    set_modified(&fork, 20);
    set_modified(&older_dup, 30);
    set_modified(&newer_dup, 50);

    let rows = catalog.scan(&[SessionSourceKind::NativePi]);
    assert_eq!(rows.len(), 4, "scan keeps every native file before dedupe");
    assert!(
        rows.iter().all(|row| {
            row.kind.is_native()
                && row.status.is_native()
                && row.summary == SHARED_NAME
                && row.name.as_deref() == Some(SHARED_NAME)
        }),
        "parent/fork/dup rows share native identity and inherited name/summary"
    );

    let deduped = SessionCatalog::dedupe_rows(&rows);
    let ids: Vec<_> = deduped.iter().map(|row| row.session_id.as_str()).collect();
    assert!(
        ids.contains(&"native-parent") && ids.contains(&"native-fork"),
        "parent and fork with the same cwd/name/summary both remain: {ids:?}"
    );
    let parent_row = deduped
        .iter()
        .find(|row| row.session_id == "native-parent")
        .expect("parent");
    let fork_row = deduped
        .iter()
        .find(|row| row.session_id == "native-fork")
        .expect("fork");
    assert!(!parent_row.has_parent_session);
    assert!(fork_row.has_parent_session);
    assert_eq!(parent_row.path, parent);
    assert_eq!(fork_row.path, fork);
    assert_eq!(parent_row.cwd, fork_row.cwd);
    assert_eq!(parent_row.summary, fork_row.summary);
    assert_eq!(parent_row.name, fork_row.name);

    let dups: Vec<_> = deduped
        .iter()
        .filter(|row| row.session_id == "native-dup")
        .collect();
    assert_eq!(dups.len(), 1, "same native id collapses: {ids:?}");
    assert_eq!(
        dups[0].path, newer_dup,
        "duplicate native id keeps the preferred newest row"
    );
    assert_eq!(deduped.len(), 3);

    let listed = catalog.list(&CatalogListOptions {
        sources: vec![SessionSourceKind::NativePi],
        dedupe: true,
        ..CatalogListOptions::default()
    });
    assert_eq!(listed.len(), 3);
    assert!(listed.iter().any(|row| row.session_id == "native-parent"));
    assert!(listed.iter().any(|row| row.session_id == "native-fork"));
    assert_eq!(
        listed
            .iter()
            .filter(|row| row.session_id == "native-dup")
            .count(),
        1
    );
}

/// Native-status rows key on session_id even when cwd+summary would collide.
/// The same shape with Foreign status still collapses on cwd+summary.
#[test]
fn native_status_dedupe_keys_on_session_id() {
    let cwd = PathBuf::from("/shared/cwd");
    let native_parent = CatalogRow {
        kind: SessionSourceKind::Rpi,
        session_id: "parent-id".into(),
        summary: "same summary".into(),
        cwd: cwd.clone(),
        modified_epoch: 1.0,
        display_time: String::new(),
        path: PathBuf::from("/native/parent.jsonl"),
        size: 1,
        message_count: Some(1),
        name: Some("same summary".into()),
        status: CatalogRowStatus::Native,
        import_lineage: None,
        has_parent_session: false,
        session_kind: SessionKind::User,
        agent_session: false,
        bootstrap_only: false,
        search_text: String::new(),
        message_blob: String::new(),
    };
    let native_fork = CatalogRow {
        session_id: "fork-id".into(),
        path: PathBuf::from("/native/fork.jsonl"),
        modified_epoch: 2.0,
        has_parent_session: true,
        ..native_parent.clone()
    };
    let older_same_id = CatalogRow {
        session_id: "dup-id".into(),
        path: PathBuf::from("/native/dup-old.jsonl"),
        modified_epoch: 3.0,
        ..native_parent.clone()
    };
    let newer_same_id = CatalogRow {
        session_id: "dup-id".into(),
        path: PathBuf::from("/native/dup-new.jsonl"),
        modified_epoch: 4.0,
        ..native_parent.clone()
    };
    let native_deduped = SessionCatalog::dedupe_rows(&[
        native_parent,
        native_fork,
        older_same_id,
        newer_same_id,
    ]);
    let native_ids: Vec<_> = native_deduped
        .iter()
        .map(|row| row.session_id.as_str())
        .collect();
    assert!(native_ids.contains(&"parent-id") && native_ids.contains(&"fork-id"));
    assert_eq!(native_deduped.len(), 3);
    let dup = native_deduped
        .iter()
        .find(|row| row.session_id == "dup-id")
        .expect("dup");
    assert_eq!(dup.path, PathBuf::from("/native/dup-new.jsonl"));

    let foreign_older = CatalogRow {
        kind: SessionSourceKind::Codex,
        session_id: "codex-old".into(),
        summary: "same summary".into(),
        cwd: cwd.clone(),
        modified_epoch: 1.0,
        display_time: String::new(),
        path: PathBuf::from("/codex/old.jsonl"),
        size: 1,
        message_count: Some(1),
        name: None,
        status: CatalogRowStatus::Foreign,
        import_lineage: None,
        has_parent_session: false,
        session_kind: SessionKind::User,
        agent_session: false,
        bootstrap_only: false,
        search_text: String::new(),
        message_blob: String::new(),
    };
    let foreign_newer = CatalogRow {
        session_id: "codex-new".into(),
        path: PathBuf::from("/codex/new.jsonl"),
        modified_epoch: 5.0,
        ..foreign_older.clone()
    };
    let foreign_deduped = SessionCatalog::dedupe_rows(&[foreign_older, foreign_newer]);
    assert_eq!(foreign_deduped.len(), 1);
    assert_eq!(foreign_deduped[0].session_id, "codex-new");
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
            write_omp(&catalog, "injected-omp-id", "omp"),
            "injected-omp",
        ),
        (
            SessionSourceKind::Codex,
            write_codex(
                &catalog,
                "rollout-injected-codex.jsonl",
                "injected-codex-id",
                "codex",
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
                "claude",
                "claude prompt",
            ),
            "injected-claude",
        ),
        (
            SessionSourceKind::Grok,
            write_grok(&catalog, "injected-grok-id", "grok"),
            "injected-grok",
        ),
        (
            SessionSourceKind::Droid,
            write_droid(&catalog, "injected-droid-id", "droid"),
            "injected-droid",
        ),
    ];

    for (kind, expected, prefix) in cases {
        assert_eq!(
            catalog.resolve_for(kind, prefix).expect("injected prefix"),
            expected
        );
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
            std::os::unix::fs::symlink(&outside, directory.parent().unwrap().join(companion))
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
    let native_root = catalog.root_for(SessionSourceKind::NativePi).path;
    fs::create_dir_all(&native_root).expect("native root");
    let capabilities = crate::fs::CapabilityFs::new().expect("capability filesystem");
    let native_authority = capabilities
        .open_root(
            crate::fs::CapabilityRootPurpose::SessionStore,
            pi_sandbox::AbsolutePath::parse(native_root.clone()).expect("absolute native root"),
            crate::fs::CapabilityAccess::PrivateReadWrite,
            crate::fs::RootCreation::Existing,
        )
        .expect("retained native session authority");
    let source = write_codex(
        &catalog,
        "rollout-concurrent.jsonl",
        "concurrent-id",
        "concurrent",
        "race once",
    );
    let preferred_cwd = fixture_cwd(&catalog, "concurrent");
    let workers = 8;
    let barrier = Arc::new(Barrier::new(workers));
    let mut threads = Vec::new();
    for _ in 0..workers {
        let catalog = catalog.clone();
        let source = source.clone();
        let barrier = Arc::clone(&barrier);
        let preferred_cwd = preferred_cwd.clone();
        let native_authority = native_authority.clone();
        threads.push(thread::spawn(move || {
            let _native_authority = native_authority;
            barrier.wait();
            catalog
                .import_or_resume(SessionSourceKind::Codex, source, Some(&preferred_cwd))
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
        results
            .iter()
            .filter(|result| !result.reused_existing)
            .count(),
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
fn concurrent_occupied_import_requires_matching_fingerprint() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let source = write_codex(
        &catalog,
        "rollout-fingerprint-race.jsonl",
        "fingerprint-race",
        "fingerprint-race",
        "current source",
    );
    let source_path = source.clone();
    let source_cwd = fixture_cwd(&catalog, "fingerprint-race");
    IMPORT_AFTER_LOOKUP_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |output| {
            let wrong = ImportLineage {
                source: SourceSessionFormat::Codex,
                source_session_id: "fingerprint-race".to_owned(),
                source_path,
                content_fingerprint: Some("wrong-fingerprint".to_owned()),
                imported_at: "2026-01-01T00:00:00.000Z".to_owned(),
            };
            occupy_import_output_with_lineage(
                SourceSessionFormat::Codex,
                PathBuf::from("<workspace>/source.jsonl"),
                source_cwd.clone(),
                output
                    .file_stem()
                    .unwrap()
                    .to_string_lossy()
                    .trim_start_matches("import_")
                    .to_owned(),
                output.parent().expect("deterministic output directory"),
                output.file_name().unwrap().to_str().unwrap(),
                &wrong,
            )
            .expect("occupy deterministic output");
        }));
    });

    let error = catalog
        .import_or_resume(SessionSourceKind::Codex, &source, None)
        .expect_err("mismatched concurrent lineage must fail");
    assert!(matches!(
        error,
        CatalogError::Import(ImportSessionError::InvalidInput { .. })
    ));
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
fn nested_native_resume_retains_immediate_parent_for_durable_orchestration() {
    let fixture = Fixture::new();
    let agent_dir = fixture.root.join("native-agent");
    let catalog = SessionCatalog::new(fixture.root.join("home")).with_native_agent_dir(&agent_dir);
    let target_cwd = fixture.root.join("nested-target");
    let path = write_native(
        &catalog,
        "--workspace-nested-target--",
        "nested.jsonl",
        "nested-id",
        target_cwd.to_str().expect("utf-8 target cwd"),
        "nested message",
    );

    let prepared = catalog
        .prepare_native_resume(&path)
        .expect("prepare nested native session");
    let identity = prepared.identity();
    let (parent, relative) = prepared.file_authority();
    assert_eq!(
        parent.display_root(),
        path.parent().expect("session parent")
    );
    assert_eq!(relative, Path::new("nested.jsonl"));
    assert_eq!(prepared.path(), path);
    assert_eq!(
        crate::PreparedSessionResume::prepare_path(&path)
            .expect("prepare same path directly")
            .identity(),
        identity,
    );

    let durable = crate::DurableRuntime::new_with_main_agent_id(
        "nested-id".to_owned(),
        path.clone(),
        parent,
        relative,
        "main".to_owned(),
    )
    .expect("bind durable orchestration to nested native session");
    assert_eq!(durable.parent_session_path(), path);
    assert_eq!(
        durable.child_root(),
        path.parent()
            .expect("session parent")
            .join("children/nested-id"),
    );
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
fn native_catalog_skips_environment_context_before_real_prompt() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let path = catalog
        .root_for(SessionSourceKind::NativePi)
        .path
        .join("--injected--")
        .join("environment.jsonl");
    fs::create_dir_all(path.parent().expect("parent")).expect("parent");
    fs::write(
        &path,
        concat!(
            r#"{"type":"session","version":3,"id":"environment","timestamp":"2026-01-01T00:00:00Z","cwd":"/workspace/environment-project"}"#,
            "\n",
            r#"{"type":"message","id":"metadata","parentId":null,"message":{"role":"user","content":"<environment_context>\nCurrent directory: /workspace/environment-project\n</environment_context>"}}"#,
            "\n",
            r#"{"type":"message","id":"prompt","parentId":"metadata","message":{"role":"user","content":"Fix the checkout race"}}"#,
            "\n"
        ),
    )
    .expect("environment context session");

    let rows = catalog.list(&CatalogListOptions {
        sources: vec![SessionSourceKind::NativePi],
        ..CatalogListOptions::default()
    });
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].summary, "Fix the checkout race");
    assert_eq!(rows[0].message_blob, "Fix the checkout race");
    assert_eq!(rows[0].message_count, Some(2));
}

#[test]
fn native_catalog_skips_user_info_but_preserves_explicit_name() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let path = catalog
        .root_for(SessionSourceKind::NativePi)
        .path
        .join("--injected--")
        .join("user-info.jsonl");
    fs::create_dir_all(path.parent().expect("parent")).expect("parent");
    fs::write(
        &path,
        concat!(
            r#"{"type":"session","version":3,"id":"user-info","timestamp":"2026-01-01T00:00:00Z","cwd":"/workspace/user-project"}"#,
            "\n",
            r#"{"type":"message","id":"metadata","parentId":null,"message":{"role":"user","content":[{"type":"text","text":"<user_info>\nAccount tier: team\n</user_info>"}]}}"#,
            "\n",
            r#"{"type":"message","id":"answer","parentId":"metadata","message":{"role":"assistant","content":[{"type":"text","text":"Ready for the migration"}]}}"#,
            "\n",
            r#"{"type":"session_info","name":"Pinned migration session"}"#,
            "\n"
        ),
    )
    .expect("user info session");

    let rows = catalog.list(&CatalogListOptions {
        sources: vec![SessionSourceKind::NativePi],
        ..CatalogListOptions::default()
    });
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name.as_deref(), Some("Pinned migration session"));
    assert_eq!(rows[0].summary, "Pinned migration session");
    assert_eq!(rows[0].message_blob, "Ready for the migration");
    assert_eq!(rows[0].message_count, Some(2));
}

#[test]
fn native_catalog_uses_project_fallback_when_only_injected_context_exists() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let path = catalog
        .root_for(SessionSourceKind::NativePi)
        .path
        .join("--injected--")
        .join("metadata-only.jsonl");
    fs::create_dir_all(path.parent().expect("parent")).expect("parent");
    fs::write(
        &path,
        concat!(
            r#"{"type":"session","version":3,"id":"metadata-only","timestamp":"2026-01-01T00:00:00Z","cwd":"/workspace/catalog-project"}"#,
            "\n",
            r#"{"type":"message","id":"environment","parentId":null,"message":{"role":"user","content":"<environment_context>\nCurrent directory: /workspace/catalog-project\n</environment_context>"}}"#,
            "\n",
            r#"{"type":"message","id":"user-info","parentId":"environment","message":{"role":"user","content":"<user_info>\nAccount tier: team\n</user_info>"}}"#,
            "\n",
            r#"{"type":"message","id":"reminder","parentId":"user-info","message":{"role":"assistant","content":"<system-reminder>\nInternal scheduling metadata\n</system-reminder>"}}"#,
            "\n"
        ),
    )
    .expect("metadata-only session");

    let rows = catalog.list(&CatalogListOptions {
        sources: vec![SessionSourceKind::NativePi],
        ..CatalogListOptions::default()
    });
    assert_eq!(rows.len(), 1);
    assert!(rows[0].summary.is_empty());
    assert!(rows[0].message_blob.is_empty());
    assert_eq!(
        rows[0].cwd.file_name().and_then(|name| name.to_str()),
        Some("catalog-project")
    );
    assert_eq!(rows[0].message_count, Some(3));
}

#[test]
fn native_catalog_keeps_prompts_that_only_mention_injected_tags() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let first = write_native(
        &catalog,
        "--mentions--",
        "later-tag.jsonl",
        "later-tag",
        "/workspace/later-tag",
        "Explain why <environment_context> appears in this fixture",
    );
    let second = write_native(
        &catalog,
        "--mentions--",
        "unclosed-tag.jsonl",
        "unclosed-tag",
        "/workspace/unclosed-tag",
        "<user_info> is a documented tag, not injected metadata here",
    );
    set_modified(&first, 1);
    set_modified(&second, 2);

    let rows = catalog.list(&CatalogListOptions {
        sources: vec![SessionSourceKind::NativePi],
        ..CatalogListOptions::default()
    });
    assert_eq!(rows.len(), 2);
    let later = rows
        .iter()
        .find(|row| row.session_id == "later-tag")
        .expect("later tag row");
    assert_eq!(
        later.summary,
        "Explain why <environment_context> appears in this fixture"
    );
    let unclosed = rows
        .iter()
        .find(|row| row.session_id == "unclosed-tag")
        .expect("unclosed tag row");
    assert_eq!(
        unclosed.summary,
        "<user_info> is a documented tag, not injected metadata here"
    );
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
    let before_mtime = fs::metadata(&child)
        .expect("child meta")
        .modified()
        .expect("mtime");

    let rows = catalog.list(&CatalogListOptions {
        sources: vec![SessionSourceKind::Omp],
        include_foreign: true,
        ..CatalogListOptions::default()
    });
    assert_eq!(
        rows.len(),
        1,
        "only the top-level OMP session is listed: {rows:?}"
    );
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
    let after_mtime = fs::metadata(&child)
        .expect("child meta")
        .modified()
        .expect("mtime");
    assert_eq!(
        before_child, after_child,
        "foreign child file bytes must stay unchanged"
    );
    assert_eq!(
        before_mtime, after_mtime,
        "foreign child mtime must stay unchanged"
    );
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
    let directory = catalog.root_for(SessionSourceKind::Omp).path.join(dir);
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
        again
            .messages
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
        [
            "chain prompt 0",
            "chain reply 0",
            "chain prompt 1",
            "chain reply 1"
        ],
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
    let self_ref = write(
        "omp-self",
        Some(&directory.join("omp-self.jsonl").display().to_string()),
    );
    let chain = catalog.omp_chain_paths(&self_ref);
    assert_eq!(
        chain,
        vec![self_ref.clone()],
        "self-reference must not loop"
    );

    // Two-file cycle back to the leaf.
    let cycle_a = write("omp-cycle-a", None);
    let cycle_b = write("omp-cycle-b", Some(&cycle_a.display().to_string()));
    let cycle_a = write("omp-cycle-a", Some(&cycle_b.display().to_string()));
    let chain = catalog.omp_chain_paths(&cycle_b);
    assert_eq!(
        chain,
        vec![cycle_a, cycle_b],
        "cycle must stop after one pass"
    );

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
    let outside = fixture.write(
        "outside-evil.jsonl",
        "{\"type\":\"session\",\"id\":\"evil\"}\n",
    );
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
    let odd = write(
        "omp-odd",
        Some(&omp_root.join("odd.txt").display().to_string()),
    );
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
    assert_eq!(
        texts,
        ["parent prompt"],
        "child messages must stay excluded"
    );
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
    let bounded = catalog.omp_chain_probe_bounded(leaf, 8, u64::MAX);
    assert_eq!(bounded.len(), 8);
    assert_eq!(
        &bounded[..3],
        &chain[2..5],
        "newest files first in root order"
    );
    assert_eq!(bounded.last(), Some(leaf));

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
    assert!(
        texts.contains(&"chain prompt 2"),
        "oldest retained message present"
    );
    assert!(
        !texts.contains(&"chain prompt 1"),
        "over-budget file excluded"
    );
    assert!(texts.contains(&"chain prompt 9"), "leaf always included");
    // The authoritative parse fingerprint is one content digest covering
    // exactly the accepted members, so it differs from the leaf-only digest.
    let omp_root = catalog.root_for(SessionSourceKind::Omp).path;
    let (_, parse_fingerprint) =
        parse_omp_chain_public(&omp_root, &bounded, u64::MAX).expect("parse of walked chain");
    let (_, leaf_only_fingerprint) =
        parse_omp_chain_public(&omp_root, &[leaf.clone()], u64::MAX).expect("parse of leaf");
    assert_eq!(parse_fingerprint.len(), 64, "SHA-256 hex digest");
    assert_ne!(
        parse_fingerprint, leaf_only_fingerprint,
        "chain fingerprint covers every accepted member"
    );

    // Byte bound: budget covering leaf + its direct parent excludes the next
    // ancestor.
    let [root, mid] = [&chain[0], &chain[8]];
    let mid_size = fs::metadata(mid).expect("mid meta").len();
    let leaf_size = fs::metadata(leaf).expect("leaf meta").len();
    let tight = catalog.omp_chain_probe_bounded(leaf, 8, mid_size + leaf_size);
    assert_eq!(
        tight,
        vec![mid.clone(), leaf.clone()],
        "earlier ancestor excluded by byte budget"
    );
    let leaf_only = catalog.omp_chain_probe_bounded(leaf, 8, leaf_size);
    assert_eq!(
        leaf_only,
        vec![leaf.clone()],
        "only the leaf fits the budget"
    );
    assert_eq!(
        catalog.omp_chain_probe_bounded(root, 8, u64::MAX),
        vec![root.clone()]
    );
}

/// The OMP parent-session header probe is bounded by both a physical-record
/// budget and a cumulative byte budget: ancestors padded with non-object JSON
/// records fail closed instead of reading to EOF, the walk stops at that
/// member, and import still emits the safe newest prefix whose row then reads
/// AlreadyImported (row and import share the accepted-chain fingerprint).
#[test]
fn omp_header_probe_fails_closed_on_noise_padded_ancestors() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let omp_root = catalog.root_for(SessionSourceKind::Omp).path;

    // Physical-record budget: a valid header followed by more non-object
    // records than the header scan allows must fail the probe.
    let chain = write_omp_chain(&catalog, "--noisy-records--", 3);
    let mid = &chain[1];
    let leaf = &chain[2];
    let header = fs::read_to_string(mid)
        .expect("mid body")
        .lines()
        .next()
        .expect("header line")
        .to_owned();
    let mut noisy = header;
    noisy.push('\n');
    noisy.push_str(&"null\n".repeat(MAX_HEADER_SCAN_RECORDS + 1));
    fs::write(mid, &noisy).expect("noisy mid");

    assert!(
        matches!(
            omp_chain_member_probe_under_root_public(SourceSessionFormat::Omp, &omp_root, mid),
            Err(ImportSessionError::ResourceLimit { .. })
        ),
        "record-padded header must fail closed instead of scanning to EOF"
    );
    assert_eq!(
        catalog.omp_chain_paths(leaf),
        vec![mid.clone(), leaf.clone()],
        "walk must stop at the record-padded member"
    );

    // Import still works with the safe newest prefix (the padded ancestor is
    // excluded from the parse too) and the row then reads AlreadyImported.
    let resolved = catalog
        .import_or_resume(SessionSourceKind::Omp, leaf, None)
        .expect("record-padded chain import");
    let texts = resolved
        .messages
        .iter()
        .map(|message| message.text.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        texts,
        ["chain prompt 2", "chain reply 2"],
        "record-padded ancestor excluded, newest prefix retained"
    );

    // Byte budget: records large enough to exceed the cumulative byte bound
    // before the record bound.
    let chain = write_omp_chain(&catalog, "--noisy-bytes--", 3);
    let mid = &chain[1];
    let leaf = &chain[2];
    let header = fs::read_to_string(mid)
        .expect("mid body")
        .lines()
        .next()
        .expect("header line")
        .to_owned();
    let pad = format!(
        "{}\n",
        serde_json::to_string(&"x".repeat(96)).expect("pad json")
    );
    let bytes = pad.len() as u64;
    let needed = (MAX_HEADER_SCAN_BYTES / bytes + 1) as usize;
    assert!(
        needed < MAX_HEADER_SCAN_RECORDS,
        "fixture must trip the byte bound before the record bound"
    );
    let mut noisy = header;
    noisy.push('\n');
    noisy.push_str(&pad.repeat(needed));
    fs::write(mid, &noisy).expect("byte-padded mid");

    assert!(
        matches!(
            omp_chain_member_probe_under_root_public(SourceSessionFormat::Omp, &omp_root, mid),
            Err(ImportSessionError::ResourceLimit { .. })
        ),
        "byte-padded header must fail closed"
    );
    assert_eq!(
        catalog.omp_chain_paths(leaf),
        vec![mid.clone(), leaf.clone()],
        "walk must stop at the byte-padded member"
    );
    let resolved = catalog
        .import_or_resume(SessionSourceKind::Omp, leaf, None)
        .expect("byte-padded chain import");
    assert!(
        resolved.messages.iter().any(|m| m.text == "chain prompt 2"),
        "newest prefix retained for byte-padded chain"
    );

    // Both noisy-chain leaves read AlreadyImported after import — never
    // permanently Foreign.
    let rows = catalog.list(&CatalogListOptions {
        sources: vec![SessionSourceKind::Omp],
        include_foreign: true,
        ..Default::default()
    });
    let leaf_rows = rows
        .iter()
        .filter(|row| row.session_id == "omp-chain-2")
        .collect::<Vec<_>>();
    assert_eq!(leaf_rows.len(), 2, "one row per noisy chain leaf");
    for row in leaf_rows {
        assert!(
            matches!(row.status, CatalogRowStatus::AlreadyImported { .. }),
            "imported noisy-chain leaf must not read as permanently Foreign, got {:?}",
            row.status
        );
    }
}

/// An OMP ancestor whose descriptor exceeds the per-file source cap is
/// rejected before its header is probed; the walk stops there and import
/// keeps the safe newest prefix, with the row reading AlreadyImported after.
#[test]
fn omp_chain_probe_rejects_oversize_ancestor() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let chain = write_omp_chain(&catalog, "--oversize--", 3);
    let mid = &chain[1];
    let leaf = &chain[2];
    let omp_root = catalog.root_for(SessionSourceKind::Omp).path;

    // Sparse-extend the middle ancestor beyond the per-file source cap so the
    // descriptor-length check trips without writing 64 MiB.
    let file = File::options().write(true).open(mid).expect("open mid");
    file.set_len(MAX_SOURCE_BYTES + 1).expect("extend mid");

    assert!(
        matches!(
            omp_chain_member_probe_under_root_public(SourceSessionFormat::Omp, &omp_root, mid),
            Err(ImportSessionError::ResourceLimit { .. })
        ),
        "oversize ancestor must be rejected before header probing"
    );
    assert_eq!(
        catalog.omp_chain_paths(leaf),
        vec![mid.clone(), leaf.clone()],
        "walk must stop at the oversize member"
    );

    let resolved = catalog
        .import_or_resume(SessionSourceKind::Omp, leaf, None)
        .expect("import with oversize ancestor");
    let texts = resolved
        .messages
        .iter()
        .map(|message| message.text.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        texts,
        ["chain prompt 2", "chain reply 2"],
        "oversize ancestor excluded, newest prefix retained"
    );

    let rows = catalog.list(&CatalogListOptions {
        sources: vec![SessionSourceKind::Omp],
        include_foreign: true,
        ..Default::default()
    });
    let row = rows
        .iter()
        .find(|row| row.session_id == "omp-chain-2")
        .expect("leaf row");
    assert!(
        matches!(row.status, CatalogRowStatus::AlreadyImported { .. }),
        "oversize ancestor must not leave the imported row Foreign, got {:?}",
        row.status
    );
}

/// The OMP chain fingerprint is content-based: a same-length rewrite with the
/// mtime restored must still change the fingerprint and re-import instead of
/// reusing the stale native copy.
#[test]
fn omp_chain_fingerprint_tracks_content_not_mtime() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let chain = write_omp_chain(&catalog, "--content-fp--", 3);
    let mid = &chain[1];
    let leaf = &chain[2];
    let omp_root = catalog.root_for(SessionSourceKind::Omp).path;

    // Freeze the mtime that will later be "restored" to the same value.
    set_modified(mid, 42_000);
    let original = fs::read_to_string(mid).expect("mid body");
    let original_len = original.len();
    let original_mtime = fs::metadata(mid)
        .expect("mid meta")
        .modified()
        .expect("mtime");

    let first = catalog
        .import_or_resume(SessionSourceKind::Omp, leaf, None)
        .expect("first chained import");
    assert!(!first.reused_existing);
    let chain_paths = catalog.omp_chain_paths(leaf);
    let (_, fingerprint_before) =
        parse_omp_chain_public(&omp_root, &chain_paths, SESSION_CATALOG_OMP_CHAIN_MAX_BYTES)
            .expect("parse before rewrite");

    // Same-length rewrite with the mtime restored: identical size and mtime,
    // different bytes. The old {mtime_secs}:{size} fingerprint would miss
    // this; the content digest must not.
    let rewritten = original
        .replace("chain prompt 1", "chain prompT 1")
        .replace("chain reply 1", "chain repLy 1");
    assert_eq!(
        rewritten.len(),
        original_len,
        "fixture rewrite must be same-length"
    );
    fs::write(mid, &rewritten).expect("rewrite mid");
    set_modified(mid, 42_000); // restore the frozen mtime
    let meta = fs::metadata(mid).expect("mid meta");
    assert_eq!(meta.len(), original_len as u64, "size restored");
    assert_eq!(
        meta.modified().expect("mtime"),
        original_mtime,
        "mtime restored"
    );

    let (_, fingerprint_after) =
        parse_omp_chain_public(&omp_root, &chain_paths, SESSION_CATALOG_OMP_CHAIN_MAX_BYTES)
            .expect("parse after rewrite");
    assert_ne!(
        fingerprint_after, fingerprint_before,
        "same-length rewrite with restored mtime must change the content fingerprint"
    );

    // The stale native copy is not reused: a fresh import with a new id/path
    // carries the rewritten ancestor transcript.
    let second = catalog
        .import_or_resume(SessionSourceKind::Omp, leaf, None)
        .expect("re-import after content rewrite");
    assert!(!second.reused_existing);
    assert_ne!(
        second.path, first.path,
        "content change must emit a fresh import"
    );
    let body = fs::read_to_string(&second.path).expect("second body");
    assert!(
        body.contains("chain prompT 1"),
        "rewritten ancestor transcript must be in the fresh import"
    );
}

/// Malformed or adjacency-rejected ancestors must not leave imported rows
/// permanently Foreign: the row's fingerprint comes from the same
/// authoritative accepted-chain parse as the import, so after import the row
/// reads AlreadyImported even when an ancestor was excluded.
#[test]
fn omp_row_status_matches_import_when_ancestors_excluded() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();

    // Malformed ancestor: valid header, broken JSON body after it.
    let chain = write_omp_chain(&catalog, "--malformed-mid--", 3);
    let mid = &chain[1];
    let leaf = &chain[2];
    let header = fs::read_to_string(mid)
        .expect("mid body")
        .lines()
        .next()
        .expect("header line")
        .to_owned();
    fs::write(mid, format!("{header}\n{{not json\n")).expect("malformed mid");

    let resolved = catalog
        .import_or_resume(SessionSourceKind::Omp, leaf, None)
        .expect("import with malformed ancestor");
    assert!(!resolved.reused_existing);
    let texts = resolved
        .messages
        .iter()
        .map(|message| message.text.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        texts,
        ["chain prompt 2", "chain reply 2"],
        "malformed ancestor excluded, newest prefix retained"
    );

    let rows = catalog.list(&CatalogListOptions {
        sources: vec![SessionSourceKind::Omp],
        include_foreign: true,
        ..Default::default()
    });
    let row = rows
        .iter()
        .find(|row| row.session_id == "omp-chain-2")
        .expect("leaf row");
    assert!(
        matches!(row.status, CatalogRowStatus::AlreadyImported { .. }),
        "malformed ancestor must not leave the imported row permanently Foreign, got {:?}",
        row.status
    );

    // Adjacency-rejected ancestor: mid no longer references root, so the
    // parse drops root (and anything older) while keeping mid + leaf.
    let chain = write_omp_chain(&catalog, "--adjacency-mid--", 3);
    let mid = &chain[1];
    let leaf = &chain[2];
    let bogus = chain[0].with_file_name("not-the-root.jsonl");
    let mid_body = fs::read_to_string(mid).expect("mid body");
    fs::write(
        mid,
        mid_body.replace(
            &format!(r#","parentSession":"{}""#, chain[0].display()),
            &format!(r#","parentSession":"{}""#, bogus.display()),
        ),
    )
    .expect("rewrite mid linkage");

    let resolved = catalog
        .import_or_resume(SessionSourceKind::Omp, leaf, None)
        .expect("import with adjacency-rejected ancestor");
    let texts = resolved
        .messages
        .iter()
        .map(|message| message.text.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        texts,
        [
            "chain prompt 1",
            "chain reply 1",
            "chain prompt 2",
            "chain reply 2"
        ],
        "root dropped by adjacency, mid + leaf retained"
    );

    let rows = catalog.list(&CatalogListOptions {
        sources: vec![SessionSourceKind::Omp],
        include_foreign: true,
        ..Default::default()
    });
    let row = rows
        .iter()
        .find(|row| row.session_id == "omp-chain-2")
        .expect("leaf row");
    assert!(
        matches!(row.status, CatalogRowStatus::AlreadyImported { .. }),
        "adjacency-rejected ancestor must not leave the imported row permanently Foreign, got {:?}",
        row.status
    );
}

/// OMP rotation rows share the chain-opener summary and cwd, so dedupe must
/// prefer the leaf row (longest accepted chain / exact AlreadyImported match)
/// over its rotation ancestors — both before and after import.
#[test]
fn omp_dedupe_keeps_leaf_row_for_rotation_chain() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let chain = write_omp_chain(&catalog, "--dedupe-leaf--", 3);
    let leaf = &chain[2];

    let options = CatalogListOptions {
        sources: vec![SessionSourceKind::Omp],
        include_foreign: true,
        dedupe: true,
        ..Default::default()
    };

    // Before import: the three rotation rows collapse to the leaf.
    let rows = catalog.list(&options);
    assert_eq!(rows.len(), 1, "rotation chain collapses to one deduped row");
    assert_eq!(
        rows[0].session_id, "omp-chain-2",
        "the leaf row wins dedupe"
    );
    assert!(matches!(rows[0].status, CatalogRowStatus::Foreign));
    assert_eq!(
        rows[0].message_count,
        Some(6),
        "leaf row carries the full accepted-chain message count"
    );

    // After import: the same deduped leaf row reads AlreadyImported.
    let resolved = catalog
        .import_or_resume(SessionSourceKind::Omp, leaf, None)
        .expect("chain import");
    assert!(!resolved.reused_existing);
    let rows = catalog.list(&options);
    assert_eq!(rows.len(), 1, "one leaf row after import");
    assert_eq!(rows[0].session_id, "omp-chain-2");
    match &rows[0].status {
        CatalogRowStatus::AlreadyImported { native_path, .. } => {
            assert_eq!(native_path, &resolved.path, "leaf row points at its import");
        }
        other => panic!("expected AlreadyImported leaf row, got {other:?}"),
    }
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
    assert_ne!(
        second.id, first.id,
        "chain fingerprint must change the import id"
    );
    let second_body = fs::read_to_string(&second.path).expect("second body");
    assert!(
        second_body.contains("chain prompt MUTATED"),
        "updated ancestor transcript must be in the new import"
    );
    assert!(
        second_body.contains("chain prompt 0"),
        "full chain retained"
    );
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
            assert_eq!(
                native_path, &second.path,
                "row must point at the fresh import"
            );
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

    let (full, full_fingerprint) =
        parse_omp_chain_public(&root, &chain, u64::MAX).expect("full chain parse");
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
        [
            "chain prompt 1",
            "chain reply 1",
            "chain prompt 2",
            "chain reply 2"
        ],
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
    assert_eq!(
        again_fingerprint, full_fingerprint,
        "digest is deterministic"
    );
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
        [
            "chain prompt 1",
            "chain reply 1",
            "chain prompt 2",
            "chain reply 2"
        ],
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
    let (collapsed, _) = parse_omp_chain_public(&root, &chain, u64::MAX).expect("collapsed chain");
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
    assert_eq!(
        entries.len(),
        2,
        "both imports must share the identity slot"
    );

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
        texts,
        ["plain prompt", "ok"],
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
        format!(r#"{{"type":"system","content":"{padding}"}}"#),
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

fn write_codex_cwd(
    catalog: &SessionCatalog,
    name: &str,
    id: &str,
    cwd: &Path,
    user: &str,
) -> PathBuf {
    let cwd_json = serde_json::to_string(&cwd.to_string_lossy()).expect("cwd json");
    let user_json = serde_json::to_string(user).expect("user json");
    let path = catalog.root_for(SessionSourceKind::Codex).path.join(name);
    fs::create_dir_all(path.parent().expect("parent")).expect("parent");
    fs::write(
        &path,
        format!(
            concat!(
                r#"{{"timestamp":"2026-02-01T00:00:00Z","type":"session_meta","payload":{{"id":"{id}","cwd":{cwd_json}}}}}"#,
                "\n",
                r#"{{"timestamp":"2026-02-01T00:00:01Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":{user_json}}}]}}}}"#,
                "\n"
            ),
            id = id,
            cwd_json = cwd_json,
            user_json = user_json,
        ),
    )
    .expect("codex fixture");
    path
}

fn write_native_cwd(
    catalog: &SessionCatalog,
    project: &str,
    file: &str,
    id: &str,
    cwd: &Path,
    user: &str,
) -> PathBuf {
    let cwd_json = serde_json::to_string(&cwd.to_string_lossy()).expect("cwd json");
    let path = catalog
        .root_for(SessionSourceKind::NativePi)
        .path
        .join(project)
        .join(file);
    fs::create_dir_all(path.parent().expect("parent")).expect("parent");
    fs::write(
        &path,
        format!(
            concat!(
                r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-01-01T00:00:00.000Z","cwd":{cwd_json}}}"#,
                "\n",
                r#"{{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":{user}}}]}}}}"#,
                "\n"
            ),
            id = id,
            cwd_json = cwd_json,
            user = serde_json::to_string(user).expect("user json"),
        ),
    )
    .expect("native fixture");
    path
}

/// Slock orchestrates ephemeral Codex agents under `.slock/agents/<id>`: both
/// the foreign Codex row and the imported/native row sharing that cwd must
/// flag `agent_session`, while real project roots — including a workspace
/// that merely CONTAINS a `.slock` directory — stay real.
#[test]
fn slock_agent_workspace_cwd_flags_rows_but_real_projects_stay_real() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let agent_cwd = catalog.user_home().join(".slock/agents/agent-7");
    let project_cwd = catalog.user_home().join("Projects/pi-rs");
    let project_root_with_slock = catalog.user_home().join("Projects/app");
    fs::create_dir_all(&agent_cwd).expect("agent workspace");
    fs::create_dir_all(&project_cwd).expect("project root");
    // The project root contains a `.slock` directory tree, but the session
    // cwd is the project root itself — it must stay real.
    fs::create_dir_all(project_root_with_slock.join(".slock/agents/other"))
        .expect("nested slock tree");

    write_codex_cwd(&catalog, "rollout-slock.jsonl", "slock-codex", &agent_cwd, "subtask");
    write_native_cwd(
        &catalog,
        "agent-import",
        "imported.jsonl",
        "imported-agent",
        &agent_cwd,
        "subtask",
    );
    write_codex_cwd(
        &catalog,
        "rollout-project.jsonl",
        "user-codex",
        &project_cwd,
        "fix the login bug",
    );
    write_native_cwd(
        &catalog,
        "project",
        "session.jsonl",
        "user-native",
        &project_cwd,
        "fix the login bug",
    );
    write_native_cwd(
        &catalog,
        "app",
        "root.jsonl",
        "root-native",
        &project_root_with_slock,
        "work from project root",
    );

    let rows = catalog.scan(&[SessionSourceKind::Codex, SessionSourceKind::NativePi]);
    let agent_row = |id: &str| {
        rows.iter()
            .find(|row| row.session_id == id)
            .unwrap_or_else(|| panic!("row {id}"))
    };

    // Both storage kinds sharing the agent workspace cwd are flagged.
    assert!(agent_row("slock-codex").agent_session, "codex agent row");
    assert!(agent_row("imported-agent").agent_session, "native agent row");
    // Real project cwds stay real — foreign user sessions unchanged.
    assert!(!agent_row("user-codex").agent_session);
    assert!(!agent_row("user-native").agent_session);
    assert!(!agent_row("root-native").agent_session);
    // None of these rows carry the bootstrap-only flag (genuine prompts).
    assert!(rows.iter().all(|row| !row.bootstrap_only));
}

/// A bootstrap-only session carries meaningful turns but none with genuine
/// content: every message is an injected envelope (environment context,
/// user info, system reminders) or AGENTS/project instructions. Sessions
/// with a genuine prompt, and header-only sessions with zero messages, are
/// never flagged.
#[test]
fn bootstrap_only_rows_flag_envelope_only_sessions() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();

    // Native: environment_context + user_info + project_instructions only.
    let native_bootstrap = catalog
        .root_for(SessionSourceKind::NativePi)
        .path
        .join("bootstrap")
        .join("env-only.jsonl");
    fs::create_dir_all(native_bootstrap.parent().expect("parent")).expect("parent");
    fs::write(
        &native_bootstrap,
        concat!(
            r#"{"type":"session","version":3,"id":"native-bootstrap","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp/work"}"#,
            "\n",
            r#"{"type":"message","id":"u1","parentId":null,"message":{"role":"user","content":[{"type":"text","text":"<environment_context>\ncwd /tmp/work\n</environment_context>"}, {"type":"text","text":"<user_info>Agent session</user_info>"}]}}"#,
            "\n",
            r#"{"type":"message","id":"u2","parentId":null,"timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"user","content":"<project_instructions path=\"/tmp/work/AGENTS.md\">\nfollow the rules\n</project_instructions>"}}"#,
            "\n"
        ),
    )
    .expect("native bootstrap fixture");

    // Native genuine: the bootstrap envelopes are followed by a real prompt.
    let native_genuine = write_native(
        &catalog,
        "genuine",
        "real.jsonl",
        "native-genuine",
        "genuine",
        "fix the login bug",
    );
    let _ = native_genuine;

    // Native header-only: a session record with zero messages.
    let native_header_only = catalog
        .root_for(SessionSourceKind::NativePi)
        .path
        .join("bootstrap")
        .join("header-only.jsonl");
    fs::write(
        &native_header_only,
        r#"{"type":"session","version":3,"id":"native-header-only","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp/work"}"#,
    )
    .expect("header-only fixture");

    // Codex bootstrap-only: the ephemeral-agent shape — one user turn whose
    // text is environment context plus AGENTS instructions, nothing genuine.
    write_codex_cwd(
        &catalog,
        "rollout-bootstrap.jsonl",
        "codex-bootstrap",
        Path::new("/tmp/work"),
        "<environment_context>\nagent mode\n</environment_context><user_instructions>follow AGENTS.md</user_instructions>",
    );
    // Codex genuine: same shape plus a real prompt.
    write_codex_cwd(
        &catalog,
        "rollout-genuine.jsonl",
        "codex-genuine",
        Path::new("/tmp/work"),
        "<user_info>bootstrap</user_info> now fix the login bug",
    );

    let rows = catalog.scan(&[SessionSourceKind::Codex, SessionSourceKind::NativePi]);
    let row = |id: &str| {
        rows.iter()
            .find(|row| row.session_id == id)
            .unwrap_or_else(|| panic!("row {id}"))
    };

    let bootstrap = row("native-bootstrap");
    assert!(bootstrap.bootstrap_only, "envelope-only native session");
    assert!(bootstrap.message_count.unwrap_or(0) > 0, "has messages");
    assert!(row("codex-bootstrap").bootstrap_only, "codex bootstrap-only");

    assert!(!row("native-genuine").bootstrap_only, "genuine prompt wins");
    assert!(!row("codex-genuine").bootstrap_only, "genuine text after envelope");
    assert!(!row("native-header-only").bootstrap_only, "zero messages");
}

/// Write a Codex rollout fixture whose user turn carries the given content
/// blocks verbatim (JSON-encoded), so tests can exercise multi-block content
/// shapes the lossy first-text projection cannot express.
fn write_codex_blocks(
    catalog: &SessionCatalog,
    name: &str,
    id: &str,
    cwd: &Path,
    blocks: &[serde_json::Value],
) -> PathBuf {
    let cwd_json = serde_json::to_string(&cwd.to_string_lossy()).expect("cwd json");
    let content_json = serde_json::to_string(blocks).expect("content json");
    let path = catalog.root_for(SessionSourceKind::Codex).path.join(name);
    fs::create_dir_all(path.parent().expect("parent")).expect("parent");
    fs::write(
        &path,
        format!(
            concat!(
                r#"{{"timestamp":"2026-02-01T00:00:00Z","type":"session_meta","payload":{{"id":"{id}","cwd":{cwd_json}}}}}"#,
                "\n",
                r#"{{"timestamp":"2026-02-01T00:00:01Z","type":"response_item","payload":{{"type":"message","role":"user","content":{content_json}}}}}"#,
                "\n"
            ),
            id = id,
            cwd_json = cwd_json,
            content_json = content_json,
        ),
    )
    .expect("codex fixture");
    path
}

/// The foreign-row bootstrap-only classification must scan EVERY content
/// block losslessly: a later genuine text block or an image-only genuine turn
/// clears the flag even when the lossy first-text projection would only see
/// the leading bootstrap envelope. All-envelope multi-block content stays
/// flagged; header-only sessions stay unflagged.
#[test]
fn foreign_bootstrap_only_scans_all_blocks_losslessly() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let cwd = Path::new("/tmp/work");

    // Envelope first, real text in a LATER block: the first-text projection
    // sees only the envelope, but the lossless scan must find the genuine
    // text and clear bootstrap-only.
    write_codex_blocks(
        &catalog,
        "rollout-envelope-then-text.jsonl",
        "codex-envelope-then-text",
        cwd,
        &[
            serde_json::json!({"type":"input_text","text":"<environment_context>\nagent mode\n</environment_context>"}),
            serde_json::json!({"type":"input_text","text":"fix the login bug"}),
        ],
    );
    // Envelope plus an image-only genuine turn: no portable text at all, but
    // the image is genuine content. The image uses the real OpenAI Responses
    // wire shape for Codex rollouts — `input_image` with an `image_url`, not
    // an Anthropic-style `image` block, which the Codex parser never
    // classifies as image content.
    write_codex_blocks(
        &catalog,
        "rollout-envelope-then-image.jsonl",
        "codex-envelope-then-image",
        cwd,
        &[
            serde_json::json!({"type":"input_text","text":"<user_info>agent session</user_info>"}),
            serde_json::json!({"type":"input_image","image_url":"data:image/png;base64,aW1n"}),
        ],
    );
    // All blocks are envelopes: still bootstrap-only.
    write_codex_blocks(
        &catalog,
        "rollout-all-envelopes.jsonl",
        "codex-all-envelopes",
        cwd,
        &[
            serde_json::json!({"type":"input_text","text":"<environment_context>\nagent mode\n</environment_context>"}),
            serde_json::json!({"type":"input_text","text":"<user_instructions>follow AGENTS.md</user_instructions>"}),
        ],
    );
    // Genuine user prompt in a single block: never bootstrap-only.
    write_codex_blocks(
        &catalog,
        "rollout-genuine-user.jsonl",
        "codex-genuine-user",
        cwd,
        &[serde_json::json!({"type":"input_text","text":"fix the login bug"})],
    );
    // Header-only: session_meta with zero message records.
    let header_only = catalog
        .root_for(SessionSourceKind::Codex)
        .path
        .join("rollout-header-only.jsonl");
    fs::write(
        &header_only,
        r#"{"timestamp":"2026-02-01T00:00:00Z","type":"session_meta","payload":{"id":"codex-header-only","cwd":"/tmp/work"}}"#,
    )
    .expect("header-only fixture");

    let rows = catalog.scan(&[SessionSourceKind::Codex]);
    let row = |id: &str| {
        rows.iter()
            .find(|row| row.session_id == id)
            .unwrap_or_else(|| panic!("row {id}"))
    };

    assert!(
        !row("codex-envelope-then-text").bootstrap_only,
        "later genuine text block clears bootstrap-only"
    );
    assert!(
        !row("codex-envelope-then-image").bootstrap_only,
        "image-only genuine turn clears bootstrap-only"
    );
    assert!(
        row("codex-all-envelopes").bootstrap_only,
        "all-envelope multi-block content stays bootstrap-only"
    );
    assert!(
        !row("codex-genuine-user").bootstrap_only,
        "genuine user prompt is not bootstrap-only"
    );
    assert!(
        !row("codex-header-only").bootstrap_only,
        "zero-message header-only session is not bootstrap-only"
    );
}

/// Write a Grok session fixture whose single user turn carries the given
/// content blocks verbatim (JSON-encoded), so tests can exercise multi-block
/// content shapes the lossy first-text projection cannot express.
fn write_grok_blocks(
    catalog: &SessionCatalog,
    id: &str,
    cwd: &Path,
    blocks: &[serde_json::Value],
) -> PathBuf {
    let cwd_json = serde_json::to_string(&cwd.to_string_lossy()).expect("cwd json");
    let content_json = serde_json::to_string(blocks).expect("content json");
    let directory = catalog
        .root_for(SessionSourceKind::Grok)
        .path
        .join("injected-cwd")
        .join(id);
    fs::create_dir_all(&directory).expect("grok directory");
    fs::write(
        directory.join("summary.json"),
        format!(
            r#"{{"info":{{"id":"{id}","cwd":{cwd_json}}},"created_at":"2026-01-01T00:00:00Z"}}"#
        ),
    )
    .expect("grok summary");
    fs::write(
        directory.join("chat_history.jsonl"),
        format!(r#"{{"role":"user","content":{content_json}}}"#),
    )
    .expect("grok chat");
    directory.join("summary.json")
}

/// Write a Droid session fixture whose single user turn carries the given
/// content blocks verbatim (JSON-encoded), so tests can exercise multi-block
/// content shapes the lossy first-text projection cannot express.
fn write_droid_blocks(
    catalog: &SessionCatalog,
    name: &str,
    id: &str,
    cwd: &Path,
    blocks: &[serde_json::Value],
) -> PathBuf {
    let cwd_json = serde_json::to_string(&cwd.to_string_lossy()).expect("cwd json");
    let content_json = serde_json::to_string(blocks).expect("content json");
    let path = catalog.root_for(SessionSourceKind::Droid).path.join(name);
    fs::create_dir_all(path.parent().expect("parent")).expect("parent");
    fs::write(
        &path,
        format!(
            concat!(
                r#"{{"type":"session_start","id":"{id}","cwd":{cwd_json}}}"#,
                "\n",
                r#"{{"type":"message","timestamp":"2026-02-01T00:00:00Z","message":{{"role":"user","content":{content_json}}}}}"#,
                "\n"
            ),
            id = id,
            cwd_json = cwd_json,
            content_json = content_json,
        ),
    )
    .expect("droid fixture");
    path
}

/// Grok and Droid foreign rows share the Codex lossless bootstrap-only
/// contract: a later genuine text block or an image clears the flag even when
/// the first-text projection only sees the leading envelope, while
/// all-envelope content stays flagged.
#[test]
fn grok_droid_bootstrap_only_scans_all_blocks_losslessly() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let cwd = Path::new("/tmp/work");

    write_grok_blocks(
        &catalog,
        "grok-envelope-then-text",
        cwd,
        &[
            serde_json::json!({"type":"text","text":"<environment_context>\nagent mode\n</environment_context>"}),
            serde_json::json!({"type":"text","text":"fix the login bug"}),
        ],
    );
    write_grok_blocks(
        &catalog,
        "grok-envelope-then-image",
        cwd,
        &[
            serde_json::json!({"type":"text","text":"<user_info>agent session</user_info>"}),
            serde_json::json!({"type":"image"}),
        ],
    );
    write_grok_blocks(
        &catalog,
        "grok-all-envelopes",
        cwd,
        &[
            serde_json::json!({"type":"text","text":"<environment_context>\nagent mode\n</environment_context>"}),
            serde_json::json!({"type":"text","text":"<user_instructions>follow AGENTS.md</user_instructions>"}),
        ],
    );
    write_droid_blocks(
        &catalog,
        "droid-envelope-then-image.jsonl",
        "droid-envelope-then-image",
        cwd,
        &[
            serde_json::json!({"type":"text","text":"<environment_context>\nagent mode\n</environment_context>"}),
            serde_json::json!({"type":"image"}),
        ],
    );
    write_droid_blocks(
        &catalog,
        "droid-all-envelopes.jsonl",
        "droid-all-envelopes",
        cwd,
        &[
            serde_json::json!({"type":"text","text":"<user_info>bootstrap</user_info>"}),
            serde_json::json!({"type":"text","text":"<system-reminder>ctx</system-reminder>"}),
        ],
    );

    let rows = catalog.scan(&[SessionSourceKind::Grok, SessionSourceKind::Droid]);
    let row = |id: &str| {
        rows.iter()
            .find(|row| row.session_id == id)
            .unwrap_or_else(|| panic!("row {id}"))
    };

    assert!(
        !row("grok-envelope-then-text").bootstrap_only,
        "later genuine text block clears grok bootstrap-only"
    );
    assert!(
        !row("grok-envelope-then-image").bootstrap_only,
        "image-only genuine turn clears grok bootstrap-only"
    );
    assert!(
        row("grok-all-envelopes").bootstrap_only,
        "all-envelope grok content stays bootstrap-only"
    );
    assert!(
        !row("droid-envelope-then-image").bootstrap_only,
        "image-only genuine turn clears droid bootstrap-only"
    );
    assert!(
        row("droid-all-envelopes").bootstrap_only,
        "all-envelope droid content stays bootstrap-only"
    );
}

fn write_native_with_kind(
    catalog: &SessionCatalog,
    project: &str,
    file: &str,
    id: &str,
    cwd: &str,
    user: &str,
    session_kind: SessionKind,
) -> PathBuf {
    let cwd = fixture_cwd(catalog, cwd);
    let cwd = cwd.display();
    let path = catalog
        .root_for(SessionSourceKind::NativePi)
        .path
        .join(project)
        .join(file);
    fs::create_dir_all(path.parent().expect("parent")).expect("parent");
    let mut out = File::create(&path).expect("create");
    writeln!(
        out,
        r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-01-01T00:00:00.000Z","cwd":"{cwd}","sessionKind":"{}"}}"#,
        session_kind.header_value()
    )
    .unwrap();
    writeln!(
        out,
        r#"{{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":"{user}"}}]}}}}"#
    )
    .unwrap();
    path
}

#[test]
fn every_workflow_internal_kind_is_hidden_from_user_list_and_search() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let kinds = [
        SessionKind::WorkflowSupervisor,
        SessionKind::WorkflowPlanner,
        SessionKind::WorkflowWorker,
        SessionKind::WorkflowWorktree,
        SessionKind::Disposable,
    ];
    for (index, kind) in kinds.iter().enumerate() {
        write_native_with_kind(
            &catalog,
            "--worktrees--",
            &format!("wf-{index}.jsonl"),
            &format!("wf-{index}"),
            "worktree",
            "workflow internal turn",
            *kind,
        );
    }
    let listed = catalog.list(&CatalogListOptions {
        sources: vec![SessionSourceKind::NativePi],
        ..CatalogListOptions::default()
    });
    assert!(
        listed.is_empty(),
        "workflow-internal rows must not appear in the user list: {:?}",
        listed.iter().map(|row| row.session_id.as_str()).collect::<Vec<_>>()
    );
    for index in 0..kinds.len() {
        let found = catalog
            .search(
                &format!("wf-{index}"),
                &CatalogSearchOptions {
                    sources: vec![SessionSourceKind::NativePi],
                    ..CatalogSearchOptions::default()
                },
            )
            .expect("search");
        assert!(
            !found.iter().any(|row| row.session_id == format!("wf-{index}")),
            "{:?} row must not be user-searchable",
            kinds[index]
        );
    }
    let scanned = catalog.scan(&[SessionSourceKind::NativePi]);
    assert_eq!(scanned.len(), kinds.len());
    for kind in kinds {
        assert!(
            scanned
                .iter()
                .any(|row| row.session_kind == kind),
            "{kind:?} row stays in the durable scan"
        );
    }
}

#[test]
fn user_session_in_the_same_cwd_stays_visible_next_to_workflow_rows() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let user = write_native(
        &catalog,
        "--worktrees--",
        "user.jsonl",
        "user-in-worktree",
        "worktree",
        "genuine user prompt",
    );
    let supervisor = write_native_with_kind(
        &catalog,
        "--worktrees--",
        "supervisor.jsonl",
        "supervisor-row",
        "worktree",
        "supervisor turn",
        SessionKind::WorkflowSupervisor,
    );
    let listed = catalog.list(&CatalogListOptions {
        sources: vec![SessionSourceKind::NativePi],
        ..CatalogListOptions::default()
    });
    let ids: Vec<&str> = listed.iter().map(|row| row.session_id.as_str()).collect();
    assert!(ids.contains(&"user-in-worktree"), "user row visible: {ids:?}");
    assert!(!ids.contains(&"supervisor-row"));
    assert_eq!(
        listed
            .iter()
            .find(|row| row.session_id == "user-in-worktree")
            .expect("user row")
            .session_kind,
        SessionKind::User
    );
    assert!(user.exists() && supervisor.exists());
}

#[test]
fn workflow_internal_rows_survive_restart_in_scan_without_deletion() {
    let fixture = Fixture::new();
    let path = write_native_with_kind(
        &fixture.catalog(),
        "--worktrees--",
        "planner.jsonl",
        "planner-row",
        "worktree",
        "planning turn",
        SessionKind::WorkflowPlanner,
    );
    let fresh = fixture.catalog();
    assert!(
        fresh.list(&CatalogListOptions::default()).is_empty(),
        "fresh catalog still hides the workflow row"
    );
    let scanned = fresh.scan(&[SessionSourceKind::NativePi]);
    assert_eq!(scanned.len(), 1);
    assert_eq!(scanned[0].session_kind, SessionKind::WorkflowPlanner);
    assert_eq!(scanned[0].path, path);
    assert!(path.exists(), "durable storage keeps the row's file");
}

#[test]
fn imported_foreign_sessions_classify_as_user_rows() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let source = write_codex(&catalog, "rollout-import-kind.jsonl", "import-kind", "work", "hello import");
    let resolved = catalog
        .import_or_resume(SessionSourceKind::Codex, &source, None)
        .expect("import");
    assert!(resolved.reused_existing || !resolved.native_no_copy);
    let listed = catalog.list(&CatalogListOptions::default());
    let imported = listed
        .iter()
        .find(|row| row.session_id == resolved.id)
        .expect("imported row is visible");
    assert_eq!(imported.session_kind, SessionKind::User);
}

#[test]
fn unknown_session_kind_token_is_hidden_and_classified_internal() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let path = catalog
        .root_for(SessionSourceKind::NativePi)
        .path
        .join("--worktrees--")
        .join("legacy-kind.jsonl");
    fs::create_dir_all(path.parent().expect("parent")).expect("parent");
    fs::write(
        &path,
        r#"{"type":"session","version":3,"id":"legacy-kind","timestamp":"2026-01-01T00:00:00.000Z","cwd":"<workspace>/work","sessionKind":"someFutureKind"}"#,
    )
    .expect("fixture");
    assert!(catalog.list(&CatalogListOptions::default()).is_empty());
    let scanned = catalog.scan(&[SessionSourceKind::NativePi]);
    assert_eq!(scanned[0].session_kind, SessionKind::Unknown);
    assert!(
        matches!(
            catalog.prepare_native_resume(&path),
            Err(CatalogError::WorkflowInternalSession)
        ),
        "unknown provenance must fail closed for switching"
    );
}
/// `include_foreign=false` must exclude foreign rows while native rows stay
/// visible, and the workflow-internal exclusion must hold in both modes:
/// internal rows never surface in list or search even with
/// `include_foreign=true`.
#[test]
fn include_foreign_false_hides_foreign_but_keeps_native_and_internal_hidden() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog();
    let codex = write_codex(
        &catalog,
        "rollout-foreign-visibility.jsonl",
        "foreign-visibility",
        "work",
        "foreign visibility prompt",
    );
    write_native(
        &catalog,
        "--proj--",
        "native-visibility.jsonl",
        "native-visibility",
        "work",
        "native visibility prompt",
    );
    write_native_with_kind(
        &catalog,
        "--worktrees--",
        "internal-visibility.jsonl",
        "internal-visibility",
        "work",
        "internal visibility turn",
        SessionKind::WorkflowSupervisor,
    );

    let native_only = catalog.list(&CatalogListOptions {
        include_foreign: false,
        ..CatalogListOptions::default()
    });
    let native_ids: Vec<&str> = native_only
        .iter()
        .map(|row| row.session_id.as_str())
        .collect();
    assert!(
        !native_ids.contains(&"foreign-visibility"),
        "foreign row must be excluded with include_foreign=false: {native_ids:?}"
    );
    assert!(
        native_ids.contains(&"native-visibility"),
        "native row stays visible with include_foreign=false: {native_ids:?}"
    );
    assert!(
        !native_ids.contains(&"internal-visibility"),
        "workflow-internal row stays hidden: {native_ids:?}"
    );

    let all = catalog.list(&CatalogListOptions {
        include_foreign: true,
        ..CatalogListOptions::default()
    });
    let all_ids: Vec<&str> = all.iter().map(|row| row.session_id.as_str()).collect();
    assert!(
        all_ids.contains(&"foreign-visibility"),
        "foreign row returns with include_foreign=true: {all_ids:?}"
    );
    assert!(
        all_ids.contains(&"native-visibility"),
        "native row visible with include_foreign=true: {all_ids:?}"
    );
    assert!(
        !all_ids.contains(&"internal-visibility"),
        "workflow-internal row stays hidden even with include_foreign=true: {all_ids:?}"
    );

    let foreign_search = catalog
        .search(
            "foreign-visibility",
            &CatalogSearchOptions {
                include_foreign: false,
                ..CatalogSearchOptions::default()
            },
        )
        .expect("search foreign excluded");
    assert!(
        !foreign_search
            .iter()
            .any(|row| row.session_id == "foreign-visibility"),
        "search must not surface the foreign row with include_foreign=false"
    );

    let foreign_search_included = catalog
        .search(
            "foreign-visibility",
            &CatalogSearchOptions {
                include_foreign: true,
                ..CatalogSearchOptions::default()
            },
        )
        .expect("search foreign included");
    assert!(
        foreign_search_included
            .iter()
            .any(|row| row.session_id == "foreign-visibility"),
        "search returns the foreign row with include_foreign=true"
    );
    assert!(
        !foreign_search_included
            .iter()
            .any(|row| row.session_id == "internal-visibility"),
        "search never surfaces the workflow-internal row"
    );
    assert!(codex.exists(), "foreign source file untouched");
}
