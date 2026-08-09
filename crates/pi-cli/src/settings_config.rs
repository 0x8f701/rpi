//! `rpi config get|set|reset|list` — headless settings-key surface reusing the
//! settings catalog and the atomic draft+apply pipeline (OMP `omp config`
//! parity: scripts can configure rpi without the TUI). `rpi config` with no
//! subcommand keeps the package-resource selector in `package_config`.

use std::io::{self, IsTerminal as _};
use std::path::Path;

use anyhow::{Context, Result, bail};
use pi_coding::{
    SettingApplyBehavior, SettingCategory, SettingSource, SettingsCatalog, SettingsManager,
    SettingsScope, SettingValueView,
};

use crate::args::{ConfigCommand, ConfigScopeArg};
use crate::settings_panel::parse_setting_input;

/// Dispatch `rpi config <get|set|reset|list>`. `local` mirrors the bare
/// selector's `--local` flag (project scope) and is overridden by an explicit
/// `--scope`; `approve`/`no_approve` drive the same trust policy as the
/// package-resource selector.
pub fn run(
    command: ConfigCommand,
    cwd: &Path,
    local: bool,
    approve: bool,
    no_approve: bool,
) -> Result<()> {
    let agent_dir = pi_coding::agent_dir_path();
    let headless = !io::stdout().is_terminal();
    let project_trusted = crate::package_config::resolve_trust(cwd, &agent_dir, approve, no_approve, headless)
        .context("resolving project trust for config")?;
    let manager = SettingsManager::load_phase_one(cwd, &agent_dir).context("loading settings")?;
    if project_trusted {
        manager.load_project(true).context("loading project settings")?;
    }
    let scope = match command.scope() {
        Some(ConfigScopeArg::Global) => SettingsScope::Global,
        Some(ConfigScopeArg::Project) => SettingsScope::Project,
        None if local => SettingsScope::Project,
        None => SettingsScope::Global,
    };
    if scope == SettingsScope::Project && !project_trusted {
        bail!(
            "project settings require a trusted project (run with --approve to trust this directory)"
        );
    }
    match command {
        ConfigCommand::Get { key, json, .. } => get(&manager, scope, &key, json),
        ConfigCommand::Set { key, value, json, .. } => set(&manager, scope, &key, &value, json),
        ConfigCommand::Reset { key, json, .. } => reset(&manager, scope, &key, json),
        ConfigCommand::List { category, json, .. } => list(&manager, scope, category.as_deref(), json),
    }
}

impl ConfigCommand {
    fn scope(&self) -> Option<ConfigScopeArg> {
        match self {
            Self::Get { scope, .. }
            | Self::Set { scope, .. }
            | Self::Reset { scope, .. }
            | Self::List { scope, .. } => *scope,
        }
    }
}

fn get(manager: &SettingsManager, scope: SettingsScope, key: &str, json: bool) -> Result<()> {
    let definition = SettingsCatalog::definition(key)
        .ok_or_else(|| anyhow::anyhow!("unknown setting key {key:?}"))?;
    if definition.secret {
        bail!(
            "{} is secret material and cannot be read or written through settings.json",
            definition.key
        );
    }
    let view = SettingsCatalog::get(manager, key)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&view)?);
    } else {
        print_get(scope, &view);
    }
    Ok(())
}

fn set(manager: &SettingsManager, scope: SettingsScope, key: &str, value: &str, json: bool) -> Result<()> {
    let mut draft = SettingsCatalog::draft(manager, scope)?;
    let parsed = parse_setting_input(key, value)?;
    draft.set(key, parsed)?;
    let writes = draft.apply(manager)?;
    let write = writes
        .first()
        .ok_or_else(|| anyhow::anyhow!("no settings write produced for {key}"))?;
    if json {
        println!("{}", serde_json::to_string_pretty(write)?);
    } else {
        print_write(scope, write);
    }
    Ok(())
}

fn reset(manager: &SettingsManager, scope: SettingsScope, key: &str, json: bool) -> Result<()> {
    let mut draft = SettingsCatalog::draft(manager, scope)?;
    draft.reset(key)?;
    let writes = draft.apply(manager)?;
    let write = writes
        .first()
        .ok_or_else(|| anyhow::anyhow!("no settings write produced for {key}"))?;
    if json {
        println!("{}", serde_json::to_string_pretty(write)?);
    } else {
        print_write(scope, write);
    }
    Ok(())
}

fn list(manager: &SettingsManager, scope: SettingsScope, category: Option<&str>, json: bool) -> Result<()> {
    let snapshot = SettingsCatalog::inspect(manager);
    let views = snapshot
        .values
        .iter()
        .filter(|view| category.is_none_or(|wanted| category_matches(view.definition.category, wanted)))
        .collect::<Vec<_>>();
    if views.is_empty() {
        bail!(
            "no settings match category {category:?} (expected one of Models, Session, Compaction, RetryTransport, TerminalUi, Orchestration, Resources, TrustSecurity, Live)"
        );
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&views)?);
        return Ok(());
    }
    let mut current = None;
    for view in views {
        if current != Some(view.definition.category) {
            if current.is_some() {
                println!();
            }
            println!("{}", category_label(view.definition.category));
            current = Some(view.definition.category);
        }
        println!(
            "  {:<30} {:<30} [{}] ({}) {}",
            view.definition.key,
            value_label(&view),
            source_label(view.source),
            behavior_label(view.definition.behavior),
            view.definition.description,
        );
    }
    println!();
    println!("({} settings, {} scope)", snapshot.values.len(), scope_label(scope));
    Ok(())
}

fn category_matches(category: SettingCategory, wanted: &str) -> bool {
    let normalized = wanted.trim().to_ascii_lowercase();
    format!("{:?}", category).to_ascii_lowercase() == normalized
        || category_label(category).to_ascii_lowercase() == normalized
        || match category {
            SettingCategory::RetryTransport => "retry",
            SettingCategory::TerminalUi => "terminal",
            SettingCategory::TrustSecurity => "trust",
            _ => "",
        } == normalized
}

fn print_get(scope: SettingsScope, view: &SettingValueView) {
    println!(
        "{} = {}  [{}]  ({})",
        view.definition.key,
        value_label(view),
        source_label(view.source),
        behavior_label(view.definition.behavior),
    );
    if let Some(value) = &view.global_value {
        println!("  global: {value}");
    }
    if let Some(value) = &view.project_value {
        println!("  project: {value}");
    }
    if let Some(value) = &view.session_override_value {
        println!("  session: {value}");
    }
    println!("  scope: {}", scope_label(scope));
    println!("  description: {}", view.definition.description);
}

fn print_write(scope: SettingsScope, write: &pi_coding::SettingWriteResult) {
    println!(
        "{} = {}  [{}]  ({})",
        write.key,
        write.effective_value,
        source_label(write.source),
        behavior_label(write.behavior),
    );
    if write.needs_reload {
        println!("  note: applying this change requires a settings reload");
    }
    if write.needs_restart {
        println!("  note: applying this change requires an rpi restart");
    }
    println!("  scope: {}", scope_label(scope));
}

fn value_label(view: &SettingValueView) -> String {
    if view.redacted {
        "[redacted]".to_owned()
    } else {
        view.effective_value.to_string()
    }
}

fn source_label(source: SettingSource) -> &'static str {
    match source {
        SettingSource::Default => "default",
        SettingSource::Global => "global",
        SettingSource::Project => "project",
        SettingSource::SessionOverride => "session",
        SettingSource::Runtime => "runtime",
    }
}

fn behavior_label(behavior: SettingApplyBehavior) -> &'static str {
    match behavior {
        SettingApplyBehavior::Live => "live",
        SettingApplyBehavior::Reload => "reload",
        SettingApplyBehavior::Restart => "restart",
    }
}

fn scope_label(scope: SettingsScope) -> &'static str {
    match scope {
        SettingsScope::Global => "global",
        SettingsScope::Project => "project",
    }
}

fn category_label(category: SettingCategory) -> &'static str {
    match category {
        SettingCategory::Models => "Models",
        SettingCategory::Session => "Session",
        SettingCategory::Compaction => "Compaction",
        SettingCategory::RetryTransport => "RetryTransport",
        SettingCategory::TerminalUi => "TerminalUi",
        SettingCategory::Orchestration => "Orchestration",
        SettingCategory::Resources => "Resources",
        SettingCategory::TrustSecurity => "TrustSecurity",
        SettingCategory::Live => "Live",
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn manager(cwd: &Path, agent: &Path) -> SettingsManager {
        SettingsManager::load_phase_one(cwd, agent).expect("manager")
    }

    fn setup() -> (tempfile::TempDir, tempfile::TempDir, SettingsManager) {
        let agent = tempfile::tempdir().expect("agent");
        let cwd = tempfile::tempdir().expect("cwd");
        let manager = manager(cwd.path(), agent.path());
        (agent, cwd, manager)
    }

    #[test]
    fn get_prints_effective_source_and_behavior() {
        let (_agent, _cwd, manager) = setup();
        let view = SettingsCatalog::get(&manager, "retry.maxRetries").expect("view");
        assert_eq!(view.source, SettingSource::Default);
        assert_eq!(view.effective_value, serde_json::json!(3));
        assert_eq!(view.definition.behavior, SettingApplyBehavior::Live);
    }

    #[test]
    fn get_rejects_secret_keys() {
        let (_agent, _cwd, manager) = setup();
        for key in ["apiKey", "images.genApiKey", "live.sttApiKey"] {
            let error = get(&manager, SettingsScope::Global, key, false).expect_err("secret rejected");
            assert!(
                error.to_string().contains("secret material"),
                "secret message for {key}: {error}"
            );
        }
    }

    #[test]
    fn get_rejects_unknown_keys() {
        let (_agent, _cwd, manager) = setup();
        let error = get(&manager, SettingsScope::Global, "no.such.key", false).expect_err("unknown key");
        assert!(error.to_string().contains("unknown setting key"));
    }

    #[test]
    fn set_reset_round_trip_persists_through_the_draft_pipeline() {
        let (agent, cwd, manager) = setup();
        // The CLI applies through the manager; simulate a trusted project for
        // the project-scope half.
        manager.load_project(true).expect("trust");

        // Global set: value persists in the file, effective value updates.
        set(&manager, SettingsScope::Global, "retry.maxRetries", "7", false).expect("set");
        assert_eq!(manager.global_settings().retry.as_ref().unwrap().max_retries, Some(7));
        let view = SettingsCatalog::get(&manager, "retry.maxRetries").expect("view");
        assert_eq!(view.source, SettingSource::Global);
        assert_eq!(view.effective_value, serde_json::json!(7));

        // Reset: clears the scoped layer and falls back to the default.
        reset(&manager, SettingsScope::Global, "retry.maxRetries", false).expect("reset");
        assert_eq!(manager.global_settings().retry, None);
        let view = SettingsCatalog::get(&manager, "retry.maxRetries").expect("view");
        assert_eq!(view.source, SettingSource::Default);
        assert_eq!(view.effective_value, serde_json::json!(3));

        // Project set with a global fallback: source reports project, reset
        // falls back to the inherited global value.
        set(&manager, SettingsScope::Global, "theme", "global-dark", false).expect("global theme");
        set(&manager, SettingsScope::Project, "theme", "project-dark", false).expect("project theme");
        let view = SettingsCatalog::get(&manager, "theme").expect("view");
        assert_eq!(view.source, SettingSource::Project);
        assert_eq!(view.effective_value, serde_json::json!("project-dark"));
        reset(&manager, SettingsScope::Project, "theme", false).expect("reset theme");
        let view = SettingsCatalog::get(&manager, "theme").expect("view");
        assert_eq!(view.source, SettingSource::Global);
        assert_eq!(view.effective_value, serde_json::json!("global-dark"));

        // The files on disk match (JSON round-trip through the loader): the
        // project reset clears the scoped layer while the global value stays.
        let saved_project: serde_json::Value = serde_json::from_slice(
            &fs::read(cwd.path().join(".pi/settings.json")).expect("project file"),
        )
        .expect("project json");
        assert_eq!(saved_project.get("theme"), None, "project reset clears the scoped layer");
        let saved_global: serde_json::Value = serde_json::from_slice(
            &fs::read(agent.path().join("settings.json")).expect("global file"),
        )
        .expect("global json");
        assert_eq!(saved_global["theme"], "global-dark");
    }

    #[test]
    fn set_validates_typed_values_and_rejects_secrets_and_unknowns() {
        let (_agent, _cwd, manager) = setup();
        let error = set(&manager, SettingsScope::Global, "transport", "udp", false).expect_err("bad enum");
        assert!(error.to_string().contains("must be one of"), "{error}");
        let error =
            set(&manager, SettingsScope::Global, "compaction.reserveTokens", "0", false).expect_err("out of range");
        assert!(error.to_string().contains("between"), "{error}");
        let error =
            set(&manager, SettingsScope::Global, "apiKey", "hunter2", false).expect_err("secret write");
        assert!(error.to_string().contains("secret material"), "{error}");
        let error =
            set(&manager, SettingsScope::Global, "no.such.key", "x", false).expect_err("unknown write");
        assert!(error.to_string().contains("unknown setting key"), "{error}");
        assert_eq!(manager.global_settings().retry, None, "failed writes must not persist");
    }

    #[test]
    fn set_accepts_json_values_for_collections() {
        let (_agent, _cwd, manager) = setup();
        set(&manager, SettingsScope::Global, "packages", r#"[{"source":"example","kind":"extension"}]"#, false)
        .expect("array set");
        let packages = &manager.global_settings().packages;
        assert_eq!(packages.len(), 1);
        assert!(
            matches!(&packages[0], pi_coding::PackageSource::Filtered(pkg) if pkg.source == "example"),
            "array rows round-trip through the typed settings: {:?}",
            packages[0]
        );
        set(&manager, SettingsScope::Global, "keybindings", r#"{"submit":"enter"}"#, false)
        .expect("object set");
        let global = manager.global_settings();
        let keybindings = global.keybindings.as_ref().expect("keybindings");
        assert_eq!(keybindings.len(), 1);
        assert!(
            keybindings.contains_key("submit"),
            "keybindings row must round-trip through the typed Settings: {keybindings:?}"
        );
        let error = set(&manager, SettingsScope::Global, "packages", "{}", false).expect_err("object for array");
        assert!(error.to_string().contains("must be an array"), "{error}");
    }

    #[test]
    fn list_filters_by_category_name_and_alias() {
        let (_agent, _cwd, manager) = setup();
        let snapshot = SettingsCatalog::inspect(&manager);
        let all = snapshot.values.len();
        assert!(all > 0, "catalog must not be empty");
        let models = snapshot
            .values
            .iter()
            .filter(|v| category_matches(v.definition.category, "Models"))
            .count();
        assert!(models > 0 && models < all);
        // The TUI tab alias ("Retry") and the Debug name both resolve.
        let by_debug = snapshot
            .values
            .iter()
            .filter(|v| category_matches(v.definition.category, "retrytransport"))
            .count();
        let by_alias = snapshot
            .values
            .iter()
            .filter(|v| category_matches(v.definition.category, "Retry"))
            .count();
        assert!(by_debug > 0 && by_debug == by_alias);
        assert!(!category_matches(SettingCategory::Models, "bogus"));
    }
}
