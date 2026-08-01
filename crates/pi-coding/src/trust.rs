//! Versioned project trust storage and fail-closed trust resolution.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::resources::{CONFIG_DIR_NAME, agent_dir_path};
use crate::settings::{DefaultProjectTrust, atomic_write};

pub const TRUST_STORE_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustDecision {
    Trusted,
    Untrusted,
    #[default]
    Ask,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustResolution {
    pub decision: TrustDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_path: Option<PathBuf>,
    pub project_path: PathBuf,
}

impl TrustResolution {
    #[must_use]
    pub const fn allows_project_resources(&self, headless: bool) -> bool {
        match self.decision {
            TrustDecision::Trusted => true,
            TrustDecision::Untrusted => false,
            TrustDecision::Ask => !headless && false,
        }
    }

    #[must_use]
    pub const fn is_trusted(&self) -> bool {
        matches!(self.decision, TrustDecision::Trusted)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TrustDocument {
    version: u32,
    decisions: BTreeMap<PathBuf, bool>,
}

impl Default for TrustDocument {
    fn default() -> Self {
        Self {
            version: TRUST_STORE_VERSION,
            decisions: BTreeMap::new(),
        }
    }
}

#[derive(Clone)]
pub struct TrustStore {
    path: PathBuf,
    write_lock: Arc<Mutex<()>>,
}

impl TrustStore {
    #[must_use]
    pub fn new(agent_dir: impl AsRef<Path>) -> Self {
        Self {
            path: agent_dir.as_ref().join("trust.json"),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    #[must_use]
    pub fn global() -> Self {
        Self::new(agent_dir_path())
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn resolve(&self, cwd: impl AsRef<Path>) -> Result<TrustResolution> {
        let project_path = canonical_path(cwd.as_ref())?;
        let document = self.read_document()?;
        let mut current = project_path.as_path();
        loop {
            if let Some(decision) = document.decisions.get(current) {
                return Ok(TrustResolution {
                    decision: if *decision {
                        TrustDecision::Trusted
                    } else {
                        TrustDecision::Untrusted
                    },
                    matched_path: Some(current.to_path_buf()),
                    project_path,
                });
            }
            let Some(parent) = current.parent() else {
                break;
            };
            if parent == current {
                break;
            }
            current = parent;
        }
        Ok(TrustResolution {
            decision: TrustDecision::Ask,
            matched_path: None,
            project_path,
        })
    }

    pub fn set(&self, path: impl AsRef<Path>, decision: TrustDecision) -> Result<()> {
        self.set_many([(path.as_ref().to_path_buf(), decision)])
    }

    pub fn set_many<I>(&self, updates: I) -> Result<()>
    where
        I: IntoIterator<Item = (PathBuf, TrustDecision)>,
    {
        let _guard = self.write_lock.lock();
        let mut document = self.read_document()?;
        for (path, decision) in updates {
            let path = canonical_path(&path)?;
            match decision {
                TrustDecision::Trusted => {
                    document.decisions.insert(path, true);
                }
                TrustDecision::Untrusted => {
                    document.decisions.insert(path, false);
                }
                TrustDecision::Ask => {
                    document.decisions.remove(&path);
                }
            }
        }
        let mut bytes = serde_json::to_vec_pretty(&document)
            .with_context(|| format!("serializing trust store {}", self.path.display()))?;
        bytes.push(b'\n');
        atomic_write(&self.path, &bytes)
            .with_context(|| format!("writing trust store {}", self.path.display()))
    }

    fn read_document(&self) -> Result<TrustDocument> {
        let content = match fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(TrustDocument::default());
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading trust store {}", self.path.display()));
            }
        };
        let document: TrustDocument = serde_json::from_str(&content)
            .with_context(|| format!("parsing trust store {}", self.path.display()))?;
        if document.version != TRUST_STORE_VERSION {
            bail!(
                "unsupported trust store version {} in {} (expected {})",
                document.version,
                self.path.display(),
                TRUST_STORE_VERSION
            );
        }
        Ok(document)
    }
}

/// Resolve the effective decision without persisting one-run overrides.
pub fn resolve_project_trust(
    store: &TrustStore,
    cwd: impl AsRef<Path>,
    one_run_override: Option<bool>,
    default: DefaultProjectTrust,
    headless: bool,
) -> Result<TrustResolution> {
    let project_path = canonical_path(cwd.as_ref())?;
    if let Some(trusted) = one_run_override {
        return Ok(TrustResolution {
            decision: if trusted {
                TrustDecision::Trusted
            } else {
                TrustDecision::Untrusted
            },
            matched_path: None,
            project_path,
        });
    }
    if !has_project_config_resources(&project_path) {
        return Ok(TrustResolution {
            decision: TrustDecision::Trusted,
            matched_path: None,
            project_path,
        });
    }
    let stored = store.resolve(&project_path)?;
    if stored.decision != TrustDecision::Ask {
        return Ok(stored);
    }
    let decision = match default {
        DefaultProjectTrust::Always => TrustDecision::Trusted,
        DefaultProjectTrust::Never => TrustDecision::Untrusted,
        DefaultProjectTrust::Ask if headless => TrustDecision::Untrusted,
        DefaultProjectTrust::Ask => TrustDecision::Ask,
    };
    Ok(TrustResolution { decision, ..stored })
}

/// Any entry under project `.pi` is trust-gated. This deliberately treats
/// future resource kinds as gated without needing to update a hard-coded list.
#[must_use]
pub fn has_project_config_resources(cwd: impl AsRef<Path>) -> bool {
    let path = cwd.as_ref().join(CONFIG_DIR_NAME);
    fs::read_dir(path)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

pub fn canonical_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("getting current directory for canonical path")?
            .join(path)
    };
    if let Ok(canonical) = absolute.canonicalize() {
        return Ok(canonical);
    }
    let mut missing = Vec::new();
    let mut existing = absolute.as_path();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            return Ok(lexical_normalize(&absolute));
        };
        missing.push(name.to_os_string());
        let Some(parent) = existing.parent() else {
            return Ok(lexical_normalize(&absolute));
        };
        existing = parent;
    }
    let mut resolved = existing
        .canonicalize()
        .with_context(|| format!("canonicalizing path {}", existing.display()))?;
    for component in missing.iter().rev() {
        resolved.push(component);
    }
    Ok(lexical_normalize(&resolved))
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}
