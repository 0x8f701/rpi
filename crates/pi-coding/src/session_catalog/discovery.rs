//! Source-root discovery and path safety checks.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use super::SessionSourceKind;
use super::helpers::make_absolute;


pub(super) fn selected_sources(filters: &[SessionSourceKind], include_all_when_empty: bool) -> Vec<SessionSourceKind> {
    if filters.is_empty() {
        if include_all_when_empty {
            return SessionSourceKind::ALL.to_vec();
        }
        return Vec::new();
    }
    let selected: HashSet<_> = filters.iter().copied().collect();
    SessionSourceKind::ALL
        .into_iter()
        .filter(|kind| selected.contains(kind))
        .collect()
}

pub(super) fn matches_source_pattern(kind: SessionSourceKind, path: &Path) -> bool {
    let Some(file_name) = path.file_name() else {
        return false;
    };
    match kind {
        SessionSourceKind::NativePi
        | SessionSourceKind::Omp
        | SessionSourceKind::Claude
        | SessionSourceKind::Droid => path.extension() == Some(OsStr::new("jsonl")),
        SessionSourceKind::Codex => {
            path.extension() == Some(OsStr::new("jsonl"))
                && file_name
                    .to_str()
                    .is_some_and(|name| name.starts_with("rollout-"))
        }
        SessionSourceKind::Grok => file_name == OsStr::new("summary.json"),
    }
}

pub(super) fn contains_component(path: &Path, root: &Path, name: &str) -> bool {
    path.strip_prefix(root).ok().is_some_and(|relative| {
        relative
            .components()
            .any(|component| component == Component::Normal(OsStr::new(name)))
    })
}

pub(super) fn path_lexically_under_root(path: &Path, root: &Path) -> bool {
    let path = make_absolute(path.to_path_buf());
    let root = make_absolute(root.to_path_buf());
    if path.components().any(|component| matches!(component, Component::ParentDir))
        || root
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return false;
    }
    path.starts_with(&root)
}

pub(super) fn is_native_tree_session(path: &Path, root: &Path) -> bool {
    let path = make_absolute(path.to_path_buf());
    let root = make_absolute(root.to_path_buf());
    let Ok(relative) = path.strip_prefix(&root) else {
        return false;
    };
    let mut components = relative.components();
    matches!(
        (components.next(), components.next(), components.next()),
        (
            Some(Component::Normal(_)),
            Some(Component::Normal(file_name)),
            None
        ) if Path::new(file_name).extension() == Some(OsStr::new("jsonl"))
    )
}

pub(super) fn is_grok_summary_depth(path: &Path, root: &Path) -> bool {
    let path = make_absolute(path.to_path_buf());
    let root = make_absolute(root.to_path_buf());
    let Ok(relative) = path.strip_prefix(&root) else {
        return false;
    };
    let mut components = relative.components();
    matches!(
        (
            components.next(),
            components.next(),
            components.next(),
            components.next()
        ),
        (
            Some(Component::Normal(_)),
            Some(Component::Normal(_)),
            Some(Component::Normal(file_name)),
            None
        ) if file_name == OsStr::new("summary.json")
    )
}

pub(super) fn path_under_depth(path: &Path, root: &Path, min: usize, max: usize) -> bool {
    let path = make_absolute(path.to_path_buf());
    let root = make_absolute(root.to_path_buf());
    let Ok(relative) = path.strip_prefix(&root) else {
        return false;
    };
    let depth = relative.components().count();
    (min..=max).contains(&depth)
}

