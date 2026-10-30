use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};


const SYNC_DIR: &str = "chat-mapping";
const SYNC_FILE: &str = "sync.json";
const MAX_DOCUMENT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SyncEnvelope {
    pub revision: u64,
    pub saved_at_ms: u64,
    pub document: Value,
}

fn sync_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join(SYNC_DIR).join(SYNC_FILE)
}

pub fn load(agent_dir: &Path) -> Result<Option<SyncEnvelope>> {
    let path = sync_path(agent_dir);
    let raw = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("reading chat mapping sync file"),
    };
    let envelope: SyncEnvelope = serde_json::from_slice(&raw)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(envelope))
}

/// Store `document` when the caller's `base_revision` still matches the server
/// copy. `None` base adopts the server state unconditionally (first write from
/// an empty client, or an explicit adoption after a conflict). Returns the
/// stored envelope, or the server's current envelope on a stale base.
static STORE_LOCKS: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, Arc<std::sync::Mutex<()>>>>> =
    std::sync::OnceLock::new();

fn store_lock(agent_dir: &Path) -> Result<Arc<std::sync::Mutex<()>>> {
    let map = STORE_LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut guards = map.lock().map_err(|_| anyhow::anyhow!("chat mapping sync lock table poisoned"))?;
    Ok(guards.entry(agent_dir.canonicalize().unwrap_or_else(|_| agent_dir.to_path_buf())).or_default().clone())
}

/// Store `document` when the caller's `base_revision` still matches the server
pub fn store(
    agent_dir: &Path,
    base_revision: Option<u64>,
    document: &Value,
) -> Result<(SyncEnvelope, bool)> {
    let guard = store_lock(agent_dir)?;
    let _serialized = guard.lock().map_err(|_| anyhow::anyhow!("chat mapping sync lock poisoned"))?;
    let current = load(agent_dir)?;
    if let (Some(base), Some(existing)) = (base_revision, current.as_ref()) {
        if base != existing.revision {
            return Ok((existing.clone(), false));
        }
    }
    let revision = current.as_ref().map_or(1, |existing| existing.revision + 1);
    let saved_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default();
    let envelope = SyncEnvelope {
        revision,
        saved_at_ms,
        document: document.clone(),
    };
    write(agent_dir, &envelope)?;
    Ok((envelope, true))
}

fn write(agent_dir: &Path, envelope: &SyncEnvelope) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(envelope).context("serializing chat mapping sync")?;
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(anyhow::anyhow!(
            "chat mapping document exceeds the {MAX_DOCUMENT_BYTES}-byte sync cap"
        ));
    }
    crate::team::write_file_atomic_private(&sync_path(agent_dir), &bytes)
}

#[derive(Debug, Deserialize)]
pub struct ChatMappingPutWire {
    pub document: Value,
    #[serde(default)]
    pub base_revision: Option<u64>,
}

pub fn handle_get(agent_dir: &Path) -> Result<Value> {
    let envelope = load(agent_dir)?;
    Ok(match envelope {
        None => json!({ "revision": 0, "savedAtMs": 0, "document": Value::Null }),
        Some(envelope) => json!({
            "revision": envelope.revision,
            "savedAtMs": envelope.saved_at_ms,
            "document": envelope.document,
        }),
    })
}

pub fn handle_put(agent_dir: &Path, wire: &ChatMappingPutWire) -> Result<Value> {
    let (envelope, stored) = store(agent_dir, wire.base_revision, &wire.document)?;
    Ok(if stored {
        json!({ "ok": true, "revision": envelope.revision, "savedAtMs": envelope.saved_at_ms })
    } else {
        json!({
            "conflict": true,
            "revision": envelope.revision,
            "savedAtMs": envelope.saved_at_ms,
            "document": envelope.document,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn doc(label: &str) -> Value {
        json!({ "version": 1, "projects": [{ "label": label }], "mappings": [] })
    }

    #[test]
    fn get_on_empty_dir_returns_null_document() {
        let dir = tempdir().expect("tmp");
        let value = handle_get(dir.path()).expect("get");
        assert_eq!(value["revision"], 0);
        assert_eq!(value["document"], Value::Null);
    }

    #[test]
    fn first_store_creates_revision_one() {
        let dir = tempdir().expect("tmp");
        let (envelope, stored) = store(dir.path(), None, &doc("a")).expect("store");
        assert!(stored);
        assert_eq!(envelope.revision, 1);
        let loaded = load(dir.path()).expect("load").expect("present");
        assert_eq!(loaded.document, doc("a"));
    }

    #[test]
    fn matching_base_revision_overwrites_and_bumps() {
        let dir = tempdir().expect("tmp");
        let (first, _) = store(dir.path(), None, &doc("a")).expect("store");
        let (second, stored) = store(dir.path(), Some(first.revision), &doc("b")).expect("store");
        assert!(stored);
        assert_eq!(second.revision, 2);
        assert_eq!(second.document, doc("b"));
        assert!(second.saved_at_ms >= first.saved_at_ms);
    }

    #[test]
    fn stale_base_revision_returns_server_copy_without_write() {
        let dir = tempdir().expect("tmp");
        let (first, _) = store(dir.path(), None, &doc("a")).expect("store");
        store(dir.path(), Some(first.revision), &doc("b")).expect("store");
        let (envelope, stored) = store(dir.path(), Some(1), &doc("c")).expect("store");
        assert!(!stored);
        assert_eq!(envelope.revision, 2);
        assert_eq!(envelope.document, doc("b"));
        let reread = load(dir.path()).expect("load").expect("present");
        assert_eq!(reread.document, doc("b"));
    }

    #[test]
    fn none_base_adopts_server_state_unconditionally() {
        let dir = tempdir().expect("tmp");
        store(dir.path(), None, &doc("a")).expect("store");
        store(dir.path(), Some(1), &doc("b")).expect("store");
        let (envelope, stored) = store(dir.path(), None, &doc("z")).expect("store");
        assert!(stored);
        assert_eq!(envelope.revision, 3);
        assert_eq!(envelope.document, doc("z"));
    }

    #[test]
    fn handle_put_reports_conflict_with_server_document() {
        let dir = tempdir().expect("tmp");
        store(dir.path(), None, &doc("a")).expect("store");
        let value = handle_put(
            dir.path(),
            &ChatMappingPutWire {
                document: doc("stale"),
                base_revision: Some(99),
            },
        )
        .expect("put");
        assert_eq!(value["conflict"], true);
        assert_eq!(value["document"], doc("a"));
    }
}