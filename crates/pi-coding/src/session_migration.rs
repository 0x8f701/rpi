//! Offline atomic relocation of a saved native session across checkouts.
//!
//! A saved session's storage location encodes its checkout path
//! (`--<encoded-cwd>--` under the native sessions root) and its header pins
//! the operator `cwd`, so moving a checkout orphans the session. This module
//! relocates a saved `.jsonl` session (plus its recognized
//! rewind/snapcompact/loops sidecars and its durable `children/<session-id>/`
//! subtree) to the default session directory of a NEW existing operator
//! directory, preserving the session id, session name, and file name exactly.
//!
//! The move is copy-then-verify-then-remove:
//!
//! 1. validate the target is an existing operator directory and the source is
//!    a retainable native session;
//! 2. copy the primary with a rewritten header `cwd` and one appended bounded
//!    `session_move` custom record, copy recognized sidecars verbatim, and
//!    copy the child subtree with rewritten child headers and
//!    orchestration-state `parentSessionPath`;
//! 3. fsync every file, then every directory bottom-up (the child leaf, the
//!    intermediate `children` directory whose entry links it, then the
//!    session-store root), validate the published destination by loading it
//!    back and by re-verifying every file against the move manifest, and only
//!    then remove the originals with identity-checked removals;
//! 4. roll back the destination on any pre-removal failure and restore any
//!    removed originals (from their retained open handles) on a mid-removal
//!    failure. A failure of the terminal source-directory fsync is routed
//!    through the same restoration path: the destination is only discarded
//!    once the source is proven restored and synced; otherwise the
//!    destination is retained and a recovery-required error is returned, so
//!    both copies are never lost.
//!
//! Collision handling is content-based: every relocated artifact's expected
//! migrated bytes are digested into a manifest, and an existing destination
//! entry is accepted only when its digest, length, schema, and session id
//! match exactly. The durable `session_move` record carries the opaque
//! manifest digest and the session id — never absolute local paths — and
//! idempotent retries re-verify the destination body against the manifest
//! before treating the move as already committed.
//!
//! The memory namespace rebind is two-phase: the additive copy runs as part
//! of the rebind and the completeness-checking commit drops the old archive
//! only after the file move has committed. A failed commit keeps the copied
//! entries as recovery state, never rolls back the committed file move, and
//! is finalized idempotently by a retry.

use std::cell::RefCell;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{SecondsFormat, Utc};
use pi_sandbox::AbsolutePath;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::fs::{
    CapabilityAccess, CapabilityFile, CapabilityFs, CapabilityRootLease, CapabilityRootPurpose,
    EntryKind, FileIdentity, FileMode, FsError, RootCreation, is_link_blocked,
};
use crate::session_store::{
    PreparedSessionResume, SessionRecord, default_session_dir, load_session_tree,
    validate_session_id,
};
use crate::trust::{TrustDecision, TrustStore};
use crate::workspace::inspect_operator_directory;

/// Largest accepted session header line, mirroring the native store's
/// per-line limit.
const HEADER_LINE_LIMIT: usize = 8 * 1024 * 1024;
/// Bounded window for scanning an existing destination's trailing
/// `session_move` record.
const MOVE_RECORD_TAIL_LIMIT: u64 = 1024 * 1024;
/// Bounded read limit for the durable orchestration sidecar, matching
/// `MAX_SIDECAR_BYTES` in `orchestration/persistence.rs`.
const ORCHESTRATION_STATE_LIMIT: u64 = 16 * 1024 * 1024;
const SESSION_MOVE_CUSTOM_TYPE: &str = "session_move";
const ORCHESTRATION_STATE_FILENAME: &str = "orchestration-state.json";
const STREAM_COPY_BUFFER_BYTES: usize = 64 * 1024;
/// Manifest schema version persisted inside the `session_move` record.
const SESSION_MOVE_MANIFEST_SCHEMA: u32 = 1;

/// Ordered record of durability-relevant directory fsyncs (test seam).
type SyncLog = Rc<std::cell::RefCell<Vec<String>>>;
/// Ordered record of memory rebind calls (test seam).
type MemoryCallLog = Rc<std::cell::RefCell<Vec<String>>>;

/// Result of a successful [`move_saved_session`].
#[derive(Clone, Debug)]
pub struct SessionMoveOutcome {
    /// Unchanged `SessionHeader.id` of the moved session.
    pub session_id: String,
    /// Absolute path the session was moved from.
    pub from: PathBuf,
    /// Absolute path of the moved primary `.jsonl`.
    pub to: PathBuf,
    /// Number of recognized rewind/snapcompact/loops sidecars relocated.
    pub sidecars: usize,
    /// Number of durable child-subtree entries relocated.
    pub children: usize,
    /// Best-effort rebind results; failures surface as `warnings`, never abort.
    pub rebound: SessionMoveRebinds,
    /// Non-fatal warnings collected during rebinds.
    pub warnings: Vec<String>,
}

/// Which auxiliary durable stores were rebound to the new checkout path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionMoveRebinds {
    pub memory: bool,
    pub workflows: bool,
    pub trust: bool,
}

/// Injected failure points for rollback tests.
#[derive(Clone, Copy, Default)]
struct MoveFaults {
    fail_copy: Option<CopyStage>,
    fail_publish: bool,
    fail_remove_at: Option<usize>,
    /// Fail the terminal source-directory fsync after every original was
    /// removed, forcing the removal commit through the restore path.
    fail_source_final_fsync: bool,
    /// Fail the intermediate `children` directory fsync at the destination.
    fail_target_children_sync: bool,
    /// Fail the memory namespace commit after the additive copy succeeded.
    fail_memory_commit: bool,
    /// Fail restoring the first removed original during rollback.
    fail_restore: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CopyStage {
    Primary,
    Sidecar,
    Child,
}

/// Test seam: explicit target session directory / agent directory overrides.
#[derive(Default)]
struct MoveOptions<'a> {
    session_dir: Option<&'a Path>,
    agent_dir: Option<&'a Path>,
    faults: MoveFaults,
    sync_log: Option<SyncLog>,
    memory_calls: Option<MemoryCallLog>,
}

/// Move a saved native session file to the default session directory of
/// `new_cwd`, preserving its session id, session name, and file name.
///
/// `new_cwd` must be an existing operator directory. The move refuses
/// collisions unless every destination entry exactly matches the expected
/// migrated content manifest for this move, in which case the rerun is an
/// idempotent success and touches nothing.
pub fn move_saved_session(
    path: impl AsRef<Path>,
    new_cwd: impl AsRef<Path>,
) -> Result<SessionMoveOutcome> {
    move_saved_session_with(path, new_cwd, MoveOptions::default())
}

fn move_saved_session_with(
    path: impl AsRef<Path>,
    new_cwd: impl AsRef<Path>,
    options: MoveOptions<'_>,
) -> Result<SessionMoveOutcome> {
    let source_path = absolute_path(path.as_ref());
    if source_path.extension().and_then(OsStr::to_str) != Some("jsonl") {
        bail!(
            "refusing to move a non-native session file: {}",
            source_path.display()
        );
    }
    let primary_name = source_path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| anyhow!("session path has no file name: {}", source_path.display()))?
        .to_owned();

    // The target must be an existing operator directory: no symlinks, no
    // implicit creation.
    let new_cwd =
        inspect_operator_directory(new_cwd.as_ref()).context("validating move target directory")?;

    let prepared = PreparedSessionResume::prepare_path(&source_path)
        .context("retaining source session for move")?;
    let header = prepared.tree().header.clone();
    let session_id = header.id.clone();
    validate_session_id(&session_id).context("source session id is invalid")?;
    let old_cwd = header.cwd.clone();
    // The durable move marker must stay connected to the source's prior
    // active branch so a reopen restores the complete prior transcript.
    let move_parent_id = prepared
        .tree()
        .active_leaf_id
        .clone()
        .or_else(|| prepared.tree().leaf_id.clone());
    let (source_lease, source_relative) = prepared.file_authority();
    let source_root = source_lease.display_root().to_path_buf();

    let target_dir = match options.session_dir {
        Some(directory) => absolute_path(directory),
        None => default_session_dir(&new_cwd),
    };
    if source_lease.display_root() == target_dir {
        bail!(
            "session {} already resides in the target session directory {}",
            source_path.display(),
            target_dir.display()
        );
    }

    let capabilities =
        CapabilityFs::new().context("initializing session filesystem capability")?;
    let target_lease = capabilities
        .open_root(
            CapabilityRootPurpose::SessionStore,
            AbsolutePath::parse(target_dir.clone())
                .map_err(|_| anyhow!("invalid session directory {}", target_dir.display()))?,
            CapabilityAccess::PrivateReadWrite,
            RootCreation::CreateDirectory,
        )
        .context("opening target session directory")?;

    let agent_dir = options
        .agent_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(crate::resources::agent_dir_path);
    // Surfaced diagnostics replace every local root with a stable label —
    // most-specific first — so neither errors nor best-effort rebind
    // warnings disclose the workstation path layout.
    let scrub_roots = [
        (source_root.clone(), "<source-session-dir>"),
        (
            target_lease.display_root().to_path_buf(),
            "<destination-session-dir>",
        ),
        (agent_dir.clone(), "<agent-dir>"),
        (old_cwd.clone(), "<old-cwd>"),
        (new_cwd.as_path().to_path_buf(), "<new-cwd>"),
    ];

    let opened = source_lease
        .open_file(&source_relative)
        .context("reopening retained source session")?;
    if opened.public_identity() != prepared.identity() {
        bail!("source session identity changed while preparing the move");
    }
    let primary_stamp = opened.metadata().context("reading source session stamp")?;

    let source_stamps = collect_source_stamps(&source_lease, &primary_name, &session_id)?;

    let mut artifacts = MoveArtifacts::default();
    let sidecars = copy_sidecars(
        &source_lease,
        &target_lease,
        &source_stamps.sidecars,
        options.faults.fail_copy == Some(CopyStage::Sidecar),
        &mut artifacts,
    )
    .map_err(|error| abort_move(&target_lease, &mut artifacts, error, &scrub_roots))?;

    let children = copy_children(
        &source_lease,
        &target_lease,
        &session_id,
        source_stamps.children.as_ref(),
        &dest_path_for(&target_lease, &primary_name),
        new_cwd.as_path(),
        options.faults.fail_copy == Some(CopyStage::Child),
        &mut artifacts,
    )
    .map_err(|error| abort_move(&target_lease, &mut artifacts, error, &scrub_roots))?;

    let dest_path = dest_path_for(&target_lease, &primary_name);
    copy_primary(
        &opened,
        &target_lease,
        &primary_name,
        &session_id,
        move_parent_id.as_deref(),
        &dest_path,
        new_cwd.as_path(),
        options.faults.fail_copy == Some(CopyStage::Primary),
        &mut artifacts,
    )
    .map_err(|error| abort_move(&target_lease, &mut artifacts, error, &scrub_roots))?;

    // Fsync directories bottom-up, then publish: validate the destination
    // loads with the preserved identity and matches the manifest before any
    // original is removed.
    sync_target_directories(
        &target_lease,
        artifacts.children_relative.as_deref(),
        options.faults,
        &options.sync_log,
    )
    .map_err(|error| abort_move(&target_lease, &mut artifacts, error, &scrub_roots))?;
    if options.faults.fail_publish {
        let error = anyhow!("publish validation failed");
        return Err(abort_move(&target_lease, &mut artifacts, error, &scrub_roots));
    }
    validate_published_destination(
        &target_lease,
        &primary_name,
        &session_id,
        new_cwd.as_path(),
        &artifacts.manifest,
    )
    .map_err(|error| abort_move(&target_lease, &mut artifacts, error, &scrub_roots))?;
    // Crash-before-removal retry: a destination that already carries this
    // move's exact manifest — and was not extended by this attempt — plus a
    // restored original source is an already-committed move. Return success
    // without removing or rewriting the source; the durable rebinds below
    // still finalize any pending memory commit.
    let already_committed =
        artifacts.already_committed && artifacts.files.is_empty() && !artifacts.children_dir_created;
    if !already_committed {
        remove_source(
            &source_lease,
            &source_relative,
            primary_stamp.identity,
            primary_stamp.len,
            primary_stamp.modified_ns,
            &source_stamps,
            &session_id,
            options.faults,
            &options.sync_log,
        )
        .map_err(|failure| {
            // The destination may only be discarded once the source originals
            // are proven restored and their directories synced. If restoration
            // could not be proven, retain the migrated destination so both
            // copies are never lost.
            let (error, restored) = match failure {
                RemovalFailure::Restored(error) => (error, true),
                RemovalFailure::RecoveryRequired(error) => (error, false),
            };
            let error = scrub_error(error, &scrub_roots);
            if !restored {
                return error.context(
                    "source originals could not be restored; retaining the migrated destination for recovery",
                );
            }
            match cleanup_target(&target_lease, &artifacts) {
                Ok(()) => error.context("destination rolled back after removal failure"),
                Err(cleanup) => {
                    error.context(format!("destination cleanup also failed: {cleanup:#}"))
                }
            }
        })?;
    }

    let (rebound, warnings) = rebind_durable_state(
        &capabilities,
        &agent_dir,
        &old_cwd,
        &new_cwd,
        &session_id,
        options.faults,
        &options.memory_calls,
        &scrub_roots,
    );
    Ok(SessionMoveOutcome {
        session_id,
        from: source_path,
        to: dest_path,
        sidecars: sidecars.len(),
        children,
        rebound,
        warnings,
    })
}

fn dest_path_for(target_lease: &CapabilityRootLease, primary_name: &str) -> PathBuf {
    target_lease.display_root().join(primary_name)
}

// ---------------------------------------------------------------------------
// Source survey
// ---------------------------------------------------------------------------

struct SourceStamps {
    sidecars: Vec<SidecarStamp>,
    children: Option<ChildrenStamp>,
}

struct SidecarStamp {
    name: OsString,
    identity: FileIdentity,
    len: u64,
    modified_ns: i128,
}

struct ChildrenStamp {
    dir_identity: FileIdentity,
    files: Vec<ChildrenFileStamp>,
}

struct ChildrenFileStamp {
    name: OsString,
    identity: FileIdentity,
    len: u64,
    modified_ns: i128,
}

/// Recognized sidecar names for a primary session file, mirroring the native
/// archive contract: `<primary>.loops.json`, `<primary>.rewind-<ts>[-n].jsonl`,
/// and `<primary>.snapcompact-<n>.jsonl`. Nothing else is relocated.
fn is_move_sidecar(primary_name: &str, candidate: &OsStr) -> bool {
    let Some(candidate) = candidate.to_str() else {
        return false;
    };
    if candidate == format!("{primary_name}.loops.json") {
        return true;
    }
    ["rewind-", "snapcompact-"].into_iter().any(|kind| {
        candidate
            .strip_prefix(&format!("{primary_name}.{kind}"))
            .is_some_and(|suffix| suffix.len() > ".jsonl".len() && suffix.ends_with(".jsonl"))
    })
}

fn collect_source_stamps(
    source_lease: &CapabilityRootLease,
    primary_name: &str,
    session_id: &str,
) -> Result<SourceStamps> {
    let mut stamps = SourceStamps {
        sidecars: Vec::new(),
        children: None,
    };
    let mut names: Vec<OsString> = source_lease
        .open_directory(Path::new(""))
        .context("enumerating source session directory")?
        .read_dir_names()
        .context("reading source session directory names")?;
    names.sort();
    for name in names {
        if name.as_encoded_bytes() == primary_name.as_bytes() || !is_move_sidecar(primary_name, &name)
        {
            continue;
        }
        let file = match source_lease.open_file(Path::new(&name)) {
            Ok(file) => file,
            Err(error) if is_link_blocked(&error) || matches!(error, crate::fs::FsError::UnsupportedEntryType) =>
            {
                continue;
            }
            Err(error) => {
                return Err(anyhow::Error::new(error).context("opening session move sidecar"));
            }
        };
        let metadata = file
            .metadata()
            .context("reading session move sidecar metadata")?;
        stamps.sidecars.push(SidecarStamp {
            name,
            identity: metadata.identity,
            len: metadata.len,
            modified_ns: metadata.modified_ns,
        });
    }
    let children_relative = Path::new("children").join(session_id);
    if !source_lease
        .entry_exists(&children_relative)
        .context("probing source child subtree")?
    {
        return Ok(stamps);
    }
    let mut child_names: Vec<OsString> = source_lease
        .open_directory(&children_relative)
        .context("opening source child subtree")?
        .read_dir_names()
        .context("reading source child subtree names")?;
    child_names.sort();
    let mut files = Vec::new();
    for name in child_names {
        let relative = children_relative.join(&name);
        let metadata = source_lease
            .metadata(&relative)
            .with_context(|| format!("reading child entry {}", relative.display()))?;
        if metadata.identity.kind() != EntryKind::RegularFile {
            bail!(
                "refusing to move unexpected non-file entry {} in the durable child subtree",
                relative.display()
            );
        }
        files.push(ChildrenFileStamp {
            name,
            identity: metadata.identity,
            len: metadata.len,
            modified_ns: metadata.modified_ns,
        });
    }
    stamps.children = Some(ChildrenStamp {
        dir_identity: source_lease
            .metadata(&children_relative)
            .context("reading source child subtree identity")?
            .identity,
        files,
    });
    Ok(stamps)
}

// ---------------------------------------------------------------------------
// Content manifest
// ---------------------------------------------------------------------------

/// One entry of the move content manifest: the exact migrated bytes of a
/// relocated artifact, identified by a store-relative logical name (never an
/// absolute path) plus its kind, byte length, and SHA-256 digest.
#[derive(Clone, Debug)]
struct ManifestEntry {
    name: String,
    kind: &'static str,
    len: u64,
    digest: String,
}

/// Canonical digest over the full content manifest for this move. The digest
/// is the only durable authority for idempotent retries: it covers the
/// session id and the exact expected migrated bytes of the primary, every
/// recognized sidecar, and every child entry, so a forged path-string marker
/// without matching content is rejected.
fn manifest_digest(session_id: &str, entries: &[ManifestEntry]) -> Result<String> {
    let payload = serde_json::to_vec(&json!({
        "schema": SESSION_MOVE_MANIFEST_SCHEMA,
        "sessionId": session_id,
        "entries": entries
            .iter()
            .map(|entry| {
                json!({
                    "name": entry.name,
                    "kind": entry.kind,
                    "len": entry.len,
                    "digest": entry.digest,
                })
            })
            .collect::<Vec<_>>(),
    }))
    .context("serializing move manifest")?;
    let mut hasher = Sha256::new();
    hasher.update(&payload);
    let (digest, _) = finish_digest(hasher, payload.len() as u64);
    Ok(digest)
}

fn finish_digest(hasher: Sha256, count: u64) -> (String, u64) {
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        let _ = write!(hex, "{byte:02x}");
    }
    (hex, count)
}

/// Write sink that optionally forwards bytes into a destination file while
/// maintaining the running SHA-256 of everything written. `dest: None`
/// computes the expected migrated bytes without writing anywhere.
struct DigestWriter<'a> {
    dest: Option<&'a mut crate::fs::CapabilityWritableFile>,
    hasher: &'a mut Sha256,
    count: u64,
}

impl Write for DigestWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(dest) = self.dest.as_mut() {
            dest.write_all(buf)
                .map_err(|error| std::io::Error::other(error.to_string()))?;
        }
        self.hasher.update(buf);
        self.count += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
struct FileDigest {
    digest: String,
    len: u64,
}

/// Streaming SHA-256 over a whole file.
fn file_digest(file: &CapabilityFile) -> Result<FileDigest> {
    let mut reader = file
        .try_clone_std()
        .context("cloning file for digest")?;
    reader
        .seek(SeekFrom::Start(0))
        .context("seeking file for digest")?;
    let mut hasher = Sha256::new();
    let mut count = 0_u64;
    let mut buffer = [0_u8; STREAM_COPY_BUFFER_BYTES];
    loop {
        let read = reader.read(&mut buffer).context("digesting file")?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        count += read as u64;
    }
    let (digest, len) = finish_digest(hasher, count);
    Ok(FileDigest { digest, len })
}

/// Streaming SHA-256 over exactly the first `bytes` of a file.
fn prefix_file_digest(file: &CapabilityFile, bytes: u64) -> Result<FileDigest> {
    let mut reader = file
        .try_clone_std()
        .context("cloning file for prefix digest")?;
    reader
        .seek(SeekFrom::Start(0))
        .context("seeking file for prefix digest")?;
    let mut hasher = Sha256::new();
    let mut remaining = bytes;
    let mut buffer = [0_u8; STREAM_COPY_BUFFER_BYTES];
    while remaining > 0 {
        let want = remaining.min(buffer.len() as u64) as usize;
        let read = reader
            .read(&mut buffer[..want])
            .context("digesting file prefix")?;
        if read == 0 {
            bail!("file ended before the expected {bytes} byte prefix");
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    let (digest, len) = finish_digest(hasher, bytes);
    Ok(FileDigest { digest, len })
}

/// SHA-256 over the verbatim bytes of a stream, with its byte count.
fn digest_reader(reader: &mut File) -> Result<FileDigest> {
    let mut hasher = Sha256::new();
    let mut count = 0_u64;
    let mut buffer = [0_u8; STREAM_COPY_BUFFER_BYTES];
    loop {
        let read = reader.read(&mut buffer).context("digesting stream")?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        count += read as u64;
    }
    let (digest, len) = finish_digest(hasher, count);
    Ok(FileDigest { digest, len })
}

// ---------------------------------------------------------------------------
// Copy phase
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MoveArtifacts {
    /// Relative paths created at the destination, in creation order.
    files: Vec<PathBuf>,
    children_dir_created: bool,
    children_parent_created: bool,
    children_relative: Option<PathBuf>,
    /// The destination primary already carried this move's exact manifest
    /// (a previously committed attempt); nothing was created by this run.
    already_committed: bool,
    /// Manifest entry for every relocated or already-migrated artifact, in
    /// survey order (sidecars, children, primary).
    manifest: Vec<ManifestEntry>,
}

fn abort_move(
    target_lease: &CapabilityRootLease,
    artifacts: &mut MoveArtifacts,
    error: anyhow::Error,
    scrub_roots: &[(PathBuf, &str)],
) -> anyhow::Error {
    let error = scrub_error(error, scrub_roots);
    match cleanup_target(target_lease, artifacts) {
        Ok(()) => error.context("move aborted; destination rolled back"),
        Err(cleanup) => error.context(format!(
            "move aborted and destination cleanup also failed: {cleanup:#}"
        )),
    }
}

fn cleanup_target(
    target_lease: &CapabilityRootLease,
    artifacts: &MoveArtifacts,
) -> Result<()> {
    for relative in artifacts.files.iter().rev() {
        match target_lease.remove_file(relative) {
            Ok(()) => {}
            // An unfinished destination write handle is rolled back on
            // drop, so a faulted copy can leave the entry already gone.
            // NotFound is already-clean success, not a cleanup failure.
            Err(FsError::System {
                kind: ErrorKind::NotFound,
                ..
            }) => {}
            Err(error) => {
                return Err(anyhow::Error::new(error).context(format!(
                    "removing created destination file {}",
                    relative.display()
                )));
            }
        }
    }
    if artifacts.children_dir_created {
        let children_relative = artifacts
            .children_relative
            .as_ref()
            .expect("children dir creation recorded its path");
        let metadata = match target_lease.metadata(children_relative) {
            Ok(metadata) => Some(metadata),
            // A faulted subtree creation can leave the leaf already gone;
            // like the file policy above, NotFound is already-clean.
            Err(FsError::System {
                kind: ErrorKind::NotFound,
                ..
            }) => None,
            Err(error) => {
                return Err(anyhow::Error::new(error).context(format!(
                    "stamping created destination child subtree {}",
                    children_relative.display()
                )));
            }
        };
        let parent = children_relative
            .parent()
            .expect("children subtree has a parent");
        if let Some(metadata) = metadata {
            target_lease.remove_tree_expected(children_relative, metadata.identity)?;
            target_lease.sync_directory(parent)?;
        }
        if artifacts.children_parent_created
            && target_lease
                .open_directory(parent)?
                .read_dir_names()?
                .is_empty()
        {
            target_lease.remove_directory(parent)?;
        }
    }
    target_lease.sync_directory(Path::new(""))?;
    Ok(())
}

/// Durability ordering for the published destination: the child leaf
/// directory's own entries first, then the intermediate `children` directory
/// whose entry links the leaf, then the session-store root.
fn sync_target_directories(
    target_lease: &CapabilityRootLease,
    children_relative: Option<&Path>,
    faults: MoveFaults,
    log: &Option<SyncLog>,
) -> Result<()> {
    if let Some(children_relative) = children_relative {
        sync_logged(target_lease, children_relative, log, "target:children-leaf")
            .context("syncing destination child subtree")?;
        if faults.fail_target_children_sync {
            bail!("injected children directory sync failure");
        }
        let parent = children_relative
            .parent()
            .expect("children subtree has a parent");
        sync_logged(target_lease, parent, log, "target:children-parent")
            .context("syncing destination children directory")?;
    }
    sync_logged(target_lease, Path::new(""), log, "target:root")
        .context("syncing destination session directory")?;
    Ok(())
}

fn sync_logged(
    lease: &CapabilityRootLease,
    relative: &Path,
    log: &Option<SyncLog>,
    label: &str,
) -> Result<(), crate::fs::FsError> {
    let result = lease.sync_directory(relative);
    if result.is_ok() && let Some(log) = log {
        log.borrow_mut().push(label.to_owned());
    }
    result
}

struct HeaderLine {
    text: String,
    consumed: u64,
    ends_with_newline: bool,
}

fn read_first_line(file: &CapabilityFile) -> Result<HeaderLine> {
    let mut reader = file
        .try_clone_std()
        .context("cloning session file for header read")?;
    reader
        .seek(SeekFrom::Start(0))
        .context("seeking session file for header read")?;
    let mut text: Vec<u8> = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut consumed = 0_u64;
    loop {
        let count = reader.read(&mut buffer).context("reading session header")?;
        if count == 0 {
            anyhow::ensure!(
                !text.is_empty(),
                "session file has no header line"
            );
            return Ok(HeaderLine {
                text: String::from_utf8(text).context("session header is not UTF-8")?,
                consumed,
                ends_with_newline: false,
            });
        }
        if let Some(position) = buffer[..count].iter().position(|&byte| byte == b'\n') {
            text.extend_from_slice(&buffer[..position]);
            consumed += position as u64 + 1;
            return Ok(HeaderLine {
                text: String::from_utf8(text).context("session header is not UTF-8")?,
                consumed,
                ends_with_newline: true,
            });
        }
        text.extend_from_slice(&buffer[..count]);
        consumed += count as u64;
        if text.len() > HEADER_LINE_LIMIT {
            bail!("session header line exceeds the {} byte limit", HEADER_LINE_LIMIT);
        }
    }
}

struct TailLine {
    text: String,
    /// Byte length of the trailing line, including its terminating newline
    /// when the file ends with one.
    bytes: u64,
}

/// Reads the final record line, bounded to [`MOVE_RECORD_TAIL_LIMIT`].
fn read_last_line(file: &CapabilityFile) -> Result<Option<TailLine>> {
    let len = file.metadata().context("reading session tail metadata")?.len;
    if len == 0 {
        return Ok(None);
    }
    let window = len.min(MOVE_RECORD_TAIL_LIMIT) as usize;
    let mut reader = file
        .try_clone_std()
        .context("cloning session file for tail read")?;
    reader
        .seek(SeekFrom::Start(len - window as u64))
        .context("seeking session file for tail read")?;
    let mut tail = vec![0_u8; window];
    reader
        .read_exact(&mut tail)
        .context("reading session tail")?;
    let ends_with_newline = tail.last() == Some(&b'\n');
    if ends_with_newline {
        tail.pop();
    }
    let start = tail
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |position| position + 1);
    let mut bytes = (tail.len() - start) as u64;
    if ends_with_newline {
        bytes += 1;
    }
    Ok(Some(TailLine {
        text: String::from_utf8_lossy(&tail[start..]).into_owned(),
        bytes,
    }))
}

/// The durable `session_move` record found at the tail of a destination
/// session, if any. Only the record's shape and its opaque `sessionId`,
/// `manifest`, and `parentId` fields are surfaced; the marker itself
/// authorizes nothing.
struct MoveRecordTail {
    /// Byte length of the trailing record line including its newline.
    bytes: u64,
    session_id: String,
    manifest: String,
    /// Ancestry of the trailing marker: `None` for a root child.
    parent_id: Option<String>,
}

fn trailing_move_record(destination: &CapabilityFile) -> Result<Option<MoveRecordTail>> {
    let Some(tail) = read_last_line(destination)? else {
        return Ok(None);
    };
    let Ok(record) = serde_json::from_str::<Value>(&tail.text) else {
        return Ok(None);
    };
    if record.get("type").and_then(Value::as_str) != Some("custom")
        || record.get("customType").and_then(Value::as_str) != Some(SESSION_MOVE_CUSTOM_TYPE)
    {
        return Ok(None);
    }
    let Some(data) = record.get("data") else {
        return Ok(None);
    };
    let (Some(session_id), Some(manifest)) = (
        data.get("sessionId").and_then(Value::as_str),
        data.get("manifest").and_then(Value::as_str),
    ) else {
        return Ok(None);
    };
    Ok(Some(MoveRecordTail {
        bytes: tail.bytes,
        session_id: session_id.to_owned(),
        manifest: manifest.to_owned(),
        parent_id: record
            .get("parentId")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }))
}

fn parse_header_object(line: &str, display: &Path) -> Result<Value> {
    let header: Value = serde_json::from_str(line.trim_end_matches(['\n', '\r']))
        .with_context(|| format!("parsing session header {}", display.display()))?;
    if !header.is_object() {
        bail!("session header {} is not a JSON object", display.display());
    }
    Ok(header)
}

/// Serializes the migrated form of a session header: the parsed source header
/// with the given field rewrites, followed by a newline.
fn migrated_header_bytes(
    header_line: &HeaderLine,
    display: &Path,
    rewrites: &[(&str, String)],
) -> Result<Vec<u8>> {
    let mut header = parse_header_object(&header_line.text, display)?;
    let object = header
        .as_object_mut()
        .expect("parse_header_object returns an object");
    for (key, value) in rewrites {
        object.insert((*key).to_owned(), Value::String(value.clone()));
    }
    let mut bytes = serde_json::to_vec(&header)
        .with_context(|| format!("serializing migrated header {}", display.display()))?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Streams the source body (everything after the header) into `writer`,
/// appending a separator newline when the source does not end with one.
fn stream_primary_body(
    source: &CapabilityFile,
    header_line: &HeaderLine,
    writer: &mut DigestWriter<'_>,
) -> Result<()> {
    let mut reader = source
        .try_clone_std()
        .context("cloning source session for body copy")?;
    reader
        .seek(SeekFrom::Start(header_line.consumed))
        .context("seeking source session past header")?;
    let mut buffer = [0_u8; STREAM_COPY_BUFFER_BYTES];
    let mut last_byte = if header_line.ends_with_newline {
        b'\n'
    } else {
        *header_line.text.as_bytes().last().unwrap_or(&b'\n')
    };
    loop {
        let count = reader
            .read(&mut buffer)
            .context("streaming moved session body")?;
        if count == 0 {
            break;
        }
        last_byte = buffer[count - 1];
        writer
            .write_all(&buffer[..count])
            .context("streaming moved session body")?;
    }
    if last_byte != b'\n' {
        writer
            .write_all(b"\n")
            .context("writing session body separator")?;
    }
    Ok(())
}

/// Streams everything after the header verbatim into `writer`.
fn stream_remainder(
    source: &CapabilityFile,
    header_line: &HeaderLine,
    writer: &mut DigestWriter<'_>,
) -> Result<()> {
    let mut reader = source
        .try_clone_std()
        .context("cloning source for body copy")?;
    reader
        .seek(SeekFrom::Start(header_line.consumed))
        .context("seeking source past header")?;
    let mut buffer = [0_u8; STREAM_COPY_BUFFER_BYTES];
    loop {
        let count = reader.read(&mut buffer).context("streaming session body")?;
        if count == 0 {
            return Ok(());
        }
        writer
            .write_all(&buffer[..count])
            .context("streaming session body")?;
    }
}

/// Streams a whole file verbatim into `writer`.
fn stream_file(source: &CapabilityFile, writer: &mut DigestWriter<'_>) -> Result<()> {
    let mut reader = source
        .try_clone_std()
        .context("cloning file for verbatim copy")?;
    reader
        .seek(SeekFrom::Start(0))
        .context("seeking file for verbatim copy")?;
    let mut buffer = [0_u8; STREAM_COPY_BUFFER_BYTES];
    loop {
        let count = reader.read(&mut buffer).context("streaming verbatim copy")?;
        if count == 0 {
            return Ok(());
        }
        writer
            .write_all(&buffer[..count])
            .context("streaming verbatim copy")?;
    }
}

/// Writes (or hashes only, when `dest` is `None`) the migrated primary: the
/// rewritten header followed by the verbatim body. The trailing
/// `session_move` record is excluded so the manifest entry is deterministic
/// across attempts.
fn migrated_primary_entry(
    source: &CapabilityFile,
    dest_display: &Path,
    new_cwd: &Path,
    primary_name: &str,
    dest: Option<&mut crate::fs::CapabilityWritableFile>,
) -> Result<ManifestEntry> {
    let header_line = read_first_line(source)?;
    let header_bytes = migrated_header_bytes(
        &header_line,
        dest_display,
        &[("cwd", new_cwd.to_string_lossy().into_owned())],
    )?;
    let mut hasher = Sha256::new();
    let count = {
        let mut writer = DigestWriter { dest, hasher: &mut hasher, count: 0 };
        writer
            .write_all(&header_bytes)
            .context("writing moved session header")?;
        stream_primary_body(source, &header_line, &mut writer)?;
        writer.count
    };
    let (digest, len) = finish_digest(hasher, count);
    Ok(ManifestEntry {
        name: primary_name.to_owned(),
        kind: "primary",
        len,
        digest,
    })
}

/// The session id of a child `.jsonl` whose first line is a genuine session
/// header (an object with `"type":"session"` and a nonempty string `id`).
/// `None` for opaque JSONL payloads (orchestration logs, partial journals,
/// foreign-format lines), which relocate verbatim.
fn child_session_header_id(header_line: &HeaderLine, display: &Path) -> Option<String> {
    let header = parse_header_object(&header_line.text, display).ok()?;
    if header.get("type").and_then(Value::as_str) != Some("session") {
        return None;
    }
    let id = header.get("id").and_then(Value::as_str)?;
    (!id.is_empty()).then(|| id.to_owned())
}

/// Writes (or hashes only) the migrated form of a child entry: a rewritten
/// child-session header, rewritten orchestration state, or verbatim bytes.
fn migrated_child_entry(
    source: &CapabilityFile,
    name: &str,
    manifest_name: &str,
    dest_primary_path: &Path,
    new_cwd: &Path,
    dest: Option<&mut crate::fs::CapabilityWritableFile>,
) -> Result<ManifestEntry> {
    if name.ends_with(".jsonl") {
        let display = dest_primary_path.join(name);
        let header_line = read_first_line(source)?;
        // A .jsonl child whose first line is a genuine session header is a
        // child SESSION: its header is rewritten and the published copy is
        // strictly validated. Any other JSONL payload (opaque orchestration
        // logs, partial journals, foreign-format lines) relocates VERBATIM —
        // rewriting a non-session header would corrupt the payload and abort
        // the whole move at publish validation.
        if child_session_header_id(&header_line, &display).is_none() {
            return migrated_verbatim_entry(source, manifest_name, "child-verbatim", dest);
        }
        let header_bytes = migrated_header_bytes(
            &header_line,
            &display,
            &[
                ("cwd", new_cwd.to_string_lossy().into_owned()),
                (
                    "parentSession",
                    dest_primary_path.to_string_lossy().into_owned(),
                ),
            ],
        )?;
        let mut hasher = Sha256::new();
        let count = {
            let mut writer = DigestWriter { dest, hasher: &mut hasher, count: 0 };
            writer
                .write_all(&header_bytes)
                .context("writing moved child header")?;
            stream_remainder(source, &header_line, &mut writer)?;
            writer.count
        };
        let (digest, len) = finish_digest(hasher, count);
        return Ok(ManifestEntry {
            name: manifest_name.to_owned(),
            kind: "child-session",
            len,
            digest,
        });
    }
    if name == ORCHESTRATION_STATE_FILENAME {
        let bytes = source
            .read_bounded(ORCHESTRATION_STATE_LIMIT)
            .context("reading source orchestration state")?;
        let out = migrated_orchestration_bytes(&bytes, dest_primary_path)?;
        let mut hasher = Sha256::new();
        let count = {
            let mut writer = DigestWriter { dest, hasher: &mut hasher, count: 0 };
            writer
                .write_all(&out)
                .context("writing moved orchestration state")?;
            writer.count
        };
        let (digest, len) = finish_digest(hasher, count);
        return Ok(ManifestEntry {
            name: manifest_name.to_owned(),
            kind: "child-orchestration",
            len,
            digest,
        });
    }
    migrated_verbatim_entry(source, manifest_name, "child-file", dest)
}

/// Writes (or hashes only) the verbatim bytes of a source file.
fn migrated_verbatim_entry(
    source: &CapabilityFile,
    manifest_name: &str,
    kind: &'static str,
    dest: Option<&mut crate::fs::CapabilityWritableFile>,
) -> Result<ManifestEntry> {
    let mut hasher = Sha256::new();
    let count = {
        let mut writer = DigestWriter { dest, hasher: &mut hasher, count: 0 };
        stream_file(source, &mut writer)?;
        writer.count
    };
    let (digest, len) = finish_digest(hasher, count);
    Ok(ManifestEntry {
        name: manifest_name.to_owned(),
        kind,
        len,
        digest,
    })
}

/// The migrated orchestration-state bytes: the parsed state with its
/// `parentSessionPath` rewritten, pretty-printed with a trailing newline.
fn migrated_orchestration_bytes(bytes: &[u8], dest_primary_path: &Path) -> Result<Vec<u8>> {
    let mut state: Value =
        serde_json::from_slice(bytes).context("parsing source orchestration state")?;
    state
        .as_object_mut()
        .ok_or_else(|| anyhow!("orchestration state is not a JSON object"))?
        .insert(
            "parentSessionPath".to_owned(),
            Value::String(dest_primary_path.to_string_lossy().into_owned()),
        );
    let mut out = serde_json::to_vec_pretty(&state).context("serializing moved orchestration state")?;
    out.push(b'\n');
    Ok(out)
}
fn copy_primary(
    source: &CapabilityFile,
    target_lease: &CapabilityRootLease,
    primary_name: &str,
    session_id: &str,
    move_parent_id: Option<&str>,
    dest_path: &Path,
    new_cwd: &Path,
    fail_copy: bool,
    artifacts: &mut MoveArtifacts,
) -> Result<()> {
    let primary_relative = PathBuf::from(primary_name);
    if target_lease
        .entry_exists(&primary_relative)
        .context("probing destination session file")?
    {
        let destination = target_lease
            .open_file(&primary_relative)
            .context("opening destination session file")?;
        let header_line = read_first_line(&destination)?;
        let header = parse_header_object(&header_line.text, dest_path)?;
        if header.get("id").and_then(Value::as_str) != Some(session_id) {
            bail!(
                "refusing move: destination {} already holds a different session",
                dest_path.display()
            );
        }
        if let Some(tail) = trailing_move_record(&destination)? {
            // A trailing marker authorizes nothing by itself: the whole body
            // must match this move's manifest before the destination is
            // accepted as an already-committed move.
            let expected =
                migrated_primary_entry(source, dest_path, new_cwd, primary_name, None)?;
            let mut entries = artifacts.manifest.clone();
            entries.push(expected.clone());
            let manifest = manifest_digest(session_id, &entries)?;
            if tail.session_id != session_id || tail.manifest != manifest {
                bail!(
                    "refusing move: destination {} carries a forged or unrelated session_move record",
                    dest_path.display()
                );
            }
            // The manifest excludes the marker itself, so ancestry must be
            // checked separately: a body-faithful marker that is detached
            // from the source's prior active branch is not this move.
            if tail.parent_id != move_parent_id.map(str::to_owned) {
                bail!(
                    "refusing move: destination {} session_move record does not connect to the source active branch",
                    dest_path.display()
                );
            }
            let dest_len = destination
                .metadata()
                .context("reading destination session length")?
                .len;
            let body_len = dest_len.checked_sub(tail.bytes).ok_or_else(|| {
                anyhow!(
                    "destination {} has a truncated session_move record",
                    dest_path.display()
                )
            })?;
            if body_len != expected.len {
                bail!(
                    "refusing move: destination {} body does not match its session_move manifest",
                    dest_path.display()
                );
            }
            if prefix_file_digest(&destination, body_len)?.digest != expected.digest {
                bail!(
                    "refusing move: destination {} body does not match its session_move manifest",
                    dest_path.display()
                );
            }
            artifacts.manifest.push(expected);
            artifacts.already_committed = true;
            // Idempotent: the destination is the exact migrated form of this
            // move; touch nothing.
            return Ok(());
        }
        let expected_source = file_digest(source)?;
        let existing = file_digest(&destination)?;
        if existing.digest == expected_source.digest {
            // Byte-equivalent destination (a prior interrupted copy): replace
            // it with the migrated copy.
            target_lease
                .remove_file_expected(&primary_relative, destination.identity())
                .context("replacing byte-equivalent destination session")?;
        } else {
            bail!(
                "refusing move: destination {} already exists with different content",
                dest_path.display()
            );
        }
    }
    let mut dest = target_lease
        .create_new(&primary_relative, FileMode::Private)
        .with_context(|| format!("creating destination session {}", dest_path.display()))?;
    artifacts.files.push(primary_relative);
    if fail_copy {
        bail!("injected copy failure (primary)");
    }

    let entry = migrated_primary_entry(source, dest_path, new_cwd, primary_name, Some(&mut dest))?;
    artifacts.manifest.push(entry);
    let manifest = manifest_digest(session_id, &artifacts.manifest)?;
    let record = SessionRecord::Custom {
        id: Uuid::now_v7().to_string(),
        parent_id: move_parent_id.map(str::to_owned),
        timestamp: iso_timestamp(),
        custom_type: SESSION_MOVE_CUSTOM_TYPE.to_owned(),
        data: Some(json!({
            "sessionId": session_id,
            "manifest": manifest,
            "movedAt": iso_timestamp(),
            "originalName": primary_name,
        })),
    };
    dest.write_all(serde_json::to_string(&record).context("serializing session_move record")?.as_bytes())
        .context("writing session_move record")?;
    dest.write_all(b"\n").context("writing session_move record")?;
    dest.finish()
        .map_err(anyhow::Error::new)
        .context("fsyncing moved session file")?;
    Ok(())
}

fn copy_sidecars(
    source_lease: &CapabilityRootLease,
    target_lease: &CapabilityRootLease,
    stamps: &[SidecarStamp],
    fail_copy: bool,
    artifacts: &mut MoveArtifacts,
) -> Result<Vec<OsString>> {
    let mut moved = Vec::with_capacity(stamps.len());
    for stamp in stamps {
        let relative = PathBuf::from(&stamp.name);
        let manifest_name = stamp.name.to_string_lossy().into_owned();
        if target_lease
            .entry_exists(&relative)
            .with_context(|| format!("probing destination sidecar {}", relative.display()))?
        {
            let destination = target_lease
                .open_file(&relative)
                .with_context(|| format!("opening destination sidecar {}", relative.display()))?;
            let source_file = source_lease
                .open_file(&relative)
                .with_context(|| format!("reopening source sidecar {}", relative.display()))?;
            let expected = file_digest(&source_file)?;
            let actual = file_digest(&destination)?;
            if actual.digest != expected.digest {
                bail!(
                    "refusing move: sidecar {} already exists with different content",
                    relative.display()
                );
            }
            artifacts
                .manifest
                .push(ManifestEntry { name: manifest_name, kind: "sidecar", len: expected.len, digest: expected.digest });
            moved.push(stamp.name.clone());
            continue;
        }
        let source_file = source_lease
            .open_file(&relative)
            .with_context(|| format!("opening source sidecar {}", relative.display()))?;
        let mut dest = target_lease
            .create_new(&relative, FileMode::Private)
            .with_context(|| format!("creating destination sidecar {}", relative.display()))?;
        artifacts.files.push(relative.clone());
        if fail_copy {
            bail!("injected copy failure (sidecar)");
        }
        let entry = migrated_verbatim_entry(&source_file, &manifest_name, "sidecar", Some(&mut dest))?;
        dest.finish()
            .map_err(anyhow::Error::new)
            .with_context(|| format!("fsyncing moved sidecar {}", relative.display()))?;
        artifacts.manifest.push(entry);
        moved.push(stamp.name.clone());
    }
    Ok(moved)
}

fn copy_children(
    source_lease: &CapabilityRootLease,
    target_lease: &CapabilityRootLease,
    session_id: &str,
    stamps: Option<&ChildrenStamp>,
    dest_primary_path: &Path,
    new_cwd: &Path,
    fail_copy: bool,
    artifacts: &mut MoveArtifacts,
) -> Result<usize> {
    let Some(stamp) = stamps else {
        return Ok(0);
    };
    let children_relative = Path::new("children").join(session_id);
    if !target_lease
        .entry_exists(&children_relative)
        .context("probing destination child subtree")?
    {
        let children_parent = Path::new("children");
        if !target_lease
            .entry_exists(children_parent)
            .context("probing destination children directory")?
        {
            artifacts.children_parent_created = true;
        }
        target_lease
            .create_dir_all(&children_relative, FileMode::Private)
            .with_context(|| format!("creating destination child subtree {}", children_relative.display()))?;
        artifacts.children_dir_created = true;
    }
    // The destination subtree is fsynced leaf-then-parent-then-root even
    // when the leaf directory already existed (a retry into a partial
    // destination): newly written child dirents are only durable once the
    // directory linking them is synced.
    artifacts.children_relative = Some(children_relative.clone());
    let mut copied = 0_usize;
    for file_stamp in &stamp.files {
        let relative = children_relative.join(&file_stamp.name);
        let name = file_stamp
            .name
            .to_str()
            .ok_or_else(|| anyhow!("child entry name is not UTF-8: {}", relative.display()))?;
        let manifest_name = format!("children/{session_id}/{name}");
        if target_lease
            .entry_exists(&relative)
            .with_context(|| format!("probing destination child entry {}", relative.display()))?
        {
            let source_file = source_lease
                .open_file(&relative)
                .with_context(|| format!("opening source child entry {}", relative.display()))?;
            let expected =
                migrated_child_entry(&source_file, name, &manifest_name, dest_primary_path, new_cwd, None)?;
            let destination = target_lease
                .open_file(&relative)
                .with_context(|| format!("opening destination child entry {}", relative.display()))?;
            let actual = file_digest(&destination)?;
            if actual.digest != expected.digest {
                bail!(
                    "refusing move: child entry {} already exists with different content",
                    relative.display()
                );
            }
            artifacts.manifest.push(expected);
            copied += 1;
            continue;
        }
        let source_file = source_lease
            .open_file(&relative)
            .with_context(|| format!("opening source child entry {}", relative.display()))?;
        let mut dest = target_lease
            .create_new(&relative, FileMode::Private)
            .with_context(|| format!("creating destination child entry {}", relative.display()))?;
        artifacts.files.push(relative.clone());
        if fail_copy {
            bail!("injected copy failure (child)");
        }
        let entry = migrated_child_entry(
            &source_file,
            name,
            &manifest_name,
            dest_primary_path,
            new_cwd,
            Some(&mut dest),
        )?;
        dest.finish()
            .map_err(anyhow::Error::new)
            .with_context(|| format!("fsyncing moved child entry {}", relative.display()))?;
        artifacts.manifest.push(entry);
        copied += 1;
    }
    Ok(copied)
}

// ---------------------------------------------------------------------------
// Publish validation
// ---------------------------------------------------------------------------

fn validate_published_destination(
    target_lease: &CapabilityRootLease,
    primary_name: &str,
    session_id: &str,
    new_cwd: &Path,
    manifest: &[ManifestEntry],
) -> Result<()> {
    validate_published_schema(target_lease, primary_name, session_id, new_cwd)?;
    validate_published_manifest(target_lease, session_id, manifest)
}

/// Schema validation: the published primary and every child session load
/// with the preserved identity and rewritten location fields.
fn validate_published_schema(
    target_lease: &CapabilityRootLease,
    primary_name: &str,
    session_id: &str,
    new_cwd: &Path,
) -> Result<()> {
    let dest_path = dest_path_for(target_lease, primary_name);
    let tree = load_session_tree(&dest_path)
        .with_context(|| format!("loading moved session {}", dest_path.display()))?;
    if tree.header.id != session_id {
        bail!(
            "moved session identity mismatch: expected {session_id}, found {}",
            tree.header.id
        );
    }
    if tree.header.cwd != new_cwd {
        bail!(
            "moved session header cwd was not rewritten: {}",
            tree.header.cwd.display()
        );
    }
    let children_relative = Path::new("children").join(session_id);
    if !target_lease
        .entry_exists(&children_relative)
        .context("probing published child subtree")?
    {
        return Ok(());
    }
    let mut names: Vec<OsString> = target_lease
        .open_directory(&children_relative)
        .context("enumerating published child subtree")?
        .read_dir_names()
        .context("reading published child subtree names")?;
    names.sort();
    for name in names {
        let name_str = name
            .to_str()
            .ok_or_else(|| anyhow!("child entry name is not UTF-8"))?;
        let relative = children_relative.join(&name);
        let display = target_lease.display_root().join(&relative);
        if name_str.ends_with(".jsonl") {
            // The verbatim classification mirrors the copy: an opaque JSONL
            // payload relocates byte-exact and carries NO session contract —
            // only a genuine child session header gets the strict identity /
            // cwd / parentSession checks.
            let is_child_session = read_first_line(
                &target_lease
                    .open_file(&relative)
                    .context("opening published child entry")?,
            )
            .ok()
            .and_then(|line| child_session_header_id(&line, &display))
            .is_some();
            if !is_child_session {
                continue;
            }
            let child = load_session_tree(&display)
                .with_context(|| format!("loading moved child session {}", display.display()))?;
            let stem = name_str.strip_suffix(".jsonl").unwrap_or(name_str);
            if child.header.id != stem {
                bail!("moved child session identity mismatch: {}", display.display());
            }
            if child.header.cwd != new_cwd {
                bail!("moved child session cwd was not rewritten: {}", display.display());
            }
            if child.header.parent_session.as_deref()
                != Some(dest_path.to_string_lossy().as_ref())
            {
                bail!(
                    "moved child session parentSession was not rewritten: {}",
                    display.display()
                );
            }
        } else if name_str == ORCHESTRATION_STATE_FILENAME {
            let file = target_lease
                .open_file(&relative)
                .context("opening published orchestration state")?;
            let bytes = file
                .read_bounded(ORCHESTRATION_STATE_LIMIT)
                .context("reading published orchestration state")?;
            let state: Value =
                serde_json::from_slice(&bytes).context("parsing published orchestration state")?;
            if state.get("parentSessionPath").and_then(Value::as_str)
                != Some(dest_path.to_string_lossy().as_ref())
            {
                bail!("published orchestration state parentSessionPath was not rewritten");
            }
        }
    }
    Ok(())
}

/// Manifest validation: every published entry re-read from disk matches its
/// expected digest and length, and the primary carries the matching
/// `session_move` record.
fn validate_published_manifest(
    target_lease: &CapabilityRootLease,
    session_id: &str,
    manifest: &[ManifestEntry],
) -> Result<()> {
    let expected_manifest = manifest_digest(session_id, manifest)?;
    for entry in manifest {
        let relative = PathBuf::from(&entry.name);
        let file = target_lease
            .open_file(&relative)
            .with_context(|| format!("opening published entry {}", entry.name))?;
        if entry.kind == "primary" {
            let tail = trailing_move_record(&file)?.ok_or_else(|| {
                anyhow!("published session {} lacks a session_move record", entry.name)
            })?;
            if tail.session_id != session_id || tail.manifest != expected_manifest {
                bail!(
                    "published session {} carries an unexpected session_move record",
                    entry.name
                );
            }
            let dest_len = file
                .metadata()
                .context("reading published session length")?
                .len;
            let body_len = dest_len.checked_sub(tail.bytes).ok_or_else(|| {
                anyhow!(
                    "published session {} has a truncated session_move record",
                    entry.name
                )
            })?;
            if body_len != entry.len {
                bail!(
                    "published session {} body length does not match the move manifest",
                    entry.name
                );
            }
            if prefix_file_digest(&file, body_len)?.digest != entry.digest {
                bail!(
                    "published session {} body does not match the move manifest",
                    entry.name
                );
            }
        } else {
            let actual = file_digest(&file)?;
            if actual.digest != entry.digest || actual.len != entry.len {
                bail!(
                    "published entry {} does not match the move manifest",
                    entry.name
                );
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Source removal with rollback
// ---------------------------------------------------------------------------

struct PlannedRemoval {
    relative: PathBuf,
    identity: FileIdentity,
    len: u64,
    modified_ns: i128,
    is_dir: bool,
    /// Retained open handle of the original, used to restore exact original
    /// bytes on a mid-removal failure. `None` for directories.
    restore: Option<CapabilityFile>,
    label: String,
}

/// Why a source removal failed. The destination may only be discarded when
/// the originals are proven restored and their directories synced
/// ([`RemovalFailure::Restored`]); otherwise the destination must be
/// retained so both copies are never lost.
enum RemovalFailure {
    Restored(anyhow::Error),
    RecoveryRequired(anyhow::Error),
}

/// Retains open handles for every planned original removal. Failures here
/// happen before anything is removed, so they leave the source intact.
fn plan_source_removals(
    source_lease: &CapabilityRootLease,
    source_relative: &Path,
    primary_identity: FileIdentity,
    primary_len: u64,
    primary_modified_ns: i128,
    stamps: &SourceStamps,
    session_id: &str,
) -> Result<Vec<PlannedRemoval>> {
    let mut plans: Vec<PlannedRemoval> = Vec::new();
    for stamp in &stamps.sidecars {
        let relative = PathBuf::from(&stamp.name);
        let restore = source_lease
            .open_file(&relative)
            .with_context(|| format!("retaining sidecar {} for removal", relative.display()))?;
        plans.push(PlannedRemoval {
            relative,
            identity: stamp.identity,
            len: stamp.len,
            modified_ns: stamp.modified_ns,
            is_dir: false,
            restore: Some(restore),
            label: format!("sidecar {}", stamp.name.to_string_lossy()),
        });
    }
    if let Some(children) = &stamps.children {
        let children_relative = Path::new("children").join(session_id);
        for file in &children.files {
            let relative = children_relative.join(&file.name);
            let restore = source_lease
                .open_file(&relative)
                .with_context(|| format!("retaining child entry {} for removal", relative.display()))?;
            let metadata = restore
                .metadata()
                .with_context(|| format!("stamping child entry {}", relative.display()))?;
            plans.push(PlannedRemoval {
                relative,
                identity: file.identity,
                len: file.len,
                modified_ns: file.modified_ns,
                is_dir: false,
                restore: Some(restore),
                label: format!("child entry {}", file.name.to_string_lossy()),
            });
        }
        plans.push(PlannedRemoval {
            relative: children_relative,
            identity: children.dir_identity,
            len: 0,
            modified_ns: 0,
            is_dir: true,
            restore: None,
            label: "child subtree".to_owned(),
        });
    }
    let restore = source_lease
        .open_file(source_relative)
        .with_context(|| format!("retaining primary session {} for removal", source_relative.display()))?;
    plans.push(PlannedRemoval {
        relative: source_relative.to_path_buf(),
        identity: primary_identity,
        len: primary_len,
        modified_ns: primary_modified_ns,
        is_dir: false,
        restore: Some(restore),
        label: "primary session".to_owned(),
    });
    Ok(plans)
}

#[allow(clippy::too_many_arguments)]
fn remove_source(
    source_lease: &CapabilityRootLease,
    source_relative: &Path,
    primary_identity: FileIdentity,
    primary_len: u64,
    primary_modified_ns: i128,
    stamps: &SourceStamps,
    session_id: &str,
    faults: MoveFaults,
    log: &Option<SyncLog>,
) -> Result<(), RemovalFailure> {
    let plans = plan_source_removals(
        source_lease,
        source_relative,
        primary_identity,
        primary_len,
        primary_modified_ns,
        stamps,
        session_id,
    )
    // Nothing has been removed yet: the source is intact, so the
    // destination may still be discarded on this failure.
    .map_err(RemovalFailure::Restored)?;

    let mut removed: Vec<PlannedRemoval> = Vec::with_capacity(plans.len());
    let mut failure: Option<anyhow::Error> = None;
    for (index, plan) in plans.into_iter().enumerate() {
        if faults.fail_remove_at == Some(index) {
            failure = Some(anyhow!("injected removal failure at {}", plan.label));
            break;
        }
        let revalidated = (|| -> Result<()> {
            if plan.is_dir {
                let current = source_lease.metadata(&plan.relative)?;
                if current.identity != plan.identity {
                    bail!("child subtree identity changed before removal");
                }
            } else {
                let handle = plan.restore.as_ref().expect("file removals retain handles");
                let current = handle.metadata().context("revalidating source before removal")?;
                if current.identity != plan.identity
                    || current.len != plan.len
                    || current.modified_ns != plan.modified_ns
                {
                    bail!("source {} changed before removal", plan.relative.display());
                }
            }
            Ok(())
        })();
        if let Err(error) = revalidated {
            failure = Some(error.context(format!("revalidating {}", plan.label)));
            break;
        }
        let result = if plan.is_dir {
            source_lease.remove_tree_expected(&plan.relative, plan.identity)
        } else {
            source_lease.remove_file_expected(&plan.relative, plan.identity)
        };
        match result {
            Ok(()) => removed.push(plan),
            Err(error) => {
                failure = Some(
                    anyhow::Error::new(error).context(format!("removing {}", plan.label)),
                );
                break;
            }
        }
    }
    let failure = match failure {
        Some(error) => Some(error),
        None => match commit_source_removal_sync(
            source_lease,
            stamps.children.is_some(),
            faults,
            log,
        ) {
            Ok(()) => return Ok(()),
            Err(error) => Some(error),
        },
    };
    // Mid-removal or terminal-fsync failure: restore every removed original
    // from its retained handle (exact original bytes), best-effort.
    let error = failure.expect("removal failure is set");
    let rollback = restore_removed(source_lease, &removed, faults, log);
    match rollback {
        Ok(()) => Err(RemovalFailure::Restored(error)),
        Err(rollback) => Err(RemovalFailure::RecoveryRequired(error.context(format!(
            "restoring removed source originals failed; retaining the migrated destination for recovery: {rollback:#}"
        )))),
    }
}

/// Terminal durability of the source removals: the containing `children`
/// directory (whose entry linked the removed subtree) and then the
/// session-store root are synced before the removal is considered committed.
fn commit_source_removal_sync(
    source_lease: &CapabilityRootLease,
    had_children: bool,
    faults: MoveFaults,
    log: &Option<SyncLog>,
) -> Result<()> {
    if faults.fail_source_final_fsync {
        bail!("injected source-directory fsync failure");
    }
    if had_children {
        sync_logged(source_lease, Path::new("children"), log, "source:children-parent")
            .context("syncing source children directory after move")?;
    }
    sync_logged(source_lease, Path::new(""), log, "source:root")
        .context("syncing source session directory after move")?;
    Ok(())
}

fn restore_removed(
    source_lease: &CapabilityRootLease,
    removed: &[PlannedRemoval],
    faults: MoveFaults,
    log: &Option<SyncLog>,
) -> Result<()> {
    let mut restored_subtree: Option<PathBuf> = None;
    for plan in removed.iter().rev() {
        if plan.is_dir {
            source_lease
                .create_dir_all(&plan.relative, FileMode::Private)
                .with_context(|| format!("recreating {}", plan.relative.display()))?;
            restored_subtree = Some(plan.relative.clone());
            continue;
        }
        if faults.fail_restore {
            bail!("injected restore failure for {}", plan.label);
        }
        let handle = plan.restore.as_ref().expect("file removals retain handles");
        let mut dest = source_lease
            .create_new(&plan.relative, FileMode::Private)
            .with_context(|| format!("recreating {}", plan.relative.display()))?;
        stream_copy_from_file(handle, &mut dest)?;
        dest.finish()
            .map_err(anyhow::Error::new)
            .with_context(|| format!("fsyncing restored {}", plan.relative.display()))?;
    }
    // Bottom-up durability for the restored hierarchy: the leaf subtree's
    // entries, then the intermediate `children` entry, then the root.
    if let Some(leaf) = restored_subtree {
        sync_logged(source_lease, &leaf, log, "source:children-leaf")
            .context("syncing restored child subtree")?;
        if let Some(parent) = leaf.parent() {
            sync_logged(source_lease, parent, log, "source:children-parent")
                .context("syncing restored children directory")?;
        }
    }
    sync_logged(source_lease, Path::new(""), log, "source:root")
        .context("syncing restored source directory")
}

fn stream_copy_from_file(source: &CapabilityFile, dest: &mut crate::fs::CapabilityWritableFile) -> Result<()> {
    let mut reader = source
        .try_clone_std()
        .context("cloning session file for copy")?;
    reader
        .seek(SeekFrom::Start(0))
        .context("seeking session file for copy")?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; STREAM_COPY_BUFFER_BYTES];
    loop {
        let count = reader.read(&mut buffer).context("streaming session copy")?;
        if count == 0 {
            return Ok(());
        }
        dest.write_all(&buffer[..count])
            .context("streaming session copy")?;
        hasher.update(&buffer[..count]);
    }
}

// ---------------------------------------------------------------------------
// Durable rebinds (best-effort, never abort the move)
// ---------------------------------------------------------------------------

fn rebind_durable_state(
    capabilities: &Arc<CapabilityFs>,
    agent_dir: &Path,
    old_cwd: &Path,
    new_cwd: &Path,
    session_id: &str,
    faults: MoveFaults,
    memory_calls: &Option<MemoryCallLog>,
    scrub_roots: &[(PathBuf, &str)],
) -> (SessionMoveRebinds, Vec<String>) {
    let mut rebound = SessionMoveRebinds::default();
    let mut warnings = Vec::new();
    // Phase one (additive): copy the old namespace into the new one. The old
    // archive stays on disk as recovery metadata until phase two commits.
    log_memory_call(memory_calls, "prepare");
    let migrated = match crate::memory::migrate_repo_namespace_with_capabilities(
        capabilities,
        agent_dir,
        old_cwd,
        new_cwd,
    ) {
        Ok(migrated) => migrated,
        Err(error) => {
            warnings.push(scrub_text(
                format!("memory namespace rebind skipped: {error:#}"),
                scrub_roots,
            ));
            false
        }
    };
    // Phase two: drop the old archive only after the file move committed. A
    // failed commit keeps the copied entries as recovery state, never rolls
    // back the committed file move, and is finalized idempotently by a
    // retry; the memory rebind is not claimed while the old archive is still
    // authoritative.
    log_memory_call(memory_calls, "commit");
    let commit = if faults.fail_memory_commit {
        Err(anyhow!("injected memory namespace commit failure"))
    } else {
        crate::memory::commit_repo_namespace_migration_with_capabilities(
            capabilities,
            agent_dir,
            old_cwd,
            new_cwd,
        )
    };
    match commit {
        // The rebind is complete when the additive copy ran or the commit
        // removed the old archive; a commit-only retry (nothing left to
        // copy) must still claim the finalized rebind.
        Ok(removed) => rebound.memory = migrated || removed,
        Err(error) => {
            warnings.push(scrub_text(
                format!(
                    "memory namespace commit incomplete; the old archive is retained for recovery and a retry finalizes it: {error:#}"
                ),
                scrub_roots,
            ));
            rebound.memory = false;
        }
    }
    match crate::workflow_worktree::rebind_session_worktree_roots(agent_dir, old_cwd, new_cwd, session_id)
    {
        Ok(relocated) => rebound.workflows = relocated,
        Err(error) => warnings.push(scrub_text(
            format!("workflow worktree roots rebind skipped: {error}"),
            scrub_roots,
        )),
    }
    match rebind_trust_entry(agent_dir, old_cwd, new_cwd) {
        Ok(rebound_trust) => rebound.trust = rebound_trust,
        Err(error) => warnings.push(scrub_text(
            format!("trust entry rebind skipped: {error:#}"),
            scrub_roots,
        )),
    }
    (rebound, warnings)
}

fn log_memory_call(log: &Option<MemoryCallLog>, call: &str) {
    if let Some(log) = log {
        log.borrow_mut().push(call.to_owned());
    }
}

/// Best-effort trust rebind: copy the stored exact decision for the old
/// project boundary to the new boundary. Missing decisions are a no-op.
fn rebind_trust_entry(agent_dir: &Path, old_cwd: &Path, new_cwd: &Path) -> Result<bool> {
    let store = TrustStore::new(agent_dir);
    let old_boundary = crate::resources::project_context_boundary(old_cwd)?;
    let new_boundary = crate::resources::project_context_boundary(new_cwd)?;
    if old_boundary == new_boundary {
        return Ok(false);
    }
    match store.resolve_exact(&old_boundary)? {
        TrustDecision::Ask => Ok(false),
        decision => {
            store.set_exact(&new_boundary, decision)?;
            Ok(true)
        }
    }
}

fn iso_timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
    }
}

/// Replaces local absolute roots in a diagnostic string with stable labels
/// so surfaced errors and best-effort rebind warnings never disclose the
/// workstation path layout. Roots are listed most-specific first: a root
/// nested under another (e.g. the session-store root under the agent dir)
/// must be replaced before its ancestor.
fn scrub_text(text: String, replacements: &[(PathBuf, &str)]) -> String {
    let mut text = text;
    for (root, label) in replacements {
        if let Some(root) = root.to_str() && !root.is_empty() {
            text = text.replace(root, label);
        }
    }
    text
}

fn scrub_error(error: anyhow::Error, replacements: &[(PathBuf, &str)]) -> anyhow::Error {
    anyhow::Error::msg(scrub_text(format!("{error:#}"), replacements))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::GOAL_SESSION_CUSTOM_TYPE;
    use crate::memory::MemoryStore;
    use crate::session_store::start_session_in;
    use crate::TodoState;
    use pi_ai::Message;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    struct Fixture {
        old_cwd: PathBuf,
        new_cwd: PathBuf,
        source_dir: PathBuf,
        target_dir: PathBuf,
        agent_dir: PathBuf,
        source_path: PathBuf,
        primary_name: String,
        original_bytes: Vec<u8>,
        _dirs: Vec<tempfile::TempDir>,
    }

    fn create_fixture(session_id: &str) -> Fixture {
        let old_cwd = tempdir().expect("old cwd");
        let new_cwd = tempdir().expect("new cwd");
        let source_dir = tempdir().expect("source dir");
        let target_dir = tempdir().expect("target dir");
        let agent_dir = tempdir().expect("agent dir");
        let recorder = start_session_in(
            old_cwd.path(),
            None,
            None,
            Some(source_dir.path()),
            Some(session_id),
            None,
        )
        .expect("source session");
        recorder
            .record_message(&Message::user_text("hello from the move fixture", 0))
            .expect("message");
        let mut assistant = pi_ai::AssistantMessage::pending(&pi_ai::Model::default());
        assistant.content = vec![pi_ai::ContentBlock::text("moved fixture reply")];
        recorder
            .record_message(&Message::Assistant(assistant))
            .expect("assistant message");
        recorder
            .record_model_change("test-provider", "test-model")
            .expect("model change");
        recorder
            .record_thinking_level("high")
            .expect("thinking level");
        recorder
            .record_todo_snapshot(&TodoState::default())
            .expect("todo snapshot");
        recorder
            .record_custom_entry(GOAL_SESSION_CUSTOM_TYPE, Some(json!({"objective": "ship the move"})))
            .expect("goal event");
        recorder
            .record_session_name("moved session name")
            .expect("session name");
        recorder.persist_now().expect("persist");
        let source_path = recorder.path().to_path_buf();
        drop(recorder);

        let primary_name = source_path
            .file_name()
            .and_then(OsStr::to_str)
            .expect("primary name")
            .to_owned();
        fs::write(
            source_dir.path().join(format!("{primary_name}.loops.json")),
            "{\"loops\":[]}",
        )
        .unwrap();
        fs::write(
            source_dir
                .path()
                .join(format!("{primary_name}.rewind-20260101T000000-000.jsonl")),
            "rewound tail record\n",
        )
        .unwrap();
        fs::write(
            source_dir
                .path()
                .join(format!("{primary_name}.snapcompact-1.jsonl")),
            "compacted tail\n",
        )
        .unwrap();
        // Unrecognized sidecar: must never be relocated by a move.
        fs::write(
            source_dir
                .path()
                .join(format!("{primary_name}.unrecognized.json")),
            "stays behind\n",
        )
        .unwrap();

        let children_dir = source_dir.path().join("children").join(session_id);
        fs::create_dir_all(&children_dir).unwrap();
        fs::write(
            children_dir.join("childone.jsonl"),
            format!(
                "{}\n{}\n",
                json!({
                    "type": "session", "version": 3, "id": "childone",
                    "timestamp": "2026-01-01T00:00:00.000Z",
                    "cwd": old_cwd.path(), "parentSession": source_path,
                }),
                json!({
                    "type": "custom", "id": "c1", "parentId": null,
                    "timestamp": "2026-01-01T00:00:01.000Z",
                    "customType": "note", "data": {"note": "child"},
                }),
            ),
        )
        .unwrap();
        fs::write(
            children_dir.join(ORCHESTRATION_STATE_FILENAME),
            json!({
                "version": 3,
                "parentSessionId": session_id,
                "parentSessionPath": source_path,
                "groupId": "group-one",
                "durablePolicy": {"mode": "durable"},
                "agents": [],
                "jobs": [],
            })
            .to_string(),
        )
        .unwrap();
        fs::write(children_dir.join("todo-manifest.json"), "{\"version\":1}\n").unwrap();

        let original_bytes = fs::read(&source_path).unwrap();
        Fixture {
            old_cwd: old_cwd.path().to_path_buf(),
            new_cwd: new_cwd.path().to_path_buf(),
            source_dir: source_dir.path().to_path_buf(),
            target_dir: target_dir.path().to_path_buf(),
            agent_dir: agent_dir.path().to_path_buf(),
            source_path,
            primary_name,
            original_bytes,
            _dirs: vec![old_cwd, new_cwd, source_dir, target_dir, agent_dir],
        }
    }

    fn move_options<'a>(
        fixture: &'a Fixture,
        faults: MoveFaults,
        sync_log: Option<SyncLog>,
        memory_calls: Option<MemoryCallLog>,
    ) -> MoveOptions<'a> {
        MoveOptions {
            session_dir: Some(&fixture.target_dir),
            agent_dir: Some(&fixture.agent_dir),
            faults,
            sync_log,
            memory_calls,
        }
    }

    fn run_move(
        fixture: &Fixture,
        faults: MoveFaults,
    ) -> Result<SessionMoveOutcome> {
        move_saved_session_with(&fixture.source_path, &fixture.new_cwd, move_options(fixture, faults, None, None))
    }

    /// Recursively copies a small test tree.
    fn copy_tree(src: &Path, dst: &Path) {
        fs::create_dir_all(dst).unwrap();
        for entry in fs::read_dir(src).unwrap() {
            let entry = entry.unwrap();
            let to = dst.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &to);
            } else {
                fs::copy(entry.path(), to).unwrap();
            }
        }
    }

    fn sidecar_paths(fixture: &Fixture) -> Vec<PathBuf> {
        [
            format!("{}.loops.json", fixture.primary_name),
            format!("{}.rewind-20260101T000000-000.jsonl", fixture.primary_name),
            format!("{}.snapcompact-1.jsonl", fixture.primary_name),
        ]
        .into_iter()
        .map(|name| fixture.source_dir.join(name))
        .collect()
    }

    fn children_root(fixture: &Fixture, session_id: &str) -> PathBuf {
        fixture.source_dir.join("children").join(session_id)
    }

    fn assert_source_intact(fixture: &Fixture, session_id: &str) {
        assert_eq!(
            fs::read(&fixture.source_path).unwrap(),
            fixture.original_bytes,
            "source primary must be untouched"
        );
        for path in sidecar_paths(fixture) {
            assert!(path.exists(), "sidecar {} must be untouched", path.display());
        }
        assert!(
            children_root(fixture, session_id).join("childone.jsonl").exists(),
            "source child subtree must be untouched"
        );
    }

    /// Inner failure causes live in the full `{error:#}` chain while the
    /// surfaced outer message stays bounded, so cause assertions must
    /// inspect the rendered chain.
    fn rendered(error: &anyhow::Error) -> String {
        format!("{error:#}")
    }

    fn last_record_line(path: &Path) -> String {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .last()
            .expect("trailing record line")
            .to_owned()
    }

    #[test]
    fn move_preserves_identity_and_migrates_subtree() {
        let fixture = create_fixture("movesession");
        // Build/prepare the moved session context: the pre-move active leaf
        // the durable marker must stay attached to across the move.
        let pre_move_leaf = PreparedSessionResume::prepare_path(&fixture.source_path)
            .expect("pre-move session loads")
            .tree()
            .active_leaf_id
            .clone()
            .expect("pre-move active leaf");
        let outcome = run_move(&fixture, MoveFaults::default()).expect("move succeeds");

        assert_eq!(outcome.session_id, "movesession");
        assert_eq!(outcome.from, fixture.source_path);
        assert_eq!(outcome.to, fixture.target_dir.join(&fixture.primary_name));
        assert_eq!(outcome.sidecars, 3);
        assert_eq!(outcome.children, 3);
        assert!(outcome.warnings.is_empty(), "warnings: {:?}", outcome.warnings);
        assert_eq!(outcome.rebound, SessionMoveRebinds::default());

        // The old authority is fully removed; unrecognized sidecars stay.
        assert!(!fixture.source_path.exists());
        for path in sidecar_paths(&fixture) {
            assert!(!path.exists(), "{} must be gone", path.display());
        }
        assert!(!children_root(&fixture, "movesession").exists());
        assert!(
            fixture
                .source_dir
                .join(format!("{}.unrecognized.json", fixture.primary_name))
                .exists(),
            "unrecognized sidecar must not be relocated"
        );

        // Destination loads with preserved identity, name, and records.
        let tree = load_session_tree(&outcome.to).expect("moved session loads");
        assert_eq!(tree.header.id, "movesession");
        assert_eq!(tree.header.cwd.as_path(), fixture.new_cwd.as_path());
        assert_eq!(tree.session_name().as_deref(), Some("moved session name"));
        let entry_types: Vec<&str> = tree
            .entries
            .iter()
            .map(|entry| entry.entry_type.as_str())
            .collect();
        assert!(entry_types.contains(&"message"), "entries: {entry_types:?}");
        assert!(entry_types.contains(&"model_change"));
        assert!(entry_types.contains(&"thinking_level_change"));
        assert!(entry_types.contains(&"todo_snapshot"));
        assert!(
            tree.entries
                .iter()
                .any(|entry| entry.custom_type.as_deref() == Some(GOAL_SESSION_CUSTOM_TYPE)),
            "goal custom entry must survive the move"
        );

        // The trailing record is the bounded, path-free session_move record.
        let last = tree.entries.last().expect("records survive");
        assert_eq!(last.entry_type, "custom");
        assert_eq!(last.custom_type.as_deref(), Some(SESSION_MOVE_CUSTOM_TYPE));
        let data = last.data.as_ref().expect("session_move data");
        assert_eq!(data["sessionId"], Value::String("movesession".to_owned()));
        assert!(data.get("from").is_none(), "record must not carry absolute paths");
        assert!(data.get("to").is_none(), "record must not carry absolute paths");
        let manifest = data["manifest"].as_str().expect("manifest digest");
        assert_eq!(manifest.len(), 64);
        assert!(
            manifest.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "manifest digest must be opaque hex"
        );
        assert_eq!(data["originalName"], Value::String(fixture.primary_name.clone()));
        assert!(data.get("movedAt").and_then(Value::as_str).is_some());
        // The marker is a child of the pre-move active leaf, so the complete
        // prior branch stays active across the move.
        assert_eq!(
            last.parent_id.as_deref(),
            Some(pre_move_leaf.as_str()),
            "move marker must connect to the pre-move active leaf"
        );
        let branch = tree.branch(None);
        assert_eq!(
            branch.last().map(|entry| entry.id.as_str()),
            Some(last.id.as_str()),
            "the reopened active leaf is the move marker"
        );
        assert!(
            branch.iter().any(|entry| entry.id == pre_move_leaf),
            "pre-move active leaf must remain in the active branch"
        );
        assert!(
            branch.iter().any(|entry| matches!(
                &entry.message,
                Some(Message::User(message))
                    if message.content.iter().any(|block| matches!(
                        block,
                        pi_ai::ContentBlock::Text { text, .. }
                            if text == "hello from the move fixture"
                    ))
            )),
            "pre-move user message must remain in the active branch"
        );
        assert!(
            branch.iter().any(|entry| matches!(
                &entry.message,
                Some(Message::Assistant(message)) if message.text() == "moved fixture reply"
            )),
            "pre-move assistant message must remain in the active branch"
        );

        // No local path material anywhere in the durable record line.
        let record_line = last_record_line(&outcome.to);
        let source_str = fixture.source_path.to_string_lossy().into_owned();
        let source_dir_str = fixture.source_dir.to_string_lossy().into_owned();
        let old_cwd_str = fixture.old_cwd.to_string_lossy().into_owned();
        for leak in [source_str.as_str(), source_dir_str.as_str(), old_cwd_str.as_str()] {
            assert!(
                !record_line.contains(leak),
                "record leaks absolute path material: {record_line}"
            );
        }

        // Restart load through the retained-resume path restores the new cwd.
        let prepared = PreparedSessionResume::prepare_path(&outcome.to).expect("restart load");
        assert_eq!(prepared.target_cwd(), fixture.new_cwd.as_path());
        // The reopened active branch carries the complete pre-move transcript.
        let context = prepared.build_context();
        assert_eq!(context.messages.len(), 2, "transcript: {:?}", context.messages);
        assert!(
            matches!(&context.messages[0], Message::User(_)),
            "the user message must lead the reopened branch"
        );
        assert!(
            matches!(&context.messages[1], Message::Assistant(_)),
            "the assistant reply must follow in the reopened branch"
        );

        // Sidecars are byte-identical copies.
        assert_eq!(
            fs::read(fixture.target_dir.join(format!("{}.loops.json", fixture.primary_name)))
                .unwrap(),
            b"{\"loops\":[]}".as_slice()
        );
        assert_eq!(
            fs::read(
                fixture
                    .target_dir
                    .join(format!("{}.rewind-20260101T000000-000.jsonl", fixture.primary_name))
            )
            .unwrap(),
            b"rewound tail record\n".as_slice()
        );
        assert_eq!(
            fs::read(
                fixture
                    .target_dir
                    .join(format!("{}.snapcompact-1.jsonl", fixture.primary_name))
            )
            .unwrap(),
            b"compacted tail\n".as_slice()
        );

        // Child subtree: headers and orchestration sidecar rewritten, manifest verbatim.
        let children_root = fixture.target_dir.join("children").join("movesession");
        let child_tree = load_session_tree(children_root.join("childone.jsonl"))
            .expect("moved child session loads");
        assert_eq!(child_tree.header.id, "childone");
        assert_eq!(child_tree.header.cwd.as_path(), fixture.new_cwd.as_path());
        assert_eq!(
            child_tree.header.parent_session.as_deref(),
            Some(outcome.to.to_string_lossy().as_ref())
        );
        let state: Value = serde_json::from_slice(
            &fs::read(children_root.join(ORCHESTRATION_STATE_FILENAME)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            state["parentSessionPath"],
            Value::String(outcome.to.to_string_lossy().into_owned())
        );
        assert_eq!(state["parentSessionId"], Value::String("movesession".to_owned()));
        assert_eq!(
            fs::read(children_root.join("todo-manifest.json")).unwrap(),
            b"{\"version\":1}\n".as_slice()
        );
    }

    /// An empty (header-only) moved session stays valid: the durable move
    /// marker becomes the root child and the file reloads.
    #[test]
    fn move_empty_session_appends_root_child_marker() {
        let old_cwd = tempdir().expect("old cwd");
        let new_cwd = tempdir().expect("new cwd");
        let source_dir = tempdir().expect("source dir");
        let target_dir = tempdir().expect("target dir");
        let agent_dir = tempdir().expect("agent dir");
        let recorder = start_session_in(
            old_cwd.path(),
            None,
            None,
            Some(source_dir.path()),
            Some("moveempty"),
            None,
        )
        .expect("source session");
        recorder.persist_now().expect("persist");
        let source_path = recorder.path().to_path_buf();
        drop(recorder);

        let outcome = move_saved_session_with(
            &source_path,
            new_cwd.path(),
            MoveOptions {
                session_dir: Some(target_dir.path()),
                agent_dir: Some(agent_dir.path()),
                ..MoveOptions::default()
            },
        )
        .expect("move succeeds");

        let tree = load_session_tree(&outcome.to).expect("moved empty session loads");
        assert_eq!(tree.header.id, "moveempty");
        assert_eq!(tree.header.cwd.as_path(), new_cwd.path());
        let marker = tree.entries.last().expect("move marker");
        assert_eq!(marker.entry_type, "custom");
        assert_eq!(marker.custom_type.as_deref(), Some(SESSION_MOVE_CUSTOM_TYPE));
        assert_eq!(
            marker.parent_id, None,
            "an empty session's marker stays a root child"
        );
        assert_eq!(tree.entries.len(), 1);
    }

    #[test]
    fn rollback_restores_source_when_primary_copy_fails() {
        let fixture = create_fixture("movecopyprimary");
        let error = run_move(
            &fixture,
            MoveFaults {
                fail_copy: Some(CopyStage::Primary),
                ..MoveFaults::default()
            },
        )
        .expect_err("injected copy failure");
        assert!(rendered(&error).contains("injected copy failure"), "{error:#}");
        assert_source_intact(&fixture, "movecopyprimary");
        assert!(
            fixture.target_dir.read_dir().unwrap().next().is_none(),
            "destination must be cleaned"
        );
    }

    #[test]
    fn rollback_restores_source_when_sidecar_copy_fails() {
        let fixture = create_fixture("movecopysidecar");
        let error = run_move(
            &fixture,
            MoveFaults {
                fail_copy: Some(CopyStage::Sidecar),
                ..MoveFaults::default()
            },
        )
        .expect_err("injected copy failure");
        assert!(rendered(&error).contains("injected copy failure"), "{error:#}");
        assert_source_intact(&fixture, "movecopysidecar");
        assert!(
            fixture.target_dir.read_dir().unwrap().next().is_none(),
            "destination must be cleaned"
        );
    }

    #[test]
    fn rollback_cleans_destination_when_publish_fails() {
        let fixture = create_fixture("movepublish");
        let error = run_move(
            &fixture,
            MoveFaults {
                fail_publish: true,
                ..MoveFaults::default()
            },
        )
        .expect_err("injected publish failure");
        assert!(rendered(&error).contains("publish validation failed"), "{error:#}");
        assert_source_intact(&fixture, "movepublish");
        assert!(
            fixture.target_dir.read_dir().unwrap().next().is_none(),
            "destination must be cleaned"
        );
    }

    #[test]
    fn rollback_restores_removed_originals_when_removal_fails() {
        let fixture = create_fixture("moveremove");
        let error = run_move(
            &fixture,
            MoveFaults {
                fail_remove_at: Some(1),
                ..MoveFaults::default()
            },
        )
        .expect_err("injected removal failure");
        assert!(rendered(&error).contains("injected removal failure"), "{error:#}");
        assert_source_intact(&fixture, "moveremove");
        assert!(
            fixture.target_dir.read_dir().unwrap().next().is_none(),
            "destination must be cleaned after removal rollback"
        );
    }

    /// P0: a terminal source-directory fsync failure after every original
    /// was removed must route through restoration; the destination is only
    /// discarded once the source is proven restored.
    #[test]
    fn source_final_fsync_failure_restores_source_and_cleans_destination() {
        let fixture = create_fixture("movefinalfsync");
        let error = run_move(
            &fixture,
            MoveFaults {
                fail_source_final_fsync: true,
                ..MoveFaults::default()
            },
        )
        .expect_err("injected source fsync failure");
        assert!(
            rendered(&error).contains("injected source-directory fsync failure"),
            "{error:#}"
        );
        assert_source_intact(&fixture, "movefinalfsync");
        assert!(
            fixture.target_dir.read_dir().unwrap().next().is_none(),
            "destination must be cleaned after proven restoration"
        );
    }

    /// P0: when the source cannot be restored, the migrated destination is
    /// retained so both copies are never lost, and a recovery-required error
    /// is surfaced.
    #[test]
    fn unrestorable_source_retains_destination_for_recovery() {
        let fixture = create_fixture("movenostroration");
        let error = run_move(
            &fixture,
            MoveFaults {
                fail_source_final_fsync: true,
                fail_restore: true,
                ..MoveFaults::default()
            },
        )
        .expect_err("restoration failure");
        let message = error.to_string();
        assert!(
            message.contains("retaining the migrated destination for recovery"),
            "{error:#}"
        );
        // The destination survives: exactly one durable copy remains.
        assert!(
            load_session_tree(&fixture.target_dir.join(&fixture.primary_name)).is_ok(),
            "migrated destination must be retained for recovery"
        );
        assert!(
            fixture
                .target_dir
                .join("children")
                .join("movenostroration")
                .join("childone.jsonl")
                .exists(),
            "migrated child subtree must be retained for recovery"
        );
        // The source was removed and its restoration was faulted: the move
        // retains the destination rather than deleting both copies.
        assert!(
            !fixture.source_path.exists(),
            "faulted restoration leaves the source removed"
        );
    }

    /// Crash-before-removal retry: an untouched source tree plus a
    /// destination already carrying the matching manifest is an idempotent
    /// success that touches nothing.
    #[test]
    fn move_is_idempotent_for_identical_target_record() {
        let fixture = create_fixture("moveidem");
        let backup = tempdir().expect("backup");
        copy_tree(&fixture.source_dir, backup.path());
        let first = run_move(&fixture, MoveFaults::default()).expect("first move");
        let destination_bytes = fs::read(&first.to).unwrap();

        // Simulate a crash before removal: the complete original source tree
        // is back while the destination already carries the committed move.
        copy_tree(backup.path(), &fixture.source_dir);

        let second = run_move(&fixture, MoveFaults::default()).expect("idempotent rerun");
        assert_eq!(second.to, first.to);
        assert!(
            fixture.source_path.exists(),
            "idempotent rerun must touch nothing"
        );
        assert_source_intact(&fixture, "moveidem");
        assert_eq!(
            fs::read(&first.to).unwrap(),
            destination_bytes,
            "destination must be byte-stable across the idempotent rerun"
        );
        assert!(second.warnings.is_empty());
    }

    #[test]
    fn move_refuses_collision_with_different_destination_content() {
        let fixture = create_fixture("movecollide");
        fs::write(
            fixture.target_dir.join(&fixture.primary_name),
            format!(
                "{}\n",
                json!({
                    "type": "session", "version": 3, "id": "othersession",
                    "timestamp": "2026-01-01T00:00:00.000Z", "cwd": fixture.new_cwd,
                }),
            ),
        )
        .unwrap();
        let error = run_move(&fixture, MoveFaults::default()).expect_err("collision refused");
        assert!(
            rendered(&error).contains("different session"),
            "{error:#}"
        );
        assert_source_intact(&fixture, "movecollide");
    }

    /// P1: a trailing session_move record with matching-looking fields but a
    /// manifest that does not match this move's content is rejected and the
    /// genuine source is never removed. Fail closed: the forged file is not
    /// ours to delete.
    #[test]
    fn forged_move_record_marker_is_rejected() {
        let fixture = create_fixture("moveforged");
        let forged = format!(
            "{}\n{}\n",
            json!({
                "type": "session", "version": 3, "id": "moveforged",
                "timestamp": "2026-01-01T00:00:00.000Z", "cwd": fixture.new_cwd,
            }),
            json!({
                "type": "custom", "id": "f1", "parentId": null,
                "timestamp": "2026-01-01T00:00:01.000Z",
                "customType": SESSION_MOVE_CUSTOM_TYPE,
                "data": {
                    "sessionId": "moveforged",
                    "manifest": "0".repeat(64),
                    "movedAt": "2026-01-01T00:00:02.000Z",
                    "originalName": fixture.primary_name,
                },
            }),
        );
        let forged_path = fixture.target_dir.join(&fixture.primary_name);
        fs::write(&forged_path, forged).unwrap();

        let error = run_move(&fixture, MoveFaults::default()).expect_err("forged marker refused");
        assert!(
            rendered(&error).contains("forged"),
            "{error:#}"
        );
        assert_source_intact(&fixture, "moveforged");
        assert!(
            forged_path.exists(),
            "fail closed: the forged destination is left untouched"
        );
        // Surfaced errors never disclose local path layout — checked across
        // the full redacted chain, not just the bounded outer message.
        let chain = rendered(&error);
        for leak in [
            fixture.target_dir.to_str().unwrap(),
            fixture.source_dir.to_str().unwrap(),
            fixture.old_cwd.to_str().unwrap(),
            fixture.agent_dir.to_str().unwrap(),
        ] {
            assert!(!chain.contains(leak), "error chain leaks {leak}: {chain}");
        }
    }

    /// A destination whose migrated body and marker identity match this
    /// move's manifest but whose marker is detached from the source's prior
    /// active branch (e.g. `parentId: null` from an older migration) must be
    /// refused, never silently accepted as already committed.
    #[test]
    fn disconnected_marker_ancestry_is_rejected() {
        let fixture = create_fixture("moveancestry");
        let backup = tempdir().expect("backup");
        copy_tree(&fixture.source_dir, backup.path());

        // Commit a genuine move, then detach its marker from the active
        // branch while leaving the body and marker identity intact.
        let first = run_move(&fixture, MoveFaults::default()).expect("first move");
        let detached = {
            let content = fs::read_to_string(&first.to).unwrap();
            let mut lines: Vec<String> = content.lines().map(str::to_owned).collect();
            let last = lines.last_mut().expect("marker line");
            let mut record: Value = serde_json::from_str(last).unwrap();
            record["parentId"] = Value::Null;
            *last = record.to_string();
            lines.join("\n") + "\n"
        };
        fs::write(&first.to, detached).unwrap();

        // Crash-before-removal retry: the untouched source tree is back
        // while the destination carries a body-faithful but disconnected
        // marker.
        copy_tree(backup.path(), &fixture.source_dir);
        let error = run_move(&fixture, MoveFaults::default())
            .expect_err("detached marker must be refused");
        assert!(
            rendered(&error).contains("does not connect to the source active branch"),
            "{error:#}"
        );
        assert_source_intact(&fixture, "moveancestry");
    }

    /// P1: an existing destination child entry is accepted only on exact
    /// content equivalence, never on shallow header/path fields.
    #[test]
    fn forged_child_entries_fail_closed() {
        let fixture = create_fixture("moveforgedchild");
        let dest_children = fixture.target_dir.join("children").join("moveforgedchild");
        fs::create_dir_all(&dest_children).unwrap();
        // Child session with a plausible migrated header but different body.
        fs::write(
            dest_children.join("childone.jsonl"),
            format!(
                "{}\ncorrupted body\n",
                json!({
                    "type": "session", "version": 3, "id": "childone",
                    "timestamp": "2026-01-01T00:00:00.000Z",
                    "cwd": fixture.new_cwd,
                    "parentSession": fixture.target_dir.join(&fixture.primary_name),
                }),
            ),
        )
        .unwrap();
        // Raw child with different bytes.
        fs::write(dest_children.join("todo-manifest.json"), "{\"version\":9}\n").unwrap();

        let error = run_move(&fixture, MoveFaults::default()).expect_err("forged child refused");
        assert!(
            rendered(&error).contains("different content"),
            "{error:#}"
        );
        assert_source_intact(&fixture, "moveforgedchild");
    }

    /// P1: the published destination is committed bottom-up — the child leaf
    /// directory, then the intermediate `children` directory linking it,
    /// then the session-store root; source deletion follows with the same
    /// parent-before-root ordering.
    #[test]
    fn commit_syncs_children_hierarchy_bottom_up() {
        let fixture = create_fixture("movesyncorder");
        let sync_log: SyncLog = Rc::new(RefCell::new(Vec::new()));
        let memory_calls: MemoryCallLog = Rc::new(RefCell::new(Vec::new()));
        move_saved_session_with(
            &fixture.source_path,
            &fixture.new_cwd,
            move_options(&fixture, MoveFaults::default(), Some(sync_log.clone()), Some(memory_calls.clone())),
        )
        .expect("move succeeds");
        let order = sync_log.borrow().clone();
        let position = |label: &str| {
            order
                .iter()
                .position(|entry| entry == label)
                .unwrap_or_else(|| panic!("{label} must be synced; order: {order:?}"))
        };
        let leaf = position("target:children-leaf");
        let children_parent = position("target:children-parent");
        let target_root = position("target:root");
        let source_children_parent = position("source:children-parent");
        let source_root = position("source:root");
        assert!(
            leaf < children_parent && children_parent < target_root,
            "destination must sync leaf, children parent, then root: {order:?}"
        );
        assert!(
            target_root < source_children_parent && source_children_parent < source_root,
            "source removal commit must sync children parent, then root: {order:?}"
        );
    }

    /// A failure while durably linking the child subtree rolls the whole
    /// destination back and keeps the source intact.
    #[test]
    fn children_parent_sync_failure_rolls_back() {
        let fixture = create_fixture("movechildsync");
        let error = run_move(
            &fixture,
            MoveFaults {
                fail_target_children_sync: true,
                ..MoveFaults::default()
            },
        )
        .expect_err("injected children sync failure");
        assert!(
            rendered(&error).contains("children directory sync failure"),
            "{error:#}"
        );
        assert_source_intact(&fixture, "movechildsync");
        assert!(
            fixture.target_dir.read_dir().unwrap().next().is_none(),
            "destination must be cleaned"
        );
    }

    fn memory_entries_path(fixture: &Fixture) -> PathBuf {
        fixture.agent_dir.join("memory").join(crate::memory::repo_namespace(&fixture.old_cwd)).join("entries.jsonl")
    }

    fn seed_memory(fixture: &Fixture) {
        let capabilities = CapabilityFs::new().expect("capabilities");
        let namespace = crate::memory::repo_namespace(&fixture.old_cwd);
        let store = MemoryStore::new_with_capabilities(&capabilities, &fixture.agent_dir, &namespace)
            .expect("memory store");
        store
            .learn("session move memory entry", Vec::new(), "movesession")
            .expect("learn");
    }

    /// P1: the memory rebind prepares the additive copy and commits the old
    /// namespace away only after the file move committed.
    #[test]
    fn memory_rebind_prepares_then_commits_after_file_move() {
        let fixture = create_fixture("movememory");
        seed_memory(&fixture);
        let memory_calls: MemoryCallLog = Rc::new(RefCell::new(Vec::new()));
        let outcome = move_saved_session_with(
            &fixture.source_path,
            &fixture.new_cwd,
            move_options(&fixture, MoveFaults::default(), None, Some(memory_calls.clone())),
        )
        .expect("move succeeds");
        assert_eq!(memory_calls.borrow().as_slice(), ["prepare", "commit"]);
        assert!(outcome.rebound.memory, "memory rebind must be claimed");
        assert!(
            outcome.warnings.is_empty(),
            "warnings: {:?}",
            outcome.warnings
        );
        assert!(
            !memory_entries_path(&fixture).exists(),
            "old memory archive must be committed away"
        );
        assert!(!fixture.source_path.exists(), "file move committed");
        assert!(load_session_tree(&outcome.to).is_ok());
    }

    /// P1: a memory commit failure surfaces a recovery warning, keeps the
    /// old archive and the committed destination, and a retry finalizes it
    /// idempotently.
    #[test]
    fn memory_commit_failure_retains_recovery_state_and_retry_finalizes() {
        let fixture = create_fixture("movememoryretry");
        seed_memory(&fixture);
        let backup = tempdir().expect("backup");
        copy_tree(&fixture.source_dir, backup.path());

        let first = run_move(
            &fixture,
            MoveFaults {
                fail_memory_commit: true,
                ..MoveFaults::default()
            },
        )
        .expect("commit failure must not abort the committed file move");
        assert!(!first.rebound.memory, "uncommitted rebind must not be claimed");
        assert!(
            first
                .warnings
                .iter()
                .any(|warning| warning.contains("memory namespace commit")),
            "warnings: {:?}",
            first.warnings
        );
        assert!(
            memory_entries_path(&fixture).exists(),
            "old archive retained for recovery"
        );
        assert!(!fixture.source_path.exists(), "file move committed");
        assert!(load_session_tree(&first.to).is_ok());

        // Crash-before-removal retry: restore the untouched source tree.
        copy_tree(backup.path(), &fixture.source_dir);
        let second = run_move(&fixture, MoveFaults::default()).expect("retry finalizes");
        assert!(
            !memory_entries_path(&fixture).exists(),
            "retry must finalize the memory commit"
        );
        assert!(second.warnings.is_empty(), "warnings: {:?}", second.warnings);
        assert!(
            second.rebound.memory,
            "commit-only retry must claim the finalized memory rebind"
        );
        assert!(fixture.source_path.exists(), "idempotent rerun touches nothing");
    }

    /// P1: a retry into a pre-existing (partial) destination child subtree
    /// must still fsync the leaf, the children parent, and the store root —
    /// dirents written into an existing directory are only durable once the
    /// directory linking them is synced, and a skipped leaf fsync could let
    /// a committed move lose its children on a later crash.
    #[test]
    fn pre_existing_child_dir_retry_syncs_subtree() {
        let fixture = create_fixture("movepartialdir");
        // Pre-existing partial destination: only the verbatim child entry,
        // byte-exact, so the move proceeds instead of refusing a collision.
        let dest_children = fixture.target_dir.join("children").join("movepartialdir");
        fs::create_dir_all(&dest_children).unwrap();
        fs::write(dest_children.join("todo-manifest.json"), "{\"version\":1}\n").unwrap();

        // First attempt faults mid-child-copy: the pre-existing directory is
        // not ours to remove, only the file this run created is rolled back.
        let error = run_move(
            &fixture,
            MoveFaults {
                fail_copy: Some(CopyStage::Child),
                ..MoveFaults::default()
            },
        )
        .expect_err("injected child copy failure");
        assert!(rendered(&error).contains("injected copy failure"), "{error:#}");
        assert_source_intact(&fixture, "movepartialdir");
        assert!(
            dest_children.join("todo-manifest.json").exists(),
            "pre-existing child directory must not be removed by rollback"
        );

        // Retry succeeds, and the sync log proves the whole hierarchy was
        // fsynced bottom-up even though the leaf directory already existed.
        let sync_log: SyncLog = Rc::new(RefCell::new(Vec::new()));
        let outcome = move_saved_session_with(
            &fixture.source_path,
            &fixture.new_cwd,
            move_options(&fixture, MoveFaults::default(), Some(sync_log.clone()), None),
        )
        .expect("retry succeeds");
        assert_eq!(outcome.children, 3);
        assert!(!fixture.source_path.exists(), "retry commits the move");
        assert!(dest_children.join("childone.jsonl").exists());
        assert!(
            dest_children.join(ORCHESTRATION_STATE_FILENAME).exists(),
            "rewritten orchestration state must land in the pre-existing subtree"
        );
        let order = sync_log.borrow().clone();
        let position = |label: &str| {
            order
                .iter()
                .position(|entry| entry == label)
                .unwrap_or_else(|| panic!("{label} must be synced; order: {order:?}"))
        };
        let leaf = position("target:children-leaf");
        let children_parent = position("target:children-parent");
        let target_root = position("target:root");
        assert!(
            leaf < children_parent && children_parent < target_root,
            "destination must sync leaf, children parent, then root even for a pre-existing leaf: {order:?}"
        );
    }

    /// P2: best-effort rebind warnings are scrubbed like errors — a failing
    /// rebind must not disclose the agent-dir layout or the old checkout on
    /// an otherwise successful move.
    #[cfg(unix)]
    #[test]
    fn rebind_warnings_are_scrubbed() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = create_fixture("movewarnscrub");
        seed_memory(&fixture);
        let memory_dir = fixture.agent_dir.join("memory");
        let original_mode = fs::metadata(&memory_dir).unwrap().permissions().mode();
        let mut permissions = fs::metadata(&memory_dir).unwrap().permissions();
        permissions.set_mode(0o500);
        fs::set_permissions(&memory_dir, permissions).unwrap();

        let outcome = run_move(&fixture, MoveFaults::default()).expect("move succeeds");

        let mut permissions = fs::metadata(&memory_dir).unwrap().permissions();
        permissions.set_mode(original_mode);
        fs::set_permissions(&memory_dir, permissions).unwrap();

        assert!(
            outcome
                .warnings
                .iter()
                .any(|warning| warning.contains("memory namespace")),
            "expected memory rebind warnings, got {:?}",
            outcome.warnings
        );
        assert!(
            !outcome.rebound.memory,
            "a failed memory rebind must not be claimed"
        );
        for warning in &outcome.warnings {
            for leak in [
                fixture.agent_dir.to_str().unwrap(),
                fixture.old_cwd.to_str().unwrap(),
                fixture.new_cwd.to_str().unwrap(),
                fixture.source_dir.to_str().unwrap(),
                fixture.target_dir.to_str().unwrap(),
            ] {
                assert!(!warning.contains(leak), "warning leaks {leak}: {warning}");
            }
        }
        assert!(
            !fixture.source_path.exists(),
            "the file move must stay committed despite rebind warnings"
        );
        assert!(load_session_tree(&outcome.to).is_ok());
    }

    #[test]
    fn move_rejects_missing_source_and_bad_targets() {
        let fixture = create_fixture("movebadtarget");
        let missing = fixture.source_dir.join("missing.jsonl");
        assert!(move_saved_session(&missing, &fixture.new_cwd).is_err());

        // A file is not an operator directory.
        let not_a_dir = fixture.source_dir.join("file.txt");
        fs::write(&not_a_dir, "x").unwrap();
        assert!(
            move_saved_session(&fixture.source_path, &not_a_dir).is_err(),
            "file target must be refused"
        );
        // A nonexistent operator directory must be refused.
        assert!(
            move_saved_session(&fixture.source_path, fixture.source_dir.join("nope"))
                .is_err(),
            "missing target must be refused"
        );
        assert_source_intact(&fixture, "movebadtarget");
    }

    #[cfg(unix)]
    #[test]
    fn move_rejects_symlinked_source_and_target() {
        let fixture = create_fixture("movesymlink");
        let link_source = fixture.source_dir.join("linked.jsonl");
        std::os::unix::fs::symlink(&fixture.source_path, &link_source).unwrap();
        assert!(
            move_saved_session(&link_source, &fixture.new_cwd).is_err(),
            "symlinked source must be refused"
        );

        let real_target = tempdir().unwrap();
        let link_target = fixture.source_dir.join("linked-target");
        std::os::unix::fs::symlink(real_target.path(), &link_target).unwrap();
        assert!(
            move_saved_session(&fixture.source_path, &link_target).is_err(),
            "symlinked target must be refused"
        );
        assert_source_intact(&fixture, "movesymlink");
    }

    #[test]
    fn sidecar_pattern_matches_archive_contract() {
        let primary = "session-file.jsonl";
        assert!(is_move_sidecar(primary, OsStr::new("session-file.jsonl.loops.json")));
        assert!(is_move_sidecar(
            primary,
            OsStr::new("session-file.jsonl.rewind-20260101T000000.jsonl")
        ));
        assert!(is_move_sidecar(
            primary,
            OsStr::new("session-file.jsonl.rewind-20260101T000000-2.jsonl")
        ));
        assert!(is_move_sidecar(
            primary,
            OsStr::new("session-file.jsonl.snapcompact-3.jsonl")
        ));
        assert!(!is_move_sidecar(primary, OsStr::new("session-file.jsonl.rewind-x.json")));
        assert!(!is_move_sidecar(primary, OsStr::new("session-file.jsonl.rewind-.jsonl")));
        assert!(!is_move_sidecar(primary, OsStr::new("session-file.jsonl.other.json")));
        assert!(!is_move_sidecar(
            "other.jsonl",
            OsStr::new("session-file.jsonl.loops.json")
        ));
        assert!(!is_move_sidecar(primary, OsStr::new("session-file.jsonl")));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            assert!(!is_move_sidecar(primary, OsStr::from_bytes(b"\xff\xfe.bad")));
        }
    }
}