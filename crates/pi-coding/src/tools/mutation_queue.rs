//! Per-file mutation serialization (port of pi's `coding/mutationqueue.go`).
//!
//! Serializes mutating operations that target the same file, keyed by resolved
//! (symlink-followed) path, while allowing operations on different files to run
//! in parallel. Implemented with refcounted per-path async mutexes; entries are
//! removed once drained so the registry does not grow without bound.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, LazyLock};
use parking_lot::Mutex as StdMutex;
use tokio::sync::Mutex;

struct LockEntry {
    mu: Arc<Mutex<()>>,
    refs: usize,
}

static REGISTRY: LazyLock<StdMutex<HashMap<String, LockEntry>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// Reports whether `err` is a "missing path" error (ENOENT or ENOTDIR).
fn is_missing_path_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Resolves the mutation-queue key for `path`: an absolute, symlink-followed
/// path. Missing paths (file not created yet) fall back to the absolute path;
/// any other realpath error propagates.
fn mutation_queue_key(path: &str) -> std::io::Result<String> {
    let abs = std::path::absolute(path)?
        .to_string_lossy()
        .replace('\\', "/");
    match std::fs::canonicalize(&abs) {
        Ok(real) => Ok(real.to_string_lossy().replace('\\', "/")),
        Err(err) if is_missing_path_error(&err) => Ok(abs),
        Err(err) => Err(err),
    }
}

/// Runs `f` while holding the per-file lock for `path`. Different files run in
/// parallel; the same file is serialized.
pub(crate) async fn with_file_mutation_queue<F, Fut, T>(path: &str, f: F) -> anyhow::Result<T>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let key = mutation_queue_key(path)?;
    let lock = {
        let mut reg = REGISTRY.lock();
        let entry = reg.entry(key.clone()).or_insert_with(|| LockEntry {
            mu: Arc::new(Mutex::new(())),
            refs: 0,
        });
        entry.refs += 1;
        entry.mu.clone()
    };
    let _guard = lock.lock().await;
    let result = f().await;
    let mut reg = REGISTRY.lock();
    if let Some(entry) = reg.get_mut(&key) {
        entry.refs = entry.refs.saturating_sub(1);
        if entry.refs == 0 {
            reg.remove(&key);
        }
    }
    result
}

/// Convenience for the common case where the path is already a `Path`.
pub(crate) async fn with_file_mutation_queue_path<F, Fut, T>(
    path: &Path,
    f: F,
) -> anyhow::Result<T>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    with_file_mutation_queue(&path.to_string_lossy(), f).await
}