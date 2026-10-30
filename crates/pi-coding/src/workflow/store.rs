//! Versioned, bounded, per-workflow durable storage.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

use crate::fs::{
    CapabilityAccess, CapabilityEntry, CapabilityFile, CapabilityRootLease, CapabilityRootPurpose,
    EntryKind, FileIdentity, FileMode, FsError, Replacement, RootCreation,
};

use super::WorkflowId;

const STORE_VERSION: u32 = 2;
const RECORDS_DIRECTORY: &str = "records";
const SELECTION_FILE: &str = "selection.json";
const MAX_RECORD_BYTES: u64 = 1024 * 1024;
const MAX_AGGREGATE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SELECTION_BYTES: u64 = 16 * 1024;

#[derive(Error)]
pub(super) enum StoreError {
    #[error("workflow id is not safe for durable storage")]
    InvalidWorkflowId,
    #[error("workflow storage lease has the wrong purpose or access")]
    InvalidLease,
    #[error("workflow storage capability failed: {0}")]
    Capability(#[from] FsError),
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
    root: CapabilityRootLease,
    records: CapabilityRootLease,
}

impl std::fmt::Debug for WorkflowStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkflowStore")
            .finish_non_exhaustive()
    }
}

impl WorkflowStore {
    pub(super) fn open(root: CapabilityRootLease) -> Result<Self, StoreError> {
        if root.purpose() != CapabilityRootPurpose::WorkflowStore
            || root.access() != CapabilityAccess::PrivateReadWrite
        {
            return Err(StoreError::InvalidLease);
        }
        let records = root.lease_directory(
            Path::new(RECORDS_DIRECTORY),
            CapabilityAccess::PrivateReadWrite,
            RootCreation::CreateDirectory,
        )?;
        Ok(Self { root, records })
    }

    pub(super) fn load_all<R>(&self) -> Result<LoadAll<R>, StoreError>
    where
        R: DeserializeOwned + StoreRecord,
    {
        let mut entries = self.records.read_dir(Path::new(""))?;
        let mut records = Vec::new();
        let mut diagnostics = Vec::new();
        let mut aggregate_bytes = 0_u64;
        while let Some(entry) = entries.next()? {
            let Some(name) = entry.name().to_str() else {
                diagnostics.push(diagnostic(
                    None,
                    StoreDiagnosticKind::InvalidRecordName,
                    "ignored a workflow record with a non-UTF-8 name",
                ));
                continue;
            };
            if is_temporary_name(name) || !name.ends_with(".json") {
                continue;
            }
            let record_id = workflow_id_from_file_name(name);
            let Some(file_id) = record_id.as_ref() else {
                diagnostics.push(diagnostic(
                    None,
                    StoreDiagnosticKind::InvalidRecordName,
                    "ignored a workflow record with an invalid file name",
                ));
                continue;
            };
            let file = match entry.into_entry() {
                CapabilityEntry::File(file) => file,
                CapabilityEntry::Directory(_) => {
                    diagnostics.push(diagnostic(
                        record_id,
                        StoreDiagnosticKind::UnsafeRecord,
                        "ignored a workflow record that is not a regular file",
                    ));
                    continue;
                }
            };
            let length = match file.metadata() {
                Ok(metadata) => metadata.len,
                Err(error) => {
                    diagnostics.push(diagnostic(
                        record_id,
                        StoreDiagnosticKind::CorruptRecord,
                        format!("could not read workflow record metadata: {error}"),
                    ));
                    continue;
                }
            };
            if length > MAX_RECORD_BYTES {
                diagnostics.push(diagnostic(
                    record_id,
                    StoreDiagnosticKind::RecordLimitExceeded,
                    "ignored a workflow record larger than 1 MiB",
                ));
                continue;
            }
            let Some(next_aggregate) = aggregate_bytes.checked_add(length) else {
                diagnostics.push(diagnostic(
                    record_id,
                    StoreDiagnosticKind::RecordLimitExceeded,
                    "ignored a workflow record beyond the 8 MiB aggregate limit",
                ));
                continue;
            };
            if next_aggregate > MAX_AGGREGATE_BYTES {
                diagnostics.push(diagnostic(
                    record_id,
                    StoreDiagnosticKind::RecordLimitExceeded,
                    "ignored a workflow record beyond the 8 MiB aggregate limit",
                ));
                continue;
            }
            aggregate_bytes = next_aggregate;
            match load_record::<R>(&file, file_id) {
                Ok(record) => records.push(record),
                Err((kind, message)) => diagnostics.push(diagnostic(record_id, kind, message)),
            }
        }
        Ok(LoadAll {
            records,
            diagnostics,
        })
    }

    pub(super) fn write<R>(&self, record: &R) -> Result<(), StoreError>
    where
        R: Serialize + StoreRecord,
    {
        validate_workflow_id(record.workflow_id())?;
        let bytes = serialize_record(record)?;
        self.ensure_aggregate_capacity(record.workflow_id(), bytes.len() as u64)?;
        let relative = record_relative(record.workflow_id())?;
        let expected = existing_identity(&self.records, &relative)?;
        self.records.replace_file(Replacement {
            relative,
            expected,
            bytes,
            mode: FileMode::Private,
        })?;
        Ok(())
    }

    pub(super) fn remove(&self, id: &WorkflowId) -> Result<(), StoreError> {
        let relative = record_relative(id)?;
        match self.records.remove_file(&relative) {
            Ok(()) => Ok(()),
            Err(error) if is_missing(&error) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn load_selection(&self) -> Result<SelectionLoad, StoreError> {
        let relative = Path::new(SELECTION_FILE);
        let file = match self.root.open_file(relative) {
            Ok(file) => file,
            Err(error) if is_missing(&error) => {
                return Ok(SelectionLoad {
                    selected: None,
                    diagnostics: Vec::new(),
                });
            }
            Err(error) => return Err(error.into()),
        };
        let metadata = match file.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                return Ok(selection_diagnostic(
                    StoreDiagnosticKind::CorruptSelection,
                    format!("ignored unreadable workflow selection: {error}"),
                ));
            }
        };
        if metadata.len > MAX_SELECTION_BYTES {
            return Ok(selection_diagnostic(
                StoreDiagnosticKind::CorruptSelection,
                "ignored workflow selection because it exceeds its size limit",
            ));
        }
        let bytes = match file.read_bounded(MAX_SELECTION_BYTES) {
            Ok(bytes) => bytes,
            Err(error) => {
                return Ok(selection_diagnostic(
                    StoreDiagnosticKind::CorruptSelection,
                    format!("ignored unreadable workflow selection: {error}"),
                ));
            }
        };
        let selection = match serde_json::from_slice::<SelectionEnvelope>(&bytes) {
            Ok(selection) => selection,
            Err(error) => {
                return Ok(selection_diagnostic(
                    StoreDiagnosticKind::CorruptSelection,
                    format!("ignored corrupt workflow selection: {error}"),
                ));
            }
        };
        if selection.version != STORE_VERSION {
            return Ok(selection_diagnostic(
                StoreDiagnosticKind::UnsupportedSelectionVersion,
                format!(
                    "ignored workflow selection version {} (supported version is {STORE_VERSION})",
                    selection.version
                ),
            ));
        }
        if let Some(id) = selection.selected_workflow_id.as_ref()
            && validate_workflow_id(id).is_err()
        {
            return Ok(selection_diagnostic(
                StoreDiagnosticKind::CorruptSelection,
                "ignored workflow selection containing an invalid workflow id",
            ));
        }
        Ok(SelectionLoad {
            selected: selection.selected_workflow_id,
            diagnostics: Vec::new(),
        })
    }

    pub(super) fn write_selection(&self, selected: Option<&WorkflowId>) -> Result<(), StoreError> {
        if let Some(id) = selected {
            validate_workflow_id(id)?;
        }
        let envelope = SelectionEnvelope {
            version: STORE_VERSION,
            selected_workflow_id: selected.cloned(),
        };
        let mut bytes =
            serde_json::to_vec_pretty(&envelope).map_err(|source| StoreError::Json {
                operation: "serializing workflow selection",
                source,
            })?;
        bytes.push(b'\n');
        if bytes.len() as u64 > MAX_SELECTION_BYTES {
            return Err(StoreError::SelectionTooLarge);
        }
        let relative = PathBuf::from(SELECTION_FILE);
        let expected = existing_identity(&self.root, &relative)?;
        self.root.replace_file(Replacement {
            relative,
            expected,
            bytes,
            mode: FileMode::Private,
        })?;
        Ok(())
    }

    fn ensure_aggregate_capacity(
        &self,
        replacing: &WorkflowId,
        replacement_bytes: u64,
    ) -> Result<(), StoreError> {
        let replacing_name = format!("{}.json", replacing.as_str());
        let mut aggregate = replacement_bytes;
        let mut entries = self.records.read_dir(Path::new(""))?;
        while let Some(entry) = entries.next()? {
            let Some(name) = entry.name().to_str() else {
                continue;
            };
            if name == replacing_name || is_temporary_name(name) || !name.ends_with(".json") {
                continue;
            }
            if entry.entry().kind() != EntryKind::RegularFile {
                continue;
            }
            let CapabilityEntry::File(file) = entry.into_entry() else {
                continue;
            };
            aggregate = aggregate
                .checked_add(file.metadata()?.len)
                .ok_or(StoreError::AggregateTooLarge)?;
            if aggregate > MAX_AGGREGATE_BYTES {
                return Err(StoreError::AggregateTooLarge);
            }
        }
        Ok(())
    }
}

fn serialize_record<R: Serialize + StoreRecord>(record: &R) -> Result<Vec<u8>, StoreError> {
    let envelope = RecordEnvelope {
        version: STORE_VERSION,
        record,
    };
    let mut bytes = serde_json::to_vec_pretty(&envelope).map_err(|source| StoreError::Json {
        operation: "serializing workflow record",
        source,
    })?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(StoreError::RecordTooLarge);
    }
    Ok(bytes)
}

fn load_record<R: DeserializeOwned + StoreRecord>(
    file: &CapabilityFile,
    file_id: &WorkflowId,
) -> Result<R, (StoreDiagnosticKind, String)> {
    let bytes = file.read_bounded(MAX_RECORD_BYTES).map_err(|error| {
        (
            StoreDiagnosticKind::CorruptRecord,
            format!("could not read workflow record: {error}"),
        )
    })?;
    let envelope: RecordEnvelope<R> = serde_json::from_slice(&bytes).map_err(|error| {
        (
            StoreDiagnosticKind::CorruptRecord,
            format!("could not decode workflow record: {error}"),
        )
    })?;
    if envelope.version != STORE_VERSION {
        return Err((
            StoreDiagnosticKind::UnsupportedRecordVersion,
            format!(
                "ignored workflow record version {} (supported version is {STORE_VERSION})",
                envelope.version
            ),
        ));
    }
    if envelope.record.workflow_id() != file_id {
        return Err((
            StoreDiagnosticKind::RecordIdentityMismatch,
            "ignored workflow record whose identity does not match its file name".to_owned(),
        ));
    }
    Ok(envelope.record)
}

fn existing_identity(
    lease: &CapabilityRootLease,
    relative: &Path,
) -> Result<Option<FileIdentity>, StoreError> {
    match lease.metadata(relative) {
        Ok(metadata) => Ok(Some(metadata.identity)),
        Err(error) if is_missing(&error) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn is_missing(error: &FsError) -> bool {
    matches!(
        error,
        FsError::System {
            kind: std::io::ErrorKind::NotFound,
            ..
        }
    )
}

fn record_relative(id: &WorkflowId) -> Result<PathBuf, StoreError> {
    validate_workflow_id(id)?;
    Ok(PathBuf::from(format!("{}.json", id.as_str())))
}

fn validate_workflow_id(id: &WorkflowId) -> Result<(), StoreError> {
    let value = id.as_str();
    let valid = !value.is_empty()
        && value.len() <= 128
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && !value.contains("..")
        && !value.ends_with('.')
        && !value.ends_with(".lock");
    if valid {
        Ok(())
    } else {
        Err(StoreError::InvalidWorkflowId)
    }
}

fn workflow_id_from_file_name(name: &str) -> Option<WorkflowId> {
    let id = WorkflowId::new(name.strip_suffix(".json")?);
    validate_workflow_id(&id).ok()?;
    Some(id)
}

fn is_temporary_name(name: &str) -> bool {
    name.starts_with(".workflow-") && name.ends_with(".tmp")
}
fn diagnostic(
    record_id: Option<WorkflowId>,
    kind: StoreDiagnosticKind,
    message: impl Into<String>,
) -> StoreDiagnostic {
    StoreDiagnostic {
        record_id,
        kind,
        message: message.into(),
    }
}
fn selection_diagnostic(kind: StoreDiagnosticKind, message: impl Into<String>) -> SelectionLoad {
    SelectionLoad {
        selected: None,
        diagnostics: vec![diagnostic(None, kind, message)],
    }
}
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pi_sandbox::AbsolutePath;

    use super::*;
    use crate::fs::{CapabilityFs, RootCreation};

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct TestRecord {
        workflow_id: WorkflowId,
        payload: String,
        terminal: bool,
    }
    impl StoreRecord for TestRecord {
        fn workflow_id(&self) -> &WorkflowId {
            &self.workflow_id
        }
    }
    fn record(id: &str, payload: impl Into<String>) -> TestRecord {
        TestRecord {
            workflow_id: WorkflowId::new(id),
            payload: payload.into(),
            terminal: false,
        }
    }
    fn store_lease(fs: &Arc<CapabilityFs>, path: &Path) -> CapabilityRootLease {
        fs.open_root(
            CapabilityRootPurpose::WorkflowStore,
            AbsolutePath::parse(path.to_path_buf()).expect("absolute workflow store"),
            CapabilityAccess::PrivateReadWrite,
            RootCreation::CreateDirectory,
        )
        .expect("workflow store lease")
    }
    fn open_store() -> (tempfile::TempDir, WorkflowStore) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let fs = CapabilityFs::new().expect("capability fs");
        let store = WorkflowStore::open(store_lease(&fs, &directory.path().join("workflow-state")))
            .expect("open workflow store");
        (directory, store)
    }

    #[test]
    fn atomic_record_and_selection_roundtrip() {
        let (_directory, store) = open_store();
        let mut expected = record("workflow-a", "first");
        store.write(&expected).expect("write record");
        expected.payload = "replacement".to_owned();
        store.write(&expected).expect("replace record");
        store
            .write_selection(Some(&expected.workflow_id))
            .expect("write selection");
        let loaded = store.load_all::<TestRecord>().expect("load records");
        assert_eq!(loaded.records, vec![expected.clone()]);
        assert!(loaded.diagnostics.is_empty());
        assert_eq!(
            store.load_selection().expect("load selection").selected,
            Some(expected.workflow_id)
        );
        let mut entries = store.records.read_dir(Path::new("")).expect("read records");
        assert!(entries.next().expect("first entry").is_some());
        assert!(entries.next().expect("end entries").is_none());
    }

    #[test]
    fn records_load_in_stable_filename_order() {
        let (_directory, store) = open_store();
        for id in ["workflow-c", "workflow-a", "workflow-b"] {
            store.write(&record(id, id)).expect("write record");
        }
        let loaded = store.load_all::<TestRecord>().expect("load records");
        assert_eq!(
            loaded
                .records
                .iter()
                .map(|record| record.workflow_id.as_str())
                .collect::<Vec<_>>(),
            vec!["workflow-a", "workflow-b", "workflow-c"]
        );
    }

    #[test]
    fn corrupt_sibling_is_reported_without_hiding_valid_records() {
        let (_directory, store) = open_store();
        store
            .write(&record("workflow-valid", "kept"))
            .expect("write valid record");
        store
            .records
            .replace_file(Replacement {
                relative: PathBuf::from("workflow-corrupt.json"),
                expected: None,
                bytes: b"not json".to_vec(),
                mode: FileMode::Private,
            })
            .expect("write corrupt sibling");
        let loaded = store.load_all::<TestRecord>().expect("load records");
        assert_eq!(loaded.records, vec![record("workflow-valid", "kept")]);
        assert_eq!(loaded.diagnostics.len(), 1);
        assert_eq!(
            loaded.diagnostics[0].kind,
            StoreDiagnosticKind::CorruptRecord
        );
    }

    #[test]
    fn previous_record_and_selection_versions_are_rejected() {
        let (_directory, store) = open_store();
        let expected = record("workflow-old", "retired schema");
        let record_bytes = serde_json::to_vec_pretty(&RecordEnvelope {
            version: STORE_VERSION - 1,
            record: expected.clone(),
        })
        .expect("serialize previous record");
        store
            .records
            .replace_file(Replacement {
                relative: PathBuf::from("workflow-old.json"),
                expected: None,
                bytes: record_bytes,
                mode: FileMode::Private,
            })
            .expect("write previous record");
        let loaded = store.load_all::<TestRecord>().expect("load records");
        assert!(loaded.records.is_empty());
        assert_eq!(
            loaded.diagnostics[0].kind,
            StoreDiagnosticKind::UnsupportedRecordVersion
        );

        let selection_bytes = serde_json::to_vec_pretty(&SelectionEnvelope {
            version: STORE_VERSION - 1,
            selected_workflow_id: Some(expected.workflow_id),
        })
        .expect("serialize previous selection");
        store
            .root
            .replace_file(Replacement {
                relative: PathBuf::from(SELECTION_FILE),
                expected: None,
                bytes: selection_bytes,
                mode: FileMode::Private,
            })
            .expect("write previous selection");
        let selection = store.load_selection().expect("load selection");
        assert_eq!(selection.selected, None);
        assert_eq!(
            selection.diagnostics[0].kind,
            StoreDiagnosticKind::UnsupportedSelectionVersion
        );
    }

    #[test]
    fn corrupt_selection_is_ignored_with_a_diagnostic() {
        let (_directory, store) = open_store();
        store
            .root
            .replace_file(Replacement {
                relative: PathBuf::from(SELECTION_FILE),
                expected: None,
                bytes: b"{".to_vec(),
                mode: FileMode::Private,
            })
            .expect("write corrupt selection");
        let loaded = store.load_selection().expect("load selection");
        assert_eq!(loaded.selected, None);
        assert_eq!(
            loaded.diagnostics[0].kind,
            StoreDiagnosticKind::CorruptSelection
        );
    }

    #[cfg(unix)]
    #[test]
    fn identity_swap_after_lease_fails_closed_without_exposing_paths() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("workflow-state");
        let fs = CapabilityFs::new().expect("capability fs");
        let lease = store_lease(&fs, &path);
        std::fs::rename(&path, directory.path().join("displaced")).expect("displace root");
        symlink(directory.path().join("displaced"), &path).expect("replace with symlink");
        let store = WorkflowStore::open(lease).expect("retained lease remains valid");
        store
            .write(&record("workflow-safe", "retained"))
            .expect("write through retained lease");
        assert!(matches!(
            fs.open_root(
                CapabilityRootPurpose::WorkflowStore,
                AbsolutePath::parse(path).expect("absolute replacement"),
                CapabilityAccess::PrivateReadWrite,
                RootCreation::Existing,
            ),
            Err(FsError::UnsupportedEntryType | FsError::System { .. })
        ));
    }

    #[test]
    fn wrong_purpose_lease_is_rejected() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let fs = CapabilityFs::new().expect("capability fs");
        let lease = fs
            .open_root(
                CapabilityRootPurpose::SessionStore,
                AbsolutePath::parse(directory.path().to_path_buf())
                    .expect("absolute session store"),
                CapabilityAccess::PrivateReadWrite,
                RootCreation::Existing,
            )
            .expect("session lease");
        assert!(matches!(
            WorkflowStore::open(lease),
            Err(StoreError::InvalidLease)
        ));
    }

    #[test]
    fn record_and_aggregate_size_limits_are_enforced() {
        let (_directory, store) = open_store();
        assert!(matches!(
            store.write(&record(
                "workflow-large",
                "x".repeat(MAX_RECORD_BYTES as usize)
            )),
            Err(StoreError::RecordTooLarge)
        ));
        let payload = "x".repeat(MAX_RECORD_BYTES as usize - 256);
        for index in 0..8 {
            store
                .write(&record(&format!("workflow-{index}"), payload.clone()))
                .expect("fill capacity");
        }
        assert!(matches!(
            store.write(&record("workflow-8", payload)),
            Err(StoreError::AggregateTooLarge)
        ));
    }

    #[test]
    fn removal_is_idempotent_and_selection_is_separate() {
        let (_directory, store) = open_store();
        let mut terminal = record("workflow-terminal", "done");
        terminal.terminal = true;
        store.write(&terminal).expect("write terminal record");
        store
            .write_selection(Some(&terminal.workflow_id))
            .expect("select record");
        store.remove(&terminal.workflow_id).expect("remove record");
        store.remove(&terminal.workflow_id).expect("repeat removal");
        assert!(
            store
                .load_all::<TestRecord>()
                .expect("load records")
                .records
                .is_empty()
        );
        assert_eq!(
            store.load_selection().expect("load selection").selected,
            Some(terminal.workflow_id.clone())
        );
        store.write_selection(None).expect("clear selection");
        assert_eq!(
            store.load_selection().expect("load selection").selected,
            None
        );
    }

    #[test]
    fn invalid_ids_cannot_escape_the_records_directory() {
        let (_directory, store) = open_store();
        for id in ["", "../escape", "a/b", ".hidden", "record.lock"] {
            assert!(matches!(
                store.write(&record(id, "blocked")),
                Err(StoreError::InvalidWorkflowId)
            ));
        }
    }
}
