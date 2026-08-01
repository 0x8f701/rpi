use crate::Model;
use anyhow::{Result, anyhow};
use std::{
    collections::{BTreeMap, HashSet},
    sync::{LazyLock, Mutex, Once, RwLock},
};

static CATALOG: LazyLock<RwLock<BTreeMap<String, BTreeMap<String, Model>>>> =
    LazyLock::new(|| RwLock::new(BTreeMap::new()));
static DYNAMIC_OWNERS: LazyLock<Mutex<BTreeMap<String, Vec<(String, String)>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));
static BUILTIN_MODELS: Once = Once::new();
const MODELS_CATALOG: &str = include_str!("models_catalog.json");

/// Parsed embedded built-in catalog, used to restore built-in models that a
/// custom `models.json` overrode. Parsed once on first use.
static BUILTIN_LOOKUP: LazyLock<BTreeMap<String, BTreeMap<String, Model>>> = LazyLock::new(|| {
    serde_json::from_str(MODELS_CATALOG).expect("embedded model catalog is valid")
});

fn catalog() -> &'static RwLock<BTreeMap<String, BTreeMap<String, Model>>> {
    &CATALOG
}

pub fn register_model(model: Model) {
    catalog()
        .write()
        .expect("catalog lock")
        .entry(model.provider.clone())
        .or_default()
        .insert(model.id.clone(), model);
}

pub fn load_builtin_models() {
    BUILTIN_MODELS.call_once(|| {
        let providers: BTreeMap<String, BTreeMap<String, Model>> =
            serde_json::from_str(MODELS_CATALOG).expect("embedded model catalog is valid");
        let mut target = catalog().write().expect("catalog lock");
        for (provider, models) in providers {
            target.entry(provider).or_default().extend(models);
        }
    });
    crate::providers::register_builtins();
}

pub fn get_providers() -> Vec<String> {
    load_builtin_models();
    catalog()
        .read()
        .expect("catalog lock")
        .keys()
        .cloned()
        .collect()
}
pub fn get_models(provider: &str) -> Vec<Model> {
    load_builtin_models();
    catalog()
        .read()
        .expect("catalog lock")
        .get(provider)
        .map(|m| m.values().cloned().collect())
        .unwrap_or_default()
}
pub fn get_model(provider: &str, id: &str) -> Option<Model> {
    load_builtin_models();
    catalog()
        .read()
        .expect("catalog lock")
        .get(provider)
        .and_then(|m| m.get(id))
        .cloned()
}
/// Apply provider-owned OAuth model entitlements without changing the static
/// catalog when credential metadata is absent.
#[must_use]
pub fn filter_models_for_credential(
    provider: &str,
    models: Vec<Model>,
    available_model_ids: Option<&[String]>,
) -> Vec<Model> {
    if provider != "github-copilot" {
        return models;
    }
    let Some(available_model_ids) = available_model_ids else {
        return models;
    };
    let available = available_model_ids
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    models
        .into_iter()
        .filter(|model| model_is_available_for_credential(model, Some(available_model_ids)))
        .collect()
}

/// Whether one model remains visible for the resolved credential.
#[must_use]
pub fn model_is_available_for_credential(
    model: &Model,
    available_model_ids: Option<&[String]>,
) -> bool {
    model.provider != "github-copilot"
        || available_model_ids.is_none_or(|ids| ids.iter().any(|id| id == &model.id))
}
pub fn clear_models() {
    let mut owners = DYNAMIC_OWNERS.lock().expect("dynamic owner lock");
    catalog().write().expect("catalog lock").clear();
    owners.clear();
}

/// The built-in (embedded catalog) model for `provider`/`id`, if any.
pub fn builtin_model(provider: &str, id: &str) -> Option<Model> {
    BUILTIN_LOOKUP
        .get(provider)
        .and_then(|m| m.get(id))
        .cloned()
}

/// All built-in (embedded catalog) models for `provider`.
pub fn builtin_models(provider: &str) -> Vec<Model> {
    BUILTIN_LOOKUP
        .get(provider)
        .map(|m| m.values().cloned().collect())
        .unwrap_or_default()
}

/// Whether `provider` has any built-in (embedded catalog) models.
pub fn is_builtin_provider(provider: &str) -> bool {
    BUILTIN_LOOKUP.get(provider).is_some_and(|m| !m.is_empty())
}

/// Atomically replaces one centrally tracked dynamic model set.
///
/// `owner` identifies the authoritative source (for Radius, one provider id).
/// The full incoming set is validated before mutation. Dynamic catalogs may
/// not shadow embedded built-ins or keys already installed by another source.
/// The ownership lock and catalog write lock cover the complete replacement,
/// so concurrent instances of the same source replace one exact set rather
/// than accumulating stale keys, and readers see either the old or new set.
pub fn replace_dynamic_models(owner: &str, new_models: Vec<Model>) -> Result<Vec<(String, String)>> {
    if owner.trim().is_empty() {
        return Err(anyhow!("dynamic model owner must not be empty"));
    }
    let mut keys = HashSet::with_capacity(new_models.len());
    for model in &new_models {
        if model.provider.trim().is_empty() || model.id.trim().is_empty() {
            return Err(anyhow!("dynamic model provider and id must not be empty"));
        }
        let key = (model.provider.clone(), model.id.clone());
        if BUILTIN_LOOKUP
            .get(&model.provider)
            .is_some_and(|models| models.contains_key(&model.id))
        {
            return Err(anyhow!(
                "dynamic model may not shadow builtin {}/{}",
                model.provider,
                model.id
            ));
        }
        if !keys.insert(key) {
            return Err(anyhow!(
                "duplicate dynamic model key {}/{}",
                model.provider,
                model.id
            ));
        }
    }

    let mut owners = DYNAMIC_OWNERS.lock().expect("dynamic owner lock");
    let previous = owners.get(owner).cloned().unwrap_or_default();
    let previous_set = previous.iter().cloned().collect::<HashSet<_>>();
    let mut cat = catalog().write().expect("catalog lock");
    for (provider, id) in &keys {
        if !previous_set.contains(&(provider.clone(), id.clone()))
            && cat.get(provider).is_some_and(|models| models.contains_key(id))
        {
            return Err(anyhow!(
                "dynamic model key {provider}/{id} is already installed by another source"
            ));
        }
    }
    let applied = keys.into_iter().collect::<Vec<_>>();
    replace_models_locked(&mut cat, &previous, new_models);
    if applied.is_empty() {
        owners.remove(owner);
    } else {
        owners.insert(owner.to_owned(), applied.clone());
    }
    Ok(applied)
}

fn replace_models_locked(
    cat: &mut BTreeMap<String, BTreeMap<String, Model>>,
    previous: &[(String, String)],
    new_models: Vec<Model>,
) {
    for (provider, id) in previous {
        match BUILTIN_LOOKUP
            .get(provider)
            .and_then(|models| models.get(id))
        {
            Some(builtin) => {
                cat.entry(provider.clone())
                    .or_default()
                    .insert(id.clone(), builtin.clone());
            }
            None => {
                if let Some(models) = cat.get_mut(provider) {
                    models.remove(id);
                }
                if cat.get(provider).is_some_and(BTreeMap::is_empty)
                    && !BUILTIN_LOOKUP.contains_key(provider)
                {
                    cat.remove(provider);
                }
            }
        }
    }
    for model in new_models {
        cat.entry(model.provider.clone())
            .or_default()
            .insert(model.id.clone(), model);
    }
}

/// Atomically (one catalog write) revert the loader-owned `previous` model ids
/// then upsert `new_models`. For each previous `(provider, id)`: if it is a
/// built-in, restore it from the embedded catalog; otherwise remove it (and
/// drop now-empty non-builtin provider entries). Then insert every model in
/// `new_models`. Used by the custom `models.json` loader for atomic reload.
pub fn replace_registered_models(previous: &[(String, String)], new_models: Vec<Model>) {
    // The custom models.json loader may re-register overridden built-ins (e.g.
    // a provider-level `baseUrl` override re-publishes every built-in model of
    // that provider). It validates provider/id itself, so bypass the
    // `replace_dynamic_models` no-shadow rule (which is for Radius/dynamic
    // catalogs) and apply the atomic revert+upsert directly under one lock.
    let mut cat = catalog().write().expect("catalog lock");
    replace_models_locked(&mut cat, previous, new_models);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_loads_representative_providers() {
        assert!(get_model("anthropic", "claude-sonnet-4-5").is_some());
        assert!(!get_models("openai").is_empty());
        assert!(!get_models("google").is_empty());
        assert!(!get_models("qwen").is_empty() || !get_models("qwen-token-plan").is_empty());
    }

    #[test]
    fn copilot_filter_uses_oauth_entitlements_only_when_present() {
        let models = vec![
            Model {
                id: "allowed".into(),
                provider: "github-copilot".into(),
                ..Model::default()
            },
            Model {
                id: "hidden".into(),
                provider: "github-copilot".into(),
                ..Model::default()
            },
        ];
        assert_eq!(
            filter_models_for_credential("github-copilot", models.clone(), None),
            models
        );
        assert_eq!(
            filter_models_for_credential(
                "github-copilot",
                models.clone(),
                Some(&["allowed".to_owned()]),
            ),
            vec![models[0].clone()]
        );
        assert_eq!(
            filter_models_for_credential("openai", models.clone(), Some(&[])),
            models
        );
        assert!(model_is_available_for_credential(&models[0], None));
        assert!(model_is_available_for_credential(
            &models[0],
            Some(&["allowed".to_owned()])
        ));
        assert!(!model_is_available_for_credential(
            &models[1],
            Some(&["allowed".to_owned()])
        ));
    }

    #[test]
    fn registered_replacement_can_override_and_restore_builtin() {
        load_builtin_models();
        let mut overridden = builtin_model("deepseek", "deepseek-v4-flash").expect("builtin model");
        let original_url = overridden.base_url.clone();
        overridden.base_url = "https://override.test".into();
        let key = (overridden.provider.clone(), overridden.id.clone());

        replace_registered_models(&[], vec![overridden]);
        assert_eq!(
            get_model(&key.0, &key.1)
                .expect("overridden model")
                .base_url,
            "https://override.test"
        );

        replace_registered_models(&[key.clone()], Vec::new());
        assert_eq!(
            get_model(&key.0, &key.1).expect("restored model").base_url,
            original_url
        );
    }

    #[test]
    fn dynamic_replacement_rejects_duplicates_before_mutation() {
        let provider = "catalog-duplicate-fixture";
        let duplicate = Model {
            id: "new".into(),
            provider: provider.into(),
            ..Model::default()
        };
        let error = replace_dynamic_models(
            "catalog-duplicate-owner",
            vec![duplicate.clone(), duplicate],
        )
        .expect_err("duplicate must fail");
        assert!(error.to_string().contains("duplicate dynamic model key"));
        assert!(get_model(provider, "new").is_none());
    }

    #[test]
    fn dynamic_replacement_rejects_builtin_collisions_before_mutation() {
        let provider = "catalog-builtin-collision-fixture";
        let mut colliding = builtin_model("anthropic", "claude-sonnet-4-5").expect("builtin");
        colliding.base_url = "https://dynamic.invalid".into();
        let error = replace_dynamic_models("catalog-builtin-collision-owner", vec![colliding])
            .expect_err("builtin shadow must fail");
        assert!(error.to_string().contains("may not shadow builtin"));
        assert_ne!(
            get_model("anthropic", "claude-sonnet-4-5")
                .expect("builtin remains")
                .base_url,
            "https://dynamic.invalid"
        );
    }

    #[test]
    fn concurrent_readers_never_observe_partial_dynamic_sets() {
        use std::sync::{
            Arc, Barrier,
            atomic::{AtomicBool, Ordering},
        };

        let provider = "catalog-atomic-fixture";
        let make_models = |prefix: &str| {
            (0..32)
                .map(|index| Model {
                    id: format!("{prefix}-{index}"),
                    provider: provider.into(),
                    ..Model::default()
                })
                .collect::<Vec<_>>()
        };
        let old = make_models("old");
        let new = make_models("new");
        let owner = "catalog-atomic-owner";
        let previous = replace_dynamic_models(owner, old).expect("install old set");
        let start = Arc::new(Barrier::new(2));
        let done = Arc::new(AtomicBool::new(false));
        let reader_start = start.clone();
        let reader_done = done.clone();
        let reader = std::thread::spawn(move || {
            reader_start.wait();
            while !reader_done.load(Ordering::Acquire) {
                let models = get_models(provider);
                let old_count = models
                    .iter()
                    .filter(|model| model.id.starts_with("old-"))
                    .count();
                let new_count = models
                    .iter()
                    .filter(|model| model.id.starts_with("new-"))
                    .count();
                assert!(
                    (old_count == 32 && new_count == 0) || (old_count == 0 && new_count == 32),
                    "observed partial replacement: old={old_count} new={new_count}"
                );
            }
        });
        start.wait();
        let applied = replace_dynamic_models(owner, new).expect("replace atomically");
        assert_eq!(applied.len(), 32);
        done.store(true, Ordering::Release);
        reader.join().expect("reader");
        let models = get_models(provider);
        assert_eq!(
            models
                .iter()
                .filter(|model| model.id.starts_with("new-"))
                .count(),
            32
        );
    }

    #[test]
    fn dynamic_owners_cannot_claim_another_sources_key() {
        let provider = "catalog-owner-collision-fixture";
        let model = Model {
            id: "shared".into(),
            provider: provider.into(),
            ..Model::default()
        };
        replace_dynamic_models("catalog-owner-a", vec![model.clone()])
            .expect("owner A install");
        let error = replace_dynamic_models("catalog-owner-b", vec![model])
            .expect_err("owner B must not claim owner A key");
        assert!(error.to_string().contains("already installed by another source"));
        assert!(get_model(provider, "shared").is_some());
    }
}
