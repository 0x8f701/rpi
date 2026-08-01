use crate::providers::{RadiusCatalogSnapshot, validate_radius_snapshot};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const RADIUS_CATALOG_STORE_VERSION: u32 = 1;
const MAX_STORE_BYTES: u64 = 16 * 1024 * 1024;
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const STALE_LOCK_AGE: Duration = Duration::from_secs(30);
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(10);

static PROCESS_STORE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Debug, Clone)]
pub struct RadiusCatalogStore {
    path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoreDocument {
    version: u32,
    #[serde(default)]
    providers: BTreeMap<String, RadiusCatalogSnapshot>,
}

impl Default for StoreDocument {
    fn default() -> Self {
        Self {
            version: RADIUS_CATALOG_STORE_VERSION,
            providers: BTreeMap::new(),
        }
    }
}

impl RadiusCatalogStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self, provider_id: &str) -> Result<Option<RadiusCatalogSnapshot>> {
        validate_provider_id(provider_id)?;
        let document = read_document(&self.path)?;
        Ok(document.providers.get(provider_id).cloned())
    }

    pub fn write(&self, provider_id: &str, snapshot: &RadiusCatalogSnapshot) -> Result<()> {
        validate_provider_id(provider_id)?;
        validate_radius_snapshot(provider_id, snapshot)?;
        let lock = StoreLock::acquire(&self.path)?;
        let mut document = read_document(&self.path)?;
        document
            .providers
            .insert(provider_id.to_owned(), snapshot.clone());
        write_document_atomic(&self.path, &document)?;
        drop(lock);
        Ok(())
    }

    pub fn delete(&self, provider_id: &str) -> Result<bool> {
        validate_provider_id(provider_id)?;
        let lock = StoreLock::acquire(&self.path)?;
        let mut document = read_document(&self.path)?;
        let removed = document.providers.remove(provider_id).is_some();
        if removed {
            write_document_atomic(&self.path, &document)?;
        }
        drop(lock);
        Ok(removed)
    }
}

fn validate_provider_id(provider_id: &str) -> Result<()> {
    if provider_id.trim().is_empty() || provider_id.len() > 256 {
        bail!("Radius catalog store provider id is invalid")
    }
    Ok(())
}

fn read_document(path: &Path) -> Result<StoreDocument> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(StoreDocument::default()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading Radius catalog store metadata {}", path.display()));
        }
    };
    if !metadata.is_file() {
        bail!("Radius catalog store is not a file: {}", path.display())
    }
    if metadata.len() > MAX_STORE_BYTES {
        bail!(
            "Radius catalog store {} exceeds {MAX_STORE_BYTES} bytes",
            path.display()
        )
    }
    let mut file = File::open(path)
        .with_context(|| format!("opening Radius catalog store {}", path.display()))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    std::io::Read::by_ref(&mut file)
        .take(MAX_STORE_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading Radius catalog store {}", path.display()))?;
    if bytes.len() as u64 > MAX_STORE_BYTES {
        bail!(
            "Radius catalog store {} exceeds {MAX_STORE_BYTES} bytes",
            path.display()
        )
    }
    let document: StoreDocument = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing Radius catalog store {}", path.display()))?;
    if document.version != RADIUS_CATALOG_STORE_VERSION {
        bail!(
            "unsupported Radius catalog store version {} in {} (expected {})",
            document.version,
            path.display(),
            RADIUS_CATALOG_STORE_VERSION
        )
    }
    for (provider_id, snapshot) in &document.providers {
        validate_provider_id(provider_id)
            .with_context(|| format!("validating Radius catalog store {}", path.display()))?;
        validate_radius_snapshot(provider_id, snapshot)
            .with_context(|| format!("validating Radius catalog store {}", path.display()))?;
    }
    Ok(document)
}

fn write_document_atomic(path: &Path, document: &StoreDocument) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("creating Radius catalog store directory {}", parent.display()))?;
    let mut bytes = serde_json::to_vec_pretty(document).context("serializing Radius catalog store")?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_STORE_BYTES {
        bail!("Radius catalog store exceeds {MAX_STORE_BYTES} bytes")
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("models-store.json");
    let temporary = parent.join(format!(
        ".{file_name}.{}.tmp",
        Uuid::new_v4().simple()
    ));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("creating temporary Radius catalog store {}", temporary.display()))?;
        file.write_all(&bytes)
            .with_context(|| format!("writing temporary Radius catalog store {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing temporary Radius catalog store {}", temporary.display()))?;
        drop(file);
        fs::rename(&temporary, path).with_context(|| {
            format!(
                "atomically replacing Radius catalog store {} from {}",
                path.display(),
                temporary.display()
            )
        })?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("syncing Radius catalog store directory {}", parent.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

struct StoreLock<'a> {
    _process: MutexGuard<'a, ()>,
    path: PathBuf,
}

impl<'a> StoreLock<'a> {
    fn acquire(store_path: &Path) -> Result<Self> {
        let process = PROCESS_STORE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let parent = store_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .with_context(|| format!("creating Radius catalog store directory {}", parent.display()))?;
        let file_name = store_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("models-store.json");
        let path = parent.join(format!(".{file_name}.lock"));
        let started = Instant::now();
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id()).with_context(|| {
                        format!("writing Radius catalog store lock {}", path.display())
                    })?;
                    file.sync_all().with_context(|| {
                        format!("syncing Radius catalog store lock {}", path.display())
                    })?;
                    return Ok(Self {
                        _process: process,
                        path,
                    });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    if lock_is_stale(&path) {
                        match fs::remove_file(&path) {
                            Ok(()) => continue,
                            Err(remove_error) if remove_error.kind() == ErrorKind::NotFound => {
                                continue;
                            }
                            Err(_) => {}
                        }
                    }
                    if started.elapsed() >= LOCK_TIMEOUT {
                        return Err(anyhow!(
                            "timed out waiting for Radius catalog store lock {}",
                            path.display()
                        ));
                    }
                    std::thread::sleep(LOCK_RETRY_DELAY);
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("creating Radius catalog store lock {}", path.display())
                    });
                }
            }
        }
    }
}

impl Drop for StoreLock<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn lock_is_stale(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
        .is_ok_and(|age| age >= STALE_LOCK_AGE)
}
