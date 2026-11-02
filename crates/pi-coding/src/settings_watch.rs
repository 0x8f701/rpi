use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use notify::event::{AccessKind, AccessMode, EventKind, ModifyKind};
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::resources::CONFIG_DIR_NAME;
use crate::settings::SettingsPaths;

const RELOAD_DEBOUNCE: Duration = Duration::from_millis(400);

#[must_use]
pub fn watched_settings_files(paths: &SettingsPaths) -> Vec<PathBuf> {
    let mut files = vec![paths.global.clone(), paths.project.clone()];
    if let Some(parent) = paths.global.parent() {
        files.push(parent.join("settings.toml"));
    }
    if let Some(parent) = paths.project.parent() {
        files.push(parent.join("settings.toml"));
    }
    files
}

#[must_use]
pub fn event_hits_settings(event_paths: &[PathBuf], watched_files: &[PathBuf]) -> bool {
    event_paths.iter().any(|path| {
        watched_files
            .iter()
            .any(|watched| config_paths_refer_to_same_file(path, watched))
            || (is_settings_file_name(path.file_name())
                && watched_files
                    .iter()
                    .any(|watched| parents_refer_to_same_dir(path.parent(), watched.parent())))
    })
}

fn event_requires_reload(event: &Event, watched_files: &[PathBuf]) -> bool {
    event.need_rescan()
        || (kind_is_settings_content_change(event.kind)
            && event_hits_settings(&event.paths, watched_files))
}

fn kind_is_settings_content_change(kind: EventKind) -> bool {
    match kind {
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Any => true,
        EventKind::Modify(ModifyKind::Metadata(_)) => false,
        EventKind::Modify(_) => true,
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        EventKind::Access(_) | EventKind::Other => false,
    }
}

fn is_settings_file_name(name: Option<&OsStr>) -> bool {
    name.and_then(OsStr::to_str)
        .is_some_and(|name| name.eq_ignore_ascii_case("settings.json") || name.eq_ignore_ascii_case("settings.toml"))
}

fn config_paths_refer_to_same_file(left: &Path, right: &Path) -> bool {
    left == right || config_path_key(left) == config_path_key(right)
}

fn parents_refer_to_same_dir(left: Option<&Path>, right: Option<&Path>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => config_paths_refer_to_same_file(left, right),
        (None, None) => true,
        _ => false,
    }
}

fn config_path_key(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let stripped = if let Some(rest) = raw.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = raw.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        raw.into_owned()
    };
    stripped.replace('\\', "/").to_ascii_lowercase()
}

fn is_project_config_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.eq_ignore_ascii_case(CONFIG_DIR_NAME))
}

fn initial_watch_dirs(paths: &SettingsPaths) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(parent) = paths.global.parent() {
        dirs.push(parent.to_path_buf());
    }
    if let Some(parent) = paths.project.parent() {
        dirs.push(parent.to_path_buf());
        if let Some(cwd) = parent.parent() {
            dirs.push(cwd.to_path_buf());
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs.into_iter().filter(|path| path.is_dir()).collect()
}

/// Watch settings files and yield coalesced reload ticks. The watcher is owned
/// by this task until the returned receiver is dropped or the task ends.
pub fn spawn(paths: SettingsPaths) -> Result<(tokio::task::JoinHandle<()>, mpsc::Receiver<()>)> {
    let watched_files = watched_settings_files(&paths);
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let mut watcher = RecommendedWatcher::new(
        move |result: notify::Result<Event>| {
            if let Ok(event) = result {
                let _ = event_tx.send(event);
            }
        },
        Config::default(),
    )
    .context("starting settings file watcher")?;
    for dir in initial_watch_dirs(&paths) {
        watcher
            .watch(&dir, RecursiveMode::NonRecursive)
            .with_context(|| format!("watching settings directory {}", dir.display()))?;
    }
    let (reload_tx, reload_rx) = mpsc::channel(8);
    let handle = tokio::spawn(async move {
        let mut pending = false;
        let debounce = tokio::time::sleep(RELOAD_DEBOUNCE);
        tokio::pin!(debounce);
        debounce.as_mut().reset(tokio::time::Instant::now() + RELOAD_DEBOUNCE);
        loop {
            tokio::select! {
                event = event_rx.recv() => {
                    let Some(event) = event else {
                        break;
                    };
                    if matches!(event.kind, EventKind::Create(_)) {
                        for path in &event.paths {
                            if is_project_config_dir(path) && path.is_dir() {
                                let _ = watcher.watch(path, RecursiveMode::NonRecursive);
                            }
                        }
                    }
                    if event_requires_reload(&event, &watched_files) {
                        pending = true;
                        debounce.as_mut().reset(
                            tokio::time::Instant::now() + RELOAD_DEBOUNCE,
                        );
                    }
                }
                _ = &mut debounce, if pending => {
                    pending = false;
                    if reload_tx.send(()).await.is_err() {
                        break;
                    }
                }
            }
        }
        drop(watcher);
    });
    Ok((handle, reload_rx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    use notify::event::CreateKind;

    fn sample_watched() -> Vec<PathBuf> {
        watched_settings_files(&SettingsPaths {
            global: PathBuf::from("<workspace>/.rpi/settings.json"),
            project: PathBuf::from("<workspace>/project/.rpi/settings.json"),
        })
    }

    #[test]
    fn toml_sibling_and_json_are_watched() {
        let files = sample_watched();
        assert!(files.iter().any(|path| path.ends_with("settings.json")));
        assert!(files.iter().any(|path| path.ends_with("settings.toml")));
        assert!(event_hits_settings(
            &[PathBuf::from("<workspace>/.rpi/settings.toml")],
            &files
        ));
        assert!(!event_hits_settings(
            &[PathBuf::from("<workspace>/.rpi/notes.md")],
            &files
        ));
    }

    #[test]
    fn windows_style_settings_paths_match_ignoring_ascii_case_and_verbatim_prefix() {
        let watched = [PathBuf::from(r"C:\Users\me\.rpi\settings.json")];
        assert!(event_hits_settings(
            &[PathBuf::from(r"c:\users\me\.rpi\SETTINGS.JSON")],
            &watched
        ));
        assert!(event_hits_settings(
            &[PathBuf::from(r"\\?\C:\Users\me\.rpi\settings.json")],
            &watched
        ));
        assert!(!event_hits_settings(
            &[PathBuf::from(r"C:\Users\me\.rpi\notes.md")],
            &watched
        ));
        assert!(is_project_config_dir(Path::new("<workspace>/project/.RPI")));
    }

    #[test]
    fn opening_or_reading_settings_does_not_require_reload() {
        let files = sample_watched();
        let path = PathBuf::from("<workspace>/.rpi/settings.json");
        let open = Event::new(EventKind::Access(AccessKind::Open(AccessMode::Any))).add_path(path.clone());
        let close_read =
            Event::new(EventKind::Access(AccessKind::Close(AccessMode::Read))).add_path(path.clone());
        assert!(!event_requires_reload(&open, &files));
        assert!(!event_requires_reload(&close_read, &files));
    }

    #[test]
    fn write_close_and_create_of_settings_require_reload() {
        let files = sample_watched();
        let path = PathBuf::from("<workspace>/.rpi/settings.json");
        let close_write =
            Event::new(EventKind::Access(AccessKind::Close(AccessMode::Write))).add_path(path.clone());
        let modify = Event::new(EventKind::Modify(ModifyKind::Data(
            notify::event::DataChange::Any,
        )))
        .add_path(path.clone());
        let create = Event::new(EventKind::Create(CreateKind::File)).add_path(path);
        assert!(event_requires_reload(&close_write, &files));
        assert!(event_requires_reload(&modify, &files));
        assert!(event_requires_reload(&create, &files));
    }

    #[tokio::test]
    async fn writing_settings_json_emits_a_reload_tick() {
        let root = tempfile::tempdir().expect("root");
        let agent = root.path().join("agent");
        fs::create_dir_all(&agent).expect("agent dir");
        let project = root.path().join("project").join(".rpi");
        fs::create_dir_all(&project).expect("project dir");
        let global = agent.join("settings.json");
        fs::write(&global, "{}\n").expect("seed settings");
        let paths = SettingsPaths {
            global: global.clone(),
            project: project.join("settings.json"),
        };
        let (_handle, mut rx) = spawn(paths).expect("watch");
        tokio::time::sleep(Duration::from_millis(80)).await;
        fs::write(&global, "{\"model\":\"x\"}\n").expect("change settings");
        let tick = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("settings watch produced a tick")
            .expect("watch channel stayed open");
        assert_eq!(tick, ());
    }
}
