//! Per-target mutation serialization keyed by captured policy authority.

use parking_lot::Mutex as StdMutex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, LazyLock};
use tokio::sync::Mutex;

struct LockEntry {
    mu: Arc<Mutex<()>>,
    refs: usize,
}

static REGISTRY: LazyLock<StdMutex<HashMap<String, LockEntry>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));
fn policy_mutation_key(
    revision: pi_sandbox::PolicyRevision,
    identity: crate::fs::FileIdentity,
    path: &Path,
) -> String {
    let normalized = path
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value.to_string_lossy()),
            std::path::Component::CurDir => None,
            _ => Some(component.as_os_str().to_string_lossy()),
        })
        .collect::<Vec<_>>()
        .join("/");
    format!("{}:{identity:?}:{normalized}", revision.get())
}

async fn with_mutation_key<F, Fut, T>(key: String, f: F) -> anyhow::Result<T>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let lock = {
        let mut registry = REGISTRY.lock();
        let entry = registry.entry(key.clone()).or_insert_with(|| LockEntry {
            mu: Arc::new(Mutex::new(())),
            refs: 0,
        });
        entry.refs += 1;
        entry.mu.clone()
    };
    let _guard = lock.lock().await;
    let result = f().await;
    let mut registry = REGISTRY.lock();
    if let Some(entry) = registry.get_mut(&key) {
        entry.refs = entry.refs.saturating_sub(1);
        if entry.refs == 0 {
            registry.remove(&key);
        }
    }
    result
}

pub(crate) async fn with_policy_mutation_queue<F, Fut, T>(
    revision: pi_sandbox::PolicyRevision,
    identity: crate::fs::FileIdentity,
    path: &Path,
    f: F,
) -> anyhow::Result<T>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    with_mutation_key(policy_mutation_key(revision, identity, path), f).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test]
    async fn same_revision_identity_and_path_serialize_mutations() {
        let revision = pi_sandbox::PolicyRevision::new(1).expect("revision");
        let identity = crate::fs::FileIdentity::Unix {
            device: 7,
            inode: 11,
            kind: crate::fs::EntryKind::Directory,
        };
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let second_entered = Arc::new(AtomicBool::new(false));

        let first_entered = entered.clone();
        let first_release = release.clone();
        let first = tokio::spawn(async move {
            with_policy_mutation_queue(revision, identity, Path::new("src/lib.rs"), || async move {
                first_entered.notify_one();
                first_release.notified().await;
                Ok::<_, anyhow::Error>(())
            })
            .await
        });
        entered.notified().await;

        let observed = second_entered.clone();
        let second = tokio::spawn(async move {
            with_policy_mutation_queue(
                revision,
                identity,
                Path::new("src/./lib.rs"),
                || async move {
                    observed.store(true, Ordering::SeqCst);
                    Ok::<_, anyhow::Error>(())
                },
            )
            .await
        });
        tokio::task::yield_now().await;
        assert!(!second_entered.load(Ordering::SeqCst));
        release.notify_one();
        first.await.expect("first join").expect("first mutation");
        second.await.expect("second join").expect("second mutation");
        assert!(second_entered.load(Ordering::SeqCst));
    }
}
