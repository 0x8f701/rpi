//! Agent Team Core — gather multiple runners (`rpi --listen` instances) into
//! one project as a durable team.
//!
//! A team is `{ project, members: [runner] }`. Members are remote runner
//! endpoints the core can probe and drive through the same JSONL command
//! envelope the Web chat client uses. Team definitions and member lists live
//! in `<agent-dir>/teams/registry.json`; a member's runner token is a
//! credential and is stored alone in an owner-only
//! `<agent-dir>/teams/tokens/<team-id>/<runner-id>.token` file — never in the
//! registry, command output, or logs.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// Hard bound for a single runner command round-trip.
pub const TEAM_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

/// Hard bound for the registry JSON document size (defense against a corrupt
/// or adversarial team file).
const TEAM_REGISTRY_MAX_BYTES: u64 = 64 * 1024;
/// Registry document version; bumping it rejects older layouts explicitly.
const TEAM_REGISTRY_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamProject {
    pub label: String,
    pub cwd: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMember {
    pub runner_id: String,
    /// `ws://host:port` (or `wss://`) of the member's `--listen` endpoint.
    pub origin: String,
    pub label: String,
    pub role: Option<String>,
    pub added_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Team {
    pub id: String,
    pub name: String,
    pub project: TeamProject,
    pub members: Vec<TeamMember>,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TeamDocument {
    version: u32,
    teams: Vec<Team>,
}

/// Result of a live probe against one member.
#[derive(Clone, Debug)]
pub struct RunnerProbe {
    pub session_id: Option<String>,
    pub cwd: String,
    pub session_count: usize,
}

/// The runner-command envelope the core forwards to a member. Mirrors the
/// Web chat protocol (`web/src/chat/protocol.ts`) so any client of either
/// surface drives the same runner surface.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TeamRunnerCommand {
    GetState,
    GetMessages,
    SessionList { scope: TeamSessionScope },
    NewSession { cwd: String },
    SetSessionName {
        name: String,
        #[serde(rename = "sessionPath")]
        session_path: String,
    },
    SessionDelete {
        #[serde(rename = "sessionPath")]
        session_path: String,
    },
    SwitchSession {
        #[serde(rename = "sessionPath")]
        session_path: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamSessionScope {
    Current,
    AllProjects,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

/// Durable team registry rooted at `<agent-dir>/teams`. Registry mutations are
/// synchronous and serialized through an internal mutex; runner I/O is async
/// and never touches the registry lock.
pub struct TeamRegistry {
    registry: PathBuf,
    tokens_dir: PathBuf,
    state: Mutex<TeamDocument>,
}

impl TeamRegistry {
    /// Open (or create) the registry under `agent_dir/teams`.
    pub fn open(agent_dir: &Path) -> Result<Self> {
        let root = agent_dir.join("teams");
        fs::create_dir_all(&root)
            .with_context(|| format!("creating team root {}", root.display()))?;
        let registry = root.join("registry.json");
        let tokens_dir = root.join("tokens");
        fs::create_dir_all(&tokens_dir)
            .with_context(|| format!("creating team token root {}", tokens_dir.display()))?;
        let state = match fs::read(&registry) {
            Ok(bytes) if bytes.len() as u64 <= TEAM_REGISTRY_MAX_BYTES => {
                let document: TeamDocument = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing team registry {}", registry.display()))?;
                if document.version != TEAM_REGISTRY_VERSION {
                    bail!(
                        "team registry {} has unsupported version {}",
                        registry.display(),
                        document.version
                    );
                }
                document
            }
            Ok(bytes) => bail!(
                "team registry {} exceeds {} bytes",
                registry.display(),
                TEAM_REGISTRY_MAX_BYTES
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => TeamDocument {
                version: TEAM_REGISTRY_VERSION,
                teams: Vec::new(),
            },
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading team registry {}", registry.display()));
            }
        };
        Ok(Self {
            registry,
            tokens_dir,
            state: Mutex::new(state),
        })
    }

    fn save(&self) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&*self.state.lock().expect("team registry lock"))
            .context("serializing team registry")?;
        write_file_atomic_private(&self.registry, &bytes)
    }

    pub fn list(&self) -> Vec<Team> {
        self.state.lock().expect("team registry lock").teams.clone()
    }

    pub fn team(&self, team_id: &str) -> Result<Team> {
        self.state
            .lock()
            .expect("team registry lock")
            .teams
            .iter()
            .find(|team| team.id == team_id)
            .cloned()
            .ok_or_else(|| anyhow!("team {team_id} was not found"))
    }

    pub fn create_team(&self, name: &str, project_label: &str, project_cwd: &str) -> Result<Team> {
        let name = name.trim();
        if name.is_empty() {
            bail!("team name must not be empty");
        }
        let mut document = self.state.lock().expect("team registry lock");
        if document.teams.iter().any(|team| team.name == name) {
            bail!("team name '{name}' already exists");
        }
        let team = Team {
            id: format!("team-{}", uuid::Uuid::now_v7()),
            name: name.to_owned(),
            project: TeamProject {
                label: project_label.trim().to_owned(),
                cwd: project_cwd.trim().to_owned(),
            },
            members: Vec::new(),
            created_at_ms: now_ms(),
        };
        document.teams.push(team.clone());
        drop(document);
        self.save().context("persisting team creation")?;
        Ok(team)
    }

    pub fn remove_team(&self, team_id: &str) -> Result<()> {
        let mut document = self.state.lock().expect("team registry lock");
        let before = document.teams.len();
        document.teams.retain(|team| team.id != team_id);
        if document.teams.len() == before {
            bail!("team {team_id} was not found");
        }
        drop(document);
        self.save().context("persisting team removal")?;
        let tokens = self.tokens_dir.join(team_id);
        if tokens.exists() {
            fs::remove_dir_all(&tokens)
                .with_context(|| format!("removing team tokens {}", tokens.display()))?;
        }
        Ok(())
    }

    /// Add a runner endpoint to a team. The token is a credential: it is
    /// written to an owner-only per-runner file and never kept in the
    /// registry document.
    pub fn join(
        &self,
        team_id: &str,
        origin: &str,
        token: &str,
        label: &str,
        role: Option<&str>,
    ) -> Result<TeamMember> {
        let origin = validate_origin(origin)?;
        let token = token.trim();
        if token.is_empty() {
            bail!("runner token must not be empty");
        }
        let label = label.trim();
        if label.is_empty() {
            bail!("runner label must not be empty");
        }
        let member = TeamMember {
            runner_id: format!("runner-{}", uuid::Uuid::now_v7()),
            origin: origin.clone(),
            label: label.to_owned(),
            role: role
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            added_at_ms: now_ms(),
        };
        let team = self.team(team_id)?;
        if team
            .members
            .iter()
            .any(|existing| existing.origin == member.origin)
        {
            bail!("runner {origin} is already a member of team {team_id}");
        }
        let path = self.token_path(team_id, &member.runner_id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        write_file_atomic_private(&path, format!("{token}\n").as_bytes())
            .with_context(|| format!("storing runner token {}", path.display()))?;
        {
            let mut document = self.state.lock().expect("team registry lock");
            document
                .teams
                .iter_mut()
                .find(|team| team.id == team_id)
                .ok_or_else(|| anyhow!("team {team_id} was not found"))?
                .members
                .push(member.clone());
        }
        self.save().context("persisting team member")?;
        Ok(member)
    }

    /// Remove a member and its stored token.
    pub fn leave(&self, team_id: &str, runner_id: &str) -> Result<()> {
        let mut removed = false;
        {
            let mut document = self.state.lock().expect("team registry lock");
            let Some(team) = document.teams.iter_mut().find(|team| team.id == team_id) else {
                bail!("team {team_id} was not found");
            };
            let before = team.members.len();
            team.members.retain(|member| member.runner_id != runner_id);
            removed = team.members.len() != before;
        }
        if !removed {
            bail!("runner {runner_id} is not a member of team {team_id}");
        }
        self.save().context("persisting team member removal")?;
        let path = self.token_path(team_id, runner_id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("removing runner token {}", path.display()))
            }
        }
    }

    fn token_path(&self, team_id: &str, runner_id: &str) -> PathBuf {
        self.tokens_dir
            .join(team_id)
            .join(format!("{runner_id}.token"))
    }

    pub(crate) fn runner_token(&self, team_id: &str, runner_id: &str) -> Result<String> {
        let path = self.token_path(team_id, runner_id);
        let token = fs::read_to_string(&path)
            .with_context(|| format!("reading runner token {}", path.display()))?
            .trim()
            .to_owned();
        if token.is_empty() {
            bail!("runner token file {} is empty", path.display());
        }
        Ok(token)
    }

    /// Member plus its stored token, resolved for a live operation.
    pub(crate) fn member_with_token(
        &self,
        team_id: &str,
        runner_id: &str,
    ) -> Result<(TeamMember, String)> {
        let team = self.team(team_id)?;
        let member = team
            .members
            .iter()
            .find(|member| member.runner_id == runner_id)
            .cloned()
            .ok_or_else(|| anyhow!("runner {runner_id} is not a member of team {team_id}"))?;
        let token = self.runner_token(team_id, runner_id)?;
        Ok((member, token))
    }
}

fn validate_origin(raw: &str) -> Result<String> {
    let raw = raw.trim();
    let (scheme, rest) = raw
        .split_once("://")
        .ok_or_else(|| anyhow!("origin must be ws://host:port (or wss://)"))?;
    if !matches!(scheme, "ws" | "wss") {
        bail!("origin scheme must be ws or wss");
    }
    if rest.is_empty()
        || rest.contains('/')
        || rest.contains('?')
        || rest.contains('#')
        || rest.contains('@')
    {
        bail!("origin must be ws(s)://host[:port] with no path, query, fragment, or credentials");
    }
    Ok(format!("{scheme}://{rest}"))
}

/// Control-plane WebSocket URL for a stored runner origin.
///
/// Mirrors `parseRunnerEndpoint` in `web/src/chat/auth.ts`: an HTTP(S) or
/// WS(S) origin becomes `ws(s)://{host}/ws`. Listeners only upgrade `/ws`
/// (`listen.rs`); `/chat` is a 404, which is why `team_status` reported every
/// member offline.
fn runner_websocket_url(origin: &str) -> Result<String> {
    let origin = origin.trim();
    let (scheme, rest) = origin
        .split_once("://")
        .ok_or_else(|| anyhow!("origin must be ws://host:port (or wss://)"))?;
    let ws_scheme = match scheme {
        "ws" | "http" => "ws",
        "wss" | "https" => "wss",
        _ => bail!("origin scheme must be ws, wss, http, or https"),
    };
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim();
    if host.is_empty() || host.contains('@') {
        bail!("origin must be a host[:port] with no credentials");
    }
    Ok(format!("{ws_scheme}://{host}/ws"))
}

/// Atomic owner-only write: write to a temp sibling then rename into place.
/// On Unix the file is created 0600 (and the parent directory 0700 when this
/// call creates it), matching the auth.json secret-at-rest policy.
pub(crate) fn write_file_atomic_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("securing {}", parent.display()))?;
        }
    }
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::now_v7()));
    {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        std::io::Write::write_all(&mut file, bytes)
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.sync_all().context("syncing team file")?;
    }
    fs::rename(&tmp, path).with_context(|| format!("finalizing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("securing {}", path.display()))?;
    }
    Ok(())
}

/// Connect to a runner's `/ws` WebSocket with the `rpi-auth.<token>`
/// subprotocol (the same handshake the Web chat client uses) and return the
/// framed socket.
async fn connect_runner(
    origin: &str,
    token: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
> {
    let url = runner_websocket_url(origin)?;
    let mut request = url
        .into_client_request()
        .context("building runner request")?;
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        format!("rpi-auth.{token}")
            .parse()
            .context("runner token contains invalid header characters")?,
    );
    let socket = connect_runner_request(request)
        .await
        .with_context(|| format!("connecting to runner {origin}"))?;
    Ok(socket)
}

/// Dial a prepared runner request, bounded by [`TEAM_COMMAND_TIMEOUT`] so a
/// black-holed host (SYN dropped) cannot stall a probe for the OS TCP
/// timeout. Split out for tests, which drive it over an in-memory stream.
async fn connect_runner_request(
    request: http::Request<()>,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
> {
    let (socket, _) = tokio::time::timeout(
        TEAM_COMMAND_TIMEOUT,
        tokio_tungstenite::connect_async(request),
    )
    .await
    .context("connecting to runner timed out")??;
    Ok(socket)
}

/// Read JSONL frames until a `response` with the given `id` arrives,
/// returning its `data` object.
async fn wait_for_response<S>(socket: &mut S, want_id: &str) -> Result<serde_json::Value>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin,
{
    let deadline = tokio::time::Instant::now() + TEAM_COMMAND_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        let frame = tokio::time::timeout_at(deadline, socket.next())
            .await
            .context("runner command timed out")?
            .ok_or_else(|| anyhow!("runner closed the chat stream"))?
            .context("runner stream failed")?;
        let Message::Text(text) = frame else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if value.get("type").and_then(serde_json::Value::as_str) == Some("response")
            && value.get("id").and_then(serde_json::Value::as_str) == Some(want_id)
        {
            return Ok(value
                .get("data")
                .cloned()
                .unwrap_or(serde_json::Value::Null));
        }
    }
    bail!("runner command {want_id} timed out")
}

/// Probe a member's live state: current session id, cwd, and session count.
pub async fn probe_runner(origin: &str, token: &str) -> Result<RunnerProbe> {
    let mut socket = connect_runner(origin, token).await?;
    socket
        .send(Message::Text(
            r#"{"type":"get_state","id":"team-probe-state"}"#.into(),
        ))
        .await
        .context("sending get_state")?;
    let state = wait_for_response(&mut socket, "team-probe-state").await?;
    let session_id = state
        .get("sessionId")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let cwd = state
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    // The listener's multi-session gate requires a top-level `sessionId` on
    // every command after the one-shot get_state bootstrap; without it the
    // session_list round-trip is rejected and session_count reads 0.
    let sessions = match &session_id {
        Some(session_id) => {
            let payload = serde_json::json!({
                "type": "session_list",
                "id": "team-probe-sessions",
                "scope": "all_projects",
                "sessionId": session_id,
            });
            socket
                .send(Message::Text(payload.to_string().into()))
                .await
                .context("sending session_list")?;
            wait_for_response(&mut socket, "team-probe-sessions").await?
        }
        None => serde_json::Value::Null,
    };
    let session_count = sessions
        .get("sessions")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    Ok(RunnerProbe {
        session_id,
        cwd,
        session_count,
    })
}

/// Forward one runner command and return its `data` object.
///
/// The listener's multi-session gate rejects session-less commands after the
/// one-shot get_state bootstrap, so every non-`GetState` command must carry
/// the owning `sessionId` (obtained via [`probe_runner`]).
pub async fn run_runner_command(
    origin: &str,
    token: &str,
    command: &TeamRunnerCommand,
    command_id: &str,
) -> Result<serde_json::Value> {
    let mut socket = connect_runner(origin, token).await?;
    let mut payload = serde_json::to_value(command).context("serializing runner command")?;
    payload["id"] = serde_json::Value::String(command_id.to_owned());
    if !matches!(command, TeamRunnerCommand::GetState) {
        let session_id = probe_session_id(&mut socket).await?;
        payload["sessionId"] = serde_json::Value::String(session_id);
    }
    socket
        .send(Message::Text(serde_json::to_string(&payload)?.into()))
        .await
        .context("sending runner command")?;
    wait_for_response(&mut socket, command_id).await
}

/// Bootstrap a fresh connection with `get_state` and return the runner's
/// current `sessionId`, which the listener requires on every later command.
async fn probe_session_id<S>(socket: &mut S) -> Result<String>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin,
{
    socket
        .send(Message::Text(
            r#"{"type":"get_state","id":"team-command-bootstrap"}"#.into(),
        ))
        .await
        .context("sending get_state bootstrap")?;
    let state = wait_for_response(socket, "team-command-bootstrap").await?;
    state
        .get("sessionId")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("runner did not report a sessionId"))
}

/// Live status for every member of a team: probe failures are returned as
/// per-member errors, never fatal for the whole team.
pub async fn team_status(
    registry: &TeamRegistry,
    team: &Team,
) -> Vec<(TeamMember, Result<RunnerProbe>)> {
    let team_id = team.id.clone();
    let members = team.members.clone();
    futures_util::stream::iter(members.into_iter().map(move |member| {
        let token = registry.runner_token(&team_id, &member.runner_id);
        async move {
            let probe = match token {
                Ok(token) => probe_runner(&member.origin, &token).await,
                Err(error) => Err(error),
            };
            (member, probe)
        }
    }))
    .buffer_unordered(TEAM_LIVENESS_CONCURRENCY)
    .collect::<Vec<_>>()
    .await
}

/// One mDNS-discovered runner endpoint on the LAN.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredRunner {
    /// `ws://host:port` (or `wss://`) origin accepted by [`TeamRegistry::join`].
    pub origin: String,
    pub host: String,
    pub port: u16,
    /// DNS-SD instance fullname (`<label>._rpi._tcp.local.`), used to match
    /// `ServiceRemoved` events back to an entry. Stored verbatim — never the
    /// first dotted label, which collides for every `rpi-<ipv4>` host.
    pub instance: String,
    pub label: String,
    pub version: Option<String>,
    /// Advertised transport scheme (`ws` or `wss`). The Web chat UI maps
    /// `wss` → `https` / `ws` → `http` before calling [`parseRunnerEndpoint`].
    pub scheme: String,
    /// Display-only codename from the optional mDNS TXT `codename` property
    /// (D2 grammar; already-valid names preserved byte-for-byte). `None` when
    /// absent (old listeners) or malformed — a malformed codename downgrades,
    /// never rejects the row.
    pub codename: Option<String>,
    pub last_seen_ms: u64,
    pub(crate) failed_probes: u8,
    /// First wall-clock ms of the current bounded-removal window. `None`
    /// on insert and resurrection; opened by the first dead verdict or a
    /// trusted `ServiceRemoved` goodbye (which also seeds two strikes
    /// without removing the row), so removal never happens before
    /// [`TEAM_REMOVAL_GRACE`] has elapsed past this instant. A
    /// re-advertisement preserves an open window — only the `/web` alive
    /// verdict resets it.
    pub(crate) failed_since_ms: Option<u64>,
}

impl DiscoveredRunner {
    /// HTTP(S) origin the Web chat client can pass to `parseRunnerEndpoint`.
    #[must_use]
    pub fn http_origin(&self) -> String {
        let http_scheme = if self.scheme == "wss" {
            "https"
        } else {
            "http"
        };
        format!("{http_scheme}://{}:{}", self.host, self.port)
    }

    #[must_use]
    pub fn to_discovered_event(&self) -> DiscoveryEvent {
        DiscoveryEvent::RunnerDiscovered {
            origin: self.origin.clone(),
            host: self.host.clone(),
            port: self.port,
            label: self.label.clone(),
            version: self.version.clone(),
            scheme: self.scheme.clone(),
            codename: self.codename.clone(),
        }
    }
}

/// Live mDNS presence events pushed to `/ws` chat clients.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DiscoveryEvent {
    RunnerDiscovered {
        origin: String,
        host: String,
        port: u16,
        label: String,
        version: Option<String>,
        scheme: String,
        codename: Option<String>,
    },
    RunnerGone {
        origin: String,
    },
}

/// A removed discovery origin kept for bounded re-entry.
///
/// When two consecutive `/web` probes fail, the row is removed from the
/// visible roster (the product must not show a dead peer) but its origin is
/// remembered here. A later steady-state cache dump re-emits
/// `ServiceResolved` for the cached instance; the row is only re-inserted
/// after a fresh `/web` identity probe succeeds, so a reused port serving
/// arbitrary HTML can never resurrect a ghost. Tombstones expire after
/// [`TEAM_DISCOVERY_EXPIRY`] so a genuinely dead peer is forgotten.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DiscoveryTombstone {
    origin: String,
    instance: String,
    host: String,
    port: u16,
    scheme: String,
    label: String,
    version: Option<String>,
    codename: Option<String>,
    removed_at_ms: u64,
    /// Last time a cache dump re-advertised this instance. Only tombstones
    /// still advertised within the window are re-verified, so a peer whose
    /// records expired from the daemon cache is never probed again.
    last_advertised_ms: u64,
}

impl DiscoveryTombstone {
    fn http_origin(&self) -> String {
        let http_scheme = if self.scheme == "wss" {
            "https"
        } else {
            "http"
        };
        format!("{http_scheme}://{}:{}", self.host, self.port)
    }
}

/// Shared candidate list + event fan-out for LAN discovery.
///
/// [`TeamDiscovery`] owns the mDNS daemon; this view is what the listen
/// control plane clones into each WebSocket. Tests construct a view without
/// starting multicast.
#[derive(Clone)]
pub struct DiscoveryView {
    discovered: Arc<Mutex<Vec<DiscoveredRunner>>>,
    tombstones: Arc<Mutex<Vec<DiscoveryTombstone>>>,
    events: tokio::sync::broadcast::Sender<DiscoveryEvent>,
}

impl DiscoveryView {
    fn new() -> Self {
        let (events, _) = tokio::sync::broadcast::channel(TEAM_DISCOVERED_CAP);
        Self {
            discovered: Arc::new(Mutex::new(Vec::new())),
            tombstones: Arc::new(Mutex::new(Vec::new())),
            events,
        }
    }

    /// Current candidate list.
    ///
    /// Presence is driven by mDNS `ServiceResolved` / `ServiceRemoved` plus a
    /// bounded last-seen expiry. mdns-sd 0.11 does not re-emit
    /// `ServiceResolved` on identical TTL refresh, so live rows are
    /// reconfirmed by the deadline-driven `/web` identity probe rather than
    /// by ambient LAN chatter.
    #[must_use]
    pub fn snapshot(&self) -> Vec<DiscoveredRunner> {
        self.discovered.lock().expect("discovery lock").clone()
    }

    #[must_use]
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<DiscoveryEvent> {
        self.events.subscribe()
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self::new()
    }

    #[cfg(test)]
    pub(crate) fn remove_origin(&self, origin: &str) {
        self.discovered
            .lock()
            .expect("discovery lock")
            .retain(|runner| runner.origin != origin);
        let _ = self.events.send(DiscoveryEvent::RunnerGone {
            origin: origin.to_owned(),
        });
    }

    #[cfg(test)]
    pub(crate) fn upsert_runner(&self, runner: DiscoveredRunner) {
        let event = runner.to_discovered_event();
        {
            let mut list = self.discovered.lock().expect("discovery lock");
            if let Some(existing) = list.iter_mut().find(|row| row.origin == runner.origin) {
                *existing = runner;
            } else {
                list.push(runner);
                list.sort_by(|left, right| left.origin.cmp(&right.origin));
                if list.len() > TEAM_DISCOVERED_CAP {
                    list.truncate(TEAM_DISCOVERED_CAP);
                }
            }
        }
        let _ = self.events.send(event);
    }

    /// Remember a removed origin so a later cache dump can re-verify it.
    fn tombstone(&self, runner: &DiscoveredRunner) {
        let mut tombstones = self.tombstones.lock().expect("tombstone lock");
        if let Some(existing) = tombstones.iter_mut().find(|t| t.origin == runner.origin) {
            existing.instance = runner.instance.clone();
            existing.host = runner.host.clone();
            existing.port = runner.port;
            existing.scheme = runner.scheme.clone();
            existing.label = runner.label.clone();
            existing.version = runner.version.clone();
            existing.codename = runner.codename.clone();
            existing.removed_at_ms = now_ms();
            existing.last_advertised_ms = runner.last_seen_ms;
        } else {
            tombstones.push(DiscoveryTombstone {
                origin: runner.origin.clone(),
                instance: runner.instance.clone(),
                host: runner.host.clone(),
                port: runner.port,
                scheme: runner.scheme.clone(),
                label: runner.label.clone(),
                version: runner.version.clone(),
                codename: runner.codename.clone(),
                removed_at_ms: now_ms(),
                last_advertised_ms: runner.last_seen_ms,
            });
        }
    }

    /// Drop tombstones older than `ttl` (a genuinely dead peer is forgotten).
    fn expire_tombstones(&self, ttl: Duration) {
        let now = now_ms();
        let ttl_ms = ttl.as_millis() as u64;
        self.tombstones
            .lock()
            .expect("tombstone lock")
            .retain(|tombstone| now.saturating_sub(tombstone.removed_at_ms) <= ttl_ms);
    }

    /// Tombstoned origins whose cached instance is still advertised.
    fn tombstoned_instances(&self) -> Vec<DiscoveryTombstone> {
        self.tombstones.lock().expect("tombstone lock").clone()
    }

    /// Refresh tombstone candidates from a re-advertised `ServiceResolved`.
    ///
    /// Returns `true` when the resolved instance is tombstoned and must not
    /// upsert/announce. A fullname match restamps `last_advertised_ms`
    /// (keeping the candidate eligible for re-verification); a well-formed
    /// record also refreshes the candidate endpoint metadata — host, port,
    /// scheme, origin and display fields — so `reverify_tombstoned_http`
    /// probes the endpoint the peer advertises now (moved hosts and scheme
    /// flips included). Resurrection stays exclusively behind the successful
    /// `/web` probe.
    fn refresh_tombstone_candidate(&self, info: &mdns_sd::ServiceInfo) -> bool {
        let fullname = info.get_fullname();
        let runner = parse_discovered_runner(info);
        let mut tombstones = self.tombstones.lock().expect("tombstone lock");
        let mut matched = false;
        for tombstone in tombstones.iter_mut().filter(|t| t.instance == fullname) {
            matched = true;
            tombstone.last_advertised_ms = now_ms();
            if let Some(runner) = runner.as_ref() {
                tombstone.origin = runner.origin.clone();
                tombstone.host = runner.host.clone();
                tombstone.port = runner.port;
                tombstone.scheme = runner.scheme.clone();
                tombstone.label = runner.label.clone();
                tombstone.version = runner.version.clone();
                tombstone.codename = runner.codename.clone();
            }
        }
        matched
    }

    /// Re-insert a tombstoned origin after a successful `/web` identity
    /// probe, emitting `RunnerDiscovered` so the Web projection converges.
    fn resurrect(&self, tombstone: &DiscoveryTombstone) {
        let runner = DiscoveredRunner {
            origin: tombstone.origin.clone(),
            host: tombstone.host.clone(),
            port: tombstone.port,
            instance: tombstone.instance.clone(),
            label: tombstone.label.clone(),
            version: tombstone.version.clone(),
            scheme: tombstone.scheme.clone(),
            codename: tombstone.codename.clone(),
            last_seen_ms: now_ms(),
            failed_probes: 0,
            failed_since_ms: None,
        };
        let event = runner.to_discovered_event();
        {
            let mut discovered = self.discovered.lock().expect("discovery lock");
            if !discovered.iter().any(|row| row.origin == runner.origin) {
                discovered.push(runner);
                discovered.sort_by(|left, right| left.origin.cmp(&right.origin));
                if discovered.len() > TEAM_DISCOVERED_CAP {
                    discovered.truncate(TEAM_DISCOVERED_CAP);
                }
            }
        }
        self.tombstones
            .lock()
            .expect("tombstone lock")
            .retain(|t| t.origin != tombstone.origin);
        let _ = self.events.send(event);
    }
}

/// Bounded mDNS browser/advertiser for the `_rpi._tcp` service type.
///
/// Browsing starts at startup and feeds a process-local candidate list used
/// by `/team join` and the Web host dropdown; advertising publishes the
/// local `--listen` endpoint so other rpi instances can find it. The
/// mdns-sd daemon owns its own worker threads; only the candidate list is
/// shared with the rest of the process.
pub const TEAM_MDNS_SERVICE: &str = "_rpi._tcp.local.";
/// Cap on remembered discoveries (defense against an mdns flood).
const TEAM_DISCOVERED_CAP: usize = 64;
/// Initial browse can observe a service before its SRV/TXT/A records finish
/// entering mdns-sd's cache. Two bounded startup re-browses recover that
/// piecemeal case; afterward a steady-state rebrowse
/// ([`TEAM_MDNS_REBROWSE_INTERVAL`]) keeps re-dumping the cache so removed
/// rows can re-enter.
const TEAM_MDNS_STARTUP_REBROWSE_DELAY: Duration = Duration::from_secs(1);
const TEAM_MDNS_STARTUP_REBROWSES: u8 = 2;
const TEAM_MDNS_SWEEP_INTERVAL: Duration = Duration::from_secs(15);
/// A discovery row is retired if no ServiceResolved event reconfirmed it
/// within this window. The steady-state rebrowse re-inserts cache-resident
/// rows, but mdns-sd evicts a dead peer's records at its 120s TTL, so a
/// missing ServiceRemoved cannot reinsert a dead row forever.
const TEAM_DISCOVERY_EXPIRY: Duration = Duration::from_secs(60);
/// Steady-state cache-dump cadence. mdns-sd 0.11 does not re-emit
/// `ServiceResolved` for records that merely refresh their TTL, so a row
/// removed by two failed `/web` probes would never re-enter on its own.
/// Rotating the browse re-dumps the daemon's cache and re-emits
/// `ServiceResolved` for every ready instance, re-inserting a falsely-removed
/// live peer within this window. Must be shorter than
/// [`TEAM_DISCOVERY_EXPIRY`] so live rows are restamped before they expire,
/// while the two-strike probe still fires for genuinely dead peers.
const TEAM_MDNS_REBROWSE_INTERVAL: Duration = Duration::from_secs(30);
/// Per-request deadline for the `/web` identity probe. 500ms was too tight
/// for a busy peer (mid-compaction, loaded LAN) and a false miss costs a
/// two-strike removal; 2s bounds a black-holed host while leaving headroom.
const TEAM_LIVENESS_TIMEOUT: Duration = Duration::from_secs(2);
const TEAM_LIVENESS_CONCURRENCY: usize = 8;
const TEAM_LIVENESS_PREFIX_MAX: usize = 4 * 1024;
/// Grace window measured from the start of a row's bounded-removal window:
/// the first consecutive failed `/web` probe or a trusted `ServiceRemoved`
/// goodbye (which seeds two strikes without removing the row, so removal
/// never fires before this much time after the goodbye). Two strikes alone
/// at the 15s sweep cadence could retire a busy peer (~30s), so removal
/// additionally waits until this much time has elapsed since the window
/// opened; the 60s window absorbs a pre-kill in-flight failure so a
/// die-then-kill peer is never retired in under 45s. A scheme flip opens
/// the stale origin's window at the first advertisement of its replacement
/// (same host+port), so false old rows retire 60-75s after the flip
/// (<=90 bound). Combined with [`TEAM_DISCOVERY_EXPIRY`] (60s) and the
/// sweep/reprobe cadence, a silently dead peer is retired within at most
/// ~150s while a healthy-but-busy peer gets the full window to answer.
const TEAM_REMOVAL_GRACE: Duration = Duration::from_secs(60);

pub struct TeamDiscovery {
    daemon: mdns_sd::ServiceDaemon,
    view: DiscoveryView,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl TeamDiscovery {
    /// Start browsing `_rpi._tcp` in the background. The returned handle keeps
    /// the browser alive and exposes the current candidate list.
    pub fn start() -> Result<Self> {
        let daemon = mdns_sd::ServiceDaemon::new().context("starting mDNS daemon")?;
        let view = DiscoveryView::new();
        let worker = view.clone();
        let browser = daemon.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let liveness_client = reqwest::Client::builder()
            .no_proxy()
            .danger_accept_invalid_certs(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(TEAM_LIVENESS_TIMEOUT)
            .build()
            .context("building discovery liveness client")?;
        let liveness_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .context("building discovery liveness runtime")?;
        let worker_handle = std::thread::spawn(move || loop {
            if worker_stop.load(Ordering::Acquire) {
                break;
            }
            let mut receiver = match browser.browse(TEAM_MDNS_SERVICE) {
                Ok(receiver) => receiver,
                Err(_) => {
                    std::thread::park_timeout(TEAM_MDNS_SWEEP_INTERVAL);
                    continue;
                }
            };
            let mut startup_rebrowses = TEAM_MDNS_STARTUP_REBROWSES;
            let mut next_rebrowse = Instant::now() + TEAM_MDNS_STARTUP_REBROWSE_DELAY;
            let mut next_sweep = Instant::now() + TEAM_MDNS_SWEEP_INTERVAL;
            let mut next_steady_rebrowse = Instant::now() + TEAM_MDNS_REBROWSE_INTERVAL;
            loop {
                if worker_stop.load(Ordering::Acquire) {
                    return;
                }
                let deadline = if startup_rebrowses > 0 {
                    next_sweep.min(next_rebrowse)
                } else {
                    next_sweep.min(next_steady_rebrowse)
                };
                match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(event) => apply_mdns_event(&worker, event),
                    Err(_) if receiver.is_disconnected() => break,
                    Err(_) => {}
                }
                if startup_rebrowses > 0 && Instant::now() >= next_rebrowse {
                    let _ = browser.stop_browse(TEAM_MDNS_SERVICE);
                    match browser.browse(TEAM_MDNS_SERVICE) {
                        Ok(next) => receiver = next,
                        Err(_) => break,
                    }
                    startup_rebrowses -= 1;
                    next_rebrowse = Instant::now() + TEAM_MDNS_STARTUP_REBROWSE_DELAY;
                }
                if Instant::now() >= next_steady_rebrowse {
                    // Re-dump the daemon cache: re-emits ServiceResolved for
                    // every ready instance, refreshing tombstone
                    // last_advertised_ms so cached rows stay eligible for
                    // `/web` re-verification.
                    let _ = browser.stop_browse(TEAM_MDNS_SERVICE);
                    match browser.browse(TEAM_MDNS_SERVICE) {
                        Ok(next) => receiver = next,
                        Err(_) => break,
                    }
                    next_steady_rebrowse = Instant::now() + TEAM_MDNS_REBROWSE_INTERVAL;
                    liveness_runtime.block_on(reverify_tombstoned_http(
                        &worker,
                        &liveness_client,
                    ));
                }
                if Instant::now() >= next_sweep {
                    liveness_runtime.block_on(expire_stale_discovered_http(
                        &worker,
                        TEAM_DISCOVERY_EXPIRY,
                        &liveness_client,
                    ));
                    next_sweep = Instant::now() + TEAM_MDNS_SWEEP_INTERVAL;
                }
            }
            std::thread::park_timeout(TEAM_MDNS_SWEEP_INTERVAL);
        });
        Ok(Self {
            daemon,
            view,
            stop,
            worker: Some(worker_handle),
        })
    }

    /// Advertise this process's `--listen` endpoint as an `_rpi._tcp` service
    /// so other instances on the LAN can discover it. `codename` is the
    /// resolved D2 display name; `None` omits the TXT property entirely so
    /// old listeners see an unchanged property set.
    pub fn advertise(
        &self,
        host: &str,
        port: u16,
        scheme: &str,
        codename: Option<&str>,
    ) -> Result<()> {
        let properties = mdns_advertise_properties(host, scheme, codename);
        let instance = mdns_instance_label(host, port);
        let service = mdns_sd::ServiceInfo::new(
            TEAM_MDNS_SERVICE,
            &instance,
            &format!("{instance}.local."),
            host,
            port,
            properties,
        )
        .context("building mDNS service record")?;
        self.daemon
            .register(service)
            .context("registering mDNS service")?;
        Ok(())
    }

    /// Current candidate list (mDNS presence plus bounded last-seen expiry).
    pub fn discovered(&self) -> Vec<DiscoveredRunner> {
        self.view.snapshot()
    }

    /// Cheap clone of the candidate list + event fan-out for the listen plane.
    #[must_use]
    pub fn view(&self) -> DiscoveryView {
        self.view.clone()
    }

    #[cfg(test)]
    fn take_worker(&mut self) -> Option<std::thread::JoinHandle<()>> {
        self.worker.take()
    }
}

impl Drop for TeamDiscovery {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.daemon.stop_browse(TEAM_MDNS_SERVICE);
        let _ = self.daemon.shutdown();
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

/// TXT property set for [`TeamDiscovery::advertise`].
///
/// The optional `codename` TXT carries the runner's D2 display name. It is
/// omitted entirely when absent or not grammar-valid — never advertised as an
/// empty-string property — so pre-codename listeners observe an unchanged
/// property set. Grammar-valid names, including consecutive hyphens, are
/// copied byte-for-byte from the authority.
fn mdns_advertise_properties(
    host: &str,
    scheme: &str,
    codename: Option<&str>,
) -> std::collections::HashMap<String, String> {
    let mut properties = std::collections::HashMap::new();
    properties.insert("version".to_owned(), env!("CARGO_PKG_VERSION").to_owned());
    properties.insert("scheme".to_owned(), scheme.to_owned());
    properties.insert("label".to_owned(), host.to_owned());
    if let Some(codename) =
        codename.filter(|name| crate::runner_identity::is_valid_codename(name))
    {
        properties.insert("codename".to_owned(), codename.to_owned());
    }
    properties
}

/// Sanitize an advertised TXT `codename` into the D2 display grammar.
///
/// [`crate::runner_identity::is_valid_codename`] is the authority: already-valid
/// lowercase names (including consecutive hyphens such as `a--b`) are preserved
/// byte-for-byte. Mixed-case, control, or otherwise invalid input is lowercased
/// after [`sanitize_mdns_label`], anything outside `[a-z0-9-]` is dropped, then
/// consecutive `-` collapse and edge `-` are trimmed, capped at 48 characters.
/// An empty result downgrades to `None` — the caller keeps the row, it never
/// rejects on codename grounds.
fn sanitize_codename(raw: &str) -> Option<String> {
    if crate::runner_identity::is_valid_codename(raw) {
        return Some(raw.to_owned());
    }
    let lowered = sanitize_mdns_label(raw).to_lowercase();
    let mut out = String::with_capacity(raw.len());
    for character in lowered.chars() {
        if !matches!(character, 'a'..='z' | '0'..='9' | '-') {
            continue;
        }
        if character == '-' && out.ends_with('-') {
            continue;
        }
        out.push(character);
    }
    let trimmed = out.trim_matches('-');
    let capped: String = trimmed
        .chars()
        .take(48)
        .collect::<String>()
        .trim_end_matches('-')
        .to_owned();
    if capped.is_empty() {
        None
    } else {
        Some(capped)
    }
}

fn apply_mdns_event(view: &DiscoveryView, event: mdns_sd::ServiceEvent) {
    match event {
        mdns_sd::ServiceEvent::ServiceResolved(info) => {
            // A cache dump re-advertises cached instances. A tombstoned
            // instance must not re-enter the roster directly (a reused port
            // serving arbitrary HTML cannot announce itself): refresh the
            // tombstone candidate instead and let the `/web` re-verification
            // (reverify_tombstoned_http) resurrect the row.
            if !view.refresh_tombstone_candidate(&info) {
                upsert_discovered(view, &info);
            }
        }
        mdns_sd::ServiceEvent::ServiceRemoved(_ty, fullname) => {
            // A goodbye is never trusted for immediate retirement: a
            // flapping peer (or a re-browse racing a shutdown) must stay
            // visible while the `/web` sweep decides. Matching rows are
            // handed two strikes up front and a fresh removal window; a
            // live verdict resets both, a dead verdict removes only after
            // the window elapsed. Matching by the full instance name keeps
            // sibling ports and remote hosts untouched.
            let mut discovered = view.discovered.lock().expect("discovery lock");
            for row in discovered.iter_mut().filter(|row| row.instance == fullname) {
                if row.failed_since_ms.is_none() {
                    row.failed_since_ms = Some(now_ms());
                }
                row.failed_probes = row.failed_probes.max(2);
            }
        }
        mdns_sd::ServiceEvent::SearchStarted(_)
        | mdns_sd::ServiceEvent::SearchStopped(_)
        | mdns_sd::ServiceEvent::ServiceFound(_, _) => {}
    }
}

/// Single DNS-SD instance label for this listen endpoint.
///
/// IPv4 dots, IPv6 colons, and any other non `[A-Za-z0-9-]` character become
/// `-` so the name is one DNS label. The listen port is appended so two
/// listeners on the same host do not share an instance (and therefore do not
/// cache-flush each other's SRV records).
fn mdns_instance_label(host: &str, port: u16) -> String {
    let mut host_part = String::new();
    for ch in host.chars() {
        if ch.is_ascii_alphanumeric() {
            host_part.push(ch);
        } else {
            host_part.push('-');
        }
    }
    let host_part = host_part.trim_matches('-');
    let host_part = if host_part.is_empty() {
        "host"
    } else {
        host_part
    };
    format!("rpi-{host_part}-{port}")
}

/// First DNS-SD label of a fullname (`<label>._rpi._tcp.local.`).
fn mdns_instance_from_fullname(fullname: &str) -> Option<&str> {
    let label = fullname.split('.').next().unwrap_or("");
    if label.is_empty() { None } else { Some(label) }
}

/// Accept only the post-fix instance shape: a single DNS label
/// `rpi-<sanitized-host>-<port>` with no extra dots. Old binaries advertised
/// `rpi-192.0.2.15`, which splits into garbage labels (`rpi-192` in domain
/// `0.2.15._rpi._tcp.local.`) and surfaces as a phantom ws/http runner.
fn is_well_formed_mdns_instance(fullname: &str) -> bool {
    let Some(label) = mdns_instance_from_fullname(fullname) else {
        return false;
    };
    if label.contains('.') {
        return false;
    }
    let Some(rest) = label.strip_prefix("rpi-") else {
        return false;
    };
    let Some((host_part, port_part)) = rest.rsplit_once('-') else {
        return false;
    };
    !host_part.is_empty()
        && host_part
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        && !port_part.is_empty()
        && port_part.chars().all(|ch| ch.is_ascii_digit())
}

fn sanitize_mdns_label(raw: &str) -> String {
    raw.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parse one `ServiceResolved` record into a candidate runner; `None` for
/// malformed or unusable records (they never enter the roster or a
/// tombstone candidate).
fn parse_discovered_runner(info: &mdns_sd::ServiceInfo) -> Option<DiscoveredRunner> {
    if !is_well_formed_mdns_instance(info.get_fullname()) {
        return None;
    }
    let Some(address) = info.get_addresses_v4().into_iter().next() else {
        return None;
    };
    let host = address.to_string();
    let port = info.get_port();
    let scheme = match info.get_properties().get("scheme") {
        Some(property) if matches!(property.val_str(), "ws" | "wss") => {
            property.val_str().to_owned()
        }
        _ => return None,
    };
    let origin = match validate_origin(&format!("{scheme}://{host}:{port}")) {
        Ok(origin) => origin,
        Err(_) => return None,
    };
    let label = match info.get_properties().get("label") {
        Some(property) => {
            let label = sanitize_mdns_label(property.val_str());
            if label.is_empty() {
                return None;
            }
            label
        }
        None => return None,
    };
    let version = info
        .get_properties()
        .get("version")
        .map(|property| property.val_str().to_owned())
        .filter(|value| !value.is_empty());
    let codename = info
        .get_properties()
        .get("codename")
        .map(|property| sanitize_codename(property.val_str()))
        .unwrap_or(None);
    Some(DiscoveredRunner {
        origin,
        host: host.to_owned(),
        port,
        label,
        version,
        instance: info.get_fullname().to_owned(),
        scheme,
        codename,
        last_seen_ms: now_ms(),
        failed_probes: 0,
        failed_since_ms: None,
    })
}

fn upsert_discovered(view: &DiscoveryView, info: &mdns_sd::ServiceInfo) {
    let Some(runner) = parse_discovered_runner(info) else {
        return;
    };
    let event = runner.to_discovered_event();
    let is_new = {
        let mut discovered = view.discovered.lock().expect("discovery lock");
        if let Some(existing) = discovered
            .iter_mut()
            .find(|row| row.origin == runner.origin)
        {
            // Liveness is owned by the `/web` sweep, never by the peer's
            // own re-advertisements: an identical cached ServiceResolved
            // (a steady-state cache dump) must not refresh the row.
            // Metadata always updates; last_seen restamps only when the
            // advertisement actually changed AND the row carries no open
            // failure window. Strikes/window are never reset here — the
            // `/web` alive verdict is the sole reset.
            let metadata_changed = existing.label != runner.label
                || existing.version != runner.version
                || existing.scheme != runner.scheme
                || existing.codename != runner.codename
                || existing.instance != runner.instance
                || existing.host != runner.host
                || existing.port != runner.port;
            existing.label = runner.label;
            existing.version = runner.version;
            existing.scheme = runner.scheme;
            existing.codename = runner.codename;
            if metadata_changed && existing.failed_since_ms.is_none() {
                existing.last_seen_ms = runner.last_seen_ms;
            }
            false
        } else {
            // A NEW origin for an already-known host+port under a
            // different scheme is a scheme flip: open the stale sibling's
            // removal window now (without striking it) so false old rows
            // are retired soon after the replacement is advertised — the
            // flip clock never counts against a genuinely live sibling.
            if let Some(sibling) = discovered.iter_mut().find(|row| {
                row.host == runner.host
                    && row.port == runner.port
                    && row.scheme != runner.scheme
            }) {
                if sibling.failed_since_ms.is_none() {
                    sibling.failed_since_ms = Some(now_ms());
                }
            }
            discovered.push(runner);
            discovered.sort_by(|left, right| left.origin.cmp(&right.origin));
            if discovered.len() > TEAM_DISCOVERED_CAP {
                discovered.truncate(TEAM_DISCOVERED_CAP);
            }
            true
        }
    };
    if is_new {
        let _ = view.events.send(event);
    }
}

/// Probe one stale row's `/web` identity. The response must be a 200
/// `text/html` page carrying the rpi web markers within the first 4KiB;
/// anything else (arbitrary 200 HTML, a proxy page, a non-HTML body) is
/// rejected so a reused port can never resurrect a dead row.
async fn discovered_runner_is_live(
    client: &reqwest::Client,
    row: &DiscoveredRunner,
) -> bool {
    http_origin_is_live(client, &row.http_origin()).await
}

/// `/web` identity probe for an HTTP(S) origin (live row or tombstone).
async fn http_origin_is_live(client: &reqwest::Client, http_origin: &str) -> bool {
    let response = match client.get(format!("{http_origin}/web")).send().await {
        Ok(response) if response.status().is_success() => response,
        _ => return false,
    };
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !content_type.starts_with("text/html") {
        return false;
    }
    let mut chunks = response.bytes_stream();
    let mut prefix = Vec::new();
    while prefix.len() < TEAM_LIVENESS_PREFIX_MAX {
        let chunk = match chunks.next().await {
            Some(Ok(chunk)) => chunk,
            Some(Err(_)) => return false,
            None => break,
        };
        let remaining = TEAM_LIVENESS_PREFIX_MAX - prefix.len();
        prefix.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if liveness_marker_present(&prefix) {
            return true;
        }
    }
    false
}

/// Re-verify tombstoned origins whose cached instance is still advertised.
///
/// Runs after each cache dump: tombstones refreshed by a `ServiceResolved`
/// within the expiry window are probed via `/web` identity; a success
/// re-inserts the row (with `RunnerDiscovered`), a failure keeps the
/// tombstone for the next dump. Tombstones older than the window are
/// forgotten so a genuinely dead peer is never probed again.
async fn reverify_tombstoned_http(view: &DiscoveryView, client: &reqwest::Client) {
    view.expire_tombstones(TEAM_DISCOVERY_EXPIRY);
    let now = now_ms();
    let window_ms = TEAM_DISCOVERY_EXPIRY.as_millis() as u64;
    let candidates = view
        .tombstoned_instances()
        .into_iter()
        .filter(|tombstone| now.saturating_sub(tombstone.last_advertised_ms) <= window_ms)
        .collect::<Vec<_>>();
    let checked = futures_util::stream::iter(candidates.into_iter().map(|tombstone| async move {
        let alive = http_origin_is_live(client, &tombstone.http_origin()).await;
        (tombstone, alive)
    }))
    .buffer_unordered(TEAM_LIVENESS_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;
    apply_tombstone_reverify_results(view, checked);
}

/// Apply tombstone re-verification verdicts (split out for deterministic
/// tests): only a live verdict resurrects the row.
fn apply_tombstone_reverify_results(
    view: &DiscoveryView,
    checked: Vec<(DiscoveryTombstone, bool)>,
) {
    for (tombstone, alive) in checked {
        if alive {
            view.resurrect(&tombstone);
        }
    }
}

/// Case-insensitive `/web` identity check over the accumulated prefix.
///
/// The embedded page always carries the lowercase doctype first and the
/// `<title>rpi web</title>` well inside the first 4KiB; matching is
/// case-insensitive so template/formatting changes (`<!DOCTYPE html>`,
/// `<TITLE>`) cannot false-negative a healthy peer. Re-checked on every
/// chunk, so the verdict never depends on chunk boundaries.
fn liveness_marker_present(prefix: &[u8]) -> bool {
    let lower: Vec<u8> = prefix.iter().map(u8::to_ascii_lowercase).collect();
    lower
        .windows(b"<!doctype html>".len())
        .any(|window| window == b"<!doctype html>")
        && lower
            .windows(b"<title>rpi web</title>".len())
            .any(|window| window == b"<title>rpi web</title>")
}

/// Sweep probe eligibility: last-seen expired, or the bounded-removal
/// window is open — once a window opens, every sweep re-probes the row
/// regardless of cache refreshes restamping `last_seen`.
fn sweep_probe_eligible(row: &DiscoveredRunner, now: u64, ttl_ms: u64) -> bool {
    now.saturating_sub(row.last_seen_ms) > ttl_ms || row.failed_since_ms.is_some()
}

async fn expire_stale_discovered_http(
    view: &DiscoveryView,
    ttl: Duration,
    client: &reqwest::Client,
) {
    let now = now_ms();
    let ttl_ms = ttl.as_millis() as u64;
    let stale = {
        let discovered = view.discovered.lock().expect("discovery lock");
        discovered
            .iter()
            .filter(|row| sweep_probe_eligible(row, now, ttl_ms))
            .cloned()
            .collect::<Vec<_>>()
    };
    let checked = futures_util::stream::iter(stale.into_iter().map(|row| async move {
        let alive = discovered_runner_is_live(client, &row).await;
        (row, alive)
    }))
    .buffer_unordered(TEAM_LIVENESS_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;
    apply_stale_probe_results(view, checked);
}

fn expire_stale_discovered(view: &DiscoveryView, ttl: Duration) {
    expire_stale_discovered_with_probe(view, ttl, |_| false);
}

fn expire_stale_discovered_with_probe<F>(view: &DiscoveryView, ttl: Duration, is_alive: F)
where
    F: Fn(&DiscoveredRunner) -> bool,
{
    let now = now_ms();
    let ttl_ms = ttl.as_millis() as u64;
    let stale = {
        let discovered = view.discovered.lock().expect("discovery lock");
        discovered
            .iter()
            .filter(|row| sweep_probe_eligible(row, now, ttl_ms))
            .cloned()
            .collect::<Vec<_>>()
    };
    if stale.is_empty() {
        return;
    }
    let mut live = Vec::new();
    let mut dead = Vec::new();
    for row in stale {
        if is_alive(&row) {
            live.push(row.origin);
        } else {
            dead.push((row.origin, row.last_seen_ms));
        }
    }
    let mut discovered = view.discovered.lock().expect("discovery lock");
    for row in &mut *discovered {
        if live.iter().any(|origin| origin == &row.origin) {
            row.last_seen_ms = now;
        }
    }
    let mut gone = Vec::new();
    discovered.retain(|row| {
        let should_remove = dead
            .iter()
            .any(|(origin, seen)| origin == &row.origin && *seen == row.last_seen_ms);
        if should_remove {
            gone.push(DiscoveryEvent::RunnerGone { origin: row.origin.clone() });
        }
        !should_remove
    });
    drop(discovered);
    for event in gone {
        let _ = view.events.send(event);
    }
}
fn apply_stale_probe_results(
    view: &DiscoveryView,
    checked: Vec<(DiscoveredRunner, bool)>,
) {
    let mut discovered = view.discovered.lock().expect("discovery lock");
    let mut gone = Vec::new();
    discovered.retain_mut(|row| {
        let Some((candidate, alive)) = checked.iter().find(|(candidate, _)| {
            candidate.origin == row.origin && candidate.last_seen_ms == row.last_seen_ms
        }) else {
            return true;
        };
        if *alive {
            row.last_seen_ms = now_ms();
            row.failed_probes = 0;
            row.failed_since_ms = None;
            return true;
        }
        if row.failed_since_ms.is_none() {
            row.failed_since_ms = Some(now_ms());
        }
        row.failed_probes = row.failed_probes.saturating_add(1);
        // Removal requires two consecutive misses AND the grace window
        // measured from the first strike: busy-but-alive peers get the full
        // window to answer, and a row refreshed by a ServiceResolved since
        // the probe snapshot never reaches this branch (last_seen mismatch),
        // so the window is re-opened only after a fresh run of misses.
        let window_elapsed = row
            .failed_since_ms
            .is_some_and(|first_failure_ms| {
                now_ms().saturating_sub(first_failure_ms) >= TEAM_REMOVAL_GRACE.as_millis() as u64
            });
        if row.failed_probes < 2 || !window_elapsed {
            return true;
        }
        gone.push(DiscoveryEvent::RunnerGone {
            origin: candidate.origin.clone(),
        });
        false
    });
    drop(discovered);
    // Remember removed origins so a later cache dump can re-verify them via
    // `/web` identity before re-inserting (bounded re-entry).
    for (candidate, _) in checked {
        if gone.iter().any(|event| {
            matches!(event, DiscoveryEvent::RunnerGone { origin } if origin == &candidate.origin)
        }) {
            view.tombstone(&candidate);
        }
    }
    for event in gone {
        let _ = view.events.send(event);
    }
}

#[cfg(test)]
mod discovery_tests {
    use super::*;

    fn sample_info(scheme: &str, label: &str) -> mdns_sd::ServiceInfo {
        sample_info_named("rpi-host-a-8765", "192.0.2.10", 8765, scheme, label)
    }

    fn sample_info_named(
        instance: &str,
        ip: &str,
        port: u16,
        scheme: &str,
        label: &str,
    ) -> mdns_sd::ServiceInfo {
        mdns_sd::ServiceInfo::new(
            TEAM_MDNS_SERVICE,
            instance,
            &format!("{instance}.local."),
            ip,
            port,
            std::collections::HashMap::from([
                ("scheme".to_owned(), scheme.to_owned()),
                ("label".to_owned(), label.to_owned()),
                ("version".to_owned(), "0.2.11".to_owned()),
            ]),
        )
        .expect("service")
    }

    fn sample_info_codename(
        instance: &str,
        ip: &str,
        port: u16,
        scheme: &str,
        label: &str,
        codename: Option<&str>,
    ) -> mdns_sd::ServiceInfo {
        let mut properties = std::collections::HashMap::from([
            ("scheme".to_owned(), scheme.to_owned()),
            ("label".to_owned(), label.to_owned()),
            ("version".to_owned(), "0.2.11".to_owned()),
        ]);
        if let Some(codename) = codename {
            properties.insert("codename".to_owned(), codename.to_owned());
        }
        mdns_sd::ServiceInfo::new(
            TEAM_MDNS_SERVICE,
            instance,
            &format!("{instance}.local."),
            ip,
            port,
            properties,
        )
        .expect("service")
    }

    #[test]
    fn upsert_dedupes_by_origin_and_caps_size() {
        let view = DiscoveryView::for_test();
        let adapter = sample_info("wss", "dev box");
        upsert_discovered(&view, &adapter);
        upsert_discovered(&view, &adapter);
        let rows = view.snapshot();
        assert_eq!(
            rows.len(),
            1,
            "duplicate advertisements collapse to one entry"
        );
        assert_eq!(&rows[0].origin, "wss://192.0.2.10:8765");
        assert_eq!(rows[0].label, "dev box");
        assert_eq!(rows[0].scheme, "wss");
        assert_eq!(rows[0].http_origin(), "https://192.0.2.10:8765");
    }

    #[test]
    fn discovery_entries_flag_invalid_origins_are_dropped() {
        let view = DiscoveryView::for_test();
        let adapter = mdns_sd::ServiceInfo::new(
            TEAM_MDNS_SERVICE,
            "evil",
            "evil.local.",
            "192.0.2.10",
            8765,
            std::collections::HashMap::from([
                ("scheme".to_owned(), "http".to_owned()),
                ("version".to_owned(), "0.2.11".to_owned()),
            ]),
        )
        .expect("service");
        upsert_discovered(&view, &adapter);
        assert!(
            view.snapshot().is_empty(),
            "bad origins never enter the list"
        );
    }

    #[test]
    fn mdns_label_control_characters_are_sanitized() {
        assert_eq!(sanitize_mdns_label("dev\x00box"), "dev box");
        assert_eq!(sanitize_mdns_label("  a   b  "), "a b");
    }

    #[test]
    fn upsert_emits_discovered_event_once_and_expiry_emits_gone() {
        let view = DiscoveryView::for_test();
        let mut events = view.subscribe();
        upsert_discovered(&view, &sample_info("wss", "dev box"));
        upsert_discovered(&view, &sample_info("wss", "dev box"));
        let first = events.try_recv().expect("first discovery event");
        assert!(matches!(
            first,
            DiscoveryEvent::RunnerDiscovered { ref origin, ref scheme, .. }
                if origin == "wss://192.0.2.10:8765" && scheme == "wss"
        ));
        assert!(
            events.try_recv().is_err(),
            "duplicate upsert must not re-broadcast"
        );

        // Rows vanish through the bounded sweep, not trusted goodbyes: one
        // stale-expiry pass emits exactly one RunnerGone for one row.
        view.upsert_runner(DiscoveredRunner {
            origin: "wss://192.0.2.10:8765".to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 8765,
            instance: sample_info("wss", "dev box").get_fullname().to_owned(),
            label: "dev box".to_owned(),
            version: Some("0.2.11".to_owned()),
            scheme: "wss".to_owned(),
            codename: None,
            last_seen_ms: now_ms().saturating_sub(61_000),
            failed_probes: 0,
            failed_since_ms: None,
        });
        while events.try_recv().is_ok() {}
        expire_stale_discovered(&view, TEAM_DISCOVERY_EXPIRY);
        assert_eq!(
            events.try_recv().expect("expiry emits gone"),
            DiscoveryEvent::RunnerGone {
                origin: "wss://192.0.2.10:8765".to_owned(),
            }
        );
        assert!(view.snapshot().is_empty());
    }

    #[test]
    fn http_origin_maps_ws_to_http() {
        let runner = DiscoveredRunner {
            origin: "ws://192.0.2.11:4321".to_owned(),
            host: "192.0.2.11".to_owned(),
            port: 4321,
            instance: "rpi-host-b".to_owned(),
            label: "box".to_owned(),
            version: None,
            scheme: "ws".to_owned(),
            codename: None,
            last_seen_ms: 1,
            failed_probes: 0,
            failed_since_ms: None,
        };
        assert_eq!(runner.http_origin(), "http://192.0.2.11:4321");
    }

    #[test]
    fn ipv4_instance_label_is_single_dotless_label_unique_per_port() {
        // Old `format!("rpi-{host}")` produced `rpi-192.0.2.6`, which
        // ServiceInfo::new concatenates as extra DNS labels
        // (`rpi-192.0.2.6._rpi._tcp.local.`). Every listener on that IP
        // then shared one instance and cache-flushed each other's SRV.
        let a = mdns_instance_label("192.0.2.6", 18801);
        let b = mdns_instance_label("192.0.2.6", 18802);
        assert_eq!(a, "rpi-192-0-2-6-18801");
        assert_eq!(b, "rpi-192-0-2-6-18802");
        assert!(!a.contains('.'), "instance must be a single DNS label: {a}");
        assert_ne!(a, b, "co-hosted listeners must not share an instance");

        let info = mdns_sd::ServiceInfo::new(
            TEAM_MDNS_SERVICE,
            &a,
            &format!("{a}.local."),
            "192.0.2.6",
            18801,
            std::collections::HashMap::<String, String>::new(),
        )
        .expect("service");
        assert_eq!(
            info.get_fullname(),
            "rpi-192-0-2-6-18801._rpi._tcp.local.",
            "fullname must not split the IPv4 host into extra labels"
        );
    }

    #[test]
    fn ipv6_instance_label_sanitizes_colons() {
        // ':' is illegal in a DNS label; the old host-as-instance path would
        // fragment an IPv6 address the same way dotted IPv4 did.
        let label = mdns_instance_label("2001:db8::1", 8765);
        assert_eq!(label, "rpi-2001-db8--1-8765");
        assert!(
            !label.contains('.'),
            "instance must be a single DNS label: {label}"
        );
        assert!(
            !label.contains(':'),
            "colons are illegal in a DNS label: {label}"
        );
    }

    #[test]
    fn identical_cached_resolves_never_refresh_liveness() {
        // The daemon cache can re-dump ServiceResolved for a peer whose
        // endpoint is long gone. An identical advertisement must never
        // restamp last_seen — the row stays honestly stale at +30/60/90s
        // of simulated age and the expiry bound still retires it.
        // Liveness is refreshed only by the `/web` sweep's alive verdict
        // (or a metadata change, which never touches the last_seen of a
        // row carrying an open window).
        let view = DiscoveryView::for_test();
        let mut events = view.subscribe();
        view.upsert_runner(DiscoveredRunner {
            origin: "wss://192.0.2.15:8765".to_owned(),
            host: "192.0.2.15".to_owned(),
            port: 8765,
            instance: "rpi-192-0-2-15-8765._rpi._tcp.local.".to_owned(),
            label: "lab-a".to_owned(),
            version: Some("0.2.11".to_owned()),
            scheme: "wss".to_owned(),
            codename: None,
            last_seen_ms: now_ms(),
            failed_probes: 0,
            failed_since_ms: None,
        });
        while events.try_recv().is_ok() {}
        for age_ms in [30_000_u64, 60_000, 90_000] {
            // Simulate elapsed time between cache dumps.
            let mut aged = view.snapshot()[0].clone();
            aged.last_seen_ms = now_ms().saturating_sub(age_ms);
            view.upsert_runner(aged);
            while events.try_recv().is_ok() {}
            let before = view.snapshot()[0].last_seen_ms;
            apply_mdns_event(
                &view,
                mdns_sd::ServiceEvent::ServiceResolved(sample_info_named(
                    "rpi-192-0-2-15-8765",
                    "192.0.2.15",
                    8765,
                    "wss",
                    "lab-a",
                )),
            );
            let rows = view.snapshot();
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0].last_seen_ms, before,
                "identical cached resolve at +{age_ms}ms must not refresh liveness"
            );
            assert!(events.try_recv().is_err());
        }

        // Honestly stale: the bound still retires the row.
        while events.try_recv().is_ok() {}
        expire_stale_discovered(&view, TEAM_DISCOVERY_EXPIRY);
        assert!(
            view.snapshot().is_empty(),
            "identical resolves cannot keep a dead peer alive"
        );
        assert!(matches!(
            events.try_recv().expect("stale row emits one gone"),
            DiscoveryEvent::RunnerGone { ref origin }
                if origin == "wss://192.0.2.15:8765"
        ));
        assert!(events.try_recv().is_err(), "exactly one RunnerGone");
    }

    #[test]
    fn metadata_change_restamps_only_without_open_window() {
        // A wire update (changed metadata) may restamp last_seen only when
        // the row carries no open failure window; with one open, metadata
        // still updates but last_seen and the window are preserved and the
        // row stays sweep-eligible.
        let view = DiscoveryView::for_test();
        let mut events = view.subscribe();
        view.upsert_runner(DiscoveredRunner {
            origin: "ws://192.0.2.10:8765".to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 8765,
            instance: "rpi-host-a-8765._rpi._tcp.local.".to_owned(),
            label: "dev box".to_owned(),
            version: Some("0.2.11".to_owned()),
            scheme: "ws".to_owned(),
            codename: None,
            last_seen_ms: now_ms().saturating_sub(61_000),
            failed_probes: 0,
            failed_since_ms: None,
        });
        while events.try_recv().is_ok() {}

        // First miss opens the window.
        apply_stale_probe_results(&view, vec![(view.snapshot()[0].clone(), false)]);
        let opened = view.snapshot()[0]
            .failed_since_ms
            .expect("first miss opens the window");
        let last_seen_before = view.snapshot()[0].last_seen_ms;

        // Metadata change with an open window: metadata updates, last_seen
        // and the window are preserved.
        apply_mdns_event(
            &view,
            mdns_sd::ServiceEvent::ServiceResolved(sample_info_named(
                "rpi-host-a-8765",
                "192.0.2.10",
                8765,
                "ws",
                "renamed box",
            )),
        );
        let rows = view.snapshot();
        assert_eq!(rows[0].label, "renamed box", "metadata always updates");
        assert_eq!(
            rows[0].last_seen_ms, last_seen_before,
            "metadata change never restamps a window-open row"
        );
        assert_eq!(
            rows[0].failed_since_ms,
            Some(opened),
            "the open window is preserved"
        );
        assert_eq!(rows[0].failed_probes, 1);
        assert!(
            sweep_probe_eligible(
                &rows[0],
                now_ms(),
                TEAM_DISCOVERY_EXPIRY.as_millis() as u64
            ),
            "window-open row remains sweep-eligible"
        );
        assert!(events.try_recv().is_err());

        // The live `/web` verdict resets, and only now a metadata change
        // may restamp last_seen.
        apply_stale_probe_results(&view, vec![(rows[0].clone(), true)]);
        apply_mdns_event(
            &view,
            mdns_sd::ServiceEvent::ServiceResolved(sample_info_named(
                "rpi-host-a-8765",
                "192.0.2.10",
                8765,
                "ws",
                "renamed again",
            )),
        );
        let rows = view.snapshot();
        assert_eq!(rows[0].label, "renamed again");
        assert_eq!(rows[0].failed_probes, 0, "live verdict resets strikes");
        assert!(
            rows[0].failed_since_ms.is_none(),
            "live verdict resets the window first"
        );
        assert!(
            now_ms().saturating_sub(rows[0].last_seen_ms) < 5_000,
            "metadata change restamps once the window is closed"
        );
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn stale_live_probe_refreshes_while_dead_peer_expires() {
        let view = DiscoveryView::for_test();
        for (host, port) in [("192.0.2.10", 18901), ("192.0.2.11", 18902)] {
            view.upsert_runner(DiscoveredRunner {
                origin: format!("ws://{host}:{port}"),
                host: host.to_owned(),
                port,
                instance: format!("rpi-{host}-{port}._rpi._tcp.local."),
                label: "peer".to_owned(),
                version: Some("0.2.11".to_owned()),
                scheme: "ws".to_owned(),
                codename: None,
                last_seen_ms: now_ms().saturating_sub(61_000),
                failed_probes: 0,
                failed_since_ms: None,
            });
        }
        let mut events = view.subscribe();
        while events.try_recv().is_ok() {}
        expire_stale_discovered_with_probe(&view, TEAM_DISCOVERY_EXPIRY, |row| row.port == 18901);
        let rows = view.snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].port, 18901);
        assert!(now_ms().saturating_sub(rows[0].last_seen_ms) < 5_000);
        assert_eq!(
            events.try_recv().expect("dead peer emits gone"),
            DiscoveryEvent::RunnerGone { origin: "ws://192.0.2.11:18902".to_owned() }
        );
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn successful_probe_resets_failure_strikes() {
        // A live verdict restamps last_seen and clears the strike counter, so
        // a transient miss (busy peer) can never accumulate into a removal.
        let view = DiscoveryView::for_test();
        let mut events = view.subscribe();
        view.upsert_runner(DiscoveredRunner {
            origin: "ws://192.0.2.10:18902".to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 18902,
            instance: "rpi-192-0-2-10-18902._rpi._tcp.local.".to_owned(),
            label: "peer".to_owned(),
            version: Some("0.2.11".to_owned()),
            scheme: "ws".to_owned(),
            codename: None,
            last_seen_ms: now_ms().saturating_sub(61_000),
            failed_probes: 0,
            failed_since_ms: None,
        });
        while events.try_recv().is_ok() {}
        let first = view.snapshot()[0].clone();
        apply_stale_probe_results(&view, vec![(first, false)]);
        let rows = view.snapshot();
        assert_eq!(rows[0].failed_probes, 1);
        assert!(
            rows[0].failed_since_ms.is_some_and(|t| now_ms().saturating_sub(t) < 5_000),
            "first miss opens the removal window"
        );

        let second = view.snapshot()[0].clone();
        apply_stale_probe_results(&view, vec![(second, true)]);
        let rows = view.snapshot();
        assert_eq!(
            rows[0].failed_probes, 0,
            "success resets the strike counter"
        );
        assert!(
            rows[0].failed_since_ms.is_none(),
            "success clears the removal window"
        );
        assert!(
            now_ms().saturating_sub(rows[0].last_seen_ms) < 5_000,
            "success restamps last_seen"
        );

        // A miss after success re-opens a fresh window: strikes cannot
        // accumulate across a success, so the row is retained.
        let third = view.snapshot()[0].clone();
        apply_stale_probe_results(&view, vec![(third, false)]);
        let rows = view.snapshot();
        assert_eq!(rows.len(), 1, "post-success miss retains the row");
        assert_eq!(rows[0].failed_probes, 1);
        assert!(
            rows[0].failed_since_ms.is_some_and(|t| now_ms().saturating_sub(t) < 5_000),
            "the next miss opens a fresh removal window"
        );
        assert!(events.try_recv().is_err(), "retained row emits no gone");
    }

    #[test]
    fn identical_resolve_keeps_row_and_stale_verdict_strikes_once() {
        // An identical cached re-advertisement must not refresh liveness
        // (no restamp, no strike reset), so an in-flight stale probe
        // verdict still matches the row — but one strike only opens the
        // bounded removal window; removal stays behind two strikes AND
        // the grace window.
        let view = DiscoveryView::for_test();
        let mut events = view.subscribe();
        view.upsert_runner(DiscoveredRunner {
            origin: "ws://192.0.2.10:18902".to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 18902,
            instance: "rpi-192-0-2-10-18902._rpi._tcp.local.".to_owned(),
            label: "peer".to_owned(),
            version: Some("0.2.11".to_owned()),
            scheme: "ws".to_owned(),
            codename: None,
            last_seen_ms: now_ms().saturating_sub(61_000),
            failed_probes: 0,
            failed_since_ms: None,
        });
        while events.try_recv().is_ok() {}
        let stale_snapshot = view.snapshot()[0].clone();
        // The daemon cache re-announces the peer verbatim while the HTTP
        // probe is in flight: the row must not change at all.
        apply_mdns_event(
            &view,
            mdns_sd::ServiceEvent::ServiceResolved(sample_info_named(
                "rpi-192-0-2-10-18902",
                "192.0.2.10",
                18902,
                "ws",
                "peer",
            )),
        );
        let rows = view.snapshot();
        assert_eq!(
            rows[0].last_seen_ms, stale_snapshot.last_seen_ms,
            "identical re-advertisement does not restamp liveness"
        );
        assert_eq!(rows[0].failed_probes, 0);
        assert!(rows[0].failed_since_ms.is_none());
        assert!(events.try_recv().is_err());

        apply_stale_probe_results(&view, vec![(stale_snapshot, false)]);
        let rows = view.snapshot();
        assert_eq!(
            rows.len(),
            1,
            "one strike can never retire the row by itself"
        );
        assert_eq!(rows[0].failed_probes, 1);
        assert!(
            rows[0]
                .failed_since_ms
                .is_some_and(|t| now_ms().saturating_sub(t) < 5_000),
            "the miss opens the bounded removal window"
        );
        assert!(
            events.try_recv().is_err(),
            "no RunnerGone for a single strike"
        );
    }

    #[test]
    fn removal_requires_two_misses_and_failure_grace_window() {
        // Old last_seen row: a miss opens the bounded removal window
        // ([`TEAM_REMOVAL_GRACE`], measured from the FIRST strike). Misses
        // before the deadline retain the row; the miss at/after the
        // deadline removes it (sweeps re-probe, so strikes keep growing).
        let view = DiscoveryView::for_test();
        let mut events = view.subscribe();
        view.upsert_runner(DiscoveredRunner {
            origin: "ws://192.0.2.10:18902".to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 18902,
            instance: "rpi-192-0-2-10-18902._rpi._tcp.local.".to_owned(),
            label: "peer".to_owned(),
            version: Some("0.2.11".to_owned()),
            scheme: "ws".to_owned(),
            codename: None,
            last_seen_ms: now_ms().saturating_sub(61_000),
            failed_probes: 0,
            failed_since_ms: None,
        });
        while events.try_recv().is_ok() {}

        // First miss opens the window; the row is retained.
        let first = view.snapshot()[0].clone();
        apply_stale_probe_results(&view, vec![(first, false)]);
        let rows = view.snapshot();
        assert_eq!(rows.len(), 1, "first miss keeps the row");
        assert_eq!(rows[0].failed_probes, 1);
        assert!(
            rows[0].failed_since_ms.is_some_and(|t| now_ms().saturating_sub(t) < 5_000),
            "first miss opens the removal window"
        );
        assert!(events.try_recv().is_err());

        // Second miss inside the window (10s into the 60s grace): retained.
        let mut mid = view.snapshot()[0].clone();
        mid.failed_since_ms = Some(now_ms().saturating_sub(10_000));
        view.upsert_runner(mid);
        while events.try_recv().is_ok() {}
        apply_stale_probe_results(&view, vec![(view.snapshot()[0].clone(), false)]);
        assert_eq!(
            view.snapshot().len(),
            1,
            "two misses inside the grace window retain the row"
        );
        assert!(events.try_recv().is_err(), "inside the window emits no gone");

        // Miss at the deadline (window fully elapsed since the first
        // strike): removed.
        let mut aged = view.snapshot()[0].clone();
        aged.failed_since_ms = Some(
            now_ms().saturating_sub(TEAM_REMOVAL_GRACE.as_millis() as u64),
        );
        view.upsert_runner(aged);
        while events.try_recv().is_ok() {}
        apply_stale_probe_results(&view, vec![(view.snapshot()[0].clone(), false)]);
        assert!(view.snapshot().is_empty(), "miss past the deadline removes");
        assert_eq!(
            events.try_recv().expect("deadline miss emits gone"),
            DiscoveryEvent::RunnerGone {
                origin: "ws://192.0.2.10:18902".to_owned(),
            }
        );
    }

    #[test]
    fn scheme_flip_opens_window_and_live_verdict_resets() {
        // A NEW origin for a known host+port under a different scheme
        // (scheme flip) opens the stale sibling's removal window at the
        // flip, without striking it. Re-advertisements of either origin
        // (stale cache dumps) preserve that window with its original
        // start; only the `/web` alive verdict resets it.
        let view = DiscoveryView::for_test();
        let mut events = view.subscribe();
        view.upsert_runner(DiscoveredRunner {
            origin: "ws://192.0.2.10:8765".to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 8765,
            instance: "rpi-host-a-8765._rpi._tcp.local.".to_owned(),
            label: "dev box".to_owned(),
            version: Some("0.2.11".to_owned()),
            scheme: "ws".to_owned(),
            codename: None,
            last_seen_ms: now_ms().saturating_sub(61_000),
            failed_probes: 0,
            failed_since_ms: None,
        });
        while events.try_recv().is_ok() {}

        // Flip: wss advertisement of the same host+port upserts the new
        // origin and opens the ws sibling's window without a strike.
        apply_mdns_event(
            &view,
            mdns_sd::ServiceEvent::ServiceResolved(sample_info_named(
                "rpi-host-a-8765",
                "192.0.2.10",
                8765,
                "wss",
                "dev box",
            )),
        );
        let rows = view.snapshot();
        assert_eq!(rows.len(), 2, "flip upserts the replacement origin");
        let (old, new): (Vec<_>, Vec<_>) = rows
            .into_iter()
            .partition(|row| row.scheme == "ws");
        assert_eq!(old[0].origin, "ws://192.0.2.10:8765");
        assert_eq!(new[0].origin, "wss://192.0.2.10:8765");
        assert_eq!(old[0].failed_probes, 0, "the flip must not strike the sibling");
        let flip_window = old[0]
            .failed_since_ms
            .filter(|t| now_ms().saturating_sub(*t) < 5_000)
            .expect("the flip opens the sibling's removal window");
        assert!(new[0].failed_since_ms.is_none(), "new origin has no window");
        assert!(matches!(
            events.try_recv().expect("flip announces the new origin"),
            DiscoveryEvent::RunnerDiscovered { ref origin, .. } if origin == "wss://192.0.2.10:8765"
        ));
        while events.try_recv().is_ok() {}

        // The old origin re-advertised again (same-origin cache resolve)
        // preserves the flip window with its original start.
        apply_mdns_event(
            &view,
            mdns_sd::ServiceEvent::ServiceResolved(sample_info_named(
                "rpi-host-a-8765",
                "192.0.2.10",
                8765,
                "ws",
                "dev box",
            )),
        );
        let rows = view.snapshot();
        assert_eq!(rows.len(), 2);
        let old = rows.iter().find(|row| row.scheme == "ws").expect("ws row");
        assert_eq!(
            old.failed_since_ms,
            Some(flip_window),
            "re-advertisement preserves the open window"
        );
        while events.try_recv().is_ok() {}

        // A repeat flip advertisement is not a NEW origin: it neither
        // restarts the window nor strikes the sibling.
        apply_mdns_event(
            &view,
            mdns_sd::ServiceEvent::ServiceResolved(sample_info_named(
                "rpi-host-a-8765",
                "192.0.2.10",
                8765,
                "wss",
                "dev box",
            )),
        );
        let rows = view.snapshot();
        assert_eq!(rows.len(), 2);
        let old = rows.iter().find(|row| row.scheme == "ws").expect("ws row");
        assert_eq!(
            old.failed_since_ms,
            Some(flip_window),
            "repeat flip advertisement never restarts the sibling window"
        );
        assert_eq!(old.failed_probes, 0, "flip never strikes");

        // The live `/web` verdict is the only reset.
        let mut ws_row = view
            .snapshot()
            .into_iter()
            .find(|row| row.scheme == "ws")
            .expect("ws row");
        ws_row.failed_since_ms = Some(
            now_ms().saturating_sub(TEAM_REMOVAL_GRACE.as_millis() as u64),
        );
        view.upsert_runner(ws_row);
        while events.try_recv().is_ok() {}
        let snapshot = view
            .snapshot()
            .into_iter()
            .find(|row| row.scheme == "ws")
            .expect("ws row");
        apply_stale_probe_results(&view, vec![(snapshot, true)]);
        let rows = view.snapshot();
        let old = rows.iter().find(|row| row.scheme == "ws").expect("ws row");
        assert_eq!(old.failed_probes, 0, "live verdict resets strikes");
        assert!(
            old.failed_since_ms.is_none(),
            "live verdict resets the window"
        );
    }

    #[test]
    fn open_window_survives_cached_resolves_and_sweep_probes_it() {
        // A cached ServiceResolved during an open removal window must not
        // reset the window or strikes (the peer's own stale cache records
        // cannot manufacture innocence); it only restamps last_seen. The
        // row stays sweep-probe-eligible despite the fresh last_seen; only
        // the `/web` alive verdict resets, and a dead verdict removes at
        // the deadline.
        let view = DiscoveryView::for_test();
        let mut events = view.subscribe();
        view.upsert_runner(DiscoveredRunner {
            origin: "ws://192.0.2.10:8765".to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 8765,
            instance: "rpi-host-a-8765._rpi._tcp.local.".to_owned(),
            label: "dev box".to_owned(),
            version: Some("0.2.11".to_owned()),
            scheme: "ws".to_owned(),
            codename: None,
            last_seen_ms: now_ms().saturating_sub(61_000),
            failed_probes: 0,
            failed_since_ms: None,
        });
        while events.try_recv().is_ok() {}

        // First dead verdict opens the window.
        apply_stale_probe_results(&view, vec![(view.snapshot()[0].clone(), false)]);
        let snapshot = view.snapshot();
        let opened = snapshot[0].failed_since_ms.expect("first miss opens the window");
        assert_eq!(snapshot[0].failed_probes, 1);

        // Cached identical ServiceResolved mid-window: neither last_seen
        // nor the window/strikes may change.
        let last_seen_before = snapshot[0].last_seen_ms;
        apply_mdns_event(
            &view,
            mdns_sd::ServiceEvent::ServiceResolved(sample_info_named(
                "rpi-host-a-8765",
                "192.0.2.10",
                8765,
                "ws",
                "dev box",
            )),
        );
        let rows = view.snapshot();
        assert_eq!(
            rows[0].last_seen_ms, last_seen_before,
            "identical cached resolve must not restamp liveness"
        );
        assert_eq!(rows[0].failed_probes, 1, "cached resolve preserves strikes");
        assert_eq!(
            rows[0].failed_since_ms,
            Some(opened),
            "cached resolve preserves the window origin"
        );

        // Production sweep selection keeps probing the row despite the
        // fresh last_seen.
        let ttl_ms = TEAM_DISCOVERY_EXPIRY.as_millis() as u64;
        let eligible: Vec<_> = view
            .snapshot()
            .into_iter()
            .filter(|row| sweep_probe_eligible(row, now_ms(), ttl_ms))
            .collect();
        assert_eq!(
            eligible.len(),
            1,
            "open window keeps the row sweep-eligible"
        );

        // The live `/web` verdict is the only reset.
        apply_stale_probe_results(&view, vec![(eligible[0].clone(), true)]);
        let rows = view.snapshot();
        assert_eq!(rows[0].failed_probes, 0, "live verdict resets strikes");
        assert!(rows[0].failed_since_ms.is_none(), "live verdict clears the window");
        assert!(events.try_recv().is_err());

        // A fresh dead verdict opens a fresh window; once aged past the
        // grace, the next dead verdict removes the row.
        apply_stale_probe_results(&view, vec![(view.snapshot()[0].clone(), false)]);
        let snapshot = view.snapshot();
        assert_eq!(snapshot[0].failed_probes, 1);
        assert!(
            snapshot[0]
                .failed_since_ms
                .is_some_and(|t| now_ms().saturating_sub(t) < 5_000),
            "a fresh window opens after the reset"
        );
        let mut aged = snapshot[0].clone();
        aged.failed_since_ms = Some(
            now_ms().saturating_sub(TEAM_REMOVAL_GRACE.as_millis() as u64),
        );
        view.upsert_runner(aged);
        while events.try_recv().is_ok() {}
        apply_stale_probe_results(&view, vec![(view.snapshot()[0].clone(), false)]);
        assert!(
            view.snapshot().is_empty(),
            "dead verdict at the deadline removes"
        );
        assert!(matches!(
            events.try_recv().expect("deadline verdict emits gone"),
            DiscoveryEvent::RunnerGone { .. }
        ));
        assert_eq!(view.tombstoned_instances().len(), 1);
    }


    #[test]
    fn cached_resolved_tombstoned_instance_resurrects_only_after_web_probe() {
        // A steady-state cache dump re-emits `ServiceResolved` for a
        // tombstoned instance. The re-advertisement must not re-insert the
        // row directly (a reused port serving arbitrary HTML cannot announce
        // itself); it only refreshes the tombstone candidate, and the row
        // returns solely after a live `/web` re-verification verdict.
        let view = DiscoveryView::for_test();
        let mut events = view.subscribe();
        view.upsert_runner(DiscoveredRunner {
            origin: "ws://192.0.2.10:8765".to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 8765,
            instance: "rpi-host-a-8765._rpi._tcp.local.".to_owned(),
            label: "dev box".to_owned(),
            version: Some("0.2.11".to_owned()),
            scheme: "ws".to_owned(),
            codename: None,
            last_seen_ms: now_ms().saturating_sub(61_000),
            failed_probes: 0,
            failed_since_ms: None,
        });
        while events.try_recv().is_ok() {}

        // Two-strike removal tombstones the origin.
        let first = view.snapshot()[0].clone();
        apply_stale_probe_results(&view, vec![(first, false)]);
        // Wind the removal window back so the second strike lands at the
        // deadline (a real sweep re-probes every 15s, so this is the point
        // the removal fires in production too).
        let mut aged = view.snapshot()[0].clone();
        aged.failed_since_ms = Some(
            now_ms().saturating_sub(TEAM_REMOVAL_GRACE.as_millis() as u64),
        );
        view.upsert_runner(aged);
        let second = view.snapshot()[0].clone();
        apply_stale_probe_results(&view, vec![(second, false)]);
        assert!(view.snapshot().is_empty());
        assert_eq!(view.tombstoned_instances().len(), 1);

        while events.try_recv().is_ok() {}

        // A sibling port resolves under its own instance and is not gated.
        apply_mdns_event(
            &view,
            mdns_sd::ServiceEvent::ServiceResolved(sample_info_named(
                "rpi-host-a-8766",
                "192.0.2.10",
                8766,
                "ws",
                "dev box",
            )),
        );
        assert_eq!(
            view.snapshot().len(),
            1,
            "a non-tombstoned sibling instance still upserts normally"
        );
        // Retire the sibling through the test-only origin removal (a real
        // goodbye would only open its removal window, which the assertions
        // below do not depend on).
        view.remove_origin("ws://192.0.2.10:8766");
        assert!(matches!(
            events.try_recv().expect("sibling announced"),
            DiscoveryEvent::RunnerDiscovered { ref origin, .. } if origin == "ws://192.0.2.10:8766"
        ));
        assert!(matches!(
            events.try_recv().expect("sibling gone"),
            DiscoveryEvent::RunnerGone { .. }
        ));

        // The daemon cache dump re-resolves the tombstoned instance:
        // candidate metadata refreshes, but the row stays gone and nothing
        // is announced.
        apply_mdns_event(
            &view,
            mdns_sd::ServiceEvent::ServiceResolved(sample_info_named(
                "rpi-host-a-8765",
                "192.0.2.10",
                8765,
                "ws",
                "dev box",
            )),
        );
        assert!(
            view.snapshot().is_empty(),
            "cache-dump re-advertisement must not resurrect a tombstone directly"
        );
        assert!(
            events.try_recv().is_err(),
            "no RunnerDiscovered for a tombstoned instance"
        );
        let candidates = view.tombstoned_instances();
        assert_eq!(candidates.len(), 1, "tombstone survives the dump");
        assert!(
            now_ms().saturating_sub(candidates[0].last_advertised_ms) < 5_000,
            "resolved record restamps the candidate's advertised window"
        );

        // A dead re-verification verdict keeps the tombstone waiting.
        apply_tombstone_reverify_results(
            &view,
            view.tombstoned_instances().into_iter().map(|t| (t, false)).collect(),
        );
        assert!(view.snapshot().is_empty());
        assert_eq!(
            view.tombstoned_instances().len(),
            1,
            "a failed probe stays tombstoned for the next dump"
        );

        // Only the live `/web` verdict resurrects the row.
        apply_tombstone_reverify_results(
            &view,
            view.tombstoned_instances().into_iter().map(|t| (t, true)).collect(),
        );
        let rows = view.snapshot();
        assert_eq!(rows.len(), 1, "live verdict resurrects via reverify only");
        assert_eq!(rows[0].origin, "ws://192.0.2.10:8765");
        assert_eq!(rows[0].failed_probes, 0, "resurrection clears strikes");
        assert!(
            view.tombstoned_instances().is_empty(),
            "resurrection clears the tombstone"
        );
        assert!(matches!(
            events.try_recv().expect("resurrection announces the row"),
            DiscoveryEvent::RunnerDiscovered { ref origin, .. } if origin == "ws://192.0.2.10:8765"
        ));
        assert!(events.try_recv().is_err());
    }


    #[test]
    fn missing_goodbye_expires_without_service_removed() {
        let view = DiscoveryView::for_test();
        let mut events = view.subscribe();
        view.upsert_runner(DiscoveredRunner {
            origin: "ws://192.0.2.10:18902".to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 18902,
            instance: "rpi-192-0-2-10-18902._rpi._tcp.local.".to_owned(),
            label: "peer".to_owned(),
            version: Some("0.2.11".to_owned()),
            scheme: "ws".to_owned(),
            codename: None,
            last_seen_ms: now_ms().saturating_sub(61_000),
            failed_probes: 0,
            failed_since_ms: None,
        });
        while events.try_recv().is_ok() {}
        expire_stale_discovered(&view, TEAM_DISCOVERY_EXPIRY);
        assert!(view.snapshot().is_empty(), "missing ServiceRemoved still expires at 60s");
        assert_eq!(
            events.try_recv().expect("stale row emits gone"),
            DiscoveryEvent::RunnerGone {
                origin: "ws://192.0.2.10:18902".to_owned(),
            }
        );
        assert!(events.try_recv().is_err(), "exactly one RunnerGone");
    }

    #[test]
    fn later_service_resolved_upserts_a_missed_peer() {
        // Restart-convergence: mdns-sd can cache a peer's records without
        // ever emitting the first ServiceResolved (piecemeal arrival while
        // browse was mid-start). A later startup re-browse must still upsert
        // that peer into an already-populated roster.
        let view = DiscoveryView::for_test();
        let first = sample_info_named("rpi-192-0-2-6-18801", "192.0.2.6", 18801, "ws", "a");
        upsert_discovered(&view, &first);
        assert_eq!(view.snapshot().len(), 1);

        let missed = sample_info_named("rpi-192-0-2-6-18802", "192.0.2.6", 18802, "ws", "b");
        apply_mdns_event(
            &view,
            mdns_sd::ServiceEvent::ServiceResolved(missed.clone()),
        );
        let origins: Vec<_> = view
            .snapshot()
            .into_iter()
            .map(|runner| runner.origin)
            .collect();
        assert_eq!(
            origins,
            vec![
                "ws://192.0.2.6:18801".to_owned(),
                "ws://192.0.2.6:18802".to_owned()
            ]
        );

        apply_mdns_event(&view, mdns_sd::ServiceEvent::ServiceResolved(missed));
        assert_eq!(
            view.snapshot().len(),
            2,
            "duplicate resolved events must not duplicate the roster"
        );
    }

    #[test]
    fn apply_mdns_event_ignores_found_and_search_noise() {
        let view = DiscoveryView::for_test();
        apply_mdns_event(
            &view,
            mdns_sd::ServiceEvent::ServiceFound(
                TEAM_MDNS_SERVICE.to_owned(),
                "rpi-192-0-2-6-18802._rpi._tcp.local.".to_owned(),
            ),
        );
        apply_mdns_event(
            &view,
            mdns_sd::ServiceEvent::SearchStarted(TEAM_MDNS_SERVICE.to_owned()),
        );
        apply_mdns_event(
            &view,
            mdns_sd::ServiceEvent::SearchStopped(TEAM_MDNS_SERVICE.to_owned()),
        );
        assert!(
            view.snapshot().is_empty(),
            "Found/Search events must not invent roster rows"
        );
    }

    #[test]
    fn dotted_old_instance_name_is_ignored() {
        // Old binaries advertised format!("rpi-{ipv4}"), so the on-wire
        // fullname is `rpi-192.0.2.15._rpi._tcp.local.` — extra DNS labels.
        // Avahi reports that as service `rpi-192` in domain
        // `0.2.15._rpi._tcp.local`. Ingest must drop it (no event, no
        // phantom ws/http runner) while a well-formed same-host record upserts.
        let view = DiscoveryView::for_test();
        let mut events = view.subscribe();
        let malformed = sample_info_named("rpi-192.0.2.15", "192.0.2.15", 8765, "ws", "");
        assert!(
            !is_well_formed_mdns_instance(malformed.get_fullname()),
            "dotted IPv4 instance must fail the single-label check: {}",
            malformed.get_fullname()
        );
        upsert_discovered(&view, &malformed);
        apply_mdns_event(&view, mdns_sd::ServiceEvent::ServiceResolved(malformed));
        assert!(
            view.snapshot().is_empty(),
            "malformed old-format record must not enter the roster"
        );
        assert!(
            events.try_recv().is_err(),
            "malformed record must be event-free"
        );

        let well_formed = sample_info_named(
            "rpi-192-0-2-15-8765",
            "192.0.2.15",
            8765,
            "wss",
            "192.0.2.15",
        );
        upsert_discovered(&view, &well_formed);
        let rows = view.snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].origin, "wss://192.0.2.15:8765");
        assert_eq!(rows[0].scheme, "wss");
        assert_eq!(rows[0].version.as_deref(), Some("0.2.11"));
        assert_eq!(rows[0].label, "192.0.2.15");
        assert!(matches!(
            events.try_recv().expect("well-formed upsert emits discovered"),
            DiscoveryEvent::RunnerDiscovered { ref origin, ref scheme, .. }
                if origin == "wss://192.0.2.15:8765" && scheme == "wss"
        ));
    }

    #[test]
    fn service_removed_opens_window_and_verdicts_decide() {
        // A goodbye is not trusted for immediate retirement: matching rows
        // stay visible with two strikes and a fresh removal window, sibling
        // port and remote host untouched. A live `/web` verdict resets the
        // window; a dead verdict retains inside the window and removes at
        // the deadline (re-tombstoning for `/web` re-verification).
        let view = DiscoveryView::for_test();
        let mut events = view.subscribe();
        let first = sample_info_named("rpi-192-0-2-6-18801", "192.0.2.6", 18801, "ws", "a");
        let second = sample_info_named("rpi-192-0-2-6-18802", "192.0.2.6", 18802, "ws", "b");
        upsert_discovered(&view, &first);
        upsert_discovered(&view, &second);
        while events.try_recv().is_ok() {}

        apply_mdns_event(
            &view,
            mdns_sd::ServiceEvent::ServiceRemoved(
                TEAM_MDNS_SERVICE.to_owned(),
                first.get_fullname().to_owned(),
            ),
        );
        let rows = view.snapshot();
        assert_eq!(rows.len(), 2, "the goodbye must not remove the row");
        assert!(events.try_recv().is_err(), "goodbye emits no RunnerGone");
        let gone_row = rows
            .iter()
            .find(|row| row.instance == first.get_fullname())
            .expect("goodbye row still visible");
        assert_eq!(gone_row.failed_probes, 2, "goodbye seeds two strikes");
        assert!(
            gone_row
                .failed_since_ms
                .is_some_and(|t| now_ms().saturating_sub(t) < 5_000),
            "goodbye opens the removal window"
        );
        let sibling = rows
            .iter()
            .find(|row| row.instance == second.get_fullname())
            .expect("sibling port untouched");
        assert_eq!(sibling.failed_probes, 0, "sibling port never struck");
        assert!(sibling.failed_since_ms.is_none(), "sibling has no window");

        // A live verdict resets strikes and the window.
        let goodbye_instance = gone_row.clone();
        apply_stale_probe_results(&view, vec![(goodbye_instance, true)]);
        let rows = view.snapshot();
        let reset = rows
            .iter()
            .find(|row| row.instance == first.get_fullname())
            .expect("row survives a live verdict");
        assert_eq!(reset.failed_probes, 0, "live verdict resets strikes");
        assert!(
            reset.failed_since_ms.is_none(),
            "live verdict resets the window"
        );

        // A dead verdict before the deadline retains the row.
        apply_mdns_event(
            &view,
            mdns_sd::ServiceEvent::ServiceRemoved(
                TEAM_MDNS_SERVICE.to_owned(),
                first.get_fullname().to_owned(),
            ),
        );
        while events.try_recv().is_ok() {}
        let snapshot = view
            .snapshot()
            .into_iter()
            .find(|row| row.instance == first.get_fullname())
            .expect("row visible");
        apply_stale_probe_results(&view, vec![(snapshot, false)]);
        assert_eq!(
            view.snapshot().len(),
            2,
            "dead verdict inside the window retains the row"
        );
        assert!(events.try_recv().is_err(), "inside the window emits no gone");

        // A dead verdict at the deadline removes the row (and re-tombstones
        // it for `/web` re-verification).
        let mut aged = view
            .snapshot()
            .into_iter()
            .find(|row| row.instance == first.get_fullname())
            .expect("row still visible");
        aged.failed_since_ms = Some(
            now_ms().saturating_sub(TEAM_REMOVAL_GRACE.as_millis() as u64),
        );
        view.upsert_runner(aged);
        while events.try_recv().is_ok() {}
        let snapshot = view
            .snapshot()
            .into_iter()
            .find(|row| row.instance == first.get_fullname())
            .expect("row visible");
        apply_stale_probe_results(&view, vec![(snapshot, false)]);
        let remaining: Vec<_> = view
            .snapshot()
            .into_iter()
            .map(|row| row.origin)
            .collect();
        assert_eq!(remaining, vec!["ws://192.0.2.6:18802".to_owned()]);
        assert_eq!(
            events.try_recv().expect("deadline verdict emits gone"),
            DiscoveryEvent::RunnerGone {
                origin: "ws://192.0.2.6:18801".to_owned(),
            }
        );
        assert!(events.try_recv().is_err());
        assert_eq!(view.tombstoned_instances().len(), 1);
    }

    #[test]
    fn stale_row_expires_and_emits_one_runner_gone() {
        let view = DiscoveryView::for_test();
        let mut events = view.subscribe();
        view.upsert_runner(DiscoveredRunner {
            origin: "ws://192.0.2.10:8765".to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 8765,
            instance: "rpi-host-a-8765._rpi._tcp.local.".to_owned(),
            label: "stale".to_owned(),
            version: Some("0.2.11".to_owned()),
            scheme: "ws".to_owned(),
            codename: None,
            last_seen_ms: now_ms().saturating_sub(61_000),
            failed_probes: 0,
            failed_since_ms: None,
        });
        view.upsert_runner(DiscoveredRunner {
            origin: "ws://192.0.2.6:18802".to_owned(),
            host: "192.0.2.6".to_owned(),
            port: 18802,
            instance: "rpi-192-0-2-6-18802._rpi._tcp.local.".to_owned(),
            label: "fresh".to_owned(),
            version: Some("0.2.11".to_owned()),
            scheme: "ws".to_owned(),
            codename: None,
            last_seen_ms: now_ms(),
            failed_probes: 0,
            failed_since_ms: None,
        });
        while events.try_recv().is_ok() {}
        expire_stale_discovered(&view, TEAM_DISCOVERY_EXPIRY);
        let remaining: Vec<_> = view
            .snapshot()
            .into_iter()
            .map(|runner| runner.origin)
            .collect();
        assert_eq!(remaining, vec!["ws://192.0.2.6:18802".to_owned()]);
        assert_eq!(
            events.try_recv().expect("stale origin emits gone"),
            DiscoveryEvent::RunnerGone {
                origin: "ws://192.0.2.10:8765".to_owned(),
            }
        );
        assert!(events.try_recv().is_err(), "exactly one RunnerGone");
    }

    #[test]
    fn flipped_scheme_converges_to_one_row_after_bound() {
        // Same instance, old ws origin never re-announced; no ServiceRemoved.
        let view = DiscoveryView::for_test();
        let mut events = view.subscribe();
        let instance = "rpi-host-a-8765._rpi._tcp.local.".to_owned();
        view.upsert_runner(DiscoveredRunner {
            origin: "ws://192.0.2.10:8765".to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 8765,
            instance: instance.clone(),
            label: "dev box".to_owned(),
            version: Some("0.2.11".to_owned()),
            scheme: "ws".to_owned(),
            codename: None,
            last_seen_ms: now_ms().saturating_sub(61_000),
            failed_probes: 0,
            failed_since_ms: None,
        });
        view.upsert_runner(DiscoveredRunner {
            origin: "wss://192.0.2.10:8765".to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 8765,
            instance,
            label: "dev box".to_owned(),
            version: Some("0.2.11".to_owned()),
            scheme: "wss".to_owned(),
            codename: None,
            last_seen_ms: now_ms(),
            failed_probes: 0,
            failed_since_ms: None,
        });
        while events.try_recv().is_ok() {}
        expire_stale_discovered(&view, TEAM_DISCOVERY_EXPIRY);
        let rows = view.snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].origin, "wss://192.0.2.10:8765");
        assert_eq!(rows[0].scheme, "wss");
        assert_eq!(
            events.try_recv().expect("phantom ws origin emits gone"),
            DiscoveryEvent::RunnerGone {
                origin: "ws://192.0.2.10:8765".to_owned(),
            }
        );
        assert!(events.try_recv().is_err(), "exactly one RunnerGone");
    }

    #[test]
    fn two_real_ports_per_host_both_survive_sweep() {
        let view = DiscoveryView::for_test();
        let mut events = view.subscribe();
        let a = sample_info_named("rpi-192-0-2-6-18801", "192.0.2.6", 18801, "ws", "a");
        let b = sample_info_named("rpi-192-0-2-6-18802", "192.0.2.6", 18802, "ws", "b");
        upsert_discovered(&view, &a);
        upsert_discovered(&view, &b);
        while events.try_recv().is_ok() {}
        expire_stale_discovered(&view, TEAM_DISCOVERY_EXPIRY);
        let origins: Vec<_> = view
            .snapshot()
            .into_iter()
            .map(|runner| runner.origin)
            .collect();
        assert_eq!(
            origins,
            vec![
                "ws://192.0.2.6:18801".to_owned(),
                "ws://192.0.2.6:18802".to_owned()
            ]
        );
        assert!(
            events.try_recv().is_err(),
            "fresh sibling ports emit nothing"
        );
    }

    #[test]
    fn expiry_never_mutates_scheme_or_origin() {
        let view = DiscoveryView::for_test();
        let mut events = view.subscribe();
        let instance = "rpi-host-a-8765._rpi._tcp.local.".to_owned();
        view.upsert_runner(DiscoveredRunner {
            origin: "ws://192.0.2.10:8765".to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 8765,
            instance: instance.clone(),
            label: "dev box".to_owned(),
            version: Some("0.2.11".to_owned()),
            scheme: "ws".to_owned(),
            codename: None,
            last_seen_ms: now_ms(),
            failed_probes: 0,
            failed_since_ms: None,
        });
        view.upsert_runner(DiscoveredRunner {
            origin: "wss://192.0.2.10:8765".to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 8765,
            instance,
            label: "dev box".to_owned(),
            version: Some("0.2.11".to_owned()),
            scheme: "wss".to_owned(),
            codename: None,
            last_seen_ms: now_ms(),
            failed_probes: 0,
            failed_since_ms: None,
        });
        let before: Vec<_> = view
            .snapshot()
            .into_iter()
            .map(|runner| (runner.origin, runner.scheme))
            .collect();
        assert_eq!(
            before,
            vec![
                ("ws://192.0.2.10:8765".to_owned(), "ws".to_owned()),
                ("wss://192.0.2.10:8765".to_owned(), "wss".to_owned()),
            ]
        );
        while events.try_recv().is_ok() {}
        expire_stale_discovered(&view, TEAM_DISCOVERY_EXPIRY);
        let after: Vec<_> = view
            .snapshot()
            .into_iter()
            .map(|runner| (runner.origin, runner.scheme))
            .collect();
        assert_eq!(after, before, "fresh scheme/origin stay unchanged");
        assert!(events.try_recv().is_err());
    }
    #[test]
    fn advertise_properties_include_codename_when_set() {
        let properties = mdns_advertise_properties("192.0.2.10", "wss", Some("dev-box"));
        assert_eq!(properties.get("codename").map(String::as_str), Some("dev-box"));
        assert_eq!(properties.get("scheme").map(String::as_str), Some("wss"));
        assert_eq!(properties.get("label").map(String::as_str), Some("192.0.2.10"));
        assert!(properties.contains_key("version"), "legacy TXT set intact");
    }

    #[test]
    fn advertise_and_ingest_preserve_valid_double_hyphen() {
        // Authority grammar allows consecutive hyphens; the mDNS mirror must
        // not collapse an already-valid name on advertise or ingest.
        let advertised = mdns_advertise_properties("192.0.2.10", "wss", Some("a--b"));
        assert_eq!(advertised.get("codename").map(String::as_str), Some("a--b"));
        assert_eq!(sanitize_codename("a--b").as_deref(), Some("a--b"));

        let view = DiscoveryView::for_test();
        let mut events = view.subscribe();
        upsert_discovered(
            &view,
            &sample_info_codename(
                "rpi-host-a-8765",
                "192.0.2.10",
                8765,
                "wss",
                "dev box",
                Some("a--b"),
            ),
        );
        let rows = view.snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].codename.as_deref(), Some("a--b"));
        assert!(matches!(
            events.try_recv().expect("discovered event"),
            DiscoveryEvent::RunnerDiscovered { ref codename, .. }
                if codename.as_deref() == Some("a--b")
        ));
    }

    #[test]
    fn advertise_properties_omit_codename_when_none() {
        // None (and empty) must omit the property entirely — no empty-string
        // TXT value, so pre-codename listeners see an unchanged property set.
        // Invalid names are omitted the same way; they never leave the
        // authority plane.
        for codename in [None, Some(""), Some("abc-")] {
            let properties = mdns_advertise_properties("192.0.2.10", "wss", codename);
            assert!(!properties.contains_key("codename"), "{codename:?}");
            assert_eq!(properties.len(), 3, "{codename:?}");
        }
    }

    #[test]
    fn sanitize_codename_downcases_collapses_and_caps() {
        assert_eq!(sanitize_codename("a--b").as_deref(), Some("a--b"));
        assert_eq!(sanitize_codename("Dev--Box").as_deref(), Some("dev-box"));
        assert_eq!(sanitize_codename("  Raspberry Pi 5  ").as_deref(), Some("raspberrypi5"));
        assert_eq!(sanitize_codename("\u{7}rpi\u{1f}box").as_deref(), Some("rpibox"));
        assert_eq!(sanitize_codename("beta--Rename!").as_deref(), Some("beta-rename"));
        assert_eq!(sanitize_codename("-edge-").as_deref(), Some("edge"));
        // Cap at 48 and re-trim a trailing dash reintroduced by the cap.
        let long = format!("{}-b", "a".repeat(50));
        assert_eq!(sanitize_codename(&long), Some("a".repeat(48)));
        let capped_dash = format!("{}b", "a".repeat(47).as_str()); // 48 chars, no dash
        assert_eq!(sanitize_codename(&capped_dash), Some(capped_dash.clone()));
        assert_eq!(sanitize_codename("---"), None);
        assert_eq!(sanitize_codename("!!!"), None);
        assert_eq!(sanitize_codename(""), None);
        assert_eq!(sanitize_codename("abc-").as_deref(), Some("abc"));
    }

    #[test]
    fn codename_txt_parses_into_row_and_discovery_event() {
        let view = DiscoveryView::for_test();
        let mut events = view.subscribe();
        upsert_discovered(
            &view,
            &sample_info_codename("rpi-host-a-8765", "192.0.2.10", 8765, "wss", "dev box", Some("Dev--Box")),
        );
        let rows = view.snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].codename.as_deref(), Some("dev-box"));
        assert!(matches!(
            events.try_recv().expect("discovered event"),
            DiscoveryEvent::RunnerDiscovered { ref codename, .. }
                if codename.as_deref() == Some("dev-box")
        ));
    }

    #[test]
    fn malformed_codename_txt_downgrades_to_none_without_rejecting() {
        // Control chars drop, non-grammar characters drop, edge dashes trim:
        // "!!! \u{7} --" collapses to nothing, which must downgrade the
        // codename to None while still admitting the advertisement.
        let view = DiscoveryView::for_test();
        let mut events = view.subscribe();
        upsert_discovered(
            &view,
            &sample_info_codename(
                "rpi-host-a-8765",
                "192.0.2.10",
                8765,
                "wss",
                "dev box",
                Some("!!! \u{7} --"),
            ),
        );
        let rows = view.snapshot();
        assert_eq!(rows.len(), 1, "malformed codename must not reject the row");
        assert_eq!(rows[0].codename, None);
        assert!(matches!(
            events.try_recv().expect("discovered event"),
            DiscoveryEvent::RunnerDiscovered { codename: None, .. }
        ));
    }

    #[test]
    fn old_listener_without_codename_txt_stays_compatible() {
        // Pre-codename binaries advertise the legacy TXT set; absence parses
        // to None and the row is otherwise byte-identical to before.
        let view = DiscoveryView::for_test();
        let mut events = view.subscribe();
        upsert_discovered(&view, &sample_info("wss", "192.0.2.15"));
        let rows = view.snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].codename, None);
        assert_eq!(rows[0].label, "192.0.2.15");
        assert!(matches!(
            events.try_recv().expect("discovered event"),
            DiscoveryEvent::RunnerDiscovered { ref codename, .. } if codename.is_none()
        ));
    }

    #[test]
    fn update_branch_refreshes_codename_in_place() {
        // Re-advertisement (startup re-browse / wire update) refreshes the
        // codename in the update branch without emitting a second event.
        let view = DiscoveryView::for_test();
        let mut events = view.subscribe();
        upsert_discovered(
            &view,
            &sample_info_codename("rpi-host-a-8765", "192.0.2.10", 8765, "wss", "dev box", Some("alpha")),
        );
        assert_eq!(view.snapshot()[0].codename.as_deref(), Some("alpha"));
        assert!(events.try_recv().is_ok(), "first insert emits discovered");

        upsert_discovered(
            &view,
            &sample_info_codename("rpi-host-a-8765", "192.0.2.10", 8765, "wss", "dev box", Some("Beta--Rename!")),
        );
        let rows = view.snapshot();
        assert_eq!(rows.len(), 1, "same origin still collapses to one row");
        assert_eq!(rows[0].codename.as_deref(), Some("beta-rename"));
        assert!(events.try_recv().is_err(), "update emits no second event");

        // A re-advertisement that stops carrying the TXT downgrades in place.
        upsert_discovered(
            &view,
            &sample_info_codename("rpi-host-a-8765", "192.0.2.10", 8765, "wss", "dev box", None),
        );
        assert_eq!(view.snapshot()[0].codename, None);
        assert!(events.try_recv().is_err(), "still exactly one event total");
    }

    #[test]
    fn liveness_marker_accepts_embedded_page_and_case_variants() {
        // The embedded page carries the lowercase doctype first and the
        // title well inside the first 4KiB; matching is case-insensitive so
        // template/formatting changes cannot false-negative a healthy peer.
        let page = b"<!doctype html>\n<html><head><title>rpi web</title></head></html>";
        assert!(liveness_marker_present(page));
        let uppercase = b"<!DOCTYPE html>\n<html><head><TITLE>RPI WEB</TITLE></head></html>";
        assert!(liveness_marker_present(uppercase));
        let mixed = b"<html><head><title>rpi web</title></head><body><!doctype html></body></html>";
        assert!(liveness_marker_present(mixed));
    }

    #[test]
    fn liveness_marker_rejects_arbitrary_html_and_partial_prefixes() {
        // A reused port serving any other 200 HTML page (proxy, captive
        // portal, unrelated app) must not resurrect a dead row.
        let arbitrary = b"<!doctype html>\n<html><head><title>Welcome to nginx</title></head></html>";
        assert!(!liveness_marker_present(arbitrary));
        let no_doctype = b"<html><head><title>rpi web</title></head></html>";
        assert!(!liveness_marker_present(no_doctype));
        // A marker split across a chunk boundary must still match once the
        // next chunk arrives (the check re-runs on the accumulated prefix).
        let page = b"<!doctype html>\n<html><head><title>rpi web</title></head></html>";
        let title_end = page
            .windows(b"</title>".len())
            .position(|window| window == b"</title>")
            .expect("title marker present")
            + b"</title>".len();
        assert!(
            !liveness_marker_present(&page[..title_end - 1]),
            "prefix ending one byte before the title marker must not match"
        );
        assert!(
            liveness_marker_present(&page[..title_end]),
            "prefix completing the title marker must match"
        );
        assert!(liveness_marker_present(page));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry_in(dir: &Path) -> TeamRegistry {
        TeamRegistry::open(dir).expect("open registry")
    }

    #[test]
    fn create_team_round_trips_through_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = registry_in(dir.path());
        let team = registry
            .create_team("release", "pi-rs", "/workspace/pi-rs")
            .expect("create");
        assert!(!team.id.is_empty());
        assert_eq!(team.members.len(), 0);

        let reloaded = TeamRegistry::open(dir.path()).expect("reopen");
        assert_eq!(reloaded.list(), vec![team]);
    }

    #[test]
    fn duplicate_team_names_are_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = registry_in(dir.path());
        registry
            .create_team("release", "pi-rs", "/workspace/pi-rs")
            .expect("first create");
        let error = registry
            .create_team("release", "other", "/workspace/other")
            .expect_err("duplicate name");
        assert!(error.to_string().contains("already exists"));
    }

    #[test]
    fn join_stores_token_owner_only_and_away_from_registry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agent_dir = dir.path();
        let registry = registry_in(agent_dir);
        let team = registry
            .create_team("release", "pi-rs", "/workspace/pi-rs")
            .expect("create");
        let member = registry
            .join(
                &team.id,
                "ws://127.0.0.1:8765",
                "secret-token",
                "dev-box",
                Some("worker"),
            )
            .expect("join");
        assert_eq!(member.origin, "ws://127.0.0.1:8765");
        assert_eq!(member.role.as_deref(), Some("worker"));

        // The registry document never carries the token.
        let registry_text =
            fs::read_to_string(agent_dir.join("teams/registry.json")).expect("registry");
        assert!(!registry_text.contains("secret-token"));
        // The token lives in its own file, owner-only on Unix.
        let token_file = agent_dir
            .join("teams/tokens")
            .join(&team.id)
            .join(format!("{}.token", member.runner_id));
        assert_eq!(
            fs::read_to_string(&token_file).expect("token").trim(),
            "secret-token"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&token_file)
                .expect("token metadata")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o077,
                0,
                "token file must not be group/other readable"
            );
        }
    }

    #[test]
    fn leave_removes_member_and_token() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = registry_in(dir.path());
        let team = registry
            .create_team("release", "pi-rs", "/workspace/pi-rs")
            .expect("create");
        let member = registry
            .join(&team.id, "ws://127.0.0.1:8765", "tok", "dev-box", None)
            .expect("join");
        registry.leave(&team.id, &member.runner_id).expect("leave");
        let team = registry.team(&team.id).expect("team");
        assert!(team.members.is_empty());
        assert_eq!(
            registry.runner_token(&team.id, &member.runner_id).is_err(),
            true,
            "token must be gone after leave"
        );
    }

    #[test]
    fn origin_validation_rejects_paths_and_credentials() {
        for bad in [
            "http://127.0.0.1:8765",
            "ws://127.0.0.1:8765/chat",
            "ws://127.0.0.1:8765?x=1",
            "ws://user:pass@127.0.0.1:8765",
            "not a url",
        ] {
            assert!(validate_origin(bad).is_err(), "{bad} must be rejected");
        }
        assert_eq!(
            validate_origin("ws://127.0.0.1:8765").expect("ok"),
            "ws://127.0.0.1:8765"
        );
        assert_eq!(
            validate_origin("  wss://host.example:443  ").expect("trimmed ok"),
            "wss://host.example:443"
        );
    }

    #[test]
    fn runner_websocket_url_maps_origin_to_ws_path() {
        // Pre-fix connect_runner used `{origin}/chat`. Listeners only upgrade
        // `/ws` (web/src/chat/auth.ts parseRunnerEndpoint), so every probe
        // 404'd and team_status reported members offline.
        assert_eq!(
            runner_websocket_url("ws://127.0.0.1:8765").expect("ws"),
            "ws://127.0.0.1:8765/ws"
        );
        assert_eq!(
            runner_websocket_url("wss://host.example:443").expect("wss"),
            "wss://host.example:443/ws"
        );
        assert_eq!(
            runner_websocket_url("http://192.0.2.6:18801").expect("http"),
            "ws://192.0.2.6:18801/ws"
        );
        assert_eq!(
            runner_websocket_url("https://192.0.2.15:8765").expect("https"),
            "wss://192.0.2.15:8765/ws"
        );
        assert_eq!(
            runner_websocket_url("ws://127.0.0.1:8765/chat").expect("legacy /chat path"),
            "ws://127.0.0.1:8765/ws"
        );
        let url = runner_websocket_url("ws://127.0.0.1:8765").expect("ws");
        assert!(
            !url.ends_with("/chat"),
            "must not dial /chat (listener 404): {url}"
        );
        assert!(url.ends_with("/ws"), "must dial /ws: {url}");
    }

    #[test]
    fn team_discovery_drop_stops_worker_thread() {
        // Drop must signal the worker and join it: the worker's recv_timeout
        // is bounded (≤ sweep interval) and unpark() wakes it immediately, so
        // join cannot deadlock. A leaked worker would keep the mDNS daemon
        // alive past the handle's drop.
        let mut discovery = TeamDiscovery::start().expect("start discovery");
        let worker = discovery
            .take_worker()
            .expect("worker handle present before drop");
        drop(discovery);
        worker
            .join()
            .expect("worker thread must exit promptly on drop");
    }
}
