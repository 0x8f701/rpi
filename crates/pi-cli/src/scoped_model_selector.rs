use std::collections::{HashMap, HashSet};

use pi_ai::Model;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ScopedModelSelection {
    All,
    Explicit(Vec<String>),
}

/// Which of the two omp-style panes has the cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModelColumn {
    /// Left pane: unique providers of the visible models (radio bullets).
    Providers,
    /// Right pane: models of the currently selected provider.
    Models,
}

pub(crate) struct ScopedModelSelector {
    models: Vec<Model>,
    models_by_id: HashMap<String, Model>,
    selection: ScopedModelSelection,
    query: String,
    column: ModelColumn,
    /// Index into [`ScopedModelSelector::providers`].
    provider_selected: usize,
    /// Index into the current provider's model list.
    model_selected: usize,
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
            column: ModelColumn::Providers,
            provider_selected: 0,
            model_selected: 0,
            dirty: false,
        }
    }

    pub(crate) fn query(&self) -> &str {
        &self.query
    }
    pub(crate) fn column(&self) -> ModelColumn {
        self.column
    }
    pub(crate) fn selected(&self) -> usize {
        match self.column {
            ModelColumn::Providers => self.provider_selected,
            ModelColumn::Models => self.model_selected,
        }
    }
    pub(crate) fn provider_selected(&self) -> usize {
        self.provider_selected
    }
    pub(crate) fn model_selected(&self) -> usize {
        self.model_selected
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

    /// Unique providers of the visible (query-filtered) models, in catalog order.
    pub(crate) fn providers(&self) -> Vec<&str> {
        let mut seen = HashSet::new();
        self.visible_ids()
            .iter()
            .filter_map(|id| {
                let provider = self.models_by_id.get(id)?.provider.as_str();
                seen.insert(provider).then_some(provider)
            })
            .collect()
    }

    /// Provider highlighted in the left pane.
    pub(crate) fn current_provider(&self) -> Option<&str> {
        self.providers().get(self.provider_selected).copied()
    }

    /// Visible model ids belonging to `provider`.
    pub(crate) fn models_for_provider(&self, provider: &str) -> Vec<String> {
        self.visible_ids()
            .into_iter()
            .filter(|id| {
                self.models_by_id
                    .get(id)
                    .is_some_and(|model| model.provider == provider)
            })
            .collect()
    }

    pub(crate) fn model_for_id(&self, id: &str) -> Option<&Model> {
        self.models_by_id.get(id)
    }

    /// Move the cursor into the right (models) pane.
    pub(crate) fn focus_models(&mut self) {
        self.column = ModelColumn::Models;
        self.clamp_selection();
    }

    /// Move the cursor back into the left (providers) pane.
    pub(crate) fn focus_providers(&mut self) {
        self.column = ModelColumn::Providers;
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        let provider_count = self.providers().len();
        self.provider_selected = self.provider_selected.min(provider_count.saturating_sub(1));
        let model_count = self
            .current_provider()
            .map_or(0, |provider| self.models_for_provider(provider).len());
        self.model_selected = self.model_selected.min(model_count.saturating_sub(1));
    }

    pub(crate) fn selected_id(&self) -> Option<String> {
        match self.column {
            ModelColumn::Providers => self
                .providers()
                .get(self.provider_selected)
                .and_then(|provider| self.models_for_provider(provider).first().cloned()),
            ModelColumn::Models => self.current_provider().and_then(|provider| {
                self.models_for_provider(provider)
                    .get(self.model_selected)
                    .cloned()
            }),
        }
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
        self.provider_selected = 0;
        self.model_selected = 0;
    }
    pub(crate) fn pop_query(&mut self) {
        self.query.pop();
        self.provider_selected = 0;
        self.model_selected = 0;
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        match self.column {
            ModelColumn::Providers => {
                let count = self.providers().len();
                if count == 0 {
                    self.provider_selected = 0;
                    return;
                }
                self.provider_selected =
                    (self.provider_selected as isize + delta).rem_euclid(count as isize) as usize;
            }
            ModelColumn::Models => {
                let count = self
                    .current_provider()
                    .map_or(0, |provider| self.models_for_provider(provider).len());
                if count == 0 {
                    self.model_selected = 0;
                    return;
                }
                self.model_selected =
                    (self.model_selected as isize + delta).rem_euclid(count as isize) as usize;
            }
        }
    }

    pub(crate) fn toggle_selected(&mut self) {
        match self.column {
            ModelColumn::Providers => self.toggle_provider(),
            ModelColumn::Models => {
                let Some(id) = self.selected_id() else {
                    return;
                };
                match &mut self.selection {
                    ScopedModelSelection::All => {
                        self.selection = ScopedModelSelection::Explicit(vec![id])
                    }
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
        }
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

    /// Toggles the selected provider, scoped to the models the active query
    /// currently shows — matching the per-model toggle, so toggling a
    /// provider never flips models hidden by the query filter.
    pub(crate) fn toggle_provider(&mut self) {
        let Some(provider) = self.selected_model().map(|model| model.provider.clone()) else {
            return;
        };
        let provider_ids = self.models_for_provider(&provider);
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
        if self.column != ModelColumn::Models {
            return;
        }
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
        self.model_selected = self.model_selected.saturating_add_signed(delta);
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
        self.clamp_selection();
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
        selector.focus_models();
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
        selector.focus_models();
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

    #[test]
    fn move_selection_wraps_around_visible_models() {
        let models = vec![model("a", "one"), model("a", "two"), model("b", "three")];
        let mut selector = ScopedModelSelector::new(models, None);
        // The providers column wraps around the provider list.
        selector.move_selection(3);
        assert_eq!(selector.selected(), 1);
        assert_eq!(selector.current_provider(), Some("b"));
        selector.move_selection(1);
        assert_eq!(selector.selected(), 0);
        assert_eq!(selector.current_provider(), Some("a"));
        // Up from the first provider wraps to the last.
        selector.move_selection(-1);
        assert_eq!(selector.selected(), 1);
        assert_eq!(selector.current_provider(), Some("b"));
        // A single-provider list stays put in both directions.
        let mut single = ScopedModelSelector::new(vec![model("solo", "only")], None);
        single.move_selection(5);
        assert_eq!(single.selected(), 0);
        single.move_selection(-5);
        assert_eq!(single.selected(), 0);
    }

    #[test]
    fn move_selection_wraps_within_filtered_visible_models() {
        let models = vec![model("a", "one"), model("a", "two"), model("b", "three")];
        let mut selector = ScopedModelSelector::new(models, None);
        selector.push_query('a');
        assert_eq!(selector.visible_ids(), vec!["a/one", "a/two"]);
        assert_eq!(selector.providers(), vec!["a"]);
        selector.focus_models();
        assert_eq!(selector.models_for_provider("a"), vec!["a/one", "a/two"]);
        selector.move_selection(1);
        assert_eq!(selector.selected_id(), Some("a/two".to_owned()));
        // Down past the end of the filtered list wraps back to its first row.
        selector.move_selection(1);
        assert_eq!(selector.selected_id(), Some("a/one".to_owned()));
        // Up from the filtered list's first row wraps to its last row.
        selector.move_selection(-1);
        assert_eq!(selector.selected_id(), Some("a/two".to_owned()));
    }

    #[test]
    fn two_column_navigation_switches_between_providers_and_models() {
        let models = vec![model("a", "one"), model("a", "two"), model("b", "three")];
        let mut selector = ScopedModelSelector::new(models, None);
        assert_eq!(selector.column(), ModelColumn::Providers);
        assert_eq!(selector.providers(), vec!["a", "b"]);
        assert_eq!(selector.current_provider(), Some("a"));
        assert_eq!(selector.selected_id(), Some("a/one".to_owned()));
        selector.move_selection(1);
        assert_eq!(selector.current_provider(), Some("b"));
        assert_eq!(selector.selected_id(), Some("b/three".to_owned()));
        // → enters the model column scoped to the selected provider.
        selector.focus_models();
        assert_eq!(selector.column(), ModelColumn::Models);
        assert_eq!(selector.selected(), 0);
        assert_eq!(selector.selected_id(), Some("b/three".to_owned()));
        // ← returns to the provider column.
        selector.focus_providers();
        assert_eq!(selector.column(), ModelColumn::Providers);
        assert_eq!(selector.selected(), 1);
    }

    #[test]
    fn model_column_moves_within_current_provider_and_wraps() {
        let models = vec![
            model("a", "one"),
            model("a", "two"),
            model("b", "three"),
            model("b", "four"),
        ];
        let mut selector = ScopedModelSelector::new(models, None);
        selector.focus_models();
        selector.move_selection(1);
        assert_eq!(selector.selected_id(), Some("a/two".to_owned()));
        // Switching providers from the providers column re-enters at the
        // current model cursor, clamped to the new provider's list.
        selector.focus_providers();
        selector.move_selection(1);
        assert_eq!(selector.current_provider(), Some("b"));
        selector.focus_models();
        assert_eq!(selector.selected(), 1);
        assert_eq!(selector.selected_id(), Some("b/four".to_owned()));
        selector.move_selection(1);
        assert_eq!(selector.selected_id(), Some("b/three".to_owned()));
        // Down past the end wraps to the provider's first model.
        selector.move_selection(1);
        assert_eq!(selector.selected_id(), Some("b/four".to_owned()));
        // Returning to provider a lands on the same cursor position.
        selector.focus_providers();
        selector.move_selection(-1);
        selector.focus_models();
        assert_eq!(selector.selected_id(), Some("a/two".to_owned()));
    }

    #[test]
    fn providers_radio_list_follows_query_filter() {
        let models = vec![
            model("alpha", "one"),
            model("alpha", "two"),
            model("beta", "three"),
        ];
        let mut selector = ScopedModelSelector::new(models, None);
        selector.push_query('b');
        assert_eq!(selector.providers(), vec!["beta"]);
        assert_eq!(selector.models_for_provider("beta"), vec!["beta/three"]);
        selector.pop_query();
        assert_eq!(selector.providers(), vec!["alpha", "beta"]);
    }

    #[test]
    fn enter_toggles_provider_column_provider_and_models_column_model() {
        let models = vec![model("a", "one"), model("a", "two"), model("b", "three")];
        let mut selector = ScopedModelSelector::new(models, None);
        selector.clear_all();
        // Providers column: Enter toggles every model of the selected provider.
        selector.toggle_selected();
        assert_eq!(
            selector.selection(),
            &ScopedModelSelection::Explicit(vec!["a/one".to_owned(), "a/two".to_owned()])
        );
        assert!(selector.dirty());
        // Models column: Enter toggles just the highlighted model.
        selector.focus_models();
        selector.toggle_selected();
        assert_eq!(
            selector.selection(),
            &ScopedModelSelection::Explicit(vec!["a/two".to_owned()])
        );
        // Move to the next provider and toggle its model.
        selector.focus_providers();
        selector.move_selection(1);
        selector.focus_models();
        selector.toggle_selected();
        assert_eq!(
            selector.selection(),
            &ScopedModelSelection::Explicit(vec!["a/two".to_owned(), "b/three".to_owned()])
        );
        selector.mark_saved();
        assert!(!selector.dirty());
    }

    #[test]
    fn toggle_provider_scopes_to_query_visible_models() {
        let models = vec![model("a", "one"), model("a", "two"), model("b", "three")];
        let mut selector = ScopedModelSelector::new(models, Some(vec![model("b", "three")]));
        for ch in "one".chars() {
            selector.push_query(ch);
        }
        // The query narrows provider a to its "one" model; "two" is hidden.
        assert_eq!(selector.visible_ids(), vec!["a/one"]);
        assert_eq!(selector.providers(), vec!["a"]);
        // Toggling the provider from the providers column must enable only the
        // visible a/one — the hidden a/two stays disabled.
        selector.toggle_provider();
        assert_eq!(
            selector.selection(),
            &ScopedModelSelection::Explicit(vec!["b/three".to_owned(), "a/one".to_owned()])
        );
        // Toggling again disables a/one without ever touching the hidden a/two.
        selector.toggle_provider();
        assert_eq!(
            selector.selection(),
            &ScopedModelSelection::Explicit(vec!["b/three".to_owned()])
        );
        // Clearing the query reveals a/two still in its original state.
        while !selector.query().is_empty() {
            selector.pop_query();
        }
        assert_eq!(selector.visible_ids(), vec!["b/three", "a/one", "a/two"]);
        assert_eq!(
            selector.selection(),
            &ScopedModelSelection::Explicit(vec!["b/three".to_owned()])
        );
    }
}
