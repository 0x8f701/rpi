//! Versioned project trust storage and fail-closed trust resolution.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use serde::de::{MapAccess, Visitor};
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

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TrustDocument {
    version: u32,
    decisions: BTreeMap<PathBuf, bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CanonicalTrustDocument {
    version: u32,
    #[serde(deserialize_with = "deserialize_canonical_decisions")]
    decisions: Vec<(String, bool)>,
}

fn deserialize_canonical_decisions<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<(String, bool)>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct DecisionsVisitor;

    impl<'de> Visitor<'de> for DecisionsVisitor {
        type Value = Vec<(String, bool)>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an object mapping canonical paths to boolean decisions")
        }

        fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut decisions = Vec::new();
            while let Some(entry) = map.next_entry()? {
                decisions.push(entry);
            }
            Ok(decisions)
        }
    }

    deserializer.deserialize_map(DecisionsVisitor)
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
        // A numeric `version` declares the canonical envelope. Other values are
        // ordinary legacy path decisions and must follow the legacy grammar.
        let value: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("parsing trust store {}", self.path.display()))?;
        if matches!(value.get("version"), Some(serde_json::Value::Number(_))) {
            let document: CanonicalTrustDocument = serde_json::from_str(&content)
                .with_context(|| format!("parsing trust store {}", self.path.display()))?;
            if document.version != TRUST_STORE_VERSION {
                bail!(
                    "unsupported trust store version {} in {} (expected {})",
                    document.version,
                    self.path.display(),
                    TRUST_STORE_VERSION
                );
            }
            parse_canonical_trust(document, &self.path)
        } else {
            parse_legacy_trust(value, &self.path)
        }
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

fn parse_canonical_trust(
    document: CanonicalTrustDocument,
    path: &Path,
) -> Result<TrustDocument> {
    let mut decisions = BTreeMap::new();
    let mut source_keys = BTreeMap::new();
    for (key, decision) in document.decisions {
        let key_path = Path::new(&key);
        if !key_path.is_absolute() {
            bail!(
                "invalid canonical trust path {} in {}: path must be absolute",
                key,
                path.display(),
            );
        }
        let canonical = canonical_path(key_path).with_context(|| {
            format!("canonicalizing trust path {} in {}", key, path.display())
        })?;
        if let Some(previous) = source_keys.insert(canonical.clone(), key.clone()) {
            bail!(
                "duplicate canonical trust path {} from keys {} and {} in {}",
                canonical.display(),
                previous,
                key,
                path.display(),
            );
        }
        if canonical.to_str() != Some(key.as_str()) {
            bail!(
                "invalid canonical trust path {} in {}: expected {}",
                key,
                path.display(),
                canonical.display(),
            );
        }
        decisions.insert(canonical, decision);
    }
    Ok(TrustDocument {
        version: document.version,
        decisions,
    })
}

/// Parse the upstream legacy flat trust map `{"<path>": true|false|null}`.
///
/// `true`/`false` become persisted decisions; `null` carries no persisted
/// decision (the path resolves to `Ask`). Keys are canonicalized before use so
/// legacy aliases cannot bypass a more specific decision. Any other value, or
/// a non-object top level, is rejected so corrupted files still fail closed.
fn parse_legacy_trust(value: serde_json::Value, path: &Path) -> Result<TrustDocument> {
    let map = match value {
        serde_json::Value::Object(map) => map,
        other => bail!(
            "invalid trust store {}: expected an object, got {}",
            path.display(),
            json_type_name(&other),
        ),
    };
    let mut decisions = BTreeMap::new();
    for (key, value) in map {
        let canonical = canonical_path(Path::new(&key)).with_context(|| {
            format!(
                "canonicalizing legacy trust path {} in {}",
                key,
                path.display(),
            )
        })?;
        let decision = match value {
            serde_json::Value::Bool(decision) => Some(decision),
            serde_json::Value::Null => None,
            other => bail!(
                "invalid trust value for {} in {}: expected true, false, or null, got {}",
                key,
                path.display(),
                json_type_name(&other),
            ),
        };
        if let Some(decision) = decision {
            if decisions
                .get(&canonical)
                .is_some_and(|existing| *existing != decision)
            {
                bail!(
                    "conflicting legacy trust decisions for canonical path {} in {}",
                    canonical.display(),
                    path.display(),
                );
            }
            decisions.insert(canonical, decision);
        }
    }
    Ok(TrustDocument {
        version: TRUST_STORE_VERSION,
        decisions,
    })
}

fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn store_in(dir: &TempDir) -> (TrustStore, PathBuf) {
        let agent = dir.path().join("agent");
        fs::create_dir_all(&agent).unwrap();
        let store = TrustStore::new(&agent);
        (store, agent.join("trust.json"))
    }

    fn project_in(dir: &TempDir, name: &str) -> PathBuf {
        let project = dir.path().join(name);
        fs::create_dir_all(&project).unwrap();
        canonical_path(&project).unwrap()
    }

    fn write_store(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    #[test]
    fn legacy_flat_map_resolves_true_and_false() {
        let dir = TempDir::new().unwrap();
        let (store, trust_path) = store_in(&dir);
        let trusted = project_in(&dir, "trusted");
        let untrusted = project_in(&dir, "untrusted");
        let unknown = project_in(&dir, "unknown");
        let body = format!(
            "{{\"{}\":true,\"{}\":false}}",
            trusted.display(),
            untrusted.display()
        );
        write_store(&trust_path, &body);

        assert_eq!(store.resolve(&trusted).unwrap().decision, TrustDecision::Trusted);
        assert_eq!(
            store.resolve(&untrusted).unwrap().decision,
            TrustDecision::Untrusted
        );
        // A path absent from the legacy map has no persisted decision.
        assert_eq!(store.resolve(&unknown).unwrap().decision, TrustDecision::Ask);
    }

    #[test]
    fn legacy_noncanonical_child_denial_overrides_trusted_ancestor() {
        let dir = TempDir::new().unwrap();
        let (store, trust_path) = store_in(&dir);
        let parent = project_in(&dir, "parent");
        let child = parent.join("child");
        fs::create_dir_all(&child).unwrap();
        let child = canonical_path(&child).unwrap();
        let child_alias = child.join("..").join("child");
        write_store(
            &trust_path,
            &format!(
                "{{\"{}\":true,\"{}\":false}}",
                parent.display(),
                child_alias.display(),
            ),
        );

        let resolution = store.resolve(&child).unwrap();
        assert_eq!(resolution.decision, TrustDecision::Untrusted);
        assert_eq!(resolution.matched_path.as_deref(), Some(child.as_path()));
    }

    #[test]
    fn legacy_null_resolves_ask_and_is_dropped_on_write() {
        let dir = TempDir::new().unwrap();
        let (store, trust_path) = store_in(&dir);
        let project = project_in(&dir, "project");
        write_store(
            &trust_path,
            &format!("{{\"{}\":null}}", project.display()),
        );

        // `null` carries no persisted decision -> Ask, not Untrusted.
        assert_eq!(store.resolve(&project).unwrap().decision, TrustDecision::Ask);

        // A `set` of an unrelated path rewrites the file in versioned shape and
        // the legacy null entry (no decision) must not survive the migration.
        let other = project_in(&dir, "other");
        store.set(&other, TrustDecision::Trusted).unwrap();

        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&trust_path).unwrap()).unwrap();
        assert_eq!(written["version"], serde_json::json!(1));
        let decisions = written["decisions"].as_object().unwrap();
        assert!(decisions.contains_key(&other.display().to_string()));
        assert!(!decisions.contains_key(&project.display().to_string()));
    }

    #[test]
    fn set_migrates_legacy_flat_map_to_versioned_shape() {
        let dir = TempDir::new().unwrap();
        let (store, trust_path) = store_in(&dir);
        let project = project_in(&dir, "project");
        write_store(
            &trust_path,
            &format!("{{\"{}\":true}}", project.display()),
        );

        // Touching the store with any decision rewrites the file canonically.
        store.set(&project, TrustDecision::Trusted).unwrap();

        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&trust_path).unwrap()).unwrap();
        assert_eq!(written["version"], serde_json::json!(1));
        assert!(written["decisions"].is_object());
        let project_key = project.display().to_string();
        assert_eq!(
            written["decisions"]
                .get(project_key.as_str())
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );

        // The migrated file still reads back through the canonical (strict) path.
        assert_eq!(
            store.resolve(&project).unwrap().decision,
            TrustDecision::Trusted
        );
    }

    #[test]
    fn legacy_migration_writes_only_canonical_keys() {
        let dir = TempDir::new().unwrap();
        let (store, trust_path) = store_in(&dir);
        let project = project_in(&dir, "project");
        let project_alias = project.join("..").join("project");
        write_store(
            &trust_path,
            &format!("{{\"{}\":true}}", project_alias.display()),
        );

        let other = project_in(&dir, "other");
        store.set(&other, TrustDecision::Trusted).unwrap();

        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&trust_path).unwrap()).unwrap();
        let decisions = written["decisions"].as_object().unwrap();
        assert_eq!(
            decisions.get(&project.display().to_string()),
            Some(&serde_json::json!(true)),
        );
        assert!(!decisions.contains_key(&project_alias.display().to_string()));
    }

    #[test]
    fn conflicting_legacy_aliases_error() {
        let dir = TempDir::new().unwrap();
        let (store, trust_path) = store_in(&dir);
        let project = project_in(&dir, "project");
        let project_alias = project.join("..").join("project");
        write_store(
            &trust_path,
            &format!(
                "{{\"{}\":true,\"{}\":false}}",
                project.display(),
                project_alias.display(),
            ),
        );

        assert!(store.resolve(&project).is_err());
    }

    #[test]
    fn agreeing_legacy_aliases_merge() {
        let dir = TempDir::new().unwrap();
        let (store, trust_path) = store_in(&dir);
        let project = project_in(&dir, "project");
        let project_alias = project.join("..").join("project");
        write_store(
            &trust_path,
            &format!(
                "{{\"{}\":true,\"{}\":true}}",
                project.display(),
                project_alias.display(),
            ),
        );

        assert_eq!(
            store.resolve(&project).unwrap().decision,
            TrustDecision::Trusted,
        );
    }

    #[test]
    fn legacy_version_path_decisions_parse_and_migrate() {
        for (value, expected) in [
            (serde_json::Value::Bool(true), Some(true)),
            (serde_json::Value::Bool(false), Some(false)),
            (serde_json::Value::Null, None),
        ] {
            let dir = TempDir::new().unwrap();
            let (store, trust_path) = store_in(&dir);
            let mut legacy = serde_json::Map::new();
            legacy.insert("version".to_owned(), value);
            write_store(
                &trust_path,
                &serde_json::Value::Object(legacy).to_string(),
            );

            let version_path = canonical_path(Path::new("version")).unwrap();
            let expected_decision = match expected {
                Some(true) => TrustDecision::Trusted,
                Some(false) => TrustDecision::Untrusted,
                None => TrustDecision::Ask,
            };
            assert_eq!(
                store.resolve(&version_path).unwrap().decision,
                expected_decision,
            );

            let other = project_in(&dir, "other");
            store.set(&other, TrustDecision::Trusted).unwrap();
            let written: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&trust_path).unwrap()).unwrap();
            assert_eq!(written["version"], serde_json::json!(1));
            let version_key = version_path.display().to_string();
            let migrated = written["decisions"]
                .get(version_key.as_str())
                .and_then(serde_json::Value::as_bool);
            assert_eq!(migrated, expected);
        }
    }

    #[test]
    fn canonical_versioned_document_still_reads() {
        let dir = TempDir::new().unwrap();
        let (store, trust_path) = store_in(&dir);
        let project = project_in(&dir, "project");
        let mut decisions = BTreeMap::new();
        decisions.insert(project.clone(), true);
        let body = serde_json::to_string(&TrustDocument {
            version: TRUST_STORE_VERSION,
            decisions,
        })
        .unwrap();
        write_store(&trust_path, &body);
        assert_eq!(
            store.resolve(&project).unwrap().decision,
            TrustDecision::Trusted
        );
    }

    #[test]
    fn canonical_noncanonical_and_relative_keys_error() {
        let dir = TempDir::new().unwrap();
        let (store, trust_path) = store_in(&dir);
        let project = project_in(&dir, "project");
        let noncanonical = project.join("..").join("project");

        for invalid in [noncanonical, PathBuf::from("relative/project")] {
            let mut decisions = BTreeMap::new();
            decisions.insert(invalid, false);
            let body = serde_json::to_string(&TrustDocument {
                version: TRUST_STORE_VERSION,
                decisions,
            })
            .unwrap();
            write_store(&trust_path, &body);
            assert!(store.resolve(&project).is_err());
        }
    }

    #[test]
    fn canonical_raw_aliases_error_before_decisions_can_collapse() {
        let dir = TempDir::new().unwrap();
        let (store, trust_path) = store_in(&dir);
        let parent = project_in(&dir, "parent");
        let child = parent.join("child");
        fs::create_dir_all(&child).unwrap();
        let child = canonical_path(&child).unwrap();
        let child_alias = format!("{}/.", child.display());
        let repeated_separator = child.display().to_string().replacen('/', "//", 1);

        let mut decisions = serde_json::Map::new();
        decisions.insert(parent.display().to_string(), serde_json::json!(true));
        decisions.insert(child.display().to_string(), serde_json::json!(false));
        decisions.insert(child_alias.clone(), serde_json::json!(true));
        write_store(
            &trust_path,
            &serde_json::json!({"version": 1, "decisions": decisions}).to_string(),
        );
        assert!(store.resolve(&child).is_err());

        for alias in [child_alias, repeated_separator] {
            let mut decisions = serde_json::Map::new();
            decisions.insert(alias, serde_json::json!(false));
            write_store(
                &trust_path,
                &serde_json::json!({"version": 1, "decisions": decisions}).to_string(),
            );
            assert!(store.resolve(&child).is_err());
        }
    }

    #[test]
    fn duplicate_canonical_key_errors() {
        let dir = TempDir::new().unwrap();
        let (store, trust_path) = store_in(&dir);
        let project = project_in(&dir, "project");
        write_store(
            &trust_path,
            &format!(
                "{{\"version\":1,\"decisions\":{{\"{}\":false,\"{}\":true}}}}",
                project.display(),
                project.display(),
            ),
        );

        assert!(store.resolve(&project).is_err());
    }

    #[test]
    fn malformed_legacy_value_errors() {
        let dir = TempDir::new().unwrap();
        let (store, trust_path) = store_in(&dir);
        let project = project_in(&dir, "project");
        // A number is not a valid legacy trust value.
        write_store(&trust_path, &format!("{{\"{}\":5}}", project.display()));
        assert!(store.resolve(&project).is_err());

        // A string is not a valid legacy trust value either.
        write_store(&trust_path, &format!("{{\"{}\":\"true\"}}", project.display()));
        assert!(store.resolve(&project).is_err());
    }

    #[test]
    fn malformed_top_level_errors() {
        let dir = TempDir::new().unwrap();
        let (store, trust_path) = store_in(&dir);
        // Not even valid JSON.
        write_store(&trust_path, "{not json");
        assert!(store.resolve(dir.path()).is_err());

        // Valid JSON but not an object.
        write_store(&trust_path, "[]");
        assert!(store.resolve(dir.path()).is_err());
    }

    #[test]
    fn unsupported_version_errors() {
        let dir = TempDir::new().unwrap();
        let (store, trust_path) = store_in(&dir);
        write_store(&trust_path, "{\"version\":2,\"decisions\":{}}");
        assert!(store.resolve(dir.path()).is_err());
    }

    #[test]
    fn numeric_version_requires_a_valid_canonical_envelope() {
        let dir = TempDir::new().unwrap();
        let (store, trust_path) = store_in(&dir);
        let project = project_in(&dir, "project");

        write_store(&trust_path, "{\"version\":1}");
        assert!(store.resolve(&project).is_err());

        write_store(
            &trust_path,
            &format!("{{\"version\":1,\"{}\":true}}", project.display()),
        );
        assert!(store.resolve(&project).is_err());
    }

    #[test]
    fn unknown_fields_in_versioned_doc_errors() {
        let dir = TempDir::new().unwrap();
        let (store, trust_path) = store_in(&dir);
        write_store(&trust_path, "{\"version\":1,\"decisions\":{},\"extra\":5}");
        assert!(store.resolve(dir.path()).is_err());
    }

    #[test]
    fn invalid_value_in_versioned_doc_errors() {
        let dir = TempDir::new().unwrap();
        let (store, trust_path) = store_in(&dir);
        let project = project_in(&dir, "project");
        write_store(
            &trust_path,
            &format!(
                "{{\"version\":1,\"decisions\":{{\"{}\":null}}}}",
                project.display()
            ),
        );
        assert!(store.resolve(&project).is_err());
    }
}
