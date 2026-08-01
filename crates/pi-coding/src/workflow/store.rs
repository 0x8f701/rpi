//! Versioned, bounded, per-workflow durable storage.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use uuid::Uuid;

use super::WorkflowId;

const STORE_VERSION: u32 = 1;
const RECORDS_DIRECTORY: &str = "records";
const SELECTION_FILE: &str = "selection.json";
const MAX_RECORD_BYTES: u64 = 1024 * 1024;
const MAX_AGGREGATE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SELECTION_BYTES: u64 = 16 * 1024;

#[derive(Error)]
pub(super) enum StoreError {
    #[error("workflow id is not safe for durable storage")]
    InvalidWorkflowId,
    #[error("workflow storage path contains a symbolic link")]
    Symlink,
    #[error("workflow storage path component is not a directory")]
    NotDirectory,
    #[error("{operation}: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("{operation}: {source}")]
    Json {
        operation: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("workflow record exceeds the 1 MiB limit")]
    RecordTooLarge,
    #[error("workflow records exceed the 8 MiB aggregate limit")]
    AggregateTooLarge,
    #[error("workflow selection exceeds its size limit")]
    SelectionTooLarge,
}

impl std::fmt::Debug for StoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, formatter)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StoreDiagnosticKind {
    CorruptRecord,
    InvalidRecordName,
    UnsupportedRecordVersion,
    RecordIdentityMismatch,
    RecordLimitExceeded,
    UnsafeRecord,
    CorruptSelection,
    UnsupportedSelectionVersion,
    UnsafeSelection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StoreDiagnostic {
    pub(super) record_id: Option<WorkflowId>,
    pub(super) kind: StoreDiagnosticKind,
    pub(super) message: String,
}

pub(super) struct LoadAll<R> {
    pub(super) records: Vec<R>,
    pub(super) diagnostics: Vec<StoreDiagnostic>,
}

pub(super) struct SelectionLoad {
    pub(super) selected: Option<WorkflowId>,
    pub(super) diagnostics: Vec<StoreDiagnostic>,
}

/// Adapter implemented by the canonical crate-private stored record.
pub(super) trait StoreRecord {
    fn workflow_id(&self) -> &WorkflowId;
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecordEnvelope<R> {
    version: u32,
    record: R,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SelectionEnvelope {
    version: u32,
    selected_workflow_id: Option<WorkflowId>,
}

#[derive(Clone)]
pub(super) struct WorkflowStore {
    root: PathBuf,
    records: PathBuf,
}

impl std::fmt::Debug for WorkflowStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkflowStore")
            .finish_non_exhaustive()
    }
}

impl WorkflowStore {
    pub(super) fn open(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        create_directory_chain(&root)?;
        let records = root.join(RECORDS_DIRECTORY);
        create_directory_chain(&records)?;
        reject_symlink_components(&root)?;
        reject_symlink_components(&records)?;
        ensure_directory(&root)?;
        ensure_directory(&records)?;
        Ok(Self { root, records })
    }

    pub(super) fn load_all<R>(&self) -> Result<LoadAll<R>, StoreError>
    where
        R: DeserializeOwned + StoreRecord,
    {
        self.secure_records_directory()?;
        let mut entries = fs::read_dir(&self.records)
            .map_err(|source| io_error("reading workflow records directory", source))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| io_error("enumerating workflow records", source))?;
        entries.sort_by_key(fs::DirEntry::file_name);

        let mut records = Vec::new();
        let mut diagnostics = Vec::new();
        let mut aggregate_bytes = 0_u64;
        for entry in entries {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                diagnostics.push(diagnostic(None, StoreDiagnosticKind::InvalidRecordName, "ignored a workflow record with a non-UTF-8 name"));
                continue;
            };
            if is_temporary_name(name) || !name.ends_with(".json") {
                continue;
            }
            let record_id = workflow_id_from_file_name(name);
            let Some(file_id) = record_id.as_ref() else {
                diagnostics.push(diagnostic(None, StoreDiagnosticKind::InvalidRecordName, "ignored a workflow record with an invalid file name"));
                continue;
            };
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    diagnostics.push(diagnostic(record_id, StoreDiagnosticKind::CorruptRecord, format!("could not inspect workflow record: {error}")));
                    continue;
                }
            };
            if file_type.is_symlink() || !file_type.is_file() {
                diagnostics.push(diagnostic(record_id, StoreDiagnosticKind::UnsafeRecord, "ignored a workflow record that is not a regular file"));
                continue;
            }
            let length = match entry.metadata() {
                Ok(metadata) => metadata.len(),
                Err(error) => {
                    diagnostics.push(diagnostic(record_id, StoreDiagnosticKind::CorruptRecord, format!("could not read workflow record metadata: {error}")));
                    continue;
                }
            };
            if length > MAX_RECORD_BYTES {
                diagnostics.push(diagnostic(record_id, StoreDiagnosticKind::RecordLimitExceeded, "ignored a workflow record larger than 1 MiB"));
                continue;
            }
            let Some(next_aggregate) = aggregate_bytes.checked_add(length) else {
                diagnostics.push(diagnostic(record_id, StoreDiagnosticKind::RecordLimitExceeded, "ignored a workflow record beyond the 8 MiB aggregate limit"));
                continue;
            };
            if next_aggregate > MAX_AGGREGATE_BYTES {
                diagnostics.push(diagnostic(record_id, StoreDiagnosticKind::RecordLimitExceeded, "ignored a workflow record beyond the 8 MiB aggregate limit"));
                continue;
            }
            aggregate_bytes = next_aggregate;
            match load_record::<R>(&entry.path(), file_id) {
                Ok(record) => records.push(record),
                Err((kind, message)) => diagnostics.push(diagnostic(record_id, kind, message)),
            }
        }
        Ok(LoadAll { records, diagnostics })
    }

    pub(super) fn write<R>(&self, record: &R) -> Result<(), StoreError>
    where
        R: Serialize + StoreRecord,
    {
        validate_workflow_id(record.workflow_id())?;
        let bytes = serialize_record(record)?;
        self.secure_records_directory()?;
        self.ensure_aggregate_capacity(record.workflow_id(), bytes.len() as u64)?;
        let target = self.record_path(record.workflow_id())?;
        reject_symlink_components(&target)?;
        atomic_write(&target, &self.records, &bytes, "writing workflow record")
    }

    pub(super) fn remove(&self, id: &WorkflowId) -> Result<(), StoreError> {
        let path = self.record_path(id)?;
        self.secure_records_directory()?;
        reject_symlink_components(&path)?;
        match fs::remove_file(&path) {
            Ok(()) => sync_directory(&self.records),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(source) => Err(io_error("removing workflow record", source)),
        }
    }

    pub(super) fn load_selection(&self) -> Result<SelectionLoad, StoreError> {
        self.secure_root_directory()?;
        let path = self.root.join(SELECTION_FILE);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(SelectionLoad { selected: None, diagnostics: Vec::new() }),
            Err(source) => return Err(io_error("inspecting workflow selection", source)),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Ok(selection_diagnostic(StoreDiagnosticKind::UnsafeSelection, "ignored workflow selection because it is not a regular file"));
        }
        if metadata.len() > MAX_SELECTION_BYTES {
            return Ok(selection_diagnostic(StoreDiagnosticKind::CorruptSelection, "ignored workflow selection because it exceeds its size limit"));
        }
        let bytes = match read_bounded(&path, MAX_SELECTION_BYTES) {
            Ok(bytes) => bytes,
            Err(error) => return Ok(selection_diagnostic(StoreDiagnosticKind::CorruptSelection, format!("ignored unreadable workflow selection: {error}"))),
        };
        let selection = match serde_json::from_slice::<SelectionEnvelope>(&bytes) {
            Ok(selection) => selection,
            Err(error) => return Ok(selection_diagnostic(StoreDiagnosticKind::CorruptSelection, format!("ignored corrupt workflow selection: {error}"))),
        };
        if selection.version != STORE_VERSION {
            return Ok(selection_diagnostic(StoreDiagnosticKind::UnsupportedSelectionVersion, format!("ignored workflow selection version {} (supported version is {STORE_VERSION})", selection.version)));
        }
        if let Some(id) = selection.selected_workflow_id.as_ref() && validate_workflow_id(id).is_err() {
            return Ok(selection_diagnostic(StoreDiagnosticKind::CorruptSelection, "ignored workflow selection containing an invalid workflow id"));
        }
        Ok(SelectionLoad { selected: selection.selected_workflow_id, diagnostics: Vec::new() })
    }

    pub(super) fn write_selection(&self, selected: Option<&WorkflowId>) -> Result<(), StoreError> {
        if let Some(id) = selected { validate_workflow_id(id)?; }
        let envelope = SelectionEnvelope { version: STORE_VERSION, selected_workflow_id: selected.cloned() };
        let mut bytes = serde_json::to_vec_pretty(&envelope).map_err(|source| StoreError::Json { operation: "serializing workflow selection", source })?;
        bytes.push(b'\n');
        if bytes.len() as u64 > MAX_SELECTION_BYTES { return Err(StoreError::SelectionTooLarge); }
        self.secure_root_directory()?;
        let target = self.root.join(SELECTION_FILE);
        reject_symlink_components(&target)?;
        atomic_write(&target, &self.root, &bytes, "writing workflow selection")
    }

    fn record_path(&self, id: &WorkflowId) -> Result<PathBuf, StoreError> {
        validate_workflow_id(id)?;
        Ok(self.records.join(format!("{}.json", id.as_str())))
    }

    fn ensure_aggregate_capacity(&self, replacing: &WorkflowId, replacement_bytes: u64) -> Result<(), StoreError> {
        let replacing_name = format!("{}.json", replacing.as_str());
        let mut aggregate = replacement_bytes;
        for entry in fs::read_dir(&self.records).map_err(|source| io_error("reading workflow records directory", source))? {
            let entry = entry.map_err(|source| io_error("enumerating workflow records", source))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue; };
            if name == replacing_name || is_temporary_name(name) || !name.ends_with(".json") { continue; }
            let file_type = entry.file_type().map_err(|source| io_error("inspecting workflow record", source))?;
            if file_type.is_symlink() { return Err(StoreError::Symlink); }
            if !file_type.is_file() { continue; }
            aggregate = aggregate.checked_add(entry.metadata().map_err(|source| io_error("reading workflow record metadata", source))?.len()).ok_or(StoreError::AggregateTooLarge)?;
            if aggregate > MAX_AGGREGATE_BYTES { return Err(StoreError::AggregateTooLarge); }
        }
        Ok(())
    }

    fn secure_root_directory(&self) -> Result<(), StoreError> {
        reject_symlink_components(&self.root)?;
        ensure_directory(&self.root)
    }

    fn secure_records_directory(&self) -> Result<(), StoreError> {
        self.secure_root_directory()?;
        reject_symlink_components(&self.records)?;
        ensure_directory(&self.records)
    }
}

fn serialize_record<R: Serialize + StoreRecord>(record: &R) -> Result<Vec<u8>, StoreError> {
    let envelope = RecordEnvelope { version: STORE_VERSION, record };
    let mut bytes = serde_json::to_vec_pretty(&envelope).map_err(|source| StoreError::Json { operation: "serializing workflow record", source })?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_RECORD_BYTES { return Err(StoreError::RecordTooLarge); }
    Ok(bytes)
}

fn load_record<R: DeserializeOwned + StoreRecord>(path: &Path, file_id: &WorkflowId) -> Result<R, (StoreDiagnosticKind, String)> {
    let bytes = read_bounded(path, MAX_RECORD_BYTES).map_err(|error| (StoreDiagnosticKind::CorruptRecord, format!("could not read workflow record: {error}")))?;
    let envelope: RecordEnvelope<R> = serde_json::from_slice(&bytes).map_err(|error| (StoreDiagnosticKind::CorruptRecord, format!("could not decode workflow record: {error}")))?;
    if envelope.version != STORE_VERSION {
        return Err((StoreDiagnosticKind::UnsupportedRecordVersion, format!("ignored workflow record version {} (supported version is {STORE_VERSION})", envelope.version)));
    }
    if envelope.record.workflow_id() != file_id {
        return Err((StoreDiagnosticKind::RecordIdentityMismatch, "ignored workflow record whose identity does not match its file name".to_owned()));
    }
    Ok(envelope.record)
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, StoreError> {
    let mut file = open_read_no_follow(path)?;
    let metadata = file.metadata().map_err(|source| io_error("reading durable file metadata", source))?;
    if !metadata.is_file() { return Err(StoreError::Symlink); }
    if metadata.len() > limit { return Err(StoreError::RecordTooLarge); }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(limit + 1).read_to_end(&mut bytes).map_err(|source| io_error("reading durable file", source))?;
    if bytes.len() as u64 > limit { return Err(StoreError::RecordTooLarge); }
    Ok(bytes)
}

fn atomic_write(target: &Path, parent: &Path, bytes: &[u8], operation: &'static str) -> Result<(), StoreError> {
    let temporary = parent.join(format!(".workflow-{}.tmp", Uuid::new_v4().simple()));
    let result = (|| {
        let mut file = private_create_new(&temporary).map_err(|source| io_error("creating private temporary file", source))?;
        file.write_all(bytes).map_err(|source| io_error(operation, source))?;
        file.sync_all().map_err(|source| io_error("syncing private temporary file", source))?;
        drop(file);
        reject_symlink_components(target)?;
        fs::rename(&temporary, target).map_err(|source| io_error(operation, source))?;
        sync_directory(parent)
    })();
    if result.is_err() { let _ = fs::remove_file(&temporary); }
    result
}

fn private_create_new(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
    options.open(path)
}

fn open_read_no_follow(path: &Path) -> Result<File, StoreError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.custom_flags(nix::libc::O_NOFOLLOW); }
    options.open(path).map_err(|source| io_error("opening durable file", source))
}

fn sync_directory(path: &Path) -> Result<(), StoreError> {
    File::open(path).and_then(|directory| directory.sync_all()).map_err(|source| io_error("syncing workflow storage directory", source))
}

fn create_directory_chain(path: &Path) -> Result<(), StoreError> {
    let absolute = absolute_path(path)?;
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component.as_os_str());
        loop {
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => return Err(StoreError::Symlink),
                Ok(metadata) if !metadata.is_dir() => return Err(StoreError::NotDirectory),
                Ok(_) => break,
                Err(error) if error.kind() == ErrorKind::NotFound => match create_private_directory(&current) {
                    Ok(()) => break,
                    Err(create_error) if create_error.kind() == ErrorKind::AlreadyExists => continue,
                    Err(source) => return Err(io_error("creating workflow storage directory", source)),
                },
                Err(source) => return Err(io_error("inspecting workflow storage directory", source)),
            }
        }
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)] { use std::os::unix::fs::DirBuilderExt; builder.mode(0o700); }
    builder.create(path)
}

fn reject_symlink_components(path: &Path) -> Result<(), StoreError> {
    let absolute = absolute_path(path)?;
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Err(StoreError::Symlink),
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => break,
            Err(source) => return Err(io_error("inspecting workflow storage path", source)),
        }
    }
    Ok(())
}

fn ensure_directory(path: &Path) -> Result<(), StoreError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| io_error("inspecting workflow storage directory", source))?;
    if metadata.file_type().is_symlink() { return Err(StoreError::Symlink); }
    if !metadata.is_dir() { return Err(StoreError::NotDirectory); }
    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf, StoreError> {
    if path.is_absolute() { return Ok(path.to_path_buf()); }
    std::env::current_dir().map(|directory| directory.join(path)).map_err(|source| io_error("resolving workflow storage path", source))
}

fn validate_workflow_id(id: &WorkflowId) -> Result<(), StoreError> {
    let value = id.as_str();
    let valid = !value.is_empty() && value.len() <= 128 && value.as_bytes()[0].is_ascii_alphanumeric()
        && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && !value.contains("..") && !value.ends_with('.') && !value.ends_with(".lock");
    if valid { Ok(()) } else { Err(StoreError::InvalidWorkflowId) }
}

fn workflow_id_from_file_name(name: &str) -> Option<WorkflowId> {
    let id = WorkflowId::new(name.strip_suffix(".json")?);
    validate_workflow_id(&id).ok()?;
    Some(id)
}

fn is_temporary_name(name: &str) -> bool { name.starts_with(".workflow-") && name.ends_with(".tmp") }
fn diagnostic(record_id: Option<WorkflowId>, kind: StoreDiagnosticKind, message: impl Into<String>) -> StoreDiagnostic { StoreDiagnostic { record_id, kind, message: message.into() } }
fn selection_diagnostic(kind: StoreDiagnosticKind, message: impl Into<String>) -> SelectionLoad { SelectionLoad { selected: None, diagnostics: vec![diagnostic(None, kind, message)] } }
fn io_error(operation: &'static str, source: std::io::Error) -> StoreError { StoreError::Io { operation, source } }

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct TestRecord { workflow_id: WorkflowId, payload: String, terminal: bool }
    impl StoreRecord for TestRecord { fn workflow_id(&self) -> &WorkflowId { &self.workflow_id } }
    fn record(id: &str, payload: impl Into<String>) -> TestRecord { TestRecord { workflow_id: WorkflowId::new(id), payload: payload.into(), terminal: false } }
    fn open_store() -> (tempfile::TempDir, WorkflowStore) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = WorkflowStore::open(directory.path().join("workflow-state")).expect("open workflow store");
        (directory, store)
    }

    #[test]
    fn atomic_record_and_selection_roundtrip() {
        let (_directory, store) = open_store();
        let mut expected = record("workflow-a", "first");
        store.write(&expected).expect("write record");
        expected.payload = "replacement".to_owned();
        store.write(&expected).expect("replace record");
        store.write_selection(Some(&expected.workflow_id)).expect("write selection");
        let loaded = store.load_all::<TestRecord>().expect("load records");
        assert_eq!(loaded.records, vec![expected.clone()]);
        assert!(loaded.diagnostics.is_empty());
        assert_eq!(store.load_selection().expect("load selection").selected, Some(expected.workflow_id));
        assert_eq!(fs::read_dir(&store.records).expect("read records").count(), 1);
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(store.records.join("workflow-a.json")).expect("record metadata").permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn records_load_in_stable_filename_order() {
        let (_directory, store) = open_store();
        for id in ["workflow-c", "workflow-a", "workflow-b"] { store.write(&record(id, id)).expect("write record"); }
        let loaded = store.load_all::<TestRecord>().expect("load records");
        assert_eq!(loaded.records.iter().map(|record| record.workflow_id.as_str()).collect::<Vec<_>>(), vec!["workflow-a", "workflow-b", "workflow-c"]);
    }

    #[test]
    fn corrupt_sibling_is_reported_without_hiding_valid_records() {
        let (_directory, store) = open_store();
        store.write(&record("workflow-valid", "kept")).expect("write valid record");
        fs::write(store.records.join("workflow-corrupt.json"), b"not json").expect("write corrupt sibling");
        let loaded = store.load_all::<TestRecord>().expect("load records");
        assert_eq!(loaded.records, vec![record("workflow-valid", "kept")]);
        assert_eq!(loaded.diagnostics.len(), 1);
        assert_eq!(loaded.diagnostics[0].kind, StoreDiagnosticKind::CorruptRecord);
    }

    #[test]
    fn corrupt_selection_is_ignored_with_a_diagnostic() {
        let (_directory, store) = open_store();
        fs::write(store.root.join(SELECTION_FILE), b"{").expect("write corrupt selection");
        let loaded = store.load_selection().expect("load selection");
        assert_eq!(loaded.selected, None);
        assert_eq!(loaded.diagnostics[0].kind, StoreDiagnosticKind::CorruptSelection);
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_links_fail_closed_without_exposing_paths() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().expect("temporary directory");
        let real = directory.path().join("real");
        fs::create_dir(&real).expect("create real directory");
        let linked = directory.path().join("linked");
        symlink(&real, &linked).expect("create directory symlink");
        let error = WorkflowStore::open(&linked).expect_err("reject symlink root");
        assert!(!format!("{error:?}").contains(&directory.path().display().to_string()));
        assert!(matches!(error, StoreError::Symlink));
        let (_directory, store) = open_store();
        symlink(store.records.join("missing"), store.records.join("workflow-link.json")).expect("create record symlink");
        assert_eq!(store.load_all::<TestRecord>().expect("load records").diagnostics[0].kind, StoreDiagnosticKind::UnsafeRecord);
        assert!(matches!(store.write(&record("workflow-link", "blocked")), Err(StoreError::Symlink)));
    }

    #[test]
    fn record_and_aggregate_size_limits_are_enforced() {
        let (_directory, store) = open_store();
        assert!(matches!(store.write(&record("workflow-large", "x".repeat(MAX_RECORD_BYTES as usize))), Err(StoreError::RecordTooLarge)));
        let payload = "x".repeat(MAX_RECORD_BYTES as usize - 256);
        for index in 0..8 { store.write(&record(&format!("workflow-{index}"), payload.clone())).expect("fill capacity"); }
        assert!(matches!(store.write(&record("workflow-8", payload)), Err(StoreError::AggregateTooLarge)));
    }

    #[test]
    fn removal_is_idempotent_and_selection_is_separate() {
        let (_directory, store) = open_store();
        let mut terminal = record("workflow-terminal", "done"); terminal.terminal = true;
        store.write(&terminal).expect("write terminal record");
        store.write_selection(Some(&terminal.workflow_id)).expect("select record");
        store.remove(&terminal.workflow_id).expect("remove record");
        store.remove(&terminal.workflow_id).expect("repeat removal");
        assert!(store.load_all::<TestRecord>().expect("load records").records.is_empty());
        assert_eq!(store.load_selection().expect("load selection").selected, Some(terminal.workflow_id.clone()));
        store.write_selection(None).expect("clear selection");
        assert_eq!(store.load_selection().expect("load selection").selected, None);
    }

    #[test]
    fn invalid_ids_cannot_escape_the_records_directory() {
        let (_directory, store) = open_store();
        for id in ["", "../escape", "a/b", ".hidden", "record.lock"] {
            assert!(matches!(store.write(&record(id, "blocked")), Err(StoreError::InvalidWorkflowId)));
        }
    }
}
