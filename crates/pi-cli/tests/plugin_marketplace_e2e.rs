//! End-to-end coverage for the plugin marketplace CLI (`rpi plugin
//! list/install/remove/update`, T107) through the REAL `rpi` binary.
//!
//! The `pi-coding` plugin module has thorough in-process tests; these tests
//! prove the CLI adapter contract: `install` stages a local plugin directory
//! into `<agent_dir>/extensions/<name>` and records it as trusted, `list`
//! renders name/version/runtime/trust, `update` swaps versions from a local
//! marketplace index (`pluginMarketplace` setting, `list --updates` preview),
//! `remove` deletes the plugin and clears trust, and an invalid manifest
//! fails install with an actionable error. No network: the index is a local
//! file, the plugin is a local directory, and the default marketplace URL is
//! never fetched.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::json;
use tempfile::TempDir;

fn rpi_bin() -> String {
    env!("CARGO_BIN_EXE_rpi").to_owned()
}

fn run(agent_dir: &Path, cwd: &Path, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(rpi_bin())
        .args(args)
        .env("PI_CODING_AGENT_DIR", agent_dir)
        .env("HOME", agent_dir)
        .env("USERPROFILE", agent_dir)
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env_remove("PI_OFFLINE")
        .env_remove("PI_PROFILE")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("GEMINI_API_KEY")
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .env_remove("GOOGLE_APPLICATION_CREDENTIALS")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .expect("run rpi plugin command");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// A minimal valid quickjs plugin package in `root`: manifest + entry.
fn write_quickjs_plugin(root: &Path, id: &str, version: &str) {
    fs::create_dir_all(root).expect("create plugin dir");
    fs::write(
        root.join("pi-extension.json"),
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 1,
            "id": id,
            "version": version,
            "runtime": "quickjs",
            "entry": "index.mjs",
            "capabilities": ["commands"],
        }))
        .expect("serialize manifest"),
    )
    .expect("write manifest");
    fs::write(root.join("index.mjs"), "export default function (pi) {}\n").expect("write entry");
}

/// Contract: `rpi plugin install` stages a local directory into
/// `<agent_dir>/extensions/<name>` and records it trusted; `list` renders
/// name/version/runtime/trust; `remove` deletes the plugin and clears trust;
/// the second `list` reports nothing installed.
#[test]
fn plugin_install_list_remove_round_trip() {
    let sandbox = TempDir::new().unwrap();
    let agent_dir = sandbox.path().join("agent");
    let cwd = sandbox.path().join("workspace");
    let source = sandbox.path().join("demo-plugin");
    fs::create_dir_all(&agent_dir).unwrap();
    fs::create_dir_all(&cwd).unwrap();
    write_quickjs_plugin(&source, "demo", "1.0.0");

    let (installed, out, install_err) =
        run(&agent_dir, &cwd, &["plugin", "install", source.to_str().unwrap()]);
    assert!(installed, "install must exit 0: {install_err}");
    assert!(
        out.contains("installed demo 1.0.0 (quickjs runtime)"),
        "install report: {out}"
    );
    assert!(out.contains("trusted and loadable"), "install report: {out}");
    let manifest = agent_dir.join("extensions").join("demo").join("pi-extension.json");
    assert!(manifest.is_file(), "plugin must land under extensions/demo");

    let (listed, list_out, list_err) = run(&agent_dir, &cwd, &["plugin", "list"]);
    assert!(listed, "list must exit 0: {list_err}");
    assert!(
        list_out.contains("demo  1.0.0  quickjs  trusted"),
        "list must render name/version/runtime/trust: {list_out}"
    );

    let (removed, remove_out, remove_err) = run(&agent_dir, &cwd, &["plugin", "remove", "demo"]);
    assert!(removed, "remove must exit 0: {remove_err}");
    assert!(remove_out.contains("removed plugin demo"), "remove report: {remove_out}");
    assert!(
        !agent_dir.join("extensions").join("demo").exists(),
        "remove must delete the installed plugin directory"
    );

    let (listed_after, after_out, after_err) = run(&agent_dir, &cwd, &["plugin", "list"]);
    assert!(listed_after, "list after remove must exit 0: {after_err}");
    assert!(after_out.contains("No plugins installed."), "after remove: {after_out}");
}

/// Contract: installing a package whose manifest fails validation (here:
/// unknown schema version) exits non-zero with an actionable error naming
/// the source, and nothing is left under `extensions/`.
#[test]
fn plugin_install_rejects_invalid_manifest() {
    let sandbox = TempDir::new().unwrap();
    let agent_dir = sandbox.path().join("agent");
    let cwd = sandbox.path().join("workspace");
    let source = sandbox.path().join("broken-plugin");
    fs::create_dir_all(&agent_dir).unwrap();
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(&source).unwrap();
    fs::write(
        source.join("pi-extension.json"),
        serde_json::to_vec_pretty(&json!({
            "schemaVersion": 999,
            "id": "broken",
            "version": "1.0.0",
            "runtime": "quickjs",
            "entry": "index.mjs",
        }))
        .expect("serialize manifest"),
    )
    .expect("write manifest");

    let (ok, _out, err) = run(&agent_dir, &cwd, &["plugin", "install", source.to_str().unwrap()]);
    assert!(!ok, "invalid manifest install must fail");
    assert!(
        err.contains("installing plugin from") && err.contains("invalid plugin manifest"),
        "error must be actionable and name the manifest problem: {err}"
    );
    assert!(
        !agent_dir.join("extensions").join("broken").exists(),
        "failed install must not leave a plugin directory"
    );
}

/// Contract: `plugin list --updates` previews a newer version from the local
/// marketplace index (`pluginMarketplace` setting) without mutating state,
/// and `plugin update <name>` re-stages from the index entry's repo,
/// swapping the version while keeping the trust decision.
#[test]
fn plugin_update_uses_local_index_and_keeps_trust() {
    let sandbox = TempDir::new().unwrap();
    let agent_dir = sandbox.path().join("agent");
    let cwd = sandbox.path().join("workspace");
    let v1 = sandbox.path().join("demo-v1");
    let v2 = sandbox.path().join("demo-v2");
    fs::create_dir_all(&agent_dir).unwrap();
    fs::create_dir_all(&cwd).unwrap();
    write_quickjs_plugin(&v1, "demo", "1.0.0");
    write_quickjs_plugin(&v2, "demo", "2.0.0");

    let (installed, _, install_err) =
        run(&agent_dir, &cwd, &["plugin", "install", v1.to_str().unwrap()]);
    assert!(installed, "install v1: {install_err}");

    let index_path = sandbox.path().join("marketplace-index.json");
    fs::write(
        &index_path,
        serde_json::to_vec_pretty(&[json!({
            "name": "demo",
            "repo": v2.to_str().unwrap(),
            "version": "2.0.0",
            "description": "next release",
        })])
        .expect("serialize index"),
    )
    .expect("write index");
    fs::write(
        agent_dir.join("settings.json"),
        serde_json::to_vec(&json!({
            "pluginMarketplace": index_path.to_str().unwrap(),
        }))
        .expect("serialize settings"),
    )
    .expect("write settings");

    let (previewed, preview, preview_err) =
        run(&agent_dir, &cwd, &["plugin", "list", "--updates"]);
    assert!(previewed, "list --updates must exit 0: {preview_err}");
    assert!(
        preview.contains("demo  1.0.0 -> 2.0.0"),
        "preview must report the newer version: {preview}"
    );

    let (updated, update_out, update_err) = run(&agent_dir, &cwd, &["plugin", "update", "demo"]);
    assert!(updated, "update must exit 0: {update_err}");
    assert!(update_out.contains("updated demo to 2.0.0"), "update report: {update_out}");

    let (listed, list_out, list_err) = run(&agent_dir, &cwd, &["plugin", "list"]);
    assert!(listed, "list after update must exit 0: {list_err}");
    assert!(
        list_out.contains("demo  2.0.0  quickjs  trusted"),
        "updated plugin keeps trust and shows the new version: {list_out}"
    );

    // With the index now current, the preview reports up to date.
    let (previewed, preview2, preview2_err) =
        run(&agent_dir, &cwd, &["plugin", "list", "--updates"]);
    assert!(previewed, "second preview must exit 0: {preview2_err}");
    assert!(
        preview2.contains("All plugins are up to date."),
        "after update the preview must be empty: {preview2}"
    );
}

/// Write a gzip tarball at `archive` with the given `(path, contents)` entries.
/// Mirrors the fixture helper in `pi-coding/src/plugin.rs` tests: the archive
/// is exactly what a real `.tgz` plugin source looks like (including a shared
/// top directory, as GitHub codeload / npm tarballs carry).
fn write_tarball(archive: &Path, entries: &[(&str, &[u8])]) {
    let file = fs::File::create(archive).expect("create archive");
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for (name, contents) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size((*contents).len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, *name, *contents)
            .expect("append archive entry");
    }
    builder.finish().expect("finish archive");
}

/// Contract: `rpi plugin install <local>.tgz` through the REAL binary —
/// a gzip tarball source stages into `<agent_dir>/extensions/<name>` with the
/// shared top directory stripped, records trust, and `list` renders
/// name/version/runtime/trust. This is the archive-source path the CLI
/// advertises (`plugin install <directory|archive|owner/repo>`), which the
/// directory-source tests alone do not cover.
#[test]
fn plugin_install_from_local_tarball_extracts_and_lists() {
    let sandbox = TempDir::new().unwrap();
    let agent_dir = sandbox.path().join("agent");
    let cwd = sandbox.path().join("workspace");
    fs::create_dir_all(&agent_dir).unwrap();
    fs::create_dir_all(&cwd).unwrap();

    let manifest = serde_json::to_vec_pretty(&json!({
        "schemaVersion": 1,
        "id": "tar-demo",
        "version": "3.1.4",
        "runtime": "quickjs",
        "entry": "index.mjs",
        "capabilities": ["commands"],
    }))
    .expect("serialize manifest");
    let archive = sandbox.path().join("tar-demo-3.1.4.tgz");
    // Shared top directory (`tar-demo-3.1.4/...`) like real archives; the
    // extractor must strip it so the manifest lands at the extension root.
    write_tarball(
        &archive,
        &[
            ("tar-demo-3.1.4/pi-extension.json", manifest.as_slice()),
            ("tar-demo-3.1.4/index.mjs", b"export default function (pi) {}\n".as_slice()),
        ],
    );

    let (installed, out, err) =
        run(&agent_dir, &cwd, &["plugin", "install", archive.to_str().unwrap()]);
    assert!(installed, "install must exit 0: {err}");
    assert!(
        out.contains("installed tar-demo 3.1.4 (quickjs runtime)"),
        "install report: {out}"
    );
    assert!(out.contains("trusted and loadable"), "install report: {out}");
    let root = agent_dir.join("extensions").join("tar-demo");
    assert!(
        root.join("pi-extension.json").is_file(),
        "manifest must land at the extension root (top dir stripped)"
    );
    assert!(root.join("index.mjs").is_file(), "entry must land at the extension root");
    assert!(
        !root.join("tar-demo-3.1.4").exists(),
        "shared top directory must be stripped"
    );

    let (listed, list_out, list_err) = run(&agent_dir, &cwd, &["plugin", "list"]);
    assert!(listed, "list must exit 0: {list_err}");
    assert!(
        list_out.contains("tar-demo  3.1.4  quickjs  trusted"),
        "list must render the tarball-installed plugin: {list_out}"
    );
}

/// Contract: a tarball whose manifest is missing fails install with an
/// actionable error naming the missing manifest, and nothing lands under
/// `extensions/` (the archive must be validated like every other source).
#[test]
fn plugin_install_from_tarball_without_manifest_fails_cleanly() {
    let sandbox = TempDir::new().unwrap();
    let agent_dir = sandbox.path().join("agent");
    let cwd = sandbox.path().join("workspace");
    fs::create_dir_all(&agent_dir).unwrap();
    fs::create_dir_all(&cwd).unwrap();

    let archive = sandbox.path().join("no-manifest.tgz");
    write_tarball(
        &archive,
        &[("no-manifest/README.md", b"no extension manifest here\n".as_slice())],
    );

    let (ok, _out, err) = run(&agent_dir, &cwd, &["plugin", "install", archive.to_str().unwrap()]);
    assert!(!ok, "missing-manifest tarball must fail install");
    assert!(
        err.contains("pi-extension.json"),
        "error must name the missing manifest: {err}"
    );
    assert!(
        !agent_dir.join("extensions").exists() || fs::read_dir(agent_dir.join("extensions")).unwrap().next().is_none(),
        "failed install must not leave a plugin directory"
    );
}
