//! Installer-compatible GitHub release notification and native self-update.
//!
//! `PI_UPDATE_BASE_URL` overrides the default `0x8f701/pi-rs` GitHub releases
//! API, matching `install.sh` and `install.ps1`. `PI_OFFLINE=1|true|yes`
//! disables updater networking; `PI_SKIP_VERSION_CHECK` disables only the
//! nonfatal interactive startup check.

use std::env;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use reqwest::Client;
use semver::Version;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const DEFAULT_API: &str = "https://api.github.com/repos/0x8f701/pi-rs/releases";
const API_LIMIT: u64 = 4 * 1024 * 1024;
const SUMS_LIMIT: u64 = 1024 * 1024;
const FILE_LIMIT: u64 = 1024 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const DOWNLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const STALE_LOCK_AGE: Duration = Duration::from_secs(6 * 60 * 60);
static UNIQUE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(Clone, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

struct ReleaseInfo {
    release: GithubRelease,
    version: Version,
}

#[derive(Clone, Serialize, Deserialize)]
struct UpdateState {
    installed_version: String,
    installed_asset: String,
    installed_sha256: String,
    installed_binary: String,
    checked_at_unix: u64,
}

#[derive(Serialize)]
struct WindowsActivationCommand {
    action: &'static str,
    staged: PathBuf,
    destination: PathBuf,
    backup: PathBuf,
    state_new: PathBuf,
    state_path: PathBuf,
    status_path: PathBuf,
}

#[derive(Deserialize)]
struct DeferredUpdateResult {
    ok: bool,
    error: String,
}

struct ReleaseClient {
    http: Client,
    api: String,
    authenticate: bool,
}

impl ReleaseClient {
    fn new() -> Result<Self> {
        let api = env::var("PI_UPDATE_BASE_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_API.to_owned())
            .trim_end_matches('/')
            .to_owned();
        Ok(Self {
            authenticate: api == DEFAULT_API,
            api,
            http: Client::builder()
                .user_agent(format!("pi-rs/{}", env!("CARGO_PKG_VERSION")))
                .connect_timeout(REQUEST_TIMEOUT)
                .redirect(reqwest::redirect::Policy::limited(5))
                .build()
                .context("creating GitHub release client")?,
        })
    }

    async fn json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let mut request = self
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json");
        if self.authenticate
            && api_child(url, &self.api)
            && let Some(token) = env::var("GITHUB_TOKEN")
                .ok()
                .filter(|token| !token.trim().is_empty())
        {
            request = request.bearer_auth(token);
        }
        let bytes = tokio::time::timeout(REQUEST_TIMEOUT, response_bytes(request, API_LIMIT))
            .await
            .context("release metadata request timed out")?
            .with_context(|| format!("fetching release metadata from {url}"))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing release metadata from {url}"))
    }

    async fn download(&self, url: &str, path: &Path, limit: u64) -> Result<String> {
        let response = self
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, "application/octet-stream")
            .send()
            .await
            .with_context(|| format!("downloading {url}"))?
            .error_for_status()
            .with_context(|| format!("downloading {url}"))?;
        if response.content_length().is_some_and(|size| size > limit) {
            bail!("download from {url} exceeds the {limit}-byte safety limit");
        }
        let mut output = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .await
            .with_context(|| format!("creating download {}", path.display()))?;
        let mut stream = response.bytes_stream();
        let mut size = 0_u64;
        let mut hasher = Sha256::new();
        while let Some(chunk) = tokio::time::timeout(DOWNLOAD_IDLE_TIMEOUT, stream.next())
            .await
            .with_context(|| format!("download from {url} stalled"))?
        {
            let chunk = chunk.with_context(|| format!("reading download from {url}"))?;
            size = size
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| anyhow!("download size overflow from {url}"))?;
            if size > limit {
                bail!("download from {url} exceeds the {limit}-byte safety limit");
            }
            hasher.update(&chunk);
            output
                .write_all(&chunk)
                .await
                .with_context(|| format!("writing download {}", path.display()))?;
        }
        output
            .flush()
            .await
            .with_context(|| format!("flushing download {}", path.display()))?;
        output
            .sync_all()
            .await
            .with_context(|| format!("syncing download {}", path.display()))?;
        Ok(hex(&hasher.finalize()))
    }
}

/// Check for a newer release without delaying or failing interactive startup.
pub async fn startup_notice() -> Option<String> {
    if offline() || env::var_os("PI_SKIP_VERSION_CHECK").is_some() {
        return None;
    }
    let current = Version::parse(env!("CARGO_PKG_VERSION")).ok()?;
    let client = ReleaseClient::new().ok()?;
    let latest = select_release(&client, &current).await.ok()?;
    if latest.version <= current {
        return None;
    }
    let mut notice = format!(
        "Update available: current v{current}, latest v{}",
        latest.version
    );
    if let Some(summary) = changelog_summary(latest.release.body.as_deref()) {
        notice.push_str(" — ");
        notice.push_str(&summary);
    }
    notice.push_str(" — ");
    notice.push_str(&latest.release.html_url);
    notice.push_str(" (run `pi update --self`)");
    Some(notice)
}

/// Download, checksum, smoke-test, and atomically activate the latest release.
pub async fn update_self(force: bool) -> Result<()> {
    if offline() {
        bail!("self-update is unavailable while PI_OFFLINE is enabled");
    }
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .context("the running pi version is not valid semantic versioning")?;
    let install = ManagedInstall::detect(&current)?;
    #[cfg(windows)]
    check_deferred_update_result(&install.root)?;
    let mut lock = InstallLock::acquire(&install.root).await?;
    let client = ReleaseClient::new()?;
    let latest = select_release(&client, &current).await?;
    if latest.version < current {
        println!(
            "pi v{current} is newer than latest v{} — {}",
            latest.version, latest.release.html_url
        );
        return Ok(());
    }

    println!("Current: v{current}");
    println!("Latest: v{}", latest.version);
    println!("Release: {}", latest.release.html_url);
    if let Some(body) = latest
        .release
        .body
        .as_deref()
        .map(str::trim)
        .filter(|body| !body.is_empty())
    {
        println!("Changelog:\n{body}");
    }

    let platform = Platform::current()?;
    let asset_name = platform.asset_name(&latest.version);
    let archive_asset = exact_asset(&latest.release, &asset_name)?;
    let sums_asset = exact_asset(&latest.release, "SHA256SUMS")?;
    if archive_asset.size > FILE_LIMIT {
        bail!("release asset {asset_name} exceeds the 1 GiB safety limit");
    }
    if sums_asset.size > SUMS_LIMIT {
        bail!("release SHA256SUMS exceeds the 1 MiB safety limit");
    }

    let scratch = ScratchDir::new(&install.root)?;
    let sums_path = scratch.path.join("SHA256SUMS");
    client
        .download(&sums_asset.browser_download_url, &sums_path, SUMS_LIMIT)
        .await?;
    let expected = expected_checksum(&sums_path, &asset_name)?;
    if !force && latest.version == current && install.state.installed_sha256 == expected {
        println!("pi v{current} is already up to date.");
        return Ok(());
    }

    let archive_path = scratch.path.join(&asset_name);
    println!("Downloading {asset_name}...");
    let actual = client
        .download(
            &archive_asset.browser_download_url,
            &archive_path,
            FILE_LIMIT,
        )
        .await?;
    if actual != expected {
        bail!("SHA256 mismatch for {asset_name}: expected {expected}, got {actual}");
    }
    println!("Checksum verified.");

    let staged = install.staged_path()?;
    let mut staged_guard = RemovePath::new(staged.clone());
    extract_binary(&archive_path, &staged, platform).await?;
    smoke(&staged, &latest.version).await?;
    let state = UpdateState {
        installed_version: latest.version.to_string(),
        installed_asset: asset_name,
        installed_sha256: expected.clone(),
        installed_binary: platform.installed_name(&latest.version, &expected),
        checked_at_unix: timestamp()?,
    };
    activate(&install, &staged, &state, &latest.version, &mut lock).await?;
    staged_guard.disarm();
    if cfg!(windows) {
        println!("The verified update will activate after this process exits.");
    } else {
        println!("pi v{} installed successfully.", latest.version);
    }
    Ok(())
}

async fn select_release(client: &ReleaseClient, current: &Version) -> Result<ReleaseInfo> {
    if current.pre.is_empty() {
        let release = client
            .json::<GithubRelease>(&format!("{}/latest", client.api))
            .await?;
        let info = validate_release(release)?;
        if info.release.draft || info.release.prerelease || !info.version.pre.is_empty() {
            bail!("stable release endpoint returned a draft or prerelease");
        }
        return Ok(info);
    }
    client
        .json::<Vec<GithubRelease>>(&client.api)
        .await?
        .into_iter()
        .filter_map(|release| validate_release(release).ok())
        .filter(|release| !release.release.draft)
        .max_by(|left, right| left.version.cmp(&right.version))
        .ok_or_else(|| anyhow!("release endpoint contains no valid published releases"))
}

fn validate_release(release: GithubRelease) -> Result<ReleaseInfo> {
    let tag = release
        .tag_name
        .strip_prefix('v')
        .ok_or_else(|| anyhow!("release tag {:?} must start with v", release.tag_name))?;
    let version = Version::parse(tag)
        .with_context(|| format!("release tag {:?} is invalid", release.tag_name))?;
    if release.prerelease != !version.pre.is_empty() {
        bail!(
            "release {} has inconsistent prerelease metadata",
            release.tag_name
        );
    }
    if release.html_url.trim().is_empty() {
        bail!("release {} has no release URL", release.tag_name);
    }
    Ok(ReleaseInfo { release, version })
}

fn exact_asset<'a>(release: &'a GithubRelease, name: &str) -> Result<&'a ReleaseAsset> {
    let mut found = release.assets.iter().filter(|asset| asset.name == name);
    let asset = found
        .next()
        .ok_or_else(|| anyhow!("release {} has no {name} asset", release.tag_name))?;
    if found.next().is_some() {
        bail!(
            "release {} contains duplicate {name} assets",
            release.tag_name
        );
    }
    if asset.browser_download_url.trim().is_empty() {
        bail!("release asset {name} has no download URL");
    }
    Ok(asset)
}

async fn response_bytes(request: reqwest::RequestBuilder, limit: u64) -> Result<Vec<u8>> {
    let response = request.send().await?.error_for_status()?;
    if response.content_length().is_some_and(|size| size > limit) {
        bail!("HTTP response exceeds the {limit}-byte safety limit");
    }
    let mut output = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if (output.len() as u64)
            .checked_add(chunk.len() as u64)
            .is_none_or(|size| size > limit)
        {
            bail!("HTTP response exceeds the {limit}-byte safety limit");
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

fn expected_checksum(path: &Path, asset: &str) -> Result<String> {
    let content =
        String::from_utf8(read_limited(path, SUMS_LIMIT)?).context("SHA256SUMS is not UTF-8")?;
    let mut matches = content.lines().filter_map(|line| {
        let mut fields = line.split_ascii_whitespace();
        let digest = fields.next()?;
        let name = fields.next()?;
        (fields.next().is_none() && name.trim_start_matches('*') == asset)
            .then(|| digest.to_ascii_lowercase())
    });
    let digest = matches
        .next()
        .ok_or_else(|| anyhow!("SHA256SUMS has no entry for {asset}"))?;
    if matches.next().is_some() {
        bail!("SHA256SUMS contains duplicate entries for {asset}");
    }
    validate_digest(&digest)?;
    Ok(digest)
}

#[derive(Clone, Copy)]
struct Platform {
    triple: &'static str,
    os: &'static str,
    arch: &'static str,
    extension: &'static str,
    binary: &'static str,
}

impl Platform {
    fn current() -> Result<Self> {
        let platform = match (env::consts::OS, env::consts::ARCH) {
            ("linux", "x86_64") => Self::new(
                "x86_64-unknown-linux-gnu",
                "linux",
                "x86_64",
                "tar.gz",
                "pi",
            ),
            ("linux", "aarch64") => Self::new(
                "aarch64-unknown-linux-gnu",
                "linux",
                "aarch64",
                "tar.gz",
                "pi",
            ),
            ("macos", "x86_64") => {
                Self::new("x86_64-apple-darwin", "macos", "x86_64", "tar.gz", "pi")
            }
            ("macos", "aarch64") => {
                Self::new("aarch64-apple-darwin", "macos", "aarch64", "tar.gz", "pi")
            }
            ("windows", "x86_64") => Self::new(
                "x86_64-pc-windows-msvc",
                "windows",
                "x86_64",
                "zip",
                "pi.exe",
            ),
            (os, arch) => bail!("no release asset is published for {os}/{arch}"),
        };
        Ok(platform)
    }

    const fn new(
        triple: &'static str,
        os: &'static str,
        arch: &'static str,
        extension: &'static str,
        binary: &'static str,
    ) -> Self {
        Self {
            triple,
            os,
            arch,
            extension,
            binary,
        }
    }

    fn asset_name(self, version: &Version) -> String {
        format!("pi-rs-{version}-{}.{}", self.triple, self.extension)
    }

    fn installed_name(self, version: &Version, digest: &str) -> String {
        if cfg!(windows) {
            "pi.exe".to_owned()
        } else {
            format!("pi-rs-{version}-{}-{}-sha256-{digest}", self.os, self.arch)
        }
    }
}

struct ManagedInstall {
    root: PathBuf,
    executable: PathBuf,
    state_path: PathBuf,
    state_bytes: Vec<u8>,
    state: UpdateState,
}

impl ManagedInstall {
    fn detect(current: &Version) -> Result<Self> {
        let executable = env::current_exe()
            .context("locating the running pi executable")?
            .canonicalize()
            .context("resolving the running pi executable")?;
        let hinted = match env::var_os("PI_HOME") {
            Some(value) => absolute(PathBuf::from(value))?,
            None if cfg!(windows) => executable
                .parent()
                .filter(|parent| parent.file_name().is_some_and(|name| name == "bin"))
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .ok_or_else(unknown_install)?,
            None => executable
                .parent()
                .filter(|parent| parent.file_name().is_some_and(|name| name == "downloads"))
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .ok_or_else(unknown_install)?,
        };
        reject_symlink(&hinted, "install root")?;
        let root = hinted
            .canonicalize()
            .with_context(|| format!("resolving install root {}", hinted.display()))?;
        safe_directory(&root, "install root")?;
        safe_directory(&root.join("bin"), "bin directory")?;
        if !cfg!(windows) {
            safe_directory(&root.join("downloads"), "downloads directory")?;
        }
        let state_path = root.join("update-state.json");
        reject_symlink(&state_path, "update state")?;
        let state_bytes = read_limited(&state_path, SUMS_LIMIT)?;
        let state: UpdateState = serde_json::from_slice(&state_bytes)
            .with_context(|| format!("parsing update state {}", state_path.display()))?;
        validate_digest(&state.installed_sha256)?;
        if state.installed_version != current.to_string() {
            bail!(
                "update state records v{}, but running pi is v{current}; refusing an ambiguous install",
                state.installed_version
            );
        }
        let platform = Platform::current()?;
        if state.installed_asset != platform.asset_name(current) {
            bail!("update state asset does not match this platform");
        }
        #[cfg(unix)]
        validate_unix(&root, &executable, &state, platform)?;
        #[cfg(windows)]
        validate_windows(&root, &executable, &state)?;
        Ok(Self {
            root,
            executable,
            state_path,
            state_bytes,
            state,
        })
    }

    fn staged_path(&self) -> Result<PathBuf> {
        let directory = if cfg!(windows) {
            self.root.join("bin")
        } else {
            self.root.join("downloads")
        };
        unique(
            &directory,
            ".pi-stage",
            if cfg!(windows) { ".exe" } else { "" },
        )
    }
}

#[cfg(unix)]
fn validate_unix(
    root: &Path,
    executable: &Path,
    state: &UpdateState,
    platform: Platform,
) -> Result<()> {
    if !single_name(&state.installed_binary) {
        bail!("update state contains an invalid installed_binary");
    }
    let expected = format!(
        "pi-rs-{}-{}-{}-sha256-{}",
        state.installed_version, platform.os, platform.arch, state.installed_sha256
    );
    if state.installed_binary != expected {
        bail!("update state contains an unexpected versioned binary name");
    }
    let recorded = root.join("downloads").join(&state.installed_binary);
    reject_symlink(&recorded, "versioned binary")?;
    if recorded.canonicalize()? != executable {
        bail!("running executable does not match managed update state");
    }
    let active = root.join("bin/pi");
    if !fs::symlink_metadata(&active)?.file_type().is_symlink() {
        bail!("{} is not a managed symlink", active.display());
    }
    if fs::read_link(&active)? != PathBuf::from("../downloads").join(&state.installed_binary)
        || active.canonicalize()? != executable
    {
        bail!("active pi symlink does not match the managed install");
    }
    Ok(())
}

#[cfg(windows)]
fn validate_windows(root: &Path, executable: &Path, state: &UpdateState) -> Result<()> {
    if state.installed_binary != "pi.exe" {
        bail!("managed Windows update state must record pi.exe");
    }
    let active = root.join("bin/pi.exe");
    reject_symlink(&active, "active executable")?;
    if active.canonicalize()? != executable {
        bail!("running executable is not the managed pi.exe");
    }
    Ok(())
}

async fn extract_binary(archive: &Path, staged: &Path, platform: Platform) -> Result<()> {
    #[cfg(unix)]
    return extract_tar(archive, staged, platform.binary).await;
    #[cfg(windows)]
    return extract_zip(archive, staged, platform.binary).await;
}

#[cfg(unix)]
async fn extract_tar(archive: &Path, staged: &Path, binary: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let listing = unique(
        archive.parent().unwrap_or(Path::new(".")),
        ".archive",
        ".list",
    )?;
    let _listing_guard = RemovePath::new(listing.clone());
    let output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&listing)?;
    let status = Command::new("tar")
        .arg("-tzf")
        .arg(archive)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output))
        .stderr(Stdio::null())
        .status()
        .await
        .context("inspecting release tar archive")?;
    if !status.success() {
        bail!("failed to inspect release archive {}", archive.display());
    }
    let text = String::from_utf8(read_limited(&listing, SUMS_LIMIT)?)?;
    let mut members = text
        .lines()
        .filter(|member| member.strip_prefix("./").unwrap_or(member) == binary);
    let member = members
        .next()
        .ok_or_else(|| anyhow!("archive has no root-level {binary}"))?;
    if members.next().is_some() {
        bail!("archive contains duplicate root-level {binary} entries");
    }
    let output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(staged)?;
    let status = Command::new("tar")
        .arg("-xOzf")
        .arg(archive)
        .arg(member)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output))
        .stderr(Stdio::null())
        .status()
        .await
        .context("extracting release binary")?;
    if !status.success() {
        bail!("failed to extract {binary} from {}", archive.display());
    }
    let size = fs::metadata(staged)?.len();
    if size == 0 || size > FILE_LIMIT {
        bail!("archive contains an invalid-size {binary}");
    }
    fs::set_permissions(staged, fs::Permissions::from_mode(0o755))?;
    File::open(staged)?.sync_all()?;
    Ok(())
}

#[cfg(windows)]
async fn extract_zip(archive: &Path, staged: &Path, binary: &str) -> Result<()> {
    let script = unique(
        archive.parent().unwrap_or(Path::new(".")),
        ".extract",
        ".ps1",
    )?;
    let _script_guard = RemovePath::new(script.clone());
    fs::write(&script, ZIP_EXTRACTOR)?;
    let output = Command::new("powershell.exe")
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-File"])
        .arg(&script)
        .arg(archive)
        .arg(staged)
        .arg(binary)
        .stdin(Stdio::null())
        .output()
        .await
        .context("extracting release zip archive")?;
    if !output.status.success() {
        bail!(
            "zip extraction failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    File::open(staged)?.sync_all()?;
    Ok(())
}

#[cfg(windows)]
const ZIP_EXTRACTOR: &str = r#"param([string]$Archive,[string]$Output,[string]$Binary)
$ErrorActionPreference='Stop'; Add-Type -AssemblyName System.IO.Compression.FileSystem
$z=[IO.Compression.ZipFile]::OpenRead($Archive)
try {$e=@($z.Entries|Where-Object {$_.FullName -eq $Binary -or $_.FullName -eq ('./'+$Binary)}); if (@($z.Entries).Count -gt 4096 -or $e.Count -ne 1 -or $e[0].Length -le 0 -or $e[0].Length -gt 1073741824) {throw 'unsafe archive'}; [IO.Compression.ZipFileExtensions]::ExtractToFile($e[0],$Output,$false)} finally {$z.Dispose()}
"#;

async fn smoke(path: &Path, expected: &Version) -> Result<()> {
    let output = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .with_context(|| format!("running staged binary {}", path.display()))?;
    if !output.status.success() {
        bail!("downloaded binary failed its --version smoke test");
    }
    let version = String::from_utf8(output.stdout)?;
    if version.trim() != format!("pi {expected}") {
        bail!(
            "downloaded binary reported unexpected version {:?}",
            version.trim()
        );
    }
    Ok(())
}

#[cfg(unix)]
async fn activate(
    install: &ManagedInstall,
    staged: &Path,
    state: &UpdateState,
    expected: &Version,
    _lock: &mut InstallLock,
) -> Result<()> {
    use std::os::unix::fs::symlink;
    let destination = install.root.join("downloads").join(&state.installed_binary);
    reject_symlink_if_exists(&destination, "versioned binary")?;
    let existed = destination.exists();
    let active = install.root.join("bin/pi");
    let old_target = fs::read_link(&active)?;
    let temporary = unique(&install.root.join("bin"), ".pi-activate", "")?;
    let mut temporary_guard = RemovePath::new(temporary.clone());
    symlink(
        PathBuf::from("../downloads").join(&state.installed_binary),
        &temporary,
    )?;
    fs::rename(staged, &destination).context("atomically installing versioned binary")?;
    sync_parent(&destination)?;
    if let Err(error) = fs::rename(&temporary, &active).context("atomically activating pi") {
        if !existed {
            let _ = fs::remove_file(&destination);
        }
        return Err(error);
    }
    temporary_guard.disarm();
    sync_parent(&active)?;
    if let Err(error) = smoke(&active, expected).await {
        return Err(rollback_context(
            error,
            rollback_unix(install, &active, &old_target, &destination, existed),
        ));
    }
    if let Err(error) = write_state_atomic(&install.state_path, state) {
        let binary = rollback_unix(install, &active, &old_target, &destination, existed);
        let metadata = restore_state(&install.state_path, &install.state_bytes);
        return Err(rollback_context(error, combine(binary, metadata)));
    }
    Ok(())
}

#[cfg(unix)]
fn rollback_unix(
    install: &ManagedInstall,
    active: &Path,
    old_target: &Path,
    destination: &Path,
    existed: bool,
) -> Result<()> {
    use std::os::unix::fs::symlink;
    let rollback = unique(&install.root.join("bin"), ".pi-rollback", "")?;
    let mut rollback_guard = RemovePath::new(rollback.clone());
    symlink(old_target, &rollback)?;
    fs::rename(&rollback, active)?;
    rollback_guard.disarm();
    if fs::read_link(active)? != old_target {
        bail!("rollback restored the wrong active link");
    }
    if !existed {
        fs::remove_file(destination)?;
        if destination.exists() {
            bail!("rollback did not remove the new versioned binary");
        }
    }
    sync_parent(active)
}

#[cfg(windows)]
async fn activate(
    install: &ManagedInstall,
    staged: &Path,
    state: &UpdateState,
    _expected: &Version,
    lock: &mut InstallLock,
) -> Result<()> {
    let backup = unique(&install.root.join("bin"), ".pi-backup", ".exe")?;
    let state_new = unique(&install.root, ".update-state", ".json")?;
    fs::copy(&install.executable, &backup).context("backing up active pi.exe")?;
    write_state_file(&state_new, state)?;
    lock.arm_windows_activation(WindowsActivationCommand {
        action: "activate",
        staged: staged.to_path_buf(),
        destination: install.executable.clone(),
        backup,
        state_new,
        state_path: install.state_path.clone(),
        status_path: install.root.join("last-update-result.json"),
    })?;
    Ok(())
}

#[cfg(windows)]
const WINDOWS_LOCKER: &str = r#"param([string]$Root,[int]$ParentId,[string]$Command,[string]$Ready,[string]$Script)
$ErrorActionPreference='Stop'
$mutexName='Local\pi-rs-install-' + ($Root -replace '[\\/:]','_')
$mutex=New-Object Threading.Mutex($false,$mutexName)
try {
 [void]$mutex.WaitOne()
 [IO.File]::WriteAllText($Ready,'ready'+[Environment]::NewLine,[Text.UTF8Encoding]::new($false))
 while (-not (Test-Path -LiteralPath $Command)) {
  if (-not (Get-Process -Id $ParentId -ErrorAction SilentlyContinue)) { return }
  Start-Sleep -Milliseconds 50
 }
 $config=Get-Content -LiteralPath $Command -Raw | ConvertFrom-Json
 if ($config.action -ne 'activate') { return }
 Wait-Process -Id $ParentId -ErrorAction SilentlyContinue
 Add-Type -TypeDefinition @'
using System; using System.Runtime.InteropServices; public static class PiMove {[DllImport("kernel32.dll",SetLastError=true,CharSet=CharSet.Unicode)] public static extern bool MoveFileEx(string a,string b,int f);}
'@
 try {
  if (-not [PiMove]::MoveFileEx($config.staged,$config.destination,1)) { throw 'atomic executable activation failed' }
  & $config.destination --version *> $null
  if ($LASTEXITCODE -ne 0) {
   if (-not [PiMove]::MoveFileEx($config.backup,$config.destination,1)) { throw 'binary failed and rollback failed' }
   & $config.destination --version *> $null
   if ($LASTEXITCODE -ne 0) { throw 'rollback verification failed' }
   throw 'new binary failed; previous binary restored and verified'
  }
  if (-not [PiMove]::MoveFileEx($config.state_new,$config.state_path,1)) {
   if (-not [PiMove]::MoveFileEx($config.backup,$config.destination,1)) { throw 'state commit and rollback failed' }
   & $config.destination --version *> $null
   if ($LASTEXITCODE -ne 0) { throw 'rollback verification failed' }
   throw 'state commit failed; previous binary restored and verified'
  }
  Remove-Item -LiteralPath $config.backup,$config.status_path -Force -ErrorAction SilentlyContinue
 } catch {
  $result=@{ok=$false;error=$_.Exception.Message} | ConvertTo-Json -Compress
  [IO.File]::WriteAllText($config.status_path,$result+[Environment]::NewLine,[Text.UTF8Encoding]::new($false))
 }
} finally {
 Remove-Item -LiteralPath $Command,$Ready,$Script -Force -ErrorAction SilentlyContinue
 try {$mutex.ReleaseMutex()} catch {}
 $mutex.Dispose()
}
"#;

#[cfg(windows)]
const WINDOWS_ACTIVATOR: &str = r#"param([int]$ParentId,[string]$New,[string]$Dest,[string]$Backup,[string]$NewState,[string]$State,[string]$Script)
$ErrorActionPreference='Stop'; Wait-Process -Id $ParentId -ErrorAction SilentlyContinue
Add-Type -TypeDefinition @'
using System; using System.Runtime.InteropServices; public static class PiMove {[DllImport("kernel32.dll",SetLastError=true,CharSet=CharSet.Unicode)] public static extern bool MoveFileEx(string a,string b,int f);}
'@
$log=Join-Path (Split-Path -Parent $State) 'last-update-error.log'
try {if (-not [PiMove]::MoveFileEx($New,$Dest,1)){throw 'activation failed'}; & $Dest --version *> $null; if ($LASTEXITCODE -ne 0){if (-not [PiMove]::MoveFileEx($Backup,$Dest,1)){throw 'rollback failed'}; & $Dest --version *> $null; if ($LASTEXITCODE -ne 0){throw 'rollback verification failed'}; throw 'new binary failed; previous binary restored'}; if (-not [PiMove]::MoveFileEx($NewState,$State,1)){if (-not [PiMove]::MoveFileEx($Backup,$Dest,1)){throw 'state commit and rollback failed'}; & $Dest --version *> $null; if ($LASTEXITCODE -ne 0){throw 'rollback verification failed'}; throw 'state commit failed; previous binary restored'}; Remove-Item -LiteralPath $Backup,$log -Force -ErrorAction SilentlyContinue} catch {[IO.File]::WriteAllText($log,$_.Exception.Message+[Environment]::NewLine,[Text.UTF8Encoding]::new($false))} finally {Remove-Item -LiteralPath $New,$NewState,$Script -Force -ErrorAction SilentlyContinue}
"#;

fn write_state_atomic(path: &Path, state: &UpdateState) -> Result<()> {
    let temporary = unique(
        path.parent().unwrap_or(Path::new(".")),
        ".update-state",
        ".json",
    )?;
    let mut guard = RemovePath::new(temporary.clone());
    write_state_file(&temporary, state)?;
    fs::rename(&temporary, path).context("atomically replacing update state")?;
    guard.disarm();
    sync_parent(path)
}

fn write_state_file(path: &Path, state: &UpdateState) -> Result<()> {
    let mut content = serde_json::to_vec_pretty(state)?;
    content.push(b'\n');
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(&content)?;
    file.sync_all()?;
    Ok(())
}

fn restore_state(path: &Path, content: &[u8]) -> Result<()> {
    let temporary = unique(
        path.parent().unwrap_or(Path::new(".")),
        ".update-state-rollback",
        ".json",
    )?;
    let mut guard = RemovePath::new(temporary.clone());
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(content)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)?;
    guard.disarm();
    sync_parent(path)?;
    if fs::read(path)? != content {
        bail!("restored update state does not match previous bytes");
    }
    Ok(())
}

fn rollback_context(error: anyhow::Error, rollback: Result<()>) -> anyhow::Error {
    match rollback {
        Ok(()) => error.context("self-update failed; previous install restored and verified"),
        Err(rollback) => error.context(format!(
            "self-update failed and rollback was incomplete: {rollback:#}"
        )),
    }
}

fn combine(first: Result<()>, second: Result<()>) -> Result<()> {
    match (first, second) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(first), Err(second)) => Err(anyhow!("{first:#}; {second:#}")),
    }
}

fn check_deferred_update_result(root: &Path) -> Result<()> {
    let path = root.join("last-update-result.json");
    match fs::read(&path) {
        Ok(bytes) => {
            let result: DeferredUpdateResult = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing deferred update result {}", path.display()))?;
            fs::remove_file(&path)
                .with_context(|| format!("removing deferred update result {}", path.display()))?;
            if !result.ok {
                bail!("previous Windows self-update failed: {}", result.error);
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("reading deferred update result {}", path.display()))
        }
    }
}

fn windows_mutex_name(root: &Path) -> String {
    let sanitized = root
        .to_string_lossy()
        .chars()
        .map(|character| match character {
            '\\' | '/' | ':' => '_',
            other => other,
        })
        .collect::<String>();
    format!("Local\\pi-rs-install-{sanitized}")
}

fn windows_activation_payload(command: &WindowsActivationCommand) -> Result<Vec<u8>> {
    let mut bytes =
        serde_json::to_vec(command).context("serializing Windows activation command")?;
    bytes.push(b'\n');
    Ok(bytes)
}

#[cfg(windows)]
struct WindowsLockHandoff {
    command_path: PathBuf,
    ready_path: PathBuf,
    script_path: PathBuf,
    child: Option<tokio::process::Child>,
    handed_off: bool,
}

#[cfg(windows)]
impl WindowsLockHandoff {
    async fn acquire(root: &Path) -> Result<Self> {
        let command_path = unique(root, ".windows-install-command", ".json")?;
        let ready_path = unique(root, ".windows-install-ready", "")?;
        let script_path = unique(root, ".windows-install-lock", ".ps1")?;
        fs::write(&script_path, WINDOWS_LOCKER).with_context(|| {
            format!(
                "writing Windows install lock helper {}",
                script_path.display()
            )
        })?;
        let child = Command::new("powershell.exe")
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-File"])
            .arg(&script_path)
            .arg(root)
            .arg(std::process::id().to_string())
            .arg(&command_path)
            .arg(&ready_path)
            .arg(&script_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("starting Windows install lock helper")?;
        let deadline = tokio::time::Instant::now() + LOCK_TIMEOUT;
        while !ready_path.exists() {
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "timed out acquiring Windows install mutex {}",
                    windows_mutex_name(root)
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(Self {
            command_path,
            ready_path,
            script_path,
            child: Some(child),
            handed_off: false,
        })
    }

    fn activate(&mut self, command: &WindowsActivationCommand) -> Result<()> {
        let bytes = windows_activation_payload(command)?;
        let temporary = unique(
            self.command_path.parent().unwrap_or(Path::new(".")),
            ".windows-install-command-staged",
            ".json",
        )?;
        let mut guard = RemovePath::new(temporary.clone());
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .with_context(|| {
                format!(
                    "creating Windows activation command {}",
                    temporary.display()
                )
            })?;
        file.write_all(&bytes).with_context(|| {
            format!("writing Windows activation command {}", temporary.display())
        })?;
        file.sync_all().with_context(|| {
            format!("syncing Windows activation command {}", temporary.display())
        })?;
        drop(file);
        fs::rename(&temporary, &self.command_path).with_context(|| {
            format!(
                "publishing Windows activation command {}",
                self.command_path.display()
            )
        })?;
        guard.disarm();
        sync_parent(&self.command_path)?;
        self.handed_off = true;
        let _ = self.child.take();
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for WindowsLockHandoff {
    fn drop(&mut self) {
        if !self.handed_off {
            if let Some(child) = self.child.as_mut() {
                let _ = child.start_kill();
            }
            let _ = fs::remove_file(&self.command_path);
            let _ = fs::remove_file(&self.ready_path);
            let _ = fs::remove_file(&self.script_path);
        }
    }
}

struct InstallLock {
    #[cfg(unix)]
    path: PathBuf,
    #[cfg(unix)]
    owner: String,
    #[cfg(windows)]
    handoff: Option<WindowsLockHandoff>,
}

impl InstallLock {
    async fn acquire(root: &Path) -> Result<Self> {
        #[cfg(windows)]
        {
            return Ok(Self {
                handoff: Some(WindowsLockHandoff::acquire(root).await?),
            });
        }
        #[cfg(unix)]
        {
            let path = root.join(".install.lock");
            reject_symlink_if_exists(&path, "install lock")?;
            let owner = std::process::id().to_string();
            let deadline = tokio::time::Instant::now() + LOCK_TIMEOUT;
            loop {
                match OpenOptions::new().create_new(true).write(true).open(&path) {
                    Ok(mut file) => {
                        writeln!(file, "{owner}")?;
                        file.sync_all()?;
                        return Ok(Self { path, owner });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        reject_symlink_if_exists(&path, "install lock")?;
                        if stale_lock(&path)? {
                            fs::remove_file(&path).context("removing stale install lock")?;
                            continue;
                        }
                        if tokio::time::Instant::now() >= deadline {
                            bail!("timed out waiting for another pi install");
                        }
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                    Err(error) => return Err(error).context("creating install lock"),
                }
            }
        }
    }

    #[cfg(windows)]
    fn arm_windows_activation(&mut self, command: WindowsActivationCommand) -> Result<()> {
        self.handoff
            .as_mut()
            .ok_or_else(|| anyhow!("Windows install mutex handoff is unavailable"))?
            .activate(&command)
    }
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        if fs::read_to_string(&self.path)
            .ok()
            .is_some_and(|value| value.trim() == self.owner)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}
#[cfg(unix)]
fn stale_lock(path: &Path) -> Result<bool> {
    let modified = fs::metadata(path)?.modified()?;
    Ok(SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age >= STALE_LOCK_AGE))
}

struct ScratchDir {
    path: PathBuf,
}
impl ScratchDir {
    fn new(root: &Path) -> Result<Self> {
        let path = unique(root, ".pi-update", "")?;
        fs::create_dir(&path)?;
        Ok(Self { path })
    }
}
impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct RemovePath {
    path: PathBuf,
    armed: bool,
}
impl RemovePath {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}
impl Drop for RemovePath {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn absolute(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env::current_dir()?.join(path))
    }
}
fn unique(directory: &Path, prefix: &str, suffix: &str) -> Result<PathBuf> {
    for _ in 0..128 {
        let sequence = UNIQUE.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(
            "{prefix}.{}.{}.{sequence}{suffix}",
            std::process::id(),
            timestamp()?
        ));
        if !path.exists() {
            return Ok(path);
        }
    }
    bail!(
        "could not allocate a unique path under {}",
        directory.display()
    )
}

fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let size = file.metadata()?.len();
    if size > limit {
        bail!("{} exceeds the {limit}-byte safety limit", path.display());
    }
    let mut content = Vec::with_capacity(size as usize);
    file.read_to_end(&mut content)?;
    Ok(content)
}

fn safe_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting {label} at {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("refusing unsafe {label}: {}", path.display());
    }
    Ok(())
}

fn reject_symlink(path: &Path, label: &str) -> Result<()> {
    if fs::symlink_metadata(path)
        .with_context(|| format!("inspecting {label} at {}", path.display()))?
        .file_type()
        .is_symlink()
    {
        bail!("refusing symlinked {label}: {}", path.display());
    }
    Ok(())
}

fn reject_symlink_if_exists(path: &Path, label: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("refusing symlinked {label}: {}", path.display())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context(format!("inspecting {label}")),
    }
}

fn single_name(value: &str) -> bool {
    let mut components = Path::new(value).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

fn validate_digest(digest: &str) -> Result<()> {
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid SHA-256 digest");
    }
    Ok(())
}

fn sync_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn offline() -> bool {
    crate::session_run::offline()
}

fn api_child(url: &str, base: &str) -> bool {
    url == base
        || url
            .strip_prefix(base)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn changelog_summary(body: Option<&str>) -> Option<String> {
    let line = body?
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("```") && *line != "---")?
        .trim_start_matches('#')
        .trim();
    if line.is_empty() {
        return None;
    }
    let mut summary = line.chars().take(160).collect::<String>();
    if line.chars().count() > 160 {
        summary.push('…');
    }
    Some(summary)
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn timestamp() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .context("system clock is before Unix epoch")
}

fn unknown_install() -> anyhow::Error {
    anyhow!(
        "running pi is not an installer-managed ~/.pi-rs binary; refusing to overwrite an unknown install method (use install.sh or install.ps1)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn Windows_mutex_name_matches_installer_replacement() {
        assert_eq!(
            windows_mutex_name(Path::new(r"C:\Users\pi\.pi-rs")),
            "Local\\pi-rs-install-C__Users_pi_.pi-rs"
        );
    }

    #[test]
    fn Windows_activation_payload_is_json_without_secrets_or_shell_quoting() {
        let command = WindowsActivationCommand {
            action: "activate",
            staged: PathBuf::from(r"C:\path with spaces\pi.new.exe"),
            destination: PathBuf::from(r"C:\path with spaces\pi.exe"),
            backup: PathBuf::from(r"C:\path with spaces\pi.backup.exe"),
            state_new: PathBuf::from(r"C:\path with spaces\state.new.json"),
            state_path: PathBuf::from(r"C:\path with spaces\update-state.json"),
            status_path: PathBuf::from(r"C:\path with spaces\last-update-result.json"),
        };
        let bytes = windows_activation_payload(&command).expect("serialize command");
        assert_eq!(bytes.last(), Some(&b'\n'));
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("parse command");
        assert_eq!(value["action"], "activate");
        assert_eq!(value["destination"], r"C:\path with spaces\pi.exe");
    }

    #[test]
    fn stable_selection_and_download_timeouts_are_separate() {
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(10));
        assert!(DOWNLOAD_IDLE_TIMEOUT > REQUEST_TIMEOUT);
    }
}
