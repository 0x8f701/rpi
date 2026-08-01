use std::collections::{HashMap, HashSet};

use pi_ai::Model;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ScopedModelSelection {
    All,
    Explicit(Vec<String>),
}

pub(crate) struct ScopedModelSelector {
    models: Vec<Model>,
    models_by_id: HashMap<String, Model>,
    selection: ScopedModelSelection,
    query: String,
    selected: usize,
    dirty: bool,
}

impl ScopedModelSelector {
    pub(crate) fn new(models: Vec<Model>, scoped: Option<Vec<Model>>) -> Self {
        let mut models_by_id = HashMap::new();
        let mut all_models = Vec::new();
        for model in models {
            let id = model_id(&model);
            if models_by_id.insert(id, model.clone()).is_none() {
                all_models.push(model);
            }
        }
        let selection = scoped.map_or(ScopedModelSelection::All, |models| {
            ScopedModelSelection::Explicit(dedup_ids(models.iter().map(model_id)))
        });
        Self {
            models: all_models,
            models_by_id,
            selection,
            query: String::new(),
            selected: 0,
            dirty: false,
        }
    }

    pub(crate) fn query(&self) -> &str {
        &self.query
    }
    pub(crate) fn selected(&self) -> usize {
        self.selected
    }
    pub(crate) fn dirty(&self) -> bool {
        self.dirty
    }
    pub(crate) fn selection(&self) -> &ScopedModelSelection {
        &self.selection
    }

    pub(crate) fn visible_ids(&self) -> Vec<String> {
        let ordered = match &self.selection {
            ScopedModelSelection::All => self.models.iter().map(model_id).collect(),
            ScopedModelSelection::Explicit(enabled) => {
                let enabled_set = enabled.iter().collect::<HashSet<_>>();
                let mut ids = enabled.clone();
                ids.extend(
                    self.models
                        .iter()
                        .map(model_id)
                        .filter(|id| !enabled_set.contains(&id)),
                );
                ids
            }
        };
        ordered
            .into_iter()
            .filter(|id| {
                let search = self.models_by_id.get(id).map_or_else(
                    || id.clone(),
                    |model| format!("{} {} {}", model.provider, model.id, model.name),
                );
                fuzzy_match(&search, &self.query)
            })
            .collect()
    }

    pub(crate) fn selected_id(&self) -> Option<String> {
        self.visible_ids().get(self.selected).cloned()
    }

    pub(crate) fn selected_model(&self) -> Option<&Model> {
        let id = self.selected_id()?;
        self.models_by_id.get(&id)
    }

    pub(crate) fn is_enabled(&self, id: &str) -> bool {
        match &self.selection {
            ScopedModelSelection::All => self.models_by_id.contains_key(id),
            ScopedModelSelection::Explicit(ids) => ids.iter().any(|candidate| candidate == id),
        }
    }

    pub(crate) fn enabled_count(&self) -> usize {
        match &self.selection {
            ScopedModelSelection::All => self.models.len(),
            ScopedModelSelection::Explicit(ids) => ids
                .iter()
                .filter(|id| self.models_by_id.contains_key(*id))
                .count(),
        }
    }

    pub(crate) fn unavailable_count(&self) -> usize {
        match &self.selection {
            ScopedModelSelection::All => 0,
            ScopedModelSelection::Explicit(ids) => ids
                .iter()
                .filter(|id| !self.models_by_id.contains_key(*id))
                .count(),
        }
    }

    pub(crate) fn push_query(&mut self, character: char) {
        self.query.push(character);
        self.selected = 0;
    }
    pub(crate) fn pop_query(&mut self) {
        self.query.pop();
        self.selected = 0;
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        let count = self.visible_ids().len();
        if count == 0 {
            self.selected = 0;
            return;
        }
        self.selected = self.selected.saturating_add_signed(delta).min(count - 1);
    }

    pub(crate) fn toggle_selected(&mut self) {
        let Some(id) = self.selected_id() else {
            return;
        };
        match &mut self.selection {
            ScopedModelSelection::All => self.selection = ScopedModelSelection::Explicit(vec![id]),
            ScopedModelSelection::Explicit(ids) => {
                if let Some(index) = ids.iter().position(|candidate| candidate == &id) {
                    ids.remove(index);
                } else {
                    ids.push(id);
                }
            }
        }
        self.mark_changed();
    }

    pub(crate) fn enable_all(&mut self) {
        let targets = self.target_ids();
        match &mut self.selection {
            ScopedModelSelection::All => return,
            ScopedModelSelection::Explicit(ids) => {
                for id in targets {
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
                let catalog = self.models.iter().map(model_id).collect::<HashSet<_>>();
                if ids.len() == catalog.len() && ids.iter().all(|id| catalog.contains(id)) {
                    self.selection = ScopedModelSelection::All;
                }
            }
        }
        self.mark_changed();
    }

    pub(crate) fn clear_all(&mut self) {
        let targets = self.target_ids().into_iter().collect::<HashSet<_>>();
        match &mut self.selection {
            ScopedModelSelection::All => {
                self.selection = ScopedModelSelection::Explicit(
                    self.models
                        .iter()
                        .map(model_id)
                        .filter(|id| !targets.contains(id))
                        .collect(),
                );
            }
            ScopedModelSelection::Explicit(ids) => ids.retain(|id| !targets.contains(id)),
        }
        self.mark_changed();
    }

    pub(crate) fn toggle_provider(&mut self) {
        let Some(provider) = self.selected_model().map(|model| model.provider.clone()) else {
            return;
        };
        let provider_ids = self
            .models
            .iter()
            .filter(|model| model.provider == provider)
            .map(model_id)
            .collect::<Vec<_>>();
        let all_enabled = provider_ids.iter().all(|id| self.is_enabled(id));
        match &mut self.selection {
            ScopedModelSelection::All if all_enabled => {
                let provider = provider.as_str();
                self.selection = ScopedModelSelection::Explicit(
                    self.models
                        .iter()
                        .filter(|model| model.provider != provider)
                        .map(model_id)
                        .collect(),
                );
            }
            ScopedModelSelection::All => return,
            ScopedModelSelection::Explicit(ids) if all_enabled => {
                ids.retain(|id| !provider_ids.contains(id))
            }
            ScopedModelSelection::Explicit(ids) => {
                for id in provider_ids {
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
            }
        }
        self.mark_changed();
    }

    pub(crate) fn reorder_selected(&mut self, delta: isize) {
        let Some(id) = self.selected_id() else {
            return;
        };
        let ScopedModelSelection::Explicit(ids) = &mut self.selection else {
            return;
        };
        let Some(index) = ids.iter().position(|candidate| candidate == &id) else {
            return;
        };
        let Some(next) = index
            .checked_add_signed(delta)
            .filter(|next| *next < ids.len())
        else {
            return;
        };
        ids.swap(index, next);
        self.selected = self.selected.saturating_add_signed(delta);
        self.mark_changed();
    }

    pub(crate) fn scoped_models(&self) -> Option<Vec<Model>> {
        match &self.selection {
            ScopedModelSelection::All => None,
            ScopedModelSelection::Explicit(ids) => Some(
                ids.iter()
                    .filter_map(|id| self.models_by_id.get(id).cloned())
                    .collect(),
            ),
        }
    }

    pub(crate) fn persisted_patterns(&self) -> Option<Vec<String>> {
        match &self.selection {
            ScopedModelSelection::All => None,
            ScopedModelSelection::Explicit(ids) => Some(ids.clone()),
        }
    }

    pub(crate) fn mark_saved(&mut self) {
        self.dirty = false;
    }

    fn target_ids(&self) -> Vec<String> {
        if self.query.is_empty() {
            self.models.iter().map(model_id).collect()
        } else {
            self.visible_ids()
        }
    }

    fn mark_changed(&mut self) {
        self.dirty = true;
        self.selected = self
            .selected
            .min(self.visible_ids().len().saturating_sub(1));
    }
}

pub(crate) fn model_id(model: &Model) -> String {
    format!("{}/{}", model.provider, model.id)
}

fn dedup_ids(ids: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    ids.into_iter()
        .filter(|id| seen.insert(id.clone()))
        .collect()
}

fn fuzzy_match(candidate: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let mut candidate = candidate.chars().flat_map(char::to_lowercase);
    query
        .chars()
        .flat_map(char::to_lowercase)
        .all(|expected| candidate.by_ref().any(|actual| actual == expected))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(provider: &str, id: &str) -> Model {
        Model {
            provider: provider.to_owned(),
            id: id.to_owned(),
            name: id.to_owned(),
            ..Model::default()
        }
    }

    #[test]
    fn enable_clear_toggle_provider_and_filter_preserve_explicit_semantics() {
        let models = vec![model("a", "one"), model("a", "two"), model("b", "three")];
        let mut selector = ScopedModelSelector::new(models, None);
        selector.clear_all();
        assert_eq!(
            selector.selection(),
            &ScopedModelSelection::Explicit(Vec::new())
        );
        selector.enable_all();
        assert_eq!(selector.selection(), &ScopedModelSelection::All);
        selector.clear_all();
        selector.toggle_selected();
        assert_eq!(
            selector.selection(),
            &ScopedModelSelection::Explicit(vec!["a/one".to_owned()])
        );
        selector.toggle_provider();
        assert_eq!(
            selector.selection(),
            &ScopedModelSelection::Explicit(vec!["a/one".to_owned(), "a/two".to_owned()])
        );
        selector.push_query('t');
        selector.clear_all();
        assert!(selector.persisted_patterns().is_some());
    }

    #[test]
    fn reorder_and_persist_keep_cycle_order() {
        let models = vec![model("p", "one"), model("p", "two"), model("p", "three")];
        let mut selector = ScopedModelSelector::new(models.clone(), Some(models));
        selector.reorder_selected(1);
        assert_eq!(
            selector.persisted_patterns(),
            Some(vec![
                "p/two".to_owned(),
                "p/one".to_owned(),
                "p/three".to_owned()
            ])
        );
        assert_eq!(
            selector
                .scoped_models()
                .unwrap()
                .iter()
                .map(model_id)
                .collect::<Vec<_>>(),
            vec!["p/two", "p/one", "p/three"]
        );
        assert!(selector.dirty());
        selector.mark_saved();
        assert!(!selector.dirty());
    }
}
