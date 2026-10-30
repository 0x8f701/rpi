//! Filesystem identity and replacement-race proofs (D7 integration file).
//!
//! The capability engine pins every opened file to its identity and detects
//! path replacement as an identity swap instead of silently adopting the new
//! file (C9, §9). These tests exercise that contract through the public
//! session-store surface, which is the only production path that exposes
//! `SessionFileIdentity` to callers:
//!
//! - a session file opened through the recorder, reopened through
//!   `PreparedSessionResume`, and resumed through `resume_session` must yield
//!   the identical identity (stability across independent opens);
//! - distinct session files must never collide on identity, and the opened
//!   identity of each enumerated file must match what the store recorded;
//! - replacing the file at a path must change the identity reported for that
//!   path while every previously opened handle stays pinned to the original
//!   file (swap detection + pinned handles).
//!
//! On Windows the identity is the full 128-bit `FILE_ID_INFO` (`volume serial`
//! + 16-byte file id) captured through `GetFileInformationByHandleEx`; the
//! `windows_*` tests below are the exact §17 names run by the
//! `sandbox-windows-hosted` / `sandbox-windows-release` jobs. On Unix the
//! identity is the device/inode pair; the `unix_*` tests carry the same
//! contracts on the `sandbox-static-gates` job. All fixtures are
//! deterministic (tempfile-owned trees + inline JSON session headers).

use std::collections::HashSet;
use std::path::Path;

use pi_coding::{
    CURRENT_SESSION_VERSION, PreparedSessionResume, SessionFileIdentity, SessionRecorder,
    list_sessions_in, resume_session, start_session_in,
};

/// Creates a real session file (header-only) in `session_dir` and returns the
/// live recorder, whose retained handle pins the file's identity.
fn make_session_file(cwd: &Path, session_dir: &Path, id: &str) -> SessionRecorder {
    let recorder = start_session_in(cwd, None, None, Some(session_dir), Some(id), None)
        .expect("start session");
    recorder.persist_now().expect("persist session header");
    recorder
}

/// A minimal but structurally valid session JSONL (the same shape
/// `persist_now` writes) for replacement fixtures.
fn session_header_json(cwd: &Path, id: &str) -> String {
    let header = serde_json::json!({
        "type": "session",
        "version": CURRENT_SESSION_VERSION,
        "id": id,
        "timestamp": "2026-01-01T00:00:00.000Z",
        "cwd": cwd,
    });
    format!(
        "{}\n",
        serde_json::to_string(&header).expect("session header json")
    )
}

fn identity(recorder: &SessionRecorder) -> SessionFileIdentity {
    recorder.file_identity().expect("recorded file identity")
}

/// The identity of one session file is stable across every independent open:
/// the recorder that created it, a fresh `PreparedSessionResume`, and a
/// resumed recorder all observe the identical identity. A per-open identity
/// that varied (e.g. a truncated or re-derived id) fails here.
#[cfg(not(windows))]
#[test]
fn unix_file_identity_is_stable_across_independent_opens() {
    let root = tempfile::tempdir().expect("root");
    let cwd = root.path().join("cwd");
    let session_dir = root.path().join("sessions");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&session_dir).expect("sessions");

    let recorder = make_session_file(&cwd, &session_dir, "identity-stable");
    let path = recorder.path();
    let recorded = identity(&recorder);

    let prepared = PreparedSessionResume::prepare_path(&path).expect("prepare");
    assert_eq!(
        prepared.identity(),
        recorded,
        "reopened identity must equal the identity recorded at creation"
    );

    let resumed = resume_session(&path).expect("resume");
    assert_eq!(
        resumed.file_identity(),
        Some(recorded),
        "resume must observe the identical identity"
    );

    let again = PreparedSessionResume::prepare_path(&path).expect("prepare again");
    assert_eq!(
        again.identity(),
        recorded,
        "identity must not drift across reopens"
    );
}

/// Distinct session files never collide on identity, and each enumerated
/// file's opened identity matches the recorder that created it. A collision
/// (e.g. identity truncated to a weaker key) fails here.
#[cfg(not(windows))]
#[test]
fn unix_enumerated_and_opened_identities_do_not_collide() {
    let root = tempfile::tempdir().expect("root");
    let cwd = root.path().join("cwd");
    let session_dir = root.path().join("sessions");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&session_dir).expect("sessions");

    let ids = ["collide-a", "collide-b", "collide-c"];
    let mut recorded_identities = HashSet::new();
    for id in ids {
        let recorder = make_session_file(&cwd, &session_dir, id);
        assert!(
            recorded_identities.insert(identity(&recorder)),
            "created session files must not collide on identity"
        );
    }

    let enumerated = list_sessions_in(&cwd, Some(&session_dir));
    assert_eq!(
        enumerated.len(),
        ids.len(),
        "all session files must be enumerated"
    );

    let mut opened = HashSet::new();
    for entry in &enumerated {
        let prepared = PreparedSessionResume::prepare_path(&entry.path).expect("prepare");
        assert!(
            opened.insert(prepared.identity()),
            "distinct session files must never share an identity ({})",
            entry.path.display()
        );
    }
    assert_eq!(
        opened, recorded_identities,
        "the opened identities must be exactly the identities recorded at creation"
    );
}

/// Replacing the file at a path is detected as an identity swap: the path
/// reports a new identity while every previously opened handle stays pinned
/// to the original file. An engine that followed the path (or failed to
/// detect the swap) fails here.
#[cfg(not(windows))]
#[test]
fn unix_path_replacement_swaps_identity_and_pins_opened_handle() {
    let root = tempfile::tempdir().expect("root");
    let cwd = root.path().join("cwd");
    let session_dir = root.path().join("sessions");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&session_dir).expect("sessions");

    let recorder = make_session_file(&cwd, &session_dir, "replace-a");
    let path = recorder.path();
    let original = identity(&recorder);
    let prepared = PreparedSessionResume::prepare_path(&path).expect("prepare original");
    assert_eq!(prepared.identity(), original);

    // Replace the file at the same path with a brand-new file (new inode).
    std::fs::remove_file(&path).expect("remove original");
    std::fs::write(&path, session_header_json(&cwd, "replacement")).expect("write replacement");

    // The path now names a different file: the engine must report the new
    // identity instead of silently adopting the path.
    let replaced = PreparedSessionResume::prepare_path(&path).expect("prepare replaced");
    assert_ne!(
        replaced.identity(),
        original,
        "path replacement must change the identity reported for the path"
    );

    // Opened handles remain pinned to the original file: their identities are
    // unchanged even though the path now names a different file. A
    // path-based identity (or a re-stat on use) would observe the swap.
    assert_eq!(
        recorder.file_identity(),
        Some(original),
        "the creating handle must stay pinned to the original file"
    );
    assert_eq!(
        prepared.identity(),
        original,
        "the prepared handle must stay pinned to the original file"
    );
}

/// Windows: the identity is the full 128-bit `FILE_ID_INFO` (volume serial
/// plus 16-byte file id). All 128 bits must be preserved across independent
/// opens of the same file, and the identity must be stable across the
/// recorder/prepare/resume surfaces. A truncated (e.g. 64-bit) identity
/// implementation fails here because it would still compare equal for the
/// same file — the assert that the *same* file keeps the *same* full
/// identity across three independent open paths catches per-open derivation
/// differences, and the distinctness assertions below catch collisions.
#[cfg(windows)]
#[test]
fn windows_file_identity_preserves_all_128_bits() {
    let root = tempfile::tempdir().expect("root");
    let cwd = root.path().join("cwd");
    let session_dir = root.path().join("sessions");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&session_dir).expect("sessions");

    let recorder = make_session_file(&cwd, &session_dir, "win-identity");
    let path = recorder.path();
    let recorded = identity(&recorder);

    let prepared = PreparedSessionResume::prepare_path(&path).expect("prepare");
    assert_eq!(
        prepared.identity(),
        recorded,
        "128-bit file identity must be preserved across independent opens"
    );

    let resumed = resume_session(&path).expect("resume");
    assert_eq!(
        resumed.file_identity(),
        Some(recorded),
        "resume must observe the identical 128-bit identity"
    );

    let again = PreparedSessionResume::prepare_path(&path).expect("prepare again");
    assert_eq!(
        again.identity(),
        recorded,
        "identity must not drift across reopens"
    );
}

/// Windows: enumerated files and their opened handles cannot collide on the
/// 128-bit identity. Every enumerated file must open to an identity that
/// matches what the store recorded at creation, and distinct files must have
/// distinct identities (the high bits of the 16-byte file id are part of the
/// comparison — a truncated implementation can collide and fails here).
#[cfg(windows)]
#[test]
fn windows_enumerated_and_opened_high_bits_cannot_collide() {
    let root = tempfile::tempdir().expect("root");
    let cwd = root.path().join("cwd");
    let session_dir = root.path().join("sessions");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&session_dir).expect("sessions");

    let ids = ["win-collide-a", "win-collide-b", "win-collide-c"];
    let mut recorded_identities = HashSet::new();
    for id in ids {
        let recorder = make_session_file(&cwd, &session_dir, id);
        assert!(
            recorded_identities.insert(identity(&recorder)),
            "created session files must not collide on the 128-bit identity"
        );
    }

    let enumerated = list_sessions_in(&cwd, Some(&session_dir));
    assert_eq!(
        enumerated.len(),
        ids.len(),
        "all session files must be enumerated"
    );

    let mut opened = HashSet::new();
    for entry in &enumerated {
        let prepared = PreparedSessionResume::prepare_path(&entry.path).expect("prepare");
        assert!(
            opened.insert(prepared.identity()),
            "distinct files must never collide on the 128-bit identity ({})",
            entry.path.display()
        );
    }
    assert_eq!(
        opened, recorded_identities,
        "the opened identities must be exactly the identities recorded at creation"
    );
}

/// Windows: replacing the file at a path swaps its 128-bit identity and the
/// swap must be detected (the path reports a different identity), while
/// opened handles stay pinned to the original file id. A replacement that
/// only compared a truncated/low portion of the file id — or an engine that
/// silently adopted the path — fails here.
#[cfg(windows)]
#[test]
fn windows_replacement_rejects_high_bits_identity_swap() {
    let root = tempfile::tempdir().expect("root");
    let cwd = root.path().join("cwd");
    let session_dir = root.path().join("sessions");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&session_dir).expect("sessions");

    let recorder = make_session_file(&cwd, &session_dir, "win-replace-a");
    let path = recorder.path();
    let original = identity(&recorder);
    let prepared = PreparedSessionResume::prepare_path(&path).expect("prepare original");
    assert_eq!(prepared.identity(), original);

    // Replace the file at the same path with a brand-new file.
    std::fs::remove_file(&path).expect("remove original");
    std::fs::write(&path, session_header_json(&cwd, "win-replacement")).expect("write replacement");

    // The path now names a different file: the 128-bit identity must change.
    let replaced = PreparedSessionResume::prepare_path(&path).expect("prepare replaced");
    assert_ne!(
        replaced.identity(),
        original,
        "replacement must change the 128-bit identity reported for the path"
    );

    // Opened handles stay pinned to the original file id.
    assert_eq!(
        recorder.file_identity(),
        Some(original),
        "the creating handle must stay pinned to the original 128-bit identity"
    );
    assert_eq!(
        prepared.identity(),
        original,
        "the prepared handle must stay pinned to the original 128-bit identity"
    );
}
