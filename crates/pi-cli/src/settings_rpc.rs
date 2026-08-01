//! Stateful schema-driven settings RPC adapter.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use parking_lot::Mutex;
use pi_coding::{
    Application, SettingApplyOutcome, SettingsCatalog, SettingsCatalogSnapshot, SettingsDraft,
    SettingsScope,
};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsDraftSnapshot {
    pub draft_id: String,
    pub scope: SettingsScope,
    pub dirty: bool,
    pub values: Vec<pi_coding::SettingValueView>,
}

#[derive(Clone, Default)]
pub struct SettingsRpcState {
    drafts: Arc<Mutex<HashMap<String, SettingsDraft>>>,
}

impl SettingsRpcState {
    #[must_use]
    pub fn inspect(application: &Application) -> Option<SettingsCatalogSnapshot> {
        application.settings_catalog_snapshot()
    }

    pub fn search(application: &Application, query: &str) -> Result<Vec<pi_coding::SettingValueView>> {
        let manager = application.settings_manager()?;
        Ok(SettingsCatalog::search(&manager, query))
    }

    pub fn open(&self, application: &Application, scope: SettingsScope) -> Result<SettingsDraftSnapshot> {
        let draft = application.settings_draft(scope)?;
        let id = Uuid::now_v7().to_string();
        let snapshot = snapshot(&id, &draft)?;
        self.drafts.lock().insert(id, draft);
        Ok(snapshot)
    }

    pub fn get(&self, draft_id: &str) -> Result<SettingsDraftSnapshot> {
        let drafts = self.drafts.lock();
        let draft = drafts
            .get(draft_id)
            .ok_or_else(|| anyhow!("unknown settings draft {draft_id:?}"))?;
        snapshot(draft_id, draft)
    }

    pub fn set(&self, draft_id: &str, key: &str, value: Value) -> Result<SettingsDraftSnapshot> {
        let mut drafts = self.drafts.lock();
        let draft = drafts
            .get_mut(draft_id)
            .ok_or_else(|| anyhow!("unknown settings draft {draft_id:?}"))?;
        draft.set(key, value)?;
        snapshot(draft_id, draft)
    }

    pub fn reset(&self, draft_id: &str, key: &str) -> Result<SettingsDraftSnapshot> {
        let mut drafts = self.drafts.lock();
        let draft = drafts
            .get_mut(draft_id)
            .ok_or_else(|| anyhow!("unknown settings draft {draft_id:?}"))?;
        draft.reset(key)?;
        snapshot(draft_id, draft)
    }

    pub fn validate(&self, draft_id: &str) -> Result<SettingsDraftSnapshot> {
        let drafts = self.drafts.lock();
        let draft = drafts
            .get(draft_id)
            .ok_or_else(|| anyhow!("unknown settings draft {draft_id:?}"))?;
        draft.validate()?;
        snapshot(draft_id, draft)
    }

    pub fn cancel(&self, draft_id: &str) -> Result<()> {
        let draft = self
            .drafts
            .lock()
            .remove(draft_id)
            .ok_or_else(|| anyhow!("unknown settings draft {draft_id:?}"))?;
        draft.cancel();
        Ok(())
    }

    pub async fn apply(
        &self,
        application: &Application,
        draft_id: &str,
    ) -> Result<SettingApplyOutcome> {
        let draft = self
            .drafts
            .lock()
            .get(draft_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown settings draft {draft_id:?}"))?;
        let outcome = application.apply_settings_draft(draft).await?;
        self.drafts.lock().remove(draft_id);
        Ok(outcome)
    }
}

fn snapshot(draft_id: &str, draft: &SettingsDraft) -> Result<SettingsDraftSnapshot> {
    let values = SettingsCatalog::definitions()
        .iter()
        .map(|definition| draft.get(definition.key))
        .collect::<Result<Vec<_>>>()?;
    Ok(SettingsDraftSnapshot {
        draft_id: draft_id.to_owned(),
        scope: draft.scope(),
        dirty: draft.is_dirty(),
        values,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;

    fn manager() -> (tempfile::TempDir, tempfile::TempDir, pi_coding::SettingsManager) {
        let agent = tempfile::tempdir().expect("agent");
        let cwd = tempfile::tempdir().expect("cwd");
        let manager = pi_coding::SettingsManager::load_phase_one(cwd.path(), agent.path())
            .expect("manager");
        (agent, cwd, manager)
    }

    #[test]
    fn draft_state_preserves_typed_values_validation_cancel_and_stale_detection() {
        let (_agent, _cwd, manager) = manager();
        let state = SettingsRpcState::default();
        let id = Uuid::now_v7().to_string();
        state
            .drafts
            .lock()
            .insert(id.clone(), manager.settings_draft(SettingsScope::Global).expect("draft"));
        let updated = state
            .set(&id, "compaction.enabled", json!(false))
            .expect("typed set");
        assert!(updated.dirty);
        assert!(state.set(&id, "compaction.enabled", json!("false")).is_err());
        state.validate(&id).expect("valid draft");
        state.cancel(&id).expect("cancel");
        assert!(state.get(&id).is_err());

        let stale_id = Uuid::now_v7().to_string();
        state.drafts.lock().insert(
            stale_id.clone(),
            manager.settings_draft(SettingsScope::Global).expect("stale draft"),
        );
        state.set(&stale_id, "theme", json!("stale")).expect("stage stale");
        manager
            .update_global(|settings| settings.quiet_startup = Some(true))
            .expect("concurrent write");
        let stale = state.drafts.lock().remove(&stale_id).expect("stale stored");
        assert!(stale.apply(&manager).is_err());
    }

    #[test]
    fn search_and_snapshots_redact_secrets_and_preserve_unknown_json() {
        let (agent, cwd, _) = manager();
        fs::write(
            agent.path().join("settings.json"),
            r#"{"apiKey":"never-leak","future":{"nested":7}}"#,
        )
        .expect("settings");
        let manager = pi_coding::SettingsManager::load_phase_one(cwd.path(), agent.path())
            .expect("reload");
        let secret = SettingsCatalog::search(&manager, "apiKey").remove(0);
        let encoded = serde_json::to_string(&secret).expect("secret json");
        assert!(!encoded.contains("never-leak"));
        let mut draft = manager.settings_draft(SettingsScope::Global).expect("draft");
        draft.set("theme", json!("new")).expect("theme");
        draft.apply(&manager).expect("apply");
        let saved: Value = serde_json::from_slice(
            &fs::read(agent.path().join("settings.json")).expect("saved"),
        )
        .expect("json");
        assert_eq!(saved["future"]["nested"], 7);
    }
}
