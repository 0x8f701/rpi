use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    future::{Future, pending},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, bail};
use parking_lot::Mutex;
use pi_agent::{
    AbortSignal, AgentTool, AgentToolResult, ThinkingLevel, ToolCapability, ToolExecutionMode,
};
use pi_ai::{
    ApiProvider, AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream,
    ContentBlock, Context, CustomMessageContent, Message, Model, Schema, SimpleStreamFn,
    SimpleStreamOptions, StopReason, StreamFn, StreamOptions, ToolCall,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::json;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter},
    process::{Child, ChildStdin, ChildStdout},
    sync::{Mutex as AsyncMutex, OwnedMutexGuard, broadcast, mpsc},
    task::JoinHandle,
    time::{sleep, timeout},
};
use uuid::Uuid;

use crate::quickjs_host::QuickJsExtensionHost;

pub const EXTENSION_PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_MAX_EXTENSION_FRAME_BYTES: usize = 1024 * 1024;
const MAX_REQUEST_TIMEOUT_MS: u64 = 10 * 60 * 1000;
const STDERR_TAIL_BYTES: usize = 16 * 1024;
const RUNTIME_EVENT_BUFFER: usize = 256;

pub type ExtensionFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionCapability {
    Commands,
    Tools,
    EventHooks,
    MessageRenderers,
    ProviderMetadata,
    Provider,
    SessionActions,
    Ui,
    /// Overlay registration (`pi.registerOverlay`): the extension supplies a
    /// render function whose returned content rows the host displays inside
    /// its own bordered overlay panel. Content only — the host draws the
    /// border and owns focus/key routing.
    Overlays,
}

impl ExtensionCapability {
    pub const ALL: [Self; 9] = [
        Self::Commands,
        Self::Tools,
        Self::EventHooks,
        Self::MessageRenderers,
        Self::ProviderMetadata,
        Self::Provider,
        Self::SessionActions,
        Self::Ui,
        Self::Overlays,
    ];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionUiCapability {
    Select,
    Confirm,
    Input,
    Editor,
    Notify,
    Status,
    Widget,
    Title,
    SetEditorText,
    EditorText,
    Working,
    HiddenThinking,
    Theme,
    ToolsExpanded,
    /// `ctx.overlay.open` / `ctx.overlay.setRows`: opening an extension
    /// overlay from an event handler and pushing dynamic content rows into
    /// the currently displayed panel.
    Overlay,
}

impl ExtensionUiCapability {
    pub const ALL: [Self; 15] = [
        Self::Select,
        Self::Confirm,
        Self::Input,
        Self::Editor,
        Self::Notify,
        Self::Status,
        Self::Widget,
        Self::Title,
        Self::SetEditorText,
        Self::EditorText,
        Self::Working,
        Self::HiddenThinking,
        Self::Theme,
        Self::ToolsExpanded,
        Self::Overlay,
    ];
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionCapabilityManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub capabilities: BTreeSet<ExtensionCapability>,
    #[serde(default)]
    pub ui_capabilities: BTreeSet<ExtensionUiCapability>,
}

impl ExtensionCapabilityManifest {
    fn validate(&self, expected_id: &str) -> Result<()> {
        validate_identifier(&self.id, "manifest id")?;
        if self.id != expected_id {
            bail!(
                "extension manifest id {:?} does not match configured id {:?}",
                self.id,
                expected_id
            );
        }
        validate_text(&self.name, "manifest name", 256)?;
        validate_text(&self.version, "manifest version", 128)?;
        if !self.ui_capabilities.is_empty()
            && !self.capabilities.contains(&ExtensionCapability::Ui)
        {
            bail!("extension declares UI operations without the ui capability");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionPermissionSet {
    pub capabilities: BTreeSet<ExtensionCapability>,
    pub ui_capabilities: BTreeSet<ExtensionUiCapability>,
}

impl ExtensionPermissionSet {
    #[must_use]
    pub fn deny_all() -> Self {
        Self {
            capabilities: BTreeSet::new(),
            ui_capabilities: BTreeSet::new(),
        }
    }

    #[must_use]
    pub fn allow_all() -> Self {
        Self {
            capabilities: ExtensionCapability::ALL.into_iter().collect(),
            ui_capabilities: ExtensionUiCapability::ALL.into_iter().collect(),
        }
    }

    #[must_use]
    pub fn allows(&self, capability: ExtensionCapability) -> bool {
        self.capabilities.contains(&capability)
    }

    #[must_use]
    pub fn allows_ui(&self, capability: ExtensionUiCapability) -> bool {
        self.allows(ExtensionCapability::Ui) && self.ui_capabilities.contains(&capability)
    }

    fn validate_manifest(&self, manifest: &ExtensionCapabilityManifest) -> Result<()> {
        if let Some(capability) = manifest
            .capabilities
            .iter()
            .find(|capability| !self.capabilities.contains(capability))
        {
            bail!("extension requested ungranted capability {capability:?}");
        }
        if let Some(capability) = manifest
            .ui_capabilities
            .iter()
            .find(|capability| !self.ui_capabilities.contains(capability))
        {
            bail!("extension requested ungranted UI capability {capability:?}");
        }
        Ok(())
    }
}

pub const PROCESS_EXTENSION_MANIFEST_VERSION: u32 = 1;
pub const PROCESS_EXTENSION_MANIFEST_FILE: &str = "pi-extension.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ManifestRuntimeTag {
    Process,
    QuickJs,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtensionManifestRuntime {
    Process {
        executable: PathBuf,
        arguments: Vec<String>,
    },
    QuickJs {
        entry: PathBuf,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessExtensionManifest {
    pub schema_version: u32,
    pub id: String,
    /// Package version consumed by the plugin marketplace (`rpi plugin`).
    /// Optional so pre-marketplace manifests keep loading unchanged.
    pub version: Option<String>,
    pub runtime: ExtensionManifestRuntime,
    pub capabilities: BTreeSet<ExtensionCapability>,
    pub ui_capabilities: BTreeSet<ExtensionUiCapability>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProcessExtensionManifestWire {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    runtime: Option<ManifestRuntimeTag>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    executable: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entry: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    arguments: Vec<String>,
    #[serde(default)]
    capabilities: BTreeSet<ExtensionCapability>,
    #[serde(default)]
    ui_capabilities: BTreeSet<ExtensionUiCapability>,
}

impl<'de> Deserialize<'de> for ProcessExtensionManifest {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ProcessExtensionManifestWire::deserialize(deserializer)?;
        let runtime = match wire.runtime.unwrap_or(ManifestRuntimeTag::Process) {
            ManifestRuntimeTag::Process => {
                if wire.entry.is_some() {
                    return Err(serde::de::Error::custom(
                        "process extension manifest must not set entry",
                    ));
                }
                let executable = wire.executable.ok_or_else(|| {
                    serde::de::Error::custom(
                        "process extension manifest requires executable",
                    )
                })?;
                ExtensionManifestRuntime::Process {
                    executable,
                    arguments: wire.arguments,
                }
            }
            ManifestRuntimeTag::QuickJs => {
                if wire.executable.is_some() {
                    return Err(serde::de::Error::custom(
                        "QuickJS extension manifest must not set executable",
                    ));
                }
                if !wire.arguments.is_empty() {
                    return Err(serde::de::Error::custom(
                        "QuickJS extension manifest does not support arguments",
                    ));
                }
                let entry = wire.entry.ok_or_else(|| {
                    serde::de::Error::custom("QuickJS extension manifest requires entry")
                })?;
                ExtensionManifestRuntime::QuickJs { entry }
            }
        };
        Ok(Self {
            schema_version: wire.schema_version,
            id: wire.id,
            version: wire.version,
            runtime,
            capabilities: wire.capabilities,
            ui_capabilities: wire.ui_capabilities,
        })
    }
}

impl Serialize for ProcessExtensionManifest {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let (runtime, executable, entry, arguments) = match &self.runtime {
            ExtensionManifestRuntime::Process {
                executable,
                arguments,
            } => (
                ManifestRuntimeTag::Process,
                Some(executable.clone()),
                None,
                arguments.clone(),
            ),
            ExtensionManifestRuntime::QuickJs { entry } => (
                ManifestRuntimeTag::QuickJs,
                None,
                Some(entry.clone()),
                Vec::new(),
            ),
        };
        ProcessExtensionManifestWire {
            schema_version: self.schema_version,
            id: self.id.clone(),
            version: self.version.clone(),
            runtime: Some(runtime),
            executable,
            entry,
            arguments,
            capabilities: self.capabilities.clone(),
            ui_capabilities: self.ui_capabilities.clone(),
        }
        .serialize(serializer)
    }
}

impl ProcessExtensionManifest {
    /// Validate the manifest schema (version, id, version text, runtime path
    /// shape, capability coherence). Public so marketplace tooling
    /// (`rpi plugin`) can validate a staged package without resolving the
    /// runtime path.
    pub fn validate(&self, manifest_path: &Path) -> Result<()> {
        if self.schema_version != PROCESS_EXTENSION_MANIFEST_VERSION {
            bail!(
                "unsupported process extension manifest version {} in {}; expected {}",
                self.schema_version,
                manifest_path.display(),
                PROCESS_EXTENSION_MANIFEST_VERSION
            );
        }
        validate_identifier(&self.id, "process extension manifest id")?;
        if let Some(version) = &self.version {
            validate_text(version, "process extension manifest version", 128)?;
        }
        let configured_path = match &self.runtime {
            ExtensionManifestRuntime::Process { executable, .. } => executable,
            ExtensionManifestRuntime::QuickJs { entry } => entry,
        };
        if configured_path.as_os_str().is_empty() {
            bail!(
                "process extension manifest runtime path is empty: {}",
                manifest_path.display()
            );
        }
        if !self.ui_capabilities.is_empty()
            && !self.capabilities.contains(&ExtensionCapability::Ui)
        {
            bail!(
                "process extension manifest declares UI operations without the ui capability: {}",
                manifest_path.display()
            );
        }
        if matches!(self.runtime, ExtensionManifestRuntime::QuickJs { .. }) {
            if self
                .capabilities
                .contains(&ExtensionCapability::MessageRenderers)
            {
                bail!("QuickJS extensions do not support the message_renderers capability");
            }
            if self
                .capabilities
                .contains(&ExtensionCapability::ProviderMetadata)
            {
                bail!("QuickJS extensions do not support the provider_metadata capability");
            }
        }
        Ok(())
    }
}

pub fn extension_spec_from_package_resource(
    resource: &crate::PackageResourceSpec,
) -> Result<ExtensionSpec> {
    use crate::{PackageResourceKind, PackageScope};

    if resource.kind != PackageResourceKind::Extension {
        bail!(
            "package resource {} is not an extension",
            resource.path.display()
        );
    }
    if resource.scope == PackageScope::Project && !resource.trusted {
        bail!(
            "refusing untrusted project extension manifest {}",
            resource.path.display()
        );
    }
    let metadata = std::fs::metadata(&resource.path).with_context(|| {
        format!("reading package extension resource {}", resource.path.display())
    })?;
    let manifest_path = if metadata.is_dir() {
        resource.path.join(PROCESS_EXTENSION_MANIFEST_FILE)
    } else {
        resource.path.clone()
    };
    if manifest_path.file_name().and_then(std::ffi::OsStr::to_str)
        != Some(PROCESS_EXTENSION_MANIFEST_FILE)
    {
        bail!(
            "package extension resource must be an explicit {} manifest: {}",
            PROCESS_EXTENSION_MANIFEST_FILE,
            manifest_path.display()
        );
    }
    let bytes = std::fs::read(&manifest_path)
        .with_context(|| format!("reading process extension manifest {}", manifest_path.display()))?;
    let manifest: ProcessExtensionManifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing process extension manifest {}", manifest_path.display()))?;
    manifest.validate(&manifest_path)?;
    let root = manifest_path
        .parent()
        .ok_or_else(|| anyhow!("extension manifest has no parent: {}", manifest_path.display()))?;
    let (launch, arguments) = match manifest.runtime {
        ExtensionManifestRuntime::Process {
            executable,
            arguments,
        } => (
            ExtensionSpecRuntime::Process {
                executable: resolve_manifest_path(root, &executable, "process extension executable")?,
            },
            arguments,
        ),
        ExtensionManifestRuntime::QuickJs { entry } => {
            validate_quickjs_entry_extension(&entry)?;
            (
                ExtensionSpecRuntime::QuickJs {
                    entry: resolve_manifest_path(root, &entry, "QuickJS extension entry")?,
                },
                Vec::new(),
            )
        }
    };
    let origin = match resource.scope {
        PackageScope::Global => ExtensionOrigin::User,
        PackageScope::Project => ExtensionOrigin::Project,
    };
    let permissions = ExtensionPermissionSet {
        capabilities: manifest.capabilities,
        ui_capabilities: manifest.ui_capabilities,
    };
    let mut spec = ExtensionSpec::new_runtime(
        manifest.id,
        launch,
        root.to_path_buf(),
        origin,
        resource.trusted,
        permissions,
    );
    spec.arguments = arguments;
    spec.environment
        .insert("PI_EXTENSION_PACKAGE_ID".to_owned(), resource.package_id.clone());
    Ok(spec)
}
fn validate_quickjs_entry_extension(entry: &Path) -> Result<()> {
    let supported = entry
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| matches!(extension, "js" | "mjs"));
    if !supported {
        bail!(
            "QuickJS extension entry must end in .js or .mjs; TypeScript is not supported by the in-process QuickJS runtime"
        );
    }
    Ok(())
}

fn resolve_manifest_path(root: &Path, relative: &Path, kind: &str) -> Result<PathBuf> {
    if relative.is_absolute() {
        bail!("{kind} must be relative to its manifest");
    }
    if relative.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    }) {
        bail!("{kind} must remain inside its manifest directory");
    }
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("resolving extension manifest directory {}", root.display()))?;
    let joined = root.join(relative);
    let resolved = joined
        .canonicalize()
        .with_context(|| format!("resolving {kind} {}", joined.display()))?;
    if !resolved.starts_with(&canonical_root) {
        bail!("{kind} escapes its manifest directory");
    }
    let metadata = resolved
        .metadata()
        .with_context(|| format!("reading {kind} {}", resolved.display()))?;
    if !metadata.is_file() {
        bail!("{kind} is not a file");
    }
    Ok(resolved)
}

pub fn extension_specs_from_package_resources(
    resources: &[crate::PackageResourceSpec],
) -> Result<Vec<ExtensionSpec>> {
    resources
        .iter()
        .filter(|resource| resource.kind == crate::PackageResourceKind::Extension)
        .map(extension_spec_from_package_resource)
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtensionOrigin {
    Bundled,
    User,
    Project,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtensionSpecRuntime {
    Process { executable: PathBuf },
    QuickJs { entry: PathBuf },
}

impl ExtensionSpecRuntime {
    fn path(&self) -> &Path {
        match self {
            Self::Process { executable } => executable,
            Self::QuickJs { entry } => entry,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ExtensionSpec {
    pub id: String,
    pub runtime: ExtensionSpecRuntime,
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub working_directory: PathBuf,
    pub environment: BTreeMap<String, String>,
    pub origin: ExtensionOrigin,
    pub project_trusted: bool,
    pub permissions: ExtensionPermissionSet,
}

impl ExtensionSpec {
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        executable: impl Into<PathBuf>,
        working_directory: impl Into<PathBuf>,
        origin: ExtensionOrigin,
        project_trusted: bool,
        permissions: ExtensionPermissionSet,
    ) -> Self {
        Self::new_runtime(
            id,
            ExtensionSpecRuntime::Process {
                executable: executable.into(),
            },
            working_directory,
            origin,
            project_trusted,
            permissions,
        )
    }

    #[must_use]
    pub fn new_runtime(
        id: impl Into<String>,
        runtime: ExtensionSpecRuntime,
        working_directory: impl Into<PathBuf>,
        origin: ExtensionOrigin,
        project_trusted: bool,
        permissions: ExtensionPermissionSet,
    ) -> Self {
        let executable = runtime.path().to_path_buf();
        Self {
            id: id.into(),
            runtime,
            executable,
            arguments: Vec::new(),
            working_directory: working_directory.into(),
            environment: BTreeMap::new(),
            origin,
            project_trusted,
            permissions,
        }
    }

    pub(crate) fn validate_before_launch(&self) -> Result<()> {
        validate_identifier(&self.id, "extension id")?;
        if self.origin == ExtensionOrigin::Project && !self.project_trusted {
            bail!("refusing to execute untrusted project extension {}", self.id);
        }
        if self.working_directory.as_os_str().is_empty() {
            bail!("extension {} has an empty working directory", self.id);
        }
        if matches!(self.runtime, ExtensionSpecRuntime::QuickJs { .. }) {
            if self
                .permissions
                .capabilities
                .contains(&ExtensionCapability::MessageRenderers)
            {
                bail!("QuickJS extensions do not support the message_renderers capability");
            }
            if self
                .permissions
                .capabilities
                .contains(&ExtensionCapability::ProviderMetadata)
            {
                bail!("QuickJS extensions do not support the provider_metadata capability");
            }
        }
        const RESERVED_ENV: [&str; 6] = [
            "PI_EXTENSION_PROTOCOL_VERSION",
            "PI_EXTENSION_ID",
            "PI_EXTENSION_ENTRY",
            "PI_EXTENSION_CAPABILITIES",
            "PI_EXTENSION_UI_CAPABILITIES",
            "PI_EXTENSION_MAX_FRAME_BYTES",
        ];
        if let Some(name) = self
            .environment
            .keys()
            .find(|name| RESERVED_ENV.contains(&name.as_str()))
        {
            bail!("extension {} cannot override reserved environment variable {name}", self.id);
        }
        if self.environment.iter().any(|(name, value)| {
            name.is_empty()
                || name.contains(['=', '\0'])
                || value.contains('\0')
        }) {
            bail!("extension {} has an invalid environment entry", self.id);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtensionMode {
    Tui,
    Rpc,
    Json,
    Print,
}

impl ExtensionMode {
    #[must_use]
    pub const fn has_ui(self) -> bool {
        matches!(self, Self::Tui | Self::Rpc)
    }
}

#[derive(Clone, Debug)]
pub struct ExtensionRuntimeOptions {
    pub mode: ExtensionMode,
    pub handshake_timeout: Duration,
    pub load_timeout: Duration,
    pub initialize_timeout: Duration,
    pub invocation_timeout: Duration,
    pub hook_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub max_frame_bytes: usize,
    pub max_parallel_loads: usize,
}

impl Default for ExtensionRuntimeOptions {
    fn default() -> Self {
        Self {
            mode: ExtensionMode::Tui,
            handshake_timeout: Duration::from_secs(5),
            load_timeout: Duration::from_secs(10),
            initialize_timeout: Duration::from_secs(15),
            invocation_timeout: Duration::from_secs(60),
            hook_timeout: Duration::from_secs(10),
            shutdown_timeout: Duration::from_secs(2),
            max_frame_bytes: DEFAULT_MAX_EXTENSION_FRAME_BYTES,
            max_parallel_loads: 4,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionInstanceId {
    pub extension_id: String,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionCommandDescriptor {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionCommandSource {
    pub command: ExtensionCommandDescriptor,
    pub path: PathBuf,
    pub source: String,
    pub scope: String,
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_dir: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionToolDescriptor {
    pub name: String,
    pub label: String,
    pub description: String,
    pub parameters: Schema,
    #[serde(default)]
    pub capability: ToolCapability,
    #[serde(default)]
    pub execution_mode: ToolExecutionMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompt_guidelines: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionEventHookDescriptor {
    pub event: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionMessageRendererDescriptor {
    pub message_type: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionProviderMetadata {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(default)]
    pub models: Vec<Model>,
    #[serde(default)]
    pub metadata: Value,
}

/// A runtime provider registered through the load-phase `registerProvider`
/// extension API. The `stream` callable itself stays JS-side (QuickJS) or
/// process-side; the host only records the identity used for resolution and
/// invocation routing: models whose `api` equals [`Self::api`] (which defaults
/// to the provider `id`) are routed to the extension's stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionProviderDescriptor {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub api: String,
    /// Optional provider feature flags (validated identifiers; informational —
    /// the host does not interpret them yet).
    #[serde(default)]
    pub capabilities: Vec<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtensionFlagType {
    Boolean,
    String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionShortcutDescriptor {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionFlagDescriptor {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub r#type: ExtensionFlagType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
}

/// An overlay registered through the load-phase `registerOverlay` extension
/// API. The `render` callable itself stays JS-side (QuickJS) or process-side;
/// the host records the identity used for `/overlay <id>` resolution and
/// render invocation routing, plus the static editor declaration for
/// interactive overlays.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionOverlayDescriptor {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<OverlayInputDeclaration>,
}

/// Hard cap on the number of content rows one overlay may render (render
/// function output or `ctx.overlay.setRows` payloads). Excess rows are
/// dropped deterministically.
pub const OVERLAY_MAX_ROWS: usize = 100;
/// Hard cap on the length (in characters) of one overlay content row, applied
/// after secret redaction. Longer rows are truncated deterministically.
pub const OVERLAY_MAX_ROW_CHARS: usize = 200;
/// Style names accepted by overlay content rows. Anything else is dropped
/// (the row still renders with the default text style). Kept in sync with the
/// TUI renderer's style mapping.
pub const OVERLAY_STYLES: [&str; 8] = [
    "default", "bold", "dim", "italic", "accent", "error", "success", "warning",
];

/// One content row of an extension overlay: either plain text or a simple
/// `{ text, style? }` object. The host draws the border and routes keys; the
/// extension supplies content only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OverlayRow {
    Plain(String),
    Styled {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        style: Option<String>,
    },
}

impl OverlayRow {
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            Self::Plain(text) | Self::Styled { text, .. } => text,
        }
    }
}

/// Enforce the overlay content contract on `rows`: at most [`OVERLAY_MAX_ROWS`]
/// rows, each row's text redacted (via [`crate::redact::redact_secrets`]) and
/// truncated to [`OVERLAY_MAX_ROW_CHARS`] characters, and only known styles
/// retained. Applied at every boundary where extension-supplied rows become
/// displayable: the render-invocation path and the `ctx.overlay.setRows` UI
/// request path.
#[must_use]
pub fn sanitize_overlay_rows(rows: Vec<OverlayRow>) -> Vec<OverlayRow> {
    rows.into_iter()
        .take(OVERLAY_MAX_ROWS)
        .map(|row| match row {
            OverlayRow::Plain(text) => OverlayRow::Plain(bound_overlay_text(&text)),
            OverlayRow::Styled { text, style } => OverlayRow::Styled {
                text: bound_overlay_text(&text),
                style: style.filter(|style| OVERLAY_STYLES.contains(&style.as_str())),
            },
        })
        .collect()
}

fn bound_overlay_text(text: &str) -> String {
    let redacted = crate::redact::redact_secrets(text);
    redacted.chars().take(OVERLAY_MAX_ROW_CHARS).collect()
}

/// Static editor declaration of an interactive extension overlay, given at
/// registration time (`pi.registerOverlay({ ..., input: { placeholder,
/// multiline } })`). The host owns the editor and cursor (like the composer
/// editor); the extension only declares the placeholder and whether the
/// editor accepts multiple lines. Initial draft text is NOT declared here —
/// it is the render result's open-time `input.value`, seeded once when the
/// overlay opens and never re-applied while the overlay is open, so a
/// streaming `setRows` update can never overwrite text the user is typing.
/// `onSubmit(text)` / `onKey(action)` are the registration-time callbacks
/// the host invokes for editor submits and for keys the editor does not
/// consume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OverlayInputDeclaration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(default)]
    pub multiline: bool,
}

/// Open-time initial draft of an interactive overlay, returned by the
/// overlay's `render(ctx)` function as part of [`OverlayRenderOutput`]. The
/// host seeds the editor ONCE when the overlay opens; subsequent `setRows`
/// updates replace rows only and never touch the draft or cursor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OverlayRenderInput {
    #[serde(default)]
    pub value: String,
}

/// The full result of one overlay `render(ctx)` invocation: content rows plus
/// an optional open-time initial draft. A bare rows array (the original
/// contract) is accepted as `{ rows, input: None }`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OverlayRenderOutput {
    #[serde(default)]
    pub rows: Vec<OverlayRow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<OverlayRenderInput>,
}

/// Limited action ids delivered to an overlay's `onKey(action)` callback for
/// keys the host-owned editor does not consume. Deliberately NOT raw terminal
/// key events: the sandbox boundary stays declarative, so extensions can
/// react (re-render rows, toggle tool mode, abort streaming) without touching
/// the terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayKeyAction {
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    Home,
    End,
    Submit,
    Abort,
    ToggleMode,
}

/// An interactive overlay event dispatched back to the extension:
/// `Submit { text }` runs the registration-time `onSubmit(text, ctx)`,
/// `Key { action }` runs `onKey(action, ctx)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum OverlayEvent {
    Submit {
        text: String,
    },
    Key {
        action: OverlayKeyAction,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ExtensionRegistration {
    Command {
        command: ExtensionCommandDescriptor,
    },
    Tool {
        tool: ExtensionToolDescriptor,
    },
    EventHook {
        hook: ExtensionEventHookDescriptor,
    },
    MessageRenderer {
        renderer: ExtensionMessageRendererDescriptor,
    },
    ProviderMetadata {
        provider: ExtensionProviderMetadata,
    },
    /// Runtime provider registration (`pi.registerProvider`): models whose
    /// `api` matches the provider's api route to the extension's JS stream.
    Provider {
        provider: ExtensionProviderDescriptor,
    },
    Shortcut {
        shortcut: ExtensionShortcutDescriptor,
    },
    Flag {
        flag: ExtensionFlagDescriptor,
    },
    /// Overlay registration (`pi.registerOverlay`): the extension supplies a
    /// render function (kept JS-side) that the host invokes to obtain the
    /// content rows for the bordered overlay panel.
    Overlay {
        overlay: ExtensionOverlayDescriptor,
    },
}

impl ExtensionRegistration {
    const fn capability(&self) -> ExtensionCapability {
        match self {
            Self::Command { .. } => ExtensionCapability::Commands,
            Self::Tool { .. } => ExtensionCapability::Tools,
            Self::EventHook { .. } => ExtensionCapability::EventHooks,
            Self::MessageRenderer { .. } => ExtensionCapability::MessageRenderers,
            Self::ProviderMetadata { .. } => ExtensionCapability::ProviderMetadata,
            Self::Provider { .. } => ExtensionCapability::Provider,
            Self::Shortcut { .. } | Self::Flag { .. } => ExtensionCapability::Commands,
            Self::Overlay { .. } => ExtensionCapability::Overlays,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionEvent {
    pub name: String,
    #[serde(default)]
    pub data: Value,
}

impl ExtensionEvent {
    #[must_use]
    pub fn new(name: impl Into<String>, data: Value) -> Self {
        Self {
            name: name.into(),
            data,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MessageRenderRequest {
    pub message_type: String,
    pub message: Value,
    #[serde(default)]
    pub expanded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RenderedSpan {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RenderedMessage {
    #[serde(default)]
    pub lines: Vec<Vec<RenderedSpan>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ExtensionInvocation {
    Command {
        name: String,
        arguments: String,
    },
    Tool {
        name: String,
        call_id: String,
        arguments: Value,
    },
    Event {
        event: ExtensionEvent,
    },
    /// A provider stream call (`pi.registerProvider`): runs the registered JS
    /// stream function with `(session_id, messages, options)` and forwards the
    /// yielded events to the host as `ExtensionFrame::Update` frames.
    Provider {
        provider_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default)]
        messages: Vec<Message>,
        #[serde(default)]
        options: Value,
    },
    RenderMessage {
        request: MessageRenderRequest,
    },
    Shortcut {
        key: String,
    },
    /// Render one registered overlay: the extension's JS `render(ctx)`
    /// function runs and returns the content rows (and an optional
    /// declarative input section); the host sanitizes the rows (bounds +
    /// redaction) before they become displayable.
    OverlayRender {
        id: String,
    },
    /// Deliver an interactive overlay event back to the extension: runs the
    /// registration-time `onSubmit(text, ctx)` / `onKey(action, ctx)`
    /// callback. The host owns the editor, so only sanitized text and the
    /// limited action ids cross the boundary.
    OverlayEvent {
        id: String,
        event: OverlayEvent,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UiSelectOption {
    pub value: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UiNotificationLevel {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiWidgetPlacement {
    AboveEditor,
    BelowEditor,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkingIndicatorOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionThemeDescriptor {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ExtensionUiRequest {
    Select {
        title: String,
        options: Vec<UiSelectOption>,
    },
    Confirm {
        title: String,
        message: String,
    },
    Input {
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
    },
    Editor {
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefill: Option<String>,
    },
    Notify {
        message: String,
        level: UiNotificationLevel,
    },
    Status {
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    Widget {
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lines: Option<Vec<String>>,
        placement: UiWidgetPlacement,
    },
    Title {
        title: String,
    },
    SetEditorText {
        text: String,
    },
    GetEditorText,
    PasteToEditor {
        text: String,
    },
    SetWorkingMessage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    SetWorkingVisible {
        visible: bool,
    },
    SetWorkingIndicator {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        options: Option<WorkingIndicatorOptions>,
    },
    SetHiddenThinkingLabel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    GetAllThemes,
    GetTheme {
        name: String,
    },
    SetTheme {
        name: String,
    },
    GetToolsExpanded,
    SetToolsExpanded {
        expanded: bool,
    },
    /// `ctx.overlay.setRows(id, rows)`: push dynamic content rows into the
    /// overlay with `id`. The rows are sanitized (bounds + redaction) before
    /// they become displayable; the overlay does not need to be open.
    OverlaySetRows {
        id: String,
        rows: Vec<OverlayRow>,
    },
    /// `ctx.overlay.open(id, { nonCapturing? })`: open the overlay with `id`
    /// (auto-open from an event handler). The host renders the overlay's
    /// current rows inside its bordered panel; an unknown id fails
    /// actionably. `title` and `input` are filled in by the runtime from the
    /// registered descriptor before the request reaches the UI host.
    /// `nonCapturing` opens the overlay unfocused (drawn but not capturing
    /// keys); the focus-toggle action (default Alt+/) then flips focus
    /// between the overlay and the composer.
    OverlayOpen {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default)]
        non_capturing: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<OverlayInputDeclaration>,
    },
}

impl ExtensionUiRequest {
    #[must_use]
    pub const fn capability(&self) -> ExtensionUiCapability {
        match self {
            Self::Select { .. } => ExtensionUiCapability::Select,
            Self::Confirm { .. } => ExtensionUiCapability::Confirm,
            Self::Input { .. } => ExtensionUiCapability::Input,
            Self::Editor { .. } => ExtensionUiCapability::Editor,
            Self::Notify { .. } => ExtensionUiCapability::Notify,
            Self::Status { .. } => ExtensionUiCapability::Status,
            Self::Widget { .. } => ExtensionUiCapability::Widget,
            Self::Title { .. } => ExtensionUiCapability::Title,
            Self::SetEditorText { .. } => ExtensionUiCapability::SetEditorText,
            Self::GetEditorText { .. } | Self::PasteToEditor { .. } => {
                ExtensionUiCapability::EditorText
            }
            Self::SetWorkingMessage { .. }
            | Self::SetWorkingVisible { .. }
            | Self::SetWorkingIndicator { .. } => ExtensionUiCapability::Working,
            Self::SetHiddenThinkingLabel { .. } => ExtensionUiCapability::HiddenThinking,
            Self::GetAllThemes { .. } | Self::GetTheme { .. } | Self::SetTheme { .. } => {
                ExtensionUiCapability::Theme
            }
            Self::GetToolsExpanded { .. } | Self::SetToolsExpanded { .. } => {
                ExtensionUiCapability::ToolsExpanded
            }
            Self::OverlaySetRows { .. } | Self::OverlayOpen { .. } => {
                ExtensionUiCapability::Overlay
            }
        }
    }

    #[must_use]
    pub const fn is_interactive(&self) -> bool {
        matches!(
            self,
            Self::Select { .. } | Self::Confirm { .. } | Self::Input { .. } | Self::Editor { .. }
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ExtensionUiResponse {
    Selected {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
    },
    Confirmed {
        confirmed: bool,
    },
    Input {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
    },
    Edited {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
    },
    Acknowledged,
    Cancelled,
    EditorText {
        value: String,
    },
    Themes {
        themes: Vec<ExtensionThemeDescriptor>,
    },
    Theme {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        theme: Option<ExtensionThemeDescriptor>,
    },
    ThemeSet {
        success: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    ToolsExpanded {
        expanded: bool,
    },
    /// `ctx.overlay.open(id)` succeeded: the host opened the overlay panel.
    OverlayOpened,
}

impl ExtensionUiResponse {
    pub fn validate_for(&self, request: &ExtensionUiRequest) -> Result<()> {
        let valid = matches!(
            (request, self),
            (
                ExtensionUiRequest::Select { .. },
                Self::Selected { .. } | Self::Cancelled
            ) | (
                ExtensionUiRequest::Confirm { .. },
                Self::Confirmed { .. } | Self::Cancelled
            ) | (
                ExtensionUiRequest::Input { .. },
                Self::Input { .. } | Self::Cancelled
            ) | (
                ExtensionUiRequest::Editor { .. },
                Self::Edited { .. } | Self::Cancelled
            ) | (
                ExtensionUiRequest::GetEditorText { .. },
                Self::EditorText { .. }
            ) | (
                ExtensionUiRequest::GetAllThemes { .. },
                Self::Themes { .. }
            ) | (
                ExtensionUiRequest::GetTheme { .. },
                Self::Theme { .. }
            ) | (
                ExtensionUiRequest::GetToolsExpanded { .. },
                Self::ToolsExpanded { .. }
            ) | (
                ExtensionUiRequest::PasteToEditor { .. }
                    | ExtensionUiRequest::SetWorkingMessage { .. }
                    | ExtensionUiRequest::SetWorkingVisible { .. }
                    | ExtensionUiRequest::SetWorkingIndicator { .. }
                    | ExtensionUiRequest::SetHiddenThinkingLabel { .. }
                    | ExtensionUiRequest::SetToolsExpanded { .. },
                Self::Acknowledged
            ) | (
                ExtensionUiRequest::SetTheme { .. },
                Self::ThemeSet { .. }
            ) | (
                ExtensionUiRequest::OverlaySetRows { .. },
                Self::Acknowledged
            ) | (
                ExtensionUiRequest::OverlayOpen { .. },
                Self::OverlayOpened
            ) | (
                ExtensionUiRequest::Notify { .. }
                    | ExtensionUiRequest::Status { .. }
                    | ExtensionUiRequest::Widget { .. }
                    | ExtensionUiRequest::Title { .. }
                    | ExtensionUiRequest::SetEditorText { .. },
                Self::Acknowledged
            )
        );
        if valid {
            Ok(())
        } else {
            bail!("UI response type does not match request type")
        }
    }
}


#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeUiRequest {
    pub request: ExtensionUiRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExtensionMessageDelivery {
    Steer,
    FollowUp,
    NextTurn,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionCustomMessage {
    pub custom_type: String,
    pub content: CustomMessageContent,
    pub display: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionContextUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<i64>,
    pub context_window: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionContextSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    pub is_idle: bool,
    pub project_trusted: bool,
    pub has_pending_messages: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<ExtensionContextUsage>,
    #[serde(default)]
    pub active_tools: Vec<String>,
    #[serde(default)]
    pub all_tools: Vec<String>,
    #[serde(default)]
    pub commands: Vec<ExtensionCommandDescriptor>,
    #[serde(default)]
    pub flag_values: BTreeMap<String, Value>,
    pub system_prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Model>,
    pub thinking_level: ThinkingLevel,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ExtensionRuntimeAction {
    SendMessage {
        message: ExtensionCustomMessage,
        delivery: ExtensionMessageDelivery,
        #[serde(default)]
        trigger_turn: bool,
    },
    SendUserMessage {
        content: CustomMessageContent,
        delivery: ExtensionMessageDelivery,
    },
    AppendEntry {
        custom_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
    SetSessionName { name: String },
    SetLabel {
        entry_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    GetActiveTools,
    GetAllTools,
    SetActiveTools { tool_names: Vec<String> },
    GetCommands,
    SetModel { model: Model },
    SetThinkingLevel { level: ThinkingLevel },
    Abort,
    Shutdown,
    Compact {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        custom_instructions: Option<String>,
    },
    Reload,
    WaitForIdle,
    GetFlag {
        name: String,
    },
}

pub trait ExtensionActionHost: Send + Sync {
    fn context_snapshot(&self) -> ExtensionFuture<'_, Result<ExtensionContextSnapshot>>;

    fn request(
        &self,
        instance: ExtensionInstanceId,
        action: ExtensionRuntimeAction,
        cancellation: ExtensionCancellation,
    ) -> ExtensionFuture<'_, Result<Value>>;
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ExtensionRuntimeRequest {
    Ui { ui: RuntimeUiRequest },
    Action { action: ExtensionRuntimeAction },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ExtensionHostRequest {
    Load,
    Initialize,
    Invoke {
        invocation: ExtensionInvocation,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<ExtensionContextSnapshot>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProtocolError {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ProtocolResult {
    Success {
        #[serde(default)]
        value: Value,
    },
    Failure {
        error: ProtocolError,
    },
}

impl ProtocolResult {
    pub(crate) fn success(value: impl Into<Value>) -> Self {
        Self::Success {
            value: value.into(),
        }
    }

    pub(crate) fn failure(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Failure {
            error: ProtocolError {
                code: code.into(),
                message: message.into(),
            },
        }
    }

    fn into_result(self) -> Result<Value> {
        match self {
            Self::Success { value } => Ok(value),
            Self::Failure { error } => Err(anyhow!("{}: {}", error.code, error.message)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ExtensionHostFrame {
    Hello {
        protocol_version: u32,
        instance: ExtensionInstanceId,
        cwd: String,
        mode: ExtensionMode,
        project_trusted: bool,
    },
    Request {
        id: String,
        generation: u64,
        request: ExtensionHostRequest,
    },
    Response {
        id: String,
        result: ProtocolResult,
    },
    Cancel {
        id: String,
    },
    Shutdown {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ExtensionFrame {
    Hello {
        protocol_version: u32,
        manifest: ExtensionCapabilityManifest,
    },
    Register {
        registration: ExtensionRegistration,
    },
    /// Load-phase unregistration (`pi.unregisterProvider`): removes a provider
    /// registered earlier in the same load phase. Any other phase rejects it.
    UnregisterProvider {
        id: String,
    },
    Response {
        id: String,
        result: ProtocolResult,
    },
    Update {
        id: String,
        value: Value,
    },
    Request {
        id: String,
        request: ExtensionRuntimeRequest,
    },
    Cancel {
        id: String,
    },
}

#[derive(Clone)]
pub struct ExtensionLaunch {
    pub spec: ExtensionSpec,
    pub max_frame_bytes: usize,
    /// Runtime-wide timeouts; hosts that own their own event loop (e.g. the
    /// in-process QuickJS runtime) use these to bound load and invocation work.
    pub timeouts: ExtensionRuntimeOptions,
    /// Live `settings.sandbox` resolver for process-extension spawns; `None`
    /// runs the extension child unsandboxed. QuickJS in-process extensions
    /// ignore this (they share the host process by design).
    pub sandbox: Option<crate::SandboxConfigFn>,
}

pub trait ExtensionTransport: Send + Sync {
    fn send(&self, frame: &ExtensionHostFrame) -> ExtensionFuture<'_, Result<()>>;
    fn receive(&self) -> ExtensionFuture<'_, Result<Option<ExtensionFrame>>>;
    fn terminate(&self) -> ExtensionFuture<'_, Result<()>>;
    fn diagnostic_context(&self) -> String;
}

pub trait ExtensionHost: Send + Sync {
    fn launch(
        &self,
        launch: ExtensionLaunch,
    ) -> ExtensionFuture<'_, Result<Arc<dyn ExtensionTransport>>>;
}

#[derive(Clone, Debug, Default)]
pub struct ProcessExtensionHost;

impl ExtensionHost for ProcessExtensionHost {
    fn launch(
        &self,
        launch: ExtensionLaunch,
    ) -> ExtensionFuture<'_, Result<Arc<dyn ExtensionTransport>>> {
        Box::pin(async move {
            let spec = launch.spec;
            spec.validate_before_launch()?;
            if matches!(spec.runtime, ExtensionSpecRuntime::QuickJs { .. }) {
                let quickjs = QuickJsExtensionHost;
                return quickjs
                    .launch(ExtensionLaunch {
                        spec,
                        max_frame_bytes: launch.max_frame_bytes,
                        timeouts: launch.timeouts,
                        sandbox: launch.sandbox,
                    })
                    .await;
            }
            let mut child_environment = spec.environment.clone();
            let working_directory = spec
                .working_directory
                .canonicalize()
                .with_context(|| format!("resolving working directory for extension {}", spec.id))?;
            let executable = match &spec.runtime {
                ExtensionSpecRuntime::Process { executable } => executable,
                ExtensionSpecRuntime::QuickJs { .. } => {
                    unreachable!("QuickJS runtimes dispatch to QuickJsExtensionHost before this match")
                }
            };
            let executable = executable.canonicalize().with_context(|| {
                format!("resolving executable for extension {}", spec.id)
            })?;
            let metadata = executable.metadata().with_context(|| {
                format!("reading extension executable for {}", spec.id)
            })?;
            if !metadata.is_file() {
                bail!("extension executable is not a file");
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 == 0 {
                    bail!("extension executable is not executable");
                }
            }
            // Route the spawn through the filesystem sandbox when
            // `settings.sandbox.enabled` is set: the same allowed/denied path
            // semantics as the bash tool, with the extension's own working
            // directory and the agent directory always visible. Fail-closed
            // validation surfaces an actionable error before the child starts
            // (e.g. the working directory inside a denied path).
            let mut argv = Vec::with_capacity(1 + spec.arguments.len());
            argv.push(executable.to_string_lossy().into_owned());
            argv.extend(spec.arguments.iter().cloned());
            let mut sandbox_config = launch
                .sandbox
                .as_ref()
                .and_then(|resolve| resolve())
                .filter(|config| config.enabled);
            if let Some(config) = &mut sandbox_config {
                for path in [working_directory.clone(), crate::agent_dir_path()] {
                    if !config.allowed_paths.iter().any(|allowed| allowed == &path) {
                        config.allowed_paths.push(path);
                    }
                }
            }
            child_environment.insert(
                "PI_EXTENSION_PROTOCOL_VERSION".to_owned(),
                EXTENSION_PROTOCOL_VERSION.to_string(),
            );
            child_environment.insert("PI_EXTENSION_ID".to_owned(), spec.id.clone());
            let mut child = crate::sandbox::spawn_piped(
                sandbox_config.as_ref(),
                &working_directory,
                &argv,
                child_environment,
            )
            .with_context(|| format!("starting extension {}", spec.id))?;
            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| anyhow!("extension {} child stdin was unavailable", spec.id))?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| anyhow!("extension {} child stdout was unavailable", spec.id))?;
            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| anyhow!("extension {} child stderr was unavailable", spec.id))?;
            let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_BYTES)));
            let stderr_task = drain_stderr(stderr, stderr_tail.clone());
            Ok(Arc::new(ProcessExtensionTransport {
                stdin: AsyncMutex::new(BufWriter::new(stdin)),
                stdout: AsyncMutex::new(BufReader::new(stdout)),
                child: AsyncMutex::new(child),
                stderr_tail,
                stderr_task: Mutex::new(Some(stderr_task)),
                max_frame_bytes: launch.max_frame_bytes.max(1024),
                cleanup_directory: None,
            }) as Arc<dyn ExtensionTransport>)
        })
    }
}

struct ProcessExtensionTransport {
    stdin: AsyncMutex<BufWriter<ChildStdin>>,
    stdout: AsyncMutex<BufReader<ChildStdout>>,
    child: AsyncMutex<Child>,
    stderr_tail: Arc<Mutex<VecDeque<u8>>>,
    stderr_task: Mutex<Option<JoinHandle<()>>>,
    max_frame_bytes: usize,
    cleanup_directory: Option<PathBuf>,
}

impl ExtensionTransport for ProcessExtensionTransport {
    fn send(&self, frame: &ExtensionHostFrame) -> ExtensionFuture<'_, Result<()>> {
        let encoded = serde_json::to_vec(frame).context("encoding extension protocol frame");
        Box::pin(async move {
            let mut encoded = encoded?;
            if encoded.len() > self.max_frame_bytes {
                bail!(
                    "extension protocol frame exceeds {} bytes",
                    self.max_frame_bytes
                );
            }
            encoded.push(b'\n');
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(&encoded)
                .await
                .context("writing extension protocol frame")?;
            stdin.flush().await.context("flushing extension protocol frame")
        })
    }

    fn receive(&self) -> ExtensionFuture<'_, Result<Option<ExtensionFrame>>> {
        Box::pin(async move {
            let mut stdout = self.stdout.lock().await;
            let mut frame = Vec::new();
            let limit = u64::try_from(self.max_frame_bytes.saturating_add(1)).unwrap_or(u64::MAX);
            let mut limited = (&mut *stdout).take(limit);
            let count = limited
                .read_until(b'\n', &mut frame)
                .await
                .context("reading extension protocol frame")?;
            if count == 0 {
                return Ok(None);
            }
            if frame.last() != Some(&b'\n') {
                if frame.len() > self.max_frame_bytes {
                    bail!(
                        "extension protocol frame exceeds {} bytes",
                        self.max_frame_bytes
                    );
                }
                bail!("extension protocol ended with a non-LF-terminated frame");
            }
            frame.pop();
            if frame.len() > self.max_frame_bytes {
                bail!(
                    "extension protocol frame exceeds {} bytes",
                    self.max_frame_bytes
                );
            }
            if frame.last() == Some(&b'\r') {
                bail!("extension protocol requires LF JSONL and rejects CRLF");
            }
            if frame.is_empty() {
                bail!("extension protocol does not permit blank lines");
            }
            let parsed = serde_json::from_slice(&frame).with_context(|| {
                format!(
                    "decoding extension protocol JSON: {}",
                    String::from_utf8_lossy(&frame)
                )
            })?;
            Ok(Some(parsed))
        })
    }

    fn terminate(&self) -> ExtensionFuture<'_, Result<()>> {
        Box::pin(async move {
            let stderr_task = { self.stderr_task.lock().take() };
            if let Some(task) = stderr_task {
                task.abort();
            }
            let mut child = self.child.lock().await;
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                use nix::sys::signal::{Signal, killpg};
                use nix::unistd::Pid;
                let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
            }
            if child.id().is_some() {
                let _ = child.start_kill();
                let _ = timeout(Duration::from_secs(1), child.wait()).await;
            }
            drop(child);
            if let Some(directory) = &self.cleanup_directory {
                let _ = std::fs::remove_dir_all(directory);
            }
            Ok(())
        })
    }

    fn diagnostic_context(&self) -> String {
        let bytes = self.stderr_tail.lock().iter().copied().collect::<Vec<_>>();
        String::from_utf8_lossy(&bytes).trim().to_owned()
    }
}

fn drain_stderr(
    mut stderr: tokio::process::ChildStderr,
    tail: Arc<Mutex<VecDeque<u8>>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buffer = [0_u8; 1024];
        loop {
            let count = match stderr.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(count) => count,
            };
            let mut tail = tail.lock();
            for byte in &buffer[..count] {
                if tail.len() == STDERR_TAIL_BYTES {
                    tail.pop_front();
                }
                tail.push_back(*byte);
            }
        }
    })
}

#[derive(Clone, Debug)]
pub struct ExtensionCancellation {
    inner: Arc<ExtensionCancellationInner>,
}

#[derive(Debug)]
struct ExtensionCancellationInner {
    cancelled: AtomicBool,
    notify: tokio::sync::Notify,
}

impl ExtensionCancellation {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ExtensionCancellationInner {
                cancelled: AtomicBool::new(false),
                notify: tokio::sync::Notify::new(),
            }),
        }
    }

    pub fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::AcqRel) {
            self.inner.notify.notify_waiters();
        }
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let notified = self.inner.notify.notified();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

impl Default for ExtensionCancellation {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionUiContext {
    pub instance: ExtensionInstanceId,
    pub mode: ExtensionMode,
}

pub trait ExtensionUiHost: Send + Sync {
    fn request(
        &self,
        context: ExtensionUiContext,
        request: ExtensionUiRequest,
        cancellation: ExtensionCancellation,
    ) -> ExtensionFuture<'_, Result<ExtensionUiResponse>>;

    fn clear_extension(
        &self,
        instance: ExtensionInstanceId,
    ) -> ExtensionFuture<'_, Result<()>>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExtensionRuntimeEvent {
    Loaded {
        instance: ExtensionInstanceId,
        name: String,
        version: String,
    },
    LoadFailed {
        extension_id: String,
        path: String,
        message: String,
    },
    Crashed {
        instance: ExtensionInstanceId,
        message: String,
    },
    Invalidated {
        instance: ExtensionInstanceId,
        reason: String,
    },
    InvocationFailed {
        instance: ExtensionInstanceId,
        operation: String,
        message: String,
    },
    Collision {
        capability: ExtensionCapability,
        name: String,
        winner: ExtensionInstanceId,
        loser: ExtensionInstanceId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionLoadFailure {
    pub extension_id: String,
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionLoadReport {
    pub generation: u64,
    pub loaded: Vec<ExtensionInstanceId>,
    pub failures: Vec<ExtensionLoadFailure>,
}

pub struct ExtensionReloadCandidate {
    owner: Weak<RuntimeInner>,
    _guard: OwnedMutexGuard<()>,
    base_generation: u64,
    next_generation: u64,
    staged: Vec<Arc<ExtensionInstance>>,
    registry: RuntimeRegistry,
    collisions: Vec<ExtensionRuntimeEvent>,
    failures: Vec<ExtensionLoadFailure>,
}

impl ExtensionReloadCandidate {
    #[must_use]
    pub fn report(&self) -> ExtensionLoadReport {
        ExtensionLoadReport {
            generation: self.next_generation,
            loaded: self
                .staged
                .iter()
                .map(|instance| instance.id.clone())
                .collect(),
            failures: self.failures.clone(),
        }
    }

    #[must_use]
    pub fn agent_tools(&self, runtime: &ExtensionRuntime) -> Vec<AgentTool> {
        agent_tools_from_registry(runtime, &self.registry)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionHookOutcome {
    pub instance: ExtensionInstanceId,
    pub result: std::result::Result<Value, String>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionBeforeAgentStartReduction {
    pub system_prompt: String,
    pub messages: Vec<ExtensionCustomMessage>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionToolCallReduction {
    pub input: Value,
    pub block: bool,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionUsage {
    #[serde(default)] pub input: i64,
    #[serde(default)] pub output: i64,
    #[serde(default)] pub cache_read: i64,
    #[serde(default)] pub cache_write: i64,
    #[serde(default)] pub cache_write_1h: i64,
    #[serde(default)] pub reasoning: i64,
    #[serde(default)] pub total_tokens: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionToolResultReduction {
    pub content: Vec<ContentBlock>,
    pub details: Option<Value>,
    pub is_error: bool,
    pub usage: Option<ExtensionUsage>,
}
#[derive(Clone, Debug, Default)]
pub struct ExtensionUserBashReduction {
    pub result: Option<crate::BashResult>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExtensionBashResultWire {
    output: String,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    cancelled: bool,
    #[serde(default)]
    truncated: bool,
    #[serde(default)]
    full_output_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserBashWire {
    #[serde(default)]
    operations: Option<Value>,
    #[serde(default)]
    result: Option<ExtensionBashResultWire>,
}


#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtensionSessionBeforeForkReduction {
    pub cancel: bool,
    pub skip_conversation_restore: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtensionSessionBeforeCompactReduction {
    pub cancel: bool,
    pub compaction: Option<crate::CompactionResult>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtensionSessionBeforeTreeReduction {
    pub cancel: bool,
    pub summary: Option<ExtensionTreeSummary>,
    pub custom_instructions: Option<String>,
    pub replace_instructions: Option<bool>,
    pub label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionTreeSummary {
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ExtensionUsage>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExtensionInputReduction {
    Continue { text: String, images: Vec<ContentBlock> },
    Handled,
}

/// Recommendation collected from the `trust_decision` event.
///
/// Fail-open by contract: an extension may only recommend approval
/// (`approve: true`), which the host applies via
/// [`crate::trust::apply_trust_hook_outcomes`] — it upgrades an undecided
/// (`ask`) tentative decision to trusted and is inert otherwise, so a stored
/// denial is never weakened by an extension recommendation.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionTrustDecisionReduction {
    pub approve: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BeforeAgentStartWire {
    #[serde(default)] message: Option<ExtensionCustomMessage>,
    #[serde(default)] messages: Vec<ExtensionCustomMessage>,
    #[serde(default)] system_prompt: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextWire { #[serde(default)] messages: Option<Vec<Message>> }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderHeadersWire { headers: BTreeMap<String, Option<String>> }

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ToolCallWire {
    #[serde(default)] block: Option<bool>,
    #[serde(default)] reason: Option<String>,
    #[serde(default)] input: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ToolResultWire {
    #[serde(default)] content: Option<Vec<ContentBlock>>,
    #[serde(default)] details: Option<Value>,
    #[serde(default)] is_error: Option<bool>,
    #[serde(default)] usage: Option<ExtensionUsage>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageEndWire { #[serde(default)] message: Option<Message> }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelWire { #[serde(default)] cancel: bool }

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BeforeForkWire {
    #[serde(default)] cancel: bool,
    #[serde(default)] skip_conversation_restore: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BeforeCompactWire {
    #[serde(default)] cancel: bool,
    #[serde(default)] compaction: Option<crate::CompactionResult>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BeforeTreeWire {
    #[serde(default)] cancel: bool,
    #[serde(default)] summary: Option<ExtensionTreeSummary>,
    #[serde(default)] custom_instructions: Option<String>,
    #[serde(default)] replace_instructions: Option<bool>,
    #[serde(default)] label: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", tag = "action", deny_unknown_fields)]
enum InputWire {
    Continue,
    Transform { text: String, #[serde(default)] images: Option<Vec<ContentBlock>> },
    Handled,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustDecisionWire {
    #[serde(default)] approve: bool,
}

#[derive(Clone)]
pub struct ExtensionRuntime {
    inner: Arc<RuntimeInner>,
}

struct RuntimeInner {
    host: Arc<dyn ExtensionHost>,
    ui: Option<Arc<dyn ExtensionUiHost>>,
    options: ExtensionRuntimeOptions,
    state: Mutex<RuntimeState>,
    reload_lock: Arc<AsyncMutex<()>>,
    events: broadcast::Sender<ExtensionRuntimeEvent>,
    action_host: Arc<Mutex<Option<Arc<dyn ExtensionActionHost>>>>,
    /// Stable per-runtime identity used to namespace this runtime's provider
    /// registrations in the shared pi-ai registry. Concurrent runtimes
    /// registering the same api never collide (each owns a distinct
    /// namespace), and unregistration on shutdown/reload only ever removes
    /// this runtime's entries.
    provider_namespace: String,
    /// Resolves the live `settings.sandbox` for process-extension spawns
    /// (RELOAD semantics: consulted per launch). `None` means process
    /// extensions run unsandboxed. QuickJS in-process extensions never go
    /// through this path — they share the host process by design.
    process_sandbox: Mutex<Option<crate::SandboxConfigFn>>,
}

struct RuntimeState {
    generation: u64,
    instances: Vec<Arc<ExtensionInstance>>,
    registry: RuntimeRegistry,
}

impl ExtensionRuntime {
    #[must_use]
    pub fn new(
        host: Arc<dyn ExtensionHost>,
        ui: Option<Arc<dyn ExtensionUiHost>>,
        options: ExtensionRuntimeOptions,
    ) -> Self {
        let (events, _) = broadcast::channel(RUNTIME_EVENT_BUFFER);
        Self {
            inner: Arc::new(RuntimeInner {
                host,
                ui,
                options,
                state: Mutex::new(RuntimeState {
                    generation: 0,
                    instances: Vec::new(),
                    registry: RuntimeRegistry::default(),
                }),
                reload_lock: Arc::new(AsyncMutex::new(())),
                events,
                action_host: Arc::new(Mutex::new(None)),
                provider_namespace: format!("extension-runtime:{}", Uuid::now_v7()),
                process_sandbox: Mutex::new(None),
            }),
        }
    }

    /// The namespace this runtime owns in the shared pi-ai provider registry.
    /// Unique per runtime instance: two runtimes registering the same api
    /// coexist, and sessions scoped to this runtime resolve its entries.
    #[must_use]
    pub fn provider_namespace(&self) -> &str {
        &self.inner.provider_namespace
    }

    pub fn set_action_host(&self, host: Arc<dyn ExtensionActionHost>) -> Result<()> {
        let mut current = self.inner.action_host.lock();
        if current.is_some() {
            bail!("extension action host is already configured");
        }
        *current = Some(host);
        Ok(())
    }

    /// Configures the live `settings.sandbox` resolver used for process
    /// extension spawns. Consulted per launch, so a settings reload applies to
    /// the next extension spawn. When set and resolving to an enabled config,
    /// process extensions run inside the filesystem sandbox (same
    /// allowed/denied semantics as the bash tool, plus the extension's own
    /// working directory); when absent or disabled they run unsandboxed.
    pub fn set_process_sandbox(&self, sandbox: Option<crate::SandboxConfigFn>) {
        *self.inner.process_sandbox.lock() = sandbox;
    }

    async fn context_snapshot(&self) -> Result<Option<ExtensionContextSnapshot>> {
        let Some(host) = self.inner.action_host.lock().clone() else {
            return Ok(None);
        };
        host.context_snapshot()
            .await
            .map(Some)
            .context("capturing extension context snapshot from the action host")
    }

    async fn invocation_context(&self) -> Result<Option<ExtensionContextSnapshot>> {
        let mut context = match self.context_snapshot().await? {
            Some(context) => context,
            None => return Ok(None),
        };
        context.flag_values = BTreeMap::new();
        Ok(Some(context))
    }

    fn action_host(&self) -> Option<Arc<dyn ExtensionActionHost>> {
        self.inner.action_host.lock().clone()
    }

    #[must_use]
    pub fn process(
        ui: Option<Arc<dyn ExtensionUiHost>>,
        options: ExtensionRuntimeOptions,
    ) -> Self {
        Self::new(Arc::new(ProcessExtensionHost), ui, options)
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<ExtensionRuntimeEvent> {
        self.inner.events.subscribe()
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.inner.state.lock().generation
    }

    pub async fn load(&self, specs: Vec<ExtensionSpec>) -> ExtensionLoadReport {
        self.reload(specs).await
    }

    pub async fn reload(&self, specs: Vec<ExtensionSpec>) -> ExtensionLoadReport {
        let candidate = self.stage_reload(specs).await;
        let report = candidate.report();
        if !report.failures.is_empty() {
            self.discard_reload(candidate).await;
            return report;
        }
        let _ = self.prepare_reload(&candidate).await;
        match self.commit_reload(candidate) {
            Ok(report) => {
                let reason = if report.generation == 1 { "startup" } else { "reload" };
                self.finish_reload(reason).await;
                report
            }
            Err(error) => ExtensionLoadReport {
                generation: self.generation(),
                loaded: Vec::new(),
                failures: vec![ExtensionLoadFailure {
                    extension_id: "runtime".to_owned(),
                    path: PathBuf::new(),
                    // Full chain: sandbox validation and spawn failures keep
                    // their actionable inner messages (e.g. which path is
                    // denied) instead of collapsing to the outer context.
                    message: format!("{error:#}"),
                }],
            },
        }
    }

    pub async fn stage_reload(&self, specs: Vec<ExtensionSpec>) -> ExtensionReloadCandidate {
        let guard = self.inner.reload_lock.clone().lock_owned().await;
        let (base_generation, next_generation) = {
            let state = self.inner.state.lock();
            (state.generation, state.generation.saturating_add(1))
        };
        let max_parallel_loads = self.inner.options.max_parallel_loads.max(1);
        let mut staged_by_index = vec![None; specs.len()];
        let mut failures_by_index = vec![None; specs.len()];
        let mut pending = specs.into_iter().enumerate();
        loop {
            let batch = pending
                .by_ref()
                .take(max_parallel_loads)
                .map(|(index, spec)| async move {
                    let extension_id = spec.id.clone();
                    let path = spec.executable.clone();
                    (index, extension_id, path, self.load_one(spec, next_generation).await)
                })
                .collect::<Vec<_>>();
            if batch.is_empty() {
                break;
            }
            for (index, extension_id, path, result) in futures_util::future::join_all(batch).await {
                match result {
                    Ok(instance) => staged_by_index[index] = Some(instance),
                    Err(error) => {
                        // Full chain: launch/sandbox failures surface their
                        // actionable inner message (e.g. the denied path).
                        let message = format!("{error:#}");
                        failures_by_index[index] = Some(ExtensionLoadFailure {
                            extension_id: extension_id.clone(),
                            path: path.clone(),
                            message: message.clone(),
                        });
                        let _ = self.inner.events.send(ExtensionRuntimeEvent::LoadFailed {
                            extension_id,
                            path: path.display().to_string(),
                            message,
                        });
                    }
                }
            }
        }
        let staged = staged_by_index.into_iter().flatten().collect::<Vec<_>>();
        let failures = failures_by_index.into_iter().flatten().collect::<Vec<_>>();
        let (registry, collisions) = RuntimeRegistry::from_instances(&staged);
        ExtensionReloadCandidate {
            owner: Arc::downgrade(&self.inner),
            _guard: guard,
            base_generation,
            next_generation,
            staged,
            registry,
            collisions,
            failures,
        }
    }

    pub async fn discard_reload(&self, candidate: ExtensionReloadCandidate) {
        for instance in &candidate.staged {
            instance.invalidate_now("extension reload candidate rejected");
        }
        for instance in candidate.staged {
            instance.finish_invalidate("candidate_rejected").await;
        }
    }

    pub async fn prepare_reload(
        &self,
        candidate: &ExtensionReloadCandidate,
    ) -> Result<Vec<ExtensionHookOutcome>> {
        if !Weak::ptr_eq(&candidate.owner, &Arc::downgrade(&self.inner)) {
            bail!("extension reload candidate belongs to another runtime");
        }
        let registry = {
            let state = self.inner.state.lock();
            if state.generation != candidate.base_generation {
                bail!("extension reload candidate is stale");
            }
            state.registry.clone()
        };
        if candidate.base_generation == 0 {
            return Ok(Vec::new());
        }
        Ok(self
            .emit_from_registry(
                &registry,
                ExtensionEvent::new(
                    "session_shutdown",
                    serde_json::json!({ "reason": "reload" }),
                ),
            )
            .await)
    }

    pub async fn finish_reload(&self, reason: &str) -> Vec<ExtensionHookOutcome> {
        self.emit(ExtensionEvent::new(
            "session_start",
            serde_json::json!({ "reason": reason }),
        ))
        .await
    }

    pub fn commit_reload(
        &self,
        candidate: ExtensionReloadCandidate,
    ) -> Result<ExtensionLoadReport> {
        if !Weak::ptr_eq(&candidate.owner, &Arc::downgrade(&self.inner)) {
            spawn_discarded_instances(candidate.staged);
            bail!("extension reload candidate belongs to another runtime");
        }
        if !candidate.failures.is_empty() {
            let report = candidate.report();
            spawn_discarded_instances(candidate.staged);
            return Err(extension_candidate_error(&report));
        }
        let old_instances = {
            let state = self.inner.state.lock();
            if state.generation != candidate.base_generation {
                drop(state);
                spawn_discarded_instances(candidate.staged);
                bail!("extension reload candidate is stale");
            }
            state.instances.clone()
        };
        for instance in &old_instances {
            instance.invalidate_now("extension runtime reloaded");
        }
        for collision in &candidate.collisions {
            let _ = self.inner.events.send(collision.clone());
        }
        for instance in &candidate.staged {
            instance.activate();
        }
        {
            let mut state = self.inner.state.lock();
            state.generation = candidate.next_generation;
            state.instances = candidate.staged.clone();
            state.registry = candidate.registry.clone();
        }
        // Publish the committed provider surface into the shared pi-ai
        // registry (drop entries of retired providers, (re)register the rest).
        self.sync_provider_registrations(&candidate.registry);
        for instance in &candidate.staged {
            let _ = self.inner.events.send(ExtensionRuntimeEvent::Loaded {
                instance: instance.id.clone(),
                name: instance.manifest.name.clone(),
                version: instance.manifest.version.clone(),
            });
        }
        tokio::spawn(async move {
            for instance in old_instances {
                instance.finish_invalidate("reload").await;
            }
        });
        Ok(candidate.report())
    }

    async fn load_one(
        &self,
        spec: ExtensionSpec,
        generation: u64,
    ) -> Result<Arc<ExtensionInstance>> {
        spec.validate_before_launch()?;
        let instance_id = ExtensionInstanceId {
            extension_id: spec.id.clone(),
            generation,
        };
        let sandbox = self.inner.process_sandbox.lock().clone();
        let transport = self
            .inner
            .host
            .launch(ExtensionLaunch {
                spec: spec.clone(),
                max_frame_bytes: self.inner.options.max_frame_bytes,
                timeouts: self.inner.options.clone(),
                sandbox,
            })
            .await
            .with_context(|| format!("launching extension {}", spec.id))?;

        let load_result = self
            .handshake_and_load(&spec, &instance_id, transport.clone())
            .await;
        let (manifest, registrations) = match load_result {
            Ok(loaded) => loaded,
            Err(error) => {
                let _ = transport.terminate().await;
                return Err(with_diagnostics(error, transport.as_ref()));
            }
        };

        let instance = ExtensionInstance::new(
            instance_id,
            manifest,
            registrations,
            spec,
            transport,
            self.inner.ui.clone(),
            self.inner.action_host.clone(),
            self.inner.options.clone(),
            self.inner.events.clone(),
        );
        instance.set_phase(ExtensionPhase::Initializing);
        instance.start_reader();
        let initialize = instance
            .request_value(
                ExtensionHostRequest::Initialize,
                self.inner.options.initialize_timeout,
                None,
                None,
                None,
            )
            .await;
        if let Err(error) = initialize {
            instance.invalidate_now("initialization failed");
            instance.finish_invalidate("initialization_failed").await;
            return Err(error).with_context(|| {
                format!("initializing extension {}", instance.id.extension_id)
            });
        }
        instance.set_phase(ExtensionPhase::Ready);
        Ok(instance)
    }

    async fn handshake_and_load(
        &self,
        spec: &ExtensionSpec,
        instance: &ExtensionInstanceId,
        transport: Arc<dyn ExtensionTransport>,
    ) -> Result<(ExtensionCapabilityManifest, ExtensionRegistrations)> {
        transport
            .send(&ExtensionHostFrame::Hello {
                protocol_version: EXTENSION_PROTOCOL_VERSION,
                instance: instance.clone(),
                cwd: spec.working_directory.display().to_string(),
                mode: self.inner.options.mode,
                project_trusted: spec.project_trusted,
            })
            .await
            .context("sending extension handshake")?;
        let hello = timeout(self.inner.options.handshake_timeout, transport.receive())
            .await
            .map_err(|_| anyhow!("extension handshake timed out"))??
            .ok_or_else(|| anyhow!("extension exited before its handshake"))?;
        let manifest = match hello {
            ExtensionFrame::Hello {
                protocol_version,
                manifest,
            } => {
                if protocol_version != EXTENSION_PROTOCOL_VERSION {
                    bail!(
                        "unsupported extension protocol version {protocol_version}; host requires {EXTENSION_PROTOCOL_VERSION}"
                    );
                }
                manifest
            }
            other => bail!("expected extension hello frame, received {other:?}"),
        };
        manifest.validate(&spec.id)?;
        spec.permissions.validate_manifest(&manifest)?;

        let load_id = Uuid::new_v4().to_string();
        transport
            .send(&ExtensionHostFrame::Request {
                id: load_id.clone(),
                generation: instance.generation,
                request: ExtensionHostRequest::Load,
            })
            .await
            .context("requesting extension load phase")?;

        let mut registrations = RegistrationBuilder::new(&manifest, &spec.permissions);
        let load = async {
            loop {
                let frame = transport
                    .receive()
                    .await?
                    .ok_or_else(|| anyhow!("extension exited during load phase"))?;
                match frame {
                    ExtensionFrame::Register { registration } => {
                        registrations.register(registration)?;
                    }
                    ExtensionFrame::UnregisterProvider { id } => {
                        registrations.unregister_provider(&id)?;
                    }
                    ExtensionFrame::Response { id, result } if id == load_id => {
                        result.into_result()?;
                        return registrations.finish();
                    }
                    ExtensionFrame::Request { id, .. } => {
                        let _ = transport
                            .send(&ExtensionHostFrame::Response {
                                id,
                                result: ProtocolResult::failure(
                                    "load_phase",
                                    "runtime actions are unavailable during the registration-only load phase",
                                ),
                            })
                            .await;
                        bail!("extension attempted a runtime action during its load phase");
                    }
                    other => bail!("unexpected frame during extension load phase: {other:?}"),
                }
            }
        };
        let registrations = timeout(self.inner.options.load_timeout, load)
            .await
            .map_err(|_| anyhow!("extension load phase timed out"))??;
        Ok((manifest, registrations))
    }

    #[must_use]
    pub fn commands(&self) -> Vec<ExtensionCommandDescriptor> {
        self.inner
            .state
            .lock()
            .registry
            .commands
            .values()
            .map(|registered| registered.descriptor.clone())
            .collect()
    }

    #[must_use]
    pub fn command_sources(&self) -> Vec<ExtensionCommandSource> {
        self.inner
            .state
            .lock()
            .registry
            .commands
            .values()
            .map(|registered| {
                let spec = &registered.instance.spec;
                ExtensionCommandSource {
                    command: registered.descriptor.clone(),
                    path: spec.executable.clone(),
                    source: "local".to_owned(),
                    scope: match spec.origin {
                        ExtensionOrigin::Project => "project",
                        ExtensionOrigin::User | ExtensionOrigin::Bundled => "user",
                    }
                    .to_owned(),
                    origin: "top-level".to_owned(),
                    base_dir: spec.executable.parent().map(PathBuf::from),
                }
            })
            .collect()
    }

    #[must_use]
    pub fn tools(&self) -> Vec<ExtensionToolDescriptor> {
        self.inner
            .state
            .lock()
            .registry
            .tools
            .values()
            .map(|registered| registered.descriptor.clone())
            .collect()
    }

    #[must_use]
    pub fn provider_metadata(&self) -> Vec<ExtensionProviderMetadata> {
        self.inner
            .state
            .lock()
            .registry
            .providers
            .values()
            .map(|registered| registered.descriptor.clone())
            .collect()
    }

    #[must_use]
    pub fn message_renderers(&self) -> Vec<ExtensionMessageRendererDescriptor> {
        self.inner
            .state
            .lock()
            .registry
            .renderers
            .values()
            .map(|registered| registered.descriptor.clone())
            .collect()
    }

    /// Providers registered through the load-phase `registerProvider` API.
    /// Each resolves by its `api` (defaults to the provider `id`): a model
    /// configured with `api: <id>` streams through the extension's JS stream.
    #[must_use]
    pub fn providers(&self) -> Vec<ExtensionProviderDescriptor> {
        self.inner
            .state
            .lock()
            .registry
            .provider_streams
            .values()
            .map(|registered| registered.descriptor.clone())
            .collect()
    }

    /// Run a registered extension provider's JS stream. Each event the JS
    /// stream yields is delivered to `on_event` (as `ExtensionFrame::Update`
    /// values, in order) before the final result resolves; an unresolved id
    /// fails actionably.
    pub async fn invoke_provider(
        &self,
        provider_id: &str,
        session_id: Option<String>,
        messages: Vec<Message>,
        options: Value,
        cancellation: Option<ExtensionCancellation>,
        on_event: Option<UpdateHandler>,
    ) -> Result<Value> {
        let registered = self
            .inner
            .state
            .lock()
            .registry
            .provider_streams
            .get(provider_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown extension provider {provider_id:?}"))?;
        registered
            .instance
            .request_value(
                ExtensionHostRequest::Invoke {
                    invocation: ExtensionInvocation::Provider {
                        provider_id: provider_id.to_owned(),
                        session_id,
                        messages,
                        options,
                    },
                    context: self.invocation_context().await?,
                },
                self.inner.options.invocation_timeout,
                cancellation,
                None,
                on_event,
            )
            .await
            .with_context(|| format!("running extension provider {provider_id}"))
    }

    #[must_use]
    pub fn shortcuts(&self) -> Vec<ExtensionShortcutDescriptor> {
        self.inner
            .state
            .lock()
            .registry
            .shortcuts
            .values()
            .map(|registered| registered.descriptor.clone())
            .collect()
    }

    #[must_use]
    pub fn flags(&self) -> Vec<ExtensionFlagDescriptor> {
        self.inner
            .state
            .lock()
            .registry
            .flags
            .values()
            .map(|registered| registered.descriptor.clone())
            .collect()
    }

    /// Overlays registered through the load-phase `registerOverlay` API.
    #[must_use]
    pub fn overlays(&self) -> Vec<ExtensionOverlayDescriptor> {
        self.inner
            .state
            .lock()
            .registry
            .overlays
            .values()
            .map(|registered| registered.descriptor.clone())
            .collect()
    }

    /// The owning extension instance of the overlay with `id`, used by the
    /// TUI to correlate `ExtensionCleared` cleanup with open overlay panels.
    #[must_use]
    pub fn overlay_instance(&self, id: &str) -> Option<ExtensionInstanceId> {
        self.inner
            .state
            .lock()
            .registry
            .overlays
            .get(id)
            .map(|registered| registered.instance.id.clone())
    }

    /// Run one registered overlay's JS `render(ctx)` function and return the
    /// sanitized content rows plus the optional declarative input section
    /// (bounds + redaction applied host-side to the rows). A bare rows array
    /// (the original contract) is accepted as `{ rows, input: None }`. An
    /// unknown id fails actionably.
    pub async fn invoke_overlay_render(&self, id: &str) -> Result<OverlayRenderOutput> {
        let registered = self
            .inner
            .state
            .lock()
            .registry
            .overlays
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown extension overlay {id:?}"))?;
        let value = registered
            .instance
            .request_value(
                ExtensionHostRequest::Invoke {
                    invocation: ExtensionInvocation::OverlayRender {
                        id: id.to_owned(),
                    },
                    context: self.invocation_context().await?,
                },
                self.inner.options.invocation_timeout,
                None,
                None,
                None,
            )
            .await
            .with_context(|| format!("rendering extension overlay {id}"))?;
        let output = if value.is_array() {
            OverlayRenderOutput {
                rows: serde_json::from_value(value)
                    .with_context(|| format!("extension overlay {id} returned invalid rows"))?,
                input: None,
            }
        } else {
            serde_json::from_value(value)
                .with_context(|| format!("extension overlay {id} returned invalid output"))?
        };
        Ok(OverlayRenderOutput {
            rows: sanitize_overlay_rows(output.rows),
            input: output.input,
        })
    }

    /// Deliver an interactive overlay event to the extension's registration-time
    /// `onSubmit(text, ctx)` / `onKey(action, ctx)` callback. The host owns the
    /// editor and cursor; only the sanitized submitted text and the limited
    /// action ids cross the boundary. An unknown id fails actionably; a
    /// callback that rejects surfaces its exception.
    pub async fn invoke_overlay_event(
        &self,
        id: &str,
        event: OverlayEvent,
    ) -> Result<serde_json::Value> {
        let registered = self
            .inner
            .state
            .lock()
            .registry
            .overlays
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown extension overlay {id:?}"))?;
        registered
            .instance
            .request_value(
                ExtensionHostRequest::Invoke {
                    invocation: ExtensionInvocation::OverlayEvent {
                        id: id.to_owned(),
                        event,
                    },
                    context: self.invocation_context().await?,
                },
                self.inner.options.invocation_timeout,
                None,
                None,
                None,
            )
            .await
            .with_context(|| format!("dispatching overlay event to extension overlay {id}"))
    }

    /// Open the overlay with `id` through the host UI adapter (auto-open from
    /// an event handler). The overlay must be registered and the host must
    /// have an extension UI adapter; failures are actionable.
    pub async fn open_overlay(&self, id: &str) -> Result<()> {
        let registered = self
            .inner
            .state
            .lock()
            .registry
            .overlays
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown extension overlay {id:?}"))?;
        let instance = registered.instance;
        let ui_host = instance
            .ui
            .clone()
            .ok_or_else(|| anyhow!("the current host has no extension UI adapter"))?;
        let response = ui_host
            .request(
                ExtensionUiContext {
                    instance: instance.id.clone(),
                    mode: instance.options.mode,
                },
                ExtensionUiRequest::OverlayOpen {
                    id: id.to_owned(),
                    title: Some(registered.descriptor.title.clone()),
                    non_capturing: false,
                    input: registered.descriptor.input.clone(),
                },
                ExtensionCancellation::new(),
            )
            .await?;
        response.validate_for(&ExtensionUiRequest::OverlayOpen {
            id: id.to_owned(),
            title: None,
            non_capturing: false,
            input: None,
        })?;
        Ok(())
    }

    pub async fn invoke_command(
        &self,
        name: &str,
        arguments: String,
        timeout_override: Option<Duration>,
        cancellation: Option<ExtensionCancellation>,
    ) -> Result<Value> {
        let registered = self
            .inner
            .state
            .lock()
            .registry
            .commands
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow!("unknown extension command {name:?}"))?;
        registered
            .instance
            .request_value(
                ExtensionHostRequest::Invoke {
                    invocation: ExtensionInvocation::Command {
                        name: name.to_owned(),
                        arguments,
                    },
                    context: self.invocation_context().await?,
                },
                timeout_override.unwrap_or(self.inner.options.invocation_timeout),
                cancellation,
                None,
                None,
            )
            .await
            .map_err(|error| anyhow!("running extension command {name}: {error:#}"))
    }

    pub async fn invoke_shortcut(
        &self,
        key: &str,
        _timeout_override: Option<Duration>,
        _cancellation: Option<ExtensionCancellation>,
    ) -> Result<Value> {
        if !self
            .inner
            .state
            .lock()
            .registry
            .shortcuts
            .contains_key(key)
        {
            bail!("unknown extension shortcut {key:?}");
        }
        bail!(
            "extension shortcut invocation is unsupported: the current TUI has no safe extension shortcut dispatcher"
        )
    }

    pub async fn invoke_tool(
        &self,
        name: &str,
        call_id: String,
        arguments: Value,
        abort: AbortSignal,
        on_update: Option<Arc<dyn Fn(AgentToolResult) + Send + Sync>>,
    ) -> Result<AgentToolResult> {
        let registered = self
            .inner
            .state
            .lock()
            .registry
            .tools
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow!("unknown extension tool {name:?}"))?;
        let update = on_update.map(|callback| {
            Arc::new(move |value: Value| -> Result<()> {
                let update: AgentToolResult = serde_json::from_value(value)
                    .context("decoding extension tool update")?;
                callback(update);
                Ok(())
            }) as UpdateHandler
        });
        let value = registered
            .instance
            .request_value(
                ExtensionHostRequest::Invoke {
                    invocation: ExtensionInvocation::Tool {
                        name: name.to_owned(),
                        call_id,
                        arguments,
                    },
                    context: self.invocation_context().await?,
                },
                self.inner.options.invocation_timeout,
                None,
                Some(abort),
                update,
            )
            .await
            .with_context(|| format!("running extension tool {name}"))?;
        serde_json::from_value(value).context("decoding extension tool result")
    }

    pub fn agent_tools(&self) -> Vec<AgentTool> {
        let registry = self.inner.state.lock().registry.clone();
        agent_tools_from_registry(self, &registry)
    }


    pub async fn render_message(
        &self,
        request: MessageRenderRequest,
        cancellation: Option<ExtensionCancellation>,
    ) -> Result<Option<RenderedMessage>> {
        let registered = self
            .inner
            .state
            .lock()
            .registry
            .renderers
            .get(&request.message_type)
            .cloned();
        let Some(registered) = registered else {
            return Ok(None);
        };
        let value = registered
            .instance
            .request_value(
                ExtensionHostRequest::Invoke {
                    invocation: ExtensionInvocation::RenderMessage { request },
                    context: self.invocation_context().await?,
                },
                self.inner.options.invocation_timeout,
                cancellation,
                None,
                None,
            )
            .await?;
        serde_json::from_value(value)
            .map(Some)
            .context("decoding extension message renderer output")
    }
    async fn reduce_event<T, EventData, Apply>(&self, name: &str, state: &mut T, mut event_data: EventData, mut apply: Apply) -> Result<()>
    where EventData: FnMut(&T) -> Result<Value>, Apply: FnMut(&mut T, Value) -> Result<()> {
        let instances = self.inner.state.lock().registry.hooks.get(name).cloned().unwrap_or_default();
        for instance in instances {
            let result = instance.request_value(ExtensionHostRequest::Invoke {
                invocation: ExtensionInvocation::Event { event: ExtensionEvent::new(name, event_data(state)?) },
                context: self.invocation_context().await?,
            }, self.inner.options.hook_timeout, None, None, None).await
                .with_context(|| format!("extension {} generation {} failed {name} hook", instance.id.extension_id, instance.id.generation))?;
            if !result.is_null() { apply(state, result).with_context(|| format!("extension {} generation {} returned invalid {name} hook result", instance.id.extension_id, instance.id.generation))?; }
        }
        Ok(())
    }
    async fn reduce_event_until<T, EventData, Apply>(
        &self,
        name: &str,
        state: &mut T,
        mut event_data: EventData,
        mut apply: Apply,
    ) -> Result<()>
    where
        EventData: FnMut(&T) -> Result<Value>,
        Apply: FnMut(&mut T, Value) -> Result<bool>,
    {
        let instances = self.inner.state.lock().registry.hooks.get(name).cloned().unwrap_or_default();
        for instance in instances {
            let result = instance.request_value(ExtensionHostRequest::Invoke {
                invocation: ExtensionInvocation::Event { event: ExtensionEvent::new(name, event_data(state)?) },
                context: self.invocation_context().await?,
            }, self.inner.options.hook_timeout, None, None, None).await
                .with_context(|| format!("extension {} generation {} failed {name} hook", instance.id.extension_id, instance.id.generation))?;
            if !result.is_null() && apply(state, result)
                .with_context(|| format!("extension {} generation {} returned invalid {name} hook result", instance.id.extension_id, instance.id.generation))? {
                break;
            }
        }
        Ok(())
    }

    pub async fn reduce_before_agent_start(
        &self,
        mut event: Value,
        system_prompt: String,
    ) -> Result<ExtensionBeforeAgentStartReduction> {
        let mut reduction = ExtensionBeforeAgentStartReduction {
            system_prompt,
            messages: Vec::new(),
        };
        self.reduce_event(
            "before_agent_start",
            &mut reduction,
            |state| {
                event
                    .as_object_mut()
                    .ok_or_else(|| anyhow!("before_agent_start event data must be an object"))?
                    .insert(
                        "systemPrompt".to_owned(),
                        Value::String(state.system_prompt.clone()),
                    );
                Ok(event.clone())
            },
            |state, value| {
                let wire: BeforeAgentStartWire = serde_json::from_value(value)?;
                if let Some(system_prompt) = wire.system_prompt {
                    state.system_prompt = system_prompt;
                }
                if wire.messages.is_empty() {
                    if let Some(message) = wire.message {
                        state.messages.push(message);
                    }
                } else {
                    state.messages.extend(wire.messages);
                }
                Ok(())
            },
        )
        .await?;
        Ok(reduction)
    }

    pub async fn reduce_context(&self, messages: Vec<Message>) -> Result<Vec<Message>> {
        let mut messages = messages;
        self.reduce_event("context", &mut messages, |v| Ok(serde_json::json!({"messages":v})), |state, value| { if let Some(v)=serde_json::from_value::<ContextWire>(value)?.messages {*state=v;} Ok(()) }).await?;
        Ok(messages)
    }

    pub async fn reduce_provider_request(&self, payload: Value) -> Result<Value> {
        let mut payload=payload;
        self.reduce_event("before_provider_request", &mut payload, |v| Ok(serde_json::json!({"payload":v})), |state,value| {*state=value; Ok(())}).await?;
        Ok(payload)
    }

    pub async fn reduce_provider_headers(&self, headers: BTreeMap<String, Option<String>>) -> Result<BTreeMap<String, Option<String>>> {
        let mut headers=headers;
        self.reduce_event("before_provider_headers", &mut headers, |v| Ok(serde_json::json!({"headers":v})), |state,value| { for (name,value) in serde_json::from_value::<ProviderHeadersWire>(value)?.headers {state.insert(name,value);} Ok(())}).await?;
        Ok(headers)
    }

    pub async fn reduce_tool_call(&self, tool_call_id: &str, tool_name: &str, input: Value) -> Result<ExtensionToolCallReduction> {
        let mut state = ExtensionToolCallReduction { input, block: false, reason: None };
        self.reduce_event_until("tool_call", &mut state, |state| Ok(serde_json::json!({ "toolCallId": tool_call_id, "toolName": tool_name, "input": state.input })), |state, value| {
            let wire: ToolCallWire = serde_json::from_value(value)?;
            if let Some(input) = wire.input { state.input = input; }
            if wire.block == Some(true) { state.block = true; if wire.reason.is_some() { state.reason = wire.reason; } }
            Ok(state.block)
        }).await?;
        Ok(state)
    }
    pub async fn reduce_input(&self, text: String, images: Vec<ContentBlock>, source: &str, streaming_behavior: Option<&str>) -> Result<ExtensionInputReduction> {
        let mut state = ExtensionInputReduction::Continue { text, images };
        self.reduce_event_until("input", &mut state, |state| match state {
            ExtensionInputReduction::Continue { text, images } => Ok(serde_json::json!({ "text": text, "images": images, "source": source, "streamingBehavior": streaming_behavior })),
            ExtensionInputReduction::Handled => Ok(serde_json::json!({ "text": "", "images": [], "source": source, "streamingBehavior": streaming_behavior })),
        }, |state, value| {
            match serde_json::from_value::<InputWire>(value)? {
                InputWire::Continue => {}
                InputWire::Handled => *state = ExtensionInputReduction::Handled,
                InputWire::Transform { text, images } => {
                    let prior = match state { ExtensionInputReduction::Continue { images, .. } => images.clone(), ExtensionInputReduction::Handled => Vec::new() };
                    *state = ExtensionInputReduction::Continue { text, images: images.unwrap_or(prior) };
                }
            }
            Ok(matches!(state, ExtensionInputReduction::Handled))
        }).await?;
        Ok(state)
    }
    pub async fn reduce_before_switch(&self, event: Value) -> Result<bool> {
        self.reduce_cancel_event("session_before_switch", event).await
    }

    pub async fn reduce_cancel_event(&self, name: &str, event: Value) -> Result<bool> {
        let mut cancelled = false;
        self.reduce_event_until(name, &mut cancelled, |_| Ok(event.clone()), |cancelled, value| {
            *cancelled |= serde_json::from_value::<CancelWire>(value)?.cancel;
            Ok(*cancelled)
        }).await?;
        Ok(cancelled)
    }
    pub async fn reduce_before_fork(&self, event: Value) -> Result<ExtensionSessionBeforeForkReduction> {
        let mut reduction = ExtensionSessionBeforeForkReduction::default();
        self.reduce_event_until("session_before_fork", &mut reduction, |_| Ok(event.clone()), |state, value| {
            let wire: BeforeForkWire = serde_json::from_value(value)?;
            state.cancel |= wire.cancel;
            state.skip_conversation_restore |= wire.skip_conversation_restore;
            Ok(state.cancel)
        }).await?;
        Ok(reduction)
    }
    pub async fn reduce_before_compact(&self, event: Value) -> Result<ExtensionSessionBeforeCompactReduction> {
        let mut reduction = ExtensionSessionBeforeCompactReduction::default();
        self.reduce_event_until("session_before_compact", &mut reduction, |_| Ok(event.clone()), |state, value| {
            let wire: BeforeCompactWire = serde_json::from_value(value)?;
            state.cancel |= wire.cancel;
            if wire.compaction.is_some() { state.compaction = wire.compaction; }
            Ok(state.cancel)
        }).await?;
        Ok(reduction)
    }
    pub async fn reduce_before_tree(&self, event: Value) -> Result<ExtensionSessionBeforeTreeReduction> {
        let mut reduction = ExtensionSessionBeforeTreeReduction::default();
        self.reduce_event_until("session_before_tree", &mut reduction, |_| Ok(event.clone()), |state, value| {
            let wire: BeforeTreeWire = serde_json::from_value(value)?;
            state.cancel |= wire.cancel;
            if wire.summary.is_some() { state.summary = wire.summary; }
            if wire.custom_instructions.is_some() { state.custom_instructions = wire.custom_instructions; }
            if wire.replace_instructions.is_some() { state.replace_instructions = wire.replace_instructions; }
            if wire.label.is_some() { state.label = wire.label; }
            Ok(state.cancel)
        }).await?;
        Ok(reduction)
    }
    pub async fn reduce_user_bash(&self, event: Value) -> Result<ExtensionUserBashReduction> {
        let mut reduction = ExtensionUserBashReduction::default();
        self.reduce_event_until("user_bash", &mut reduction, |_| Ok(event.clone()), |state, value| {
            let wire: UserBashWire = serde_json::from_value(value)?;
            if wire.operations.is_some() { bail!("user_bash custom operations are unavailable in the process-hosted ExtensionAPI"); }
            if let Some(result) = wire.result {
                state.result = Some(crate::BashResult { output: result.output, exit_code: result.exit_code, cancelled: result.cancelled, truncated: result.truncated, full_output_path: result.full_output_path });
            }
            Ok(state.result.is_some())
        }).await?;
        Ok(reduction)
    }
    /// Consult the `trust_decision` event: extensions observe the tentative
    /// trust decision for a path (payload `{path, decision, isNew}`) and may
    /// recommend approval (`{approve: true}`).
    ///
    /// Fail-open by contract: the event never carries a deny surface, and the
    /// approval can only upgrade an undecided (`ask`) tentative decision —
    /// the host applies it through [`crate::trust::apply_trust_hook_outcomes`]
    /// so a stored denial is never weakened. Returns `None` when no extension
    /// approved.
    pub async fn reduce_trust_decision(&self, event: Value) -> Result<Option<ExtensionTrustDecisionReduction>> {
        let mut reduction = None;
        self.reduce_event("trust_decision", &mut reduction, |_| Ok(event.clone()), |reduction, value| {
            let wire: TrustDecisionWire = serde_json::from_value(value)?;
            if wire.approve {
                *reduction = Some(ExtensionTrustDecisionReduction { approve: true });
            }
            Ok(())
        }).await?;
        Ok(reduction)
    }

    pub async fn reduce_tool_result(&self, tool_call_id:&str, tool_name:&str, input:Value, content:Vec<ContentBlock>, details:Option<Value>, is_error:bool) -> Result<ExtensionToolResultReduction> {
        let mut state=ExtensionToolResultReduction{content,details,is_error,usage:None};
        self.reduce_event("tool_result", &mut state, |v| Ok(serde_json::json!({"toolCallId":tool_call_id,"toolName":tool_name,"input":input,"content":v.content,"details":v.details,"isError":v.is_error,"usage":v.usage})), |state,value| {let has_details=value.as_object().is_some_and(|o|o.contains_key("details"));let v:ToolResultWire=serde_json::from_value(value)?;if let Some(x)=v.content{state.content=x;}if has_details{state.details=v.details;}if let Some(x)=v.is_error{state.is_error=x;}if v.usage.is_some(){state.usage=v.usage;}Ok(())}).await?;
        Ok(state)
    }

    pub async fn reduce_message_end(&self, message:Message) -> Result<Message> {
        let role=std::mem::discriminant(&message);let mut state=message;
        self.reduce_event("message_end", &mut state, |v| Ok(serde_json::json!({"message":v})), |state,value| {if let Some(next)=serde_json::from_value::<MessageEndWire>(value)?.message{if std::mem::discriminant(&next)!=role{bail!("message_end replacement must preserve the original message role");}*state=next;}Ok(())}).await?;
        Ok(state)
    }

    pub async fn emit_checked(&self, event: ExtensionEvent) -> Result<()> {
        for outcome in self.emit(event.clone()).await {
            if let Err(message) = outcome.result {
                bail!(
                    "extension {} generation {} failed {} hook: {message}",
                    outcome.instance.extension_id,
                    outcome.instance.generation,
                    event.name,
                );
            }
        }
        Ok(())
    }

    pub async fn emit(&self, event: ExtensionEvent) -> Vec<ExtensionHookOutcome> {
        let registry = self.inner.state.lock().registry.clone();
        self.emit_from_registry(&registry, event).await
    }

    async fn emit_from_registry(
        &self,
        registry: &RuntimeRegistry,
        event: ExtensionEvent,
    ) -> Vec<ExtensionHookOutcome> {
        let instances = registry
            .hooks
            .get(&event.name)
            .cloned()
            .unwrap_or_default();
        let mut outcomes = Vec::with_capacity(instances.len());
        for instance in instances {
            let context = match self.invocation_context().await {
                Ok(context) => context,
                Err(error) => {
                    // A real host context error is an explicit per-hook
                    // failure, never a silent None context.
                    let _ = self
                        .inner
                        .events
                        .send(ExtensionRuntimeEvent::InvocationFailed {
                            instance: instance.id.clone(),
                            operation: format!("event:{}", event.name),
                            message: error.to_string(),
                        });
                    outcomes.push(ExtensionHookOutcome {
                        instance: instance.id.clone(),
                        result: Err(error.to_string()),
                    });
                    continue;
                }
            };
            let result = instance
                .request_value(
                    ExtensionHostRequest::Invoke {
                        invocation: ExtensionInvocation::Event {
                            event: event.clone(),
                        },
                        context,
                    },
                    self.inner.options.hook_timeout,
                    None,
                    None,
                    None,
                )
                .await;
            if let Err(error) = &result {
                let _ = self.inner.events.send(ExtensionRuntimeEvent::InvocationFailed {
                    instance: instance.id.clone(),
                    operation: format!("event:{}", event.name),
                    message: error.to_string(),
                });
            }
            outcomes.push(ExtensionHookOutcome {
                instance: instance.id.clone(),
                result: result.map_err(|error| error.to_string()),
            });
        }
        outcomes
    }

    pub async fn shutdown(&self) {
        self.shutdown_with_reason("quit").await;
    }

    pub async fn shutdown_with_reason(&self, reason: &str) {
        let _reload_guard = self.inner.reload_lock.lock().await;
        let (instances, registry) = {
            let mut state = self.inner.state.lock();
            let instances = std::mem::take(&mut state.instances);
            let registry = std::mem::take(&mut state.registry);
            (instances, registry)
        };
        let event = ExtensionEvent::new(
            "session_shutdown",
            serde_json::json!({ "reason": reason }),
        );
        let _ = self.emit_from_registry(&registry, event).await;
        for instance in &instances {
            instance.invalidate_now("extension runtime shut down");
        }
        // Drop every provider entry this runtime registered in the shared
        // pi-ai registry so future resolution fails actionably.
        self.unregister_provider_registrations();
        for instance in instances {
            instance.finish_invalidate(reason).await;
        }
    }

    /// Drop every provider entry this runtime registered in the shared pi-ai
    /// registry so future resolution fails actionably. Namespace-scoped: only
    /// this runtime's entries are removed — concurrent runtimes that
    /// registered the same api are untouched.
    fn unregister_provider_registrations(&self) {
        pi_ai::unregister_extension_providers(&self.inner.provider_namespace);
    }

    /// Keep the shared pi-ai provider registry aligned with the committed
    /// extension registry: entries for providers that left are dropped, and
    /// every committed provider is (re)registered. Runs on every commit
    /// (startup and reload), so re-registration within this runtime's
    /// namespace replaces the previous generation's entry while stale entries
    /// are removed.
    fn sync_provider_registrations(&self, new: &RuntimeRegistry) {
        self.unregister_provider_registrations();
        for registered in new.provider_streams.values() {
            let descriptor = registered.descriptor.clone();
            let namespace = self.inner.provider_namespace.clone();
            let api = descriptor.api.clone();
            let provider_id = descriptor.id.clone();

            let stream_runtime = self.clone();
            let stream_provider_id = provider_id.clone();
            let stream: StreamFn = Arc::new(move |model, context, options| {
                let runtime = stream_runtime.clone();
                let provider_id = stream_provider_id.clone();
                Box::pin(async move {
                    extension_provider_stream(&runtime, &provider_id, &model, &context, options)
                        .await
                })
            });

            let simple_runtime = self.clone();
            let simple_provider_id = provider_id.clone();
            let simple: SimpleStreamFn = Arc::new(move |model, context, options| {
                let runtime = simple_runtime.clone();
                let provider_id = simple_provider_id.clone();
                Box::pin(async move {
                    extension_provider_stream(
                        &runtime,
                        &provider_id,
                        &model,
                        &context,
                        options.stream,
                    )
                    .await
                })
            });
            pi_ai::register_extension_provider(
                ApiProvider {
                    api,
                    stream,
                    stream_simple: simple,
                    generate_image: None,
                },
                namespace,
            );
        }
    }
}

/// The `options` argument handed to the extension's JS stream function. The
/// bridge deliberately forwards only the portable subset of [`StreamOptions`];
/// transport details and hooks stay host-side.
fn provider_stream_options_json(options: &StreamOptions) -> Value {
    let mut map = serde_json::Map::new();
    if let Some(temperature) = options.temperature {
        map.insert("temperature".to_owned(), json!(temperature));
    }
    if let Some(max_tokens) = options.max_tokens {
        map.insert("maxTokens".to_owned(), json!(max_tokens));
    }
    if let Some(timeout_ms) = options.timeout_ms {
        map.insert("timeoutMs".to_owned(), json!(timeout_ms));
    }
    map.insert(
        "metadata".to_owned(),
        options.metadata.clone().unwrap_or(Value::Null),
    );
    Value::Object(map)
}

/// Stream bridge for extension-registered providers: calls the extension's JS
/// stream through [`ExtensionRuntime::invoke_provider`] and translates the
/// events it yields into an [`AssistantMessageEventStream`] (the vocabulary
/// the agent loop consumes). JS stream failures surface as typed `Error`
/// events — never a crashed session — with secret-looking text redacted via
/// [`crate::redact::redact_secrets`].
///
/// JS event vocabulary (compact; the bridge maintains the partial message):
/// - `{ type: "start" }` — opens the stream (optional; a `Start` event is
///   always emitted first anyway).
/// - `{ type: "text", text }` — a complete text block.
/// - `{ type: "text_delta", delta }` — appends to the most recent text block.
/// - `{ type: "thinking", thinking }` — a complete thinking block.
/// - `{ type: "tool_call", id?, name, arguments }` — a tool-call block.
/// - `{ type: "done", stopReason?, message? }` — terminal; `message` may carry
///   the final [`AssistantMessage`], otherwise it is synthesized.
/// - `{ type: "error", error? }` — typed stream error.
///
/// Any other shape fails the invocation (and becomes a typed stream error).
async fn extension_provider_stream(
    runtime: &ExtensionRuntime,
    provider_id: &str,
    model: &Model,
    context: &Context,
    options: StreamOptions,
) -> AssistantMessageEventStream {
    let stream = pi_ai::new_assistant_message_event_stream();
    let returned = stream.clone();
    let task_writer = stream.clone();
    let runtime = runtime.clone();
    let provider_id = provider_id.to_owned();
    let session_id = options.session_id.clone();
    let messages = context.messages.clone();
    let options_json = provider_stream_options_json(&options);
    // The invocation's `on_event` bridge is a plain `Fn`, so translated events
    // hop through an unbounded channel and the spawned task pushes them into
    // the stream (order preserved, nothing blocks the extension host).
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = Arc::new(Mutex::new(ProviderStreamTranslator::new(
        stream,
        model.clone(),
    )));
    let event_state = state.clone();
    let on_event: UpdateHandler = Arc::new(move |value: Value| -> Result<()> {
        let mut translator = event_state.lock();
        for event in translator.apply_event(value)? {
            let _ = event_tx.send(event);
        }
        Ok(())
    });
    tokio::spawn(async move {
        let invocation = runtime.invoke_provider(
            &provider_id,
            session_id,
            messages,
            options_json,
            None,
            Some(on_event),
        );
        tokio::pin!(invocation);
        let mut outcome = None;
        loop {
            tokio::select! {
                biased;
                event = event_rx.recv() => match event {
                    Some(event) => task_writer.push(event).await,
                    None => break,
                },
                result = &mut invocation => {
                    outcome = Some(result);
                    break;
                }
            }
        }
        // Drop the completed invocation so its `on_event` sender goes away,
        // then drain events that raced the completion in order.
        drop(invocation);
        while let Some(event) = event_rx.recv().await {
            task_writer.push(event).await;
        }
        let (stream, terminal) = {
            let mut translator = state.lock();
            let stream = translator.stream.clone();
            let terminal = match outcome {
                Some(Ok(_)) => translator.finish_terminal(),
                Some(Err(error)) => translator.error_terminal(&format!("{error:#}")),
                None => translator.error_terminal("extension provider channel closed"),
            };
            // The parking_lot guard is not Send; drop it before any await.
            drop(translator);
            (stream, terminal)
        };
        // Push the terminal event after releasing the (non-Send) guard.
        let (message, is_error) = terminal;
        if is_error {
            stream
                .push(AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error: message.clone(),
                })
                .await;
        } else {
            stream
                .push(AssistantMessageEvent::Done {
                    reason: message.stop_reason,
                    message: message.clone(),
                })
                .await;
        }
        stream.end(Some(message)).await;
    });
    returned
}

/// Keeps the accumulated [`AssistantMessage`] while translating compact JS
/// stream events into [`AssistantMessageEvent`]s. All mutation happens
/// synchronously (the `on_event` bridge is a plain `Fn`), so the lock is
/// never held across an await.
struct ProviderStreamTranslator {
    stream: AssistantMessageEventStream,
    partial: AssistantMessage,
    started: bool,
    stop_reason: StopReason,
    terminal_message: Option<AssistantMessage>,
    error_message: Option<String>,
}

impl ProviderStreamTranslator {
    fn new(stream: AssistantMessageEventStream, model: Model) -> Self {
        Self {
            stream,
            partial: AssistantMessage::pending(&model),
            started: false,
            stop_reason: StopReason::Stop,
            terminal_message: None,
            error_message: None,
        }
    }

    /// Translate one JS event into zero or more [`AssistantMessageEvent`]s.
    fn apply_event(&mut self, value: Value) -> Result<Vec<AssistantMessageEvent>> {
        let Some(event) = value.as_object() else {
            bail!("provider stream event must be an object");
        };
        let Some(event_type) = event.get("type").and_then(Value::as_str) else {
            bail!("provider stream event is missing a string \"type\"");
        };
        match event_type {
            "start" => {
                if self.started {
                    return Ok(Vec::new());
                }
                self.started = true;
                Ok(vec![AssistantMessageEvent::Start {
                    partial: self.partial.clone(),
                }])
            }
            "text" => {
                let text = required_event_string(event, "text")?;
                let mut events = self.ensure_started();
                let index = self.partial.content.len();
                self.partial.content.push(ContentBlock::text(""));
                events.extend([
                    AssistantMessageEvent::TextStart {
                        content_index: index,
                        partial: self.partial.clone(),
                    },
                    AssistantMessageEvent::TextDelta {
                        content_index: index,
                        delta: text.clone(),
                        partial: self.partial.clone(),
                    },
                ]);
                self.partial.content[index] = ContentBlock::text(text.clone());
                events.push(AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content: text,
                    partial: self.partial.clone(),
                });
                Ok(events)
            }
            "text_delta" => {
                let delta = required_event_string(event, "delta")?;
                let index = self
                    .partial
                    .content
                    .iter()
                    .rposition(|block| matches!(block, ContentBlock::Text { .. }))
                    .ok_or_else(|| anyhow!("text_delta before any text block"))?;
                let accumulated = match &self.partial.content[index] {
                    ContentBlock::Text { text, .. } => format!("{text}{delta}"),
                    _ => unreachable!("rposition matched a text block"),
                };
                self.partial.content[index] = ContentBlock::text(accumulated);
                Ok(vec![AssistantMessageEvent::TextDelta {
                    content_index: index,
                    delta,
                    partial: self.partial.clone(),
                }])
            }
            "thinking" => {
                let thinking = required_event_string(event, "thinking")?;
                let mut events = self.ensure_started();
                let index = self.partial.content.len();
                self.partial.content.push(ContentBlock::thinking(""));
                events.extend([
                    AssistantMessageEvent::ThinkingStart {
                        content_index: index,
                        partial: self.partial.clone(),
                    },
                    AssistantMessageEvent::ThinkingDelta {
                        content_index: index,
                        delta: thinking.clone(),
                        partial: self.partial.clone(),
                    },
                ]);
                self.partial.content[index] = ContentBlock::thinking(thinking.clone());
                events.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content: thinking,
                    partial: self.partial.clone(),
                });
                Ok(events)
            }
            "tool_call" => {
                let name = required_event_string(event, "name")?;
                let id = event
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("call_extension")
                    .to_owned();
                let arguments = event
                    .get("arguments")
                    .cloned()
                    .unwrap_or(Value::Object(Default::default()));
                let mut events = self.ensure_started();
                let index = self.partial.content.len();
                self.partial
                    .content
                    .push(ContentBlock::ToolCall(ToolCall {
                        id,
                        name: name.clone(),
                        arguments: arguments.clone(),
                        thought_signature: None,
                    }));
                events.extend([
                    AssistantMessageEvent::ToolCallStart {
                        content_index: index,
                        partial: self.partial.clone(),
                    },
                    AssistantMessageEvent::ToolCallDelta {
                        content_index: index,
                        delta: arguments.to_string(),
                        partial: self.partial.clone(),
                    },
                ]);
                let tool_call = match &self.partial.content[index] {
                    ContentBlock::ToolCall(call) => call.clone(),
                    _ => unreachable!("content block was just pushed as a tool call"),
                };
                events.push(AssistantMessageEvent::ToolCallEnd {
                    content_index: index,
                    tool_call,
                    partial: self.partial.clone(),
                });
                Ok(events)
            }
            "done" => {
                if let Some(message) = event.get("message") {
                    let message: AssistantMessage = serde_json::from_value(message.clone())
                        .context("provider done message must be an AssistantMessage")?;
                    self.terminal_message = Some(message);
                }
                if let Some(stop_reason) = event.get("stopReason").and_then(Value::as_str) {
                    self.stop_reason = parse_stop_reason(stop_reason)?;
                }
                Ok(Vec::new())
            }
            "error" => {
                self.error_message = Some(
                    event
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("extension provider stream failed")
                        .to_owned(),
                );
                self.stop_reason = StopReason::Error;
                Ok(Vec::new())
            }
            other => bail!("unknown provider stream event type {other:?}"),
        }
    }

    /// Emit the opening `Start` event on first content; returns it so the
    /// caller folds it into its translated event batch.
    fn ensure_started(&mut self) -> Vec<AssistantMessageEvent> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        vec![AssistantMessageEvent::Start {
            partial: self.partial.clone(),
        }]
    }

    /// Terminal state on normal completion: `Done` (or `Error` when the JS
    /// emitted an error event) with the accumulated/overridden message.
    fn finish_terminal(&self) -> (AssistantMessage, bool) {
        self.terminal()
    }

    /// Terminal state after a JS-side failure (invocation error): a typed
    /// error message with the redacted failure text.
    fn error_terminal(&self, message: &str) -> (AssistantMessage, bool) {
        let mut error = self.partial.clone();
        error.stop_reason = StopReason::Error;
        error.error_message = Some(crate::redact::redact_secrets(message));
        (error, true)
    }

    fn terminal(&self) -> (AssistantMessage, bool) {
        let mut message = self
            .terminal_message
            .clone()
            .unwrap_or_else(|| self.partial.clone());
        if message.stop_reason == StopReason::Pending {
            message.stop_reason = self.stop_reason;
        }
        let is_error = self.error_message.is_some() || message.stop_reason == StopReason::Error;
        if is_error {
            message.stop_reason = StopReason::Error;
            message.error_message = Some(crate::redact::redact_secrets(
                self.error_message
                    .as_deref()
                    .unwrap_or("extension provider stream failed"),
            ));
        }
        (message, is_error)
    }
}

fn required_event_string(event: &serde_json::Map<String, Value>, field: &str) -> Result<String> {
    event
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("provider stream event is missing a string {field:?}"))
}

fn parse_stop_reason(value: &str) -> Result<StopReason> {
    match value {
        "stop" => Ok(StopReason::Stop),
        "length" => Ok(StopReason::Length),
        "tool_use" => Ok(StopReason::ToolUse),
        "error" => Ok(StopReason::Error),
        "aborted" => Ok(StopReason::Aborted),
        other => bail!("unknown provider stream stopReason {other:?}"),
    }
}
fn spawn_discarded_instances(instances: Vec<Arc<ExtensionInstance>>) {
    for instance in &instances {
        instance.invalidate_now("extension reload candidate rejected");
    }
    let cleanup = async move {
        for instance in instances {
            instance.finish_invalidate("candidate_rejected").await;
        }
    };
    run_cleanup_future(cleanup, || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
    });
}

/// Runs a cleanup future, preferring the ambient Tokio runtime when one is
/// available. Otherwise the future runs on a fresh current-thread runtime on
/// a spawned thread; when that runtime cannot be built (e.g. resource
/// exhaustion), the failure is handled explicitly with a fixed-level warning
/// and best-available synchronous cleanup: the future's captured resources
/// (extension instances and their transports) drop here, which cancels
/// invalidation and reaps sandbox-spawned children (`kill_on_drop`). The
/// warning carries no instance data, so cleanup/resource exhaustion can never
/// panic the process or leak secrets into diagnostics. `build_runtime` is
/// injectable so the failure path is unit-testable without a panic.
fn run_cleanup_future<F>(
    cleanup: F,
    build_runtime: impl FnOnce() -> std::io::Result<tokio::runtime::Runtime>,
) where
    F: Future<Output = ()> + Send + 'static,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(cleanup);
        return;
    }
    match build_runtime() {
        Ok(runtime) => {
            std::thread::spawn(move || runtime.block_on(cleanup));
        }
        Err(error) => {
            eprintln!(
                "extensions: cleanup runtime construction failed ({error}); \
                 dropping extension instances for best-effort cleanup"
            );
            drop(cleanup);
        }
    }
}
fn agent_tools_from_registry(
    runtime: &ExtensionRuntime,
    registry: &RuntimeRegistry,
) -> Vec<AgentTool> {
    registry
        .tools
        .values()
        .map(|registered| registered.descriptor.clone())
        .map(|descriptor| {
            let runtime = runtime.clone();
            let name = descriptor.name.clone();
            let mut tool = AgentTool::new(
                descriptor.name,
                descriptor.description,
                descriptor.parameters,
                move |context| {
                    let runtime = runtime.clone();
                    let name = name.clone();
                    async move {
                        runtime
                            .invoke_tool(
                                &name,
                                context.tool_call_id,
                                context.arguments,
                                context.abort,
                                Some(context.on_update),
                            )
                            .await
                    }
                },
            )
            .with_label(descriptor.label)
            .with_capability(descriptor.capability)
            .with_execution_mode(descriptor.execution_mode);
            tool.prompt_guidelines = descriptor.prompt_guidelines;
            tool
        })
        .collect()
}

#[derive(Clone, Default)]
struct RuntimeRegistry {
    commands: BTreeMap<String, RegisteredCommand>,
    tools: BTreeMap<String, RegisteredTool>,
    hooks: BTreeMap<String, Vec<Arc<ExtensionInstance>>>,
    renderers: BTreeMap<String, RegisteredRenderer>,
    providers: BTreeMap<String, RegisteredProvider>,
    provider_streams: BTreeMap<String, RegisteredProviderStream>,
    shortcuts: BTreeMap<String, RegisteredShortcut>,
    flags: BTreeMap<String, RegisteredFlag>,
    overlays: BTreeMap<String, RegisteredOverlay>,
}

impl RuntimeRegistry {
    fn from_instances(
        instances: &[Arc<ExtensionInstance>],
    ) -> (Self, Vec<ExtensionRuntimeEvent>) {
        let mut registry = Self::default();
        let mut collisions = Vec::new();
        for instance in instances {
            for descriptor in &instance.registrations.commands {
                insert_unique(
                    &mut registry.commands,
                    descriptor.name.clone(),
                    RegisteredCommand {
                        descriptor: descriptor.clone(),
                        instance: instance.clone(),
                    },
                    ExtensionCapability::Commands,
                    instance,
                    &mut collisions,
                    |registered| &registered.instance,
                );
            }
            for descriptor in &instance.registrations.tools {
                insert_unique(
                    &mut registry.tools,
                    descriptor.name.clone(),
                    RegisteredTool {
                        descriptor: descriptor.clone(),
                        instance: instance.clone(),
                    },
                    ExtensionCapability::Tools,
                    instance,
                    &mut collisions,
                    |registered| &registered.instance,
                );
            }
            for descriptor in &instance.registrations.hooks {
                registry
                    .hooks
                    .entry(descriptor.event.clone())
                    .or_default()
                    .push(instance.clone());
            }
            for descriptor in &instance.registrations.renderers {
                insert_unique(
                    &mut registry.renderers,
                    descriptor.message_type.clone(),
                    RegisteredRenderer {
                        descriptor: descriptor.clone(),
                        instance: instance.clone(),
                    },
                    ExtensionCapability::MessageRenderers,
                    instance,
                    &mut collisions,
                    |registered| &registered.instance,
                );
            }
            for descriptor in &instance.registrations.providers {
                insert_unique(
                    &mut registry.providers,
                    descriptor.id.clone(),
                    RegisteredProvider {
                        descriptor: descriptor.clone(),
                        instance: instance.clone(),
                    },
                    ExtensionCapability::ProviderMetadata,
                    instance,
                    &mut collisions,
                    |registered| &registered.instance,
                );
            }
            for descriptor in &instance.registrations.provider_streams {
                insert_unique(
                    &mut registry.provider_streams,
                    descriptor.id.clone(),
                    RegisteredProviderStream {
                        descriptor: descriptor.clone(),
                        instance: instance.clone(),
                    },
                    ExtensionCapability::Provider,
                    instance,
                    &mut collisions,
                    |registered| &registered.instance,
                );
            }
            for descriptor in &instance.registrations.shortcuts {
                insert_unique(
                    &mut registry.shortcuts,
                    descriptor.key.clone(),
                    RegisteredShortcut {
                        descriptor: descriptor.clone(),
                        instance: instance.clone(),
                    },
                    ExtensionCapability::Commands,
                    instance,
                    &mut collisions,
                    |registered| &registered.instance,
                );
            }
            for descriptor in &instance.registrations.flags {
                insert_unique(
                    &mut registry.flags,
                    descriptor.name.clone(),
                    RegisteredFlag {
                        descriptor: descriptor.clone(),
                        instance: instance.clone(),
                    },
                    ExtensionCapability::Commands,
                    instance,
                    &mut collisions,
                    |registered| &registered.instance,
                );
            }
            for descriptor in &instance.registrations.overlays {
                insert_unique(
                    &mut registry.overlays,
                    descriptor.id.clone(),
                    RegisteredOverlay {
                        descriptor: descriptor.clone(),
                        instance: instance.clone(),
                    },
                    ExtensionCapability::Overlays,
                    instance,
                    &mut collisions,
                    |registered| &registered.instance,
                );
            }
        }
        (registry, collisions)
    }
}

fn insert_unique<T, F>(
    target: &mut BTreeMap<String, T>,
    name: String,
    value: T,
    capability: ExtensionCapability,
    loser: &Arc<ExtensionInstance>,
    collisions: &mut Vec<ExtensionRuntimeEvent>,
    owner: F,
) where
    F: Fn(&T) -> &Arc<ExtensionInstance>,
{
    if let Some(existing) = target.get(&name) {
        collisions.push(ExtensionRuntimeEvent::Collision {
            capability,
            name,
            winner: owner(existing).id.clone(),
            loser: loser.id.clone(),
        });
    } else {
        target.insert(name, value);
    }
}

#[derive(Clone)]
struct RegisteredCommand {
    descriptor: ExtensionCommandDescriptor,
    instance: Arc<ExtensionInstance>,
}

#[derive(Clone)]
struct RegisteredTool {
    descriptor: ExtensionToolDescriptor,
    instance: Arc<ExtensionInstance>,
}

#[derive(Clone)]
struct RegisteredRenderer {
    descriptor: ExtensionMessageRendererDescriptor,
    instance: Arc<ExtensionInstance>,
}

#[derive(Clone)]
struct RegisteredProvider {
    descriptor: ExtensionProviderMetadata,
    instance: Arc<ExtensionInstance>,
}

#[derive(Clone)]
struct RegisteredProviderStream {
    descriptor: ExtensionProviderDescriptor,
    instance: Arc<ExtensionInstance>,
}

#[derive(Clone)]
struct RegisteredShortcut {
    descriptor: ExtensionShortcutDescriptor,
    instance: Arc<ExtensionInstance>,
}

#[derive(Clone)]
struct RegisteredFlag {
    descriptor: ExtensionFlagDescriptor,
    instance: Arc<ExtensionInstance>,
}

#[derive(Clone)]
struct RegisteredOverlay {
    descriptor: ExtensionOverlayDescriptor,
    instance: Arc<ExtensionInstance>,
}

#[derive(Clone, Default)]
struct ExtensionRegistrations {
    commands: Vec<ExtensionCommandDescriptor>,
    tools: Vec<ExtensionToolDescriptor>,
    hooks: Vec<ExtensionEventHookDescriptor>,
    renderers: Vec<ExtensionMessageRendererDescriptor>,
    providers: Vec<ExtensionProviderMetadata>,
    provider_streams: Vec<ExtensionProviderDescriptor>,
    shortcuts: Vec<ExtensionShortcutDescriptor>,
    flags: Vec<ExtensionFlagDescriptor>,
    overlays: Vec<ExtensionOverlayDescriptor>,
}

struct RegistrationBuilder<'a> {
    manifest: &'a ExtensionCapabilityManifest,
    permissions: &'a ExtensionPermissionSet,
    registrations: ExtensionRegistrations,
    commands: BTreeSet<String>,
    tools: BTreeSet<String>,
    hooks: BTreeSet<String>,
    renderers: BTreeSet<String>,
    providers: BTreeSet<String>,
    provider_streams: BTreeSet<String>,
    shortcuts: BTreeSet<String>,
    flags: BTreeSet<String>,
    overlays: BTreeSet<String>,
}

impl<'a> RegistrationBuilder<'a> {
    fn new(
        manifest: &'a ExtensionCapabilityManifest,
        permissions: &'a ExtensionPermissionSet,
    ) -> Self {
        Self {
            manifest,
            permissions,
            registrations: ExtensionRegistrations::default(),
            commands: BTreeSet::new(),
            tools: BTreeSet::new(),
            hooks: BTreeSet::new(),
            renderers: BTreeSet::new(),
            providers: BTreeSet::new(),
            provider_streams: BTreeSet::new(),
            shortcuts: BTreeSet::new(),
            flags: BTreeSet::new(),
            overlays: BTreeSet::new(),
        }
    }

    fn register(&mut self, registration: ExtensionRegistration) -> Result<()> {
        let capability = registration.capability();
        if !self.manifest.capabilities.contains(&capability) {
            bail!("extension registered undeclared capability {capability:?}");
        }
        if !self.permissions.allows(capability) {
            bail!("extension registered ungranted capability {capability:?}");
        }
        match registration {
            ExtensionRegistration::Command { command } => {
                validate_command_name(&command.name)?;
                if let Some(description) = &command.description {
                    validate_text(description, "command description", 4096)?;
                }
                insert_local(&mut self.commands, &command.name, "command")?;
                self.registrations.commands.push(command);
            }
            ExtensionRegistration::Tool { tool } => {
                validate_identifier(&tool.name, "tool name")?;
                validate_text(&tool.label, "tool label", 256)?;
                validate_text(&tool.description, "tool description", 16 * 1024)?;
                for guideline in &tool.prompt_guidelines {
                    validate_text(guideline, "tool prompt guideline", 16 * 1024)?;
                }
                insert_local(&mut self.tools, &tool.name, "tool")?;
                self.registrations.tools.push(tool);
            }
            ExtensionRegistration::EventHook { hook } => {
                validate_event_name(&hook.event)?;
                insert_local(&mut self.hooks, &hook.event, "event hook")?;
                self.registrations.hooks.push(hook);
            }
            ExtensionRegistration::MessageRenderer { renderer } => {
                validate_identifier(&renderer.message_type, "message renderer type")?;
                insert_local(
                    &mut self.renderers,
                    &renderer.message_type,
                    "message renderer",
                )?;
                self.registrations.renderers.push(renderer);
            }
            ExtensionRegistration::ProviderMetadata { provider } => {
                validate_identifier(&provider.id, "provider id")?;
                if let Some(name) = &provider.name {
                    validate_text(name, "provider name", 256)?;
                }
                insert_local(&mut self.providers, &provider.id, "provider metadata")?;
                self.registrations.providers.push(provider);
            }
            ExtensionRegistration::Provider { provider } => {
                validate_identifier(&provider.id, "provider id")?;
                validate_identifier(&provider.api, "provider api")?;
                if let Some(label) = &provider.label {
                    validate_text(label, "provider label", 256)?;
                }
                for capability in &provider.capabilities {
                    validate_identifier(capability, "provider capability")?;
                }
                insert_local(
                    &mut self.provider_streams,
                    &provider.id,
                    "provider",
                )?;
                self.registrations.provider_streams.push(provider);
            }
            ExtensionRegistration::Shortcut { shortcut } => {
                validate_text(&shortcut.key, "shortcut key", 128)?;
                if let Some(description) = &shortcut.description {
                    validate_text(description, "shortcut description", 4096)?;
                }
                insert_local(&mut self.shortcuts, &shortcut.key, "shortcut")?;
                self.registrations.shortcuts.push(shortcut);
            }
            ExtensionRegistration::Flag { flag } => {
                validate_identifier(&flag.name, "flag name")?;
                if let Some(description) = &flag.description {
                    validate_text(description, "flag description", 4096)?;
                }
                if let Some(default) = &flag.default {
                    let valid = matches!(
                        (&flag.r#type, default),
                        (ExtensionFlagType::Boolean, Value::Bool(_))
                            | (ExtensionFlagType::String, Value::String(_))
                    );
                    if !valid {
                        bail!("extension flag default does not match its declared type");
                    }
                }
                insert_local(&mut self.flags, &flag.name, "flag")?;
                self.registrations.flags.push(flag);
            }
            ExtensionRegistration::Overlay { overlay } => {
                validate_identifier(&overlay.id, "overlay id")?;
                validate_text(&overlay.title, "overlay title", 256)?;
                insert_local(&mut self.overlays, &overlay.id, "overlay")?;
                self.registrations.overlays.push(overlay);
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<ExtensionRegistrations> {
        Ok(self.registrations)
    }

    /// Load-phase `pi.unregisterProvider`: drop a provider registered earlier
    /// in the same load phase. Unregistering an unknown id fails actionably;
    /// re-registering the id afterwards works again (last registration wins).
    fn unregister_provider(&mut self, id: &str) -> Result<()> {
        if !self.provider_streams.remove(id) {
            bail!("extension cannot unregister unknown provider {id:?}");
        }
        self.registrations
            .provider_streams
            .retain(|provider| provider.id != id);
        Ok(())
    }
}

fn insert_local(set: &mut BTreeSet<String>, name: &str, kind: &str) -> Result<()> {
    if set.insert(name.to_owned()) {
        Ok(())
    } else {
        bail!("extension registered duplicate {kind} {name:?}")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum ExtensionPhase {
    Loading = 0,
    Initializing = 1,
    Ready = 2,
    Active = 3,
    Invalidated = 4,
    Crashed = 5,
}

impl ExtensionPhase {
    fn from_byte(value: u8) -> Self {
        match value {
            0 => Self::Loading,
            1 => Self::Initializing,
            2 => Self::Ready,
            3 => Self::Active,
            4 => Self::Invalidated,
            _ => Self::Crashed,
        }
    }
}

type UpdateHandler = Arc<dyn Fn(Value) -> Result<()> + Send + Sync>;

enum PendingRequestEvent {
    Update(Value),
    Finished(ProtocolResult),
    ConnectionFailed(String),
}

struct ExtensionInstance {
    id: ExtensionInstanceId,
    manifest: ExtensionCapabilityManifest,
    registrations: ExtensionRegistrations,
    spec: ExtensionSpec,
    transport: Arc<dyn ExtensionTransport>,
    ui: Option<Arc<dyn ExtensionUiHost>>,
    action_host: Arc<Mutex<Option<Arc<dyn ExtensionActionHost>>>>,
    options: ExtensionRuntimeOptions,
    phase: AtomicU8,
    invalidation: ExtensionCancellation,
    pending: Mutex<HashMap<String, mpsc::UnboundedSender<PendingRequestEvent>>>,
    inbound: Mutex<HashMap<String, ExtensionCancellation>>,
    events: broadcast::Sender<ExtensionRuntimeEvent>,
}

impl ExtensionInstance {
    fn new(
        id: ExtensionInstanceId,
        manifest: ExtensionCapabilityManifest,
        registrations: ExtensionRegistrations,
        spec: ExtensionSpec,
        transport: Arc<dyn ExtensionTransport>,
        ui: Option<Arc<dyn ExtensionUiHost>>,
        action_host: Arc<Mutex<Option<Arc<dyn ExtensionActionHost>>>>,
        options: ExtensionRuntimeOptions,
        events: broadcast::Sender<ExtensionRuntimeEvent>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id,
            manifest,
            registrations,
            spec,
            transport,
            ui,
            action_host,
            options,
            phase: AtomicU8::new(ExtensionPhase::Loading as u8),
            invalidation: ExtensionCancellation::new(),
            pending: Mutex::new(HashMap::new()),
            inbound: Mutex::new(HashMap::new()),
            events,
        })
    }

    fn phase(&self) -> ExtensionPhase {
        ExtensionPhase::from_byte(self.phase.load(Ordering::Acquire))
    }

    fn set_phase(&self, phase: ExtensionPhase) {
        self.phase.store(phase as u8, Ordering::Release);
    }

    fn activate(&self) {
        if self.phase() == ExtensionPhase::Ready {
            self.set_phase(ExtensionPhase::Active);
        }
    }

    fn start_reader(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let transport = self.transport.clone();
        let invalidation = self.invalidation.clone();
        tokio::spawn(async move {
            let failure = loop {
                let received = tokio::select! {
                    () = invalidation.cancelled() => return,
                    received = transport.receive() => received,
                };
                let frame = match received {
                    Ok(Some(frame)) => frame,
                    Ok(None) => break "extension process exited".to_owned(),
                    Err(error) => break format!("extension protocol failed: {error}"),
                };
                let Some(instance) = weak.upgrade() else {
                    return;
                };
                if let Err(error) = instance.handle_frame(frame).await {
                    break format!("extension protocol violation: {error}");
                }
            };
            if let Some(instance) = weak.upgrade() {
                instance.mark_crashed(&failure);
                if let Some(ui) = &instance.ui {
                    let _ = ui.clear_extension(instance.id.clone()).await;
                }
            }
            let _ = transport.terminate().await;
        });
    }

    async fn handle_frame(self: &Arc<Self>, frame: ExtensionFrame) -> Result<()> {
        match frame {
            ExtensionFrame::Response { id, result } => {
                validate_request_id(&id)?;
                if let Some(sender) = self.pending.lock().get(&id).cloned() {
                    let _ = sender.send(PendingRequestEvent::Finished(result));
                }
                Ok(())
            }
            ExtensionFrame::Update { id, value } => {
                validate_request_id(&id)?;
                if let Some(sender) = self.pending.lock().get(&id).cloned() {
                    let _ = sender.send(PendingRequestEvent::Update(value));
                }
                Ok(())
            }
            ExtensionFrame::Request { id, request } => {
                validate_request_id(&id)?;
                self.handle_runtime_request(id, request).await
            }
            ExtensionFrame::Cancel { id } => {
                validate_request_id(&id)?;
                if let Some(cancellation) = self.inbound.lock().get(&id).cloned() {
                    cancellation.cancel();
                }
                Ok(())
            }
            ExtensionFrame::Register { .. } => {
                bail!("registration is only allowed during the load phase")
            }
            ExtensionFrame::UnregisterProvider { .. } => {
                bail!("provider unregistration is only allowed during the load phase")
            }
            ExtensionFrame::Hello { .. } => bail!("duplicate hello frame"),
        }
    }

    async fn handle_runtime_request(
        self: &Arc<Self>,
        id: String,
        request: ExtensionRuntimeRequest,
    ) -> Result<()> {
        match self.phase() {
            ExtensionPhase::Loading => {
                bail!("runtime actions are forbidden during the load phase")
            }
            ExtensionPhase::Initializing | ExtensionPhase::Active => {}
            ExtensionPhase::Ready => {
                return self
                    .send_runtime_failure(id, "not_active", "extension is not active yet")
                    .await;
            }
            ExtensionPhase::Invalidated | ExtensionPhase::Crashed => {
                return self
                    .send_runtime_failure(
                        id,
                        "invalidated",
                        "extension generation is no longer active",
                    )
                    .await;
            }
        }
        if self.inbound.lock().contains_key(&id) {
            bail!("duplicate extension request id {id:?}");
        }
        match request {
            ExtensionRuntimeRequest::Ui { ui } => {
                let capability = ui.request.capability();
                if !self
                    .manifest
                    .capabilities
                    .contains(&ExtensionCapability::Ui)
                    || !self.manifest.ui_capabilities.contains(&capability)
                    || !self.spec.permissions.allows_ui(capability)
                {
                    return self
                        .send_runtime_failure(
                            id,
                            "permission_denied",
                            format!("UI capability {capability:?} was not granted"),
                        )
                        .await;
                }
                if ui.request.is_interactive() && !self.options.mode.has_ui() {
                    return self
                        .send_runtime_failure(
                            id,
                            "ui_unavailable",
                            format!("interactive UI is unavailable in {:?} mode", self.options.mode),
                        )
                        .await;
                }
                let Some(ui_host) = self.ui.clone() else {
                    return self
                        .send_runtime_failure(
                            id,
                            "ui_unavailable",
                            "the current host has no extension UI adapter",
                        )
                        .await;
                };
                let cancellation = ExtensionCancellation::new();
                self.inbound
                    .lock()
                    .insert(id.clone(), cancellation.clone());
                let instance = self.clone();
                tokio::spawn(async move {
                    let request = ui.request;
                    let request_for_validation = request.clone();
                    let requested_timeout = ui.timeout_ms.map(|milliseconds| {
                        Duration::from_millis(milliseconds.min(MAX_REQUEST_TIMEOUT_MS))
                    });
                    let result = if let Some(deadline) = requested_timeout {
                        tokio::select! {
                            result = ui_host.request(
                                ExtensionUiContext {
                                    instance: instance.id.clone(),
                                    mode: instance.options.mode,
                                },
                                request,
                                cancellation.clone(),
                            ) => result,
                            () = cancellation.cancelled() => Err(anyhow!("UI request was cancelled")),
                            () = instance.invalidation.cancelled() => Err(anyhow!("extension generation was invalidated")),
                            () = sleep(deadline) => Err(anyhow!("UI request timed out")),
                        }
                    } else {
                        tokio::select! {
                            result = ui_host.request(
                                ExtensionUiContext {
                                    instance: instance.id.clone(),
                                    mode: instance.options.mode,
                                },
                                request,
                                cancellation.clone(),
                            ) => result,
                            () = cancellation.cancelled() => Err(anyhow!("UI request was cancelled")),
                            () = instance.invalidation.cancelled() => Err(anyhow!("extension generation was invalidated")),
                        }
                    };
                    instance.inbound.lock().remove(&id);
                    let protocol_result = match result.and_then(|response| {
                        response.validate_for(&request_for_validation)?;
                        serde_json::to_value(response).context("encoding UI response")
                    }) {
                        Ok(value) => ProtocolResult::success(value),
                        Err(error) => ProtocolResult::failure("ui_request_failed", error.to_string()),
                    };
                    if let Err(error) = instance
                        .transport
                        .send(&ExtensionHostFrame::Response {
                            id,
                            result: protocol_result,
                        })
                        .await
                    {
                        instance.mark_crashed(&format!("sending UI response failed: {error}"));
                    }
                });
                Ok(())
            }
            ExtensionRuntimeRequest::Action { action } => {
                if !self
                    .manifest
                    .capabilities
                    .contains(&ExtensionCapability::SessionActions)
                    || !self
                        .spec
                        .permissions
                        .allows(ExtensionCapability::SessionActions)
                {
                    return self
                        .send_runtime_failure(
                            id,
                            "permission_denied",
                            "session action capability was not granted",
                        )
                        .await;
                }
                if let ExtensionRuntimeAction::GetFlag { name } = &action {
                    let result = match self
                        .registrations
                        .flags
                        .iter()
                        .find(|flag| &flag.name == name)
                    {
                        Some(_) => ProtocolResult::failure(
                            "unsupported_extension_flag",
                            format!(
                                "extension flag {name:?} is registered but CLI flag dispatch is unsupported in this startup architecture"
                            ),
                        ),
                        None => ProtocolResult::success(Value::Null),
                    };
                    return self
                        .transport
                        .send(&ExtensionHostFrame::Response { id, result })
                        .await;
                }
                let Some(host) = self.action_host.lock().clone() else {
                    return self
                        .send_runtime_failure(
                            id,
                            "action_unavailable",
                            "the current application has no extension action host",
                        )
                        .await;
                };
                let cancellation = ExtensionCancellation::new();
                self.inbound
                    .lock()
                    .insert(id.clone(), cancellation.clone());
                let instance = self.clone();
                tokio::spawn(async move {
                    let result = tokio::select! {
                        result = host.request(instance.id.clone(), action, cancellation.clone()) => result,
                        () = cancellation.cancelled() => Err(anyhow!("extension action was cancelled")),
                        () = instance.invalidation.cancelled() => Err(anyhow!("extension generation was invalidated")),
                    };
                    instance.inbound.lock().remove(&id);
                    let protocol_result = match result {
                        Ok(value) => ProtocolResult::success(value),
                        Err(error) => ProtocolResult::failure("action_failed", error.to_string()),
                    };
                    if let Err(error) = instance
                        .transport
                        .send(&ExtensionHostFrame::Response { id, result: protocol_result })
                        .await
                    {
                        instance.mark_crashed(&format!("sending action response failed: {error}"));
                    }
                });
                Ok(())
            }
        }
    }

    async fn send_runtime_failure(
        &self,
        id: String,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<()> {
        self.transport
            .send(&ExtensionHostFrame::Response {
                id,
                result: ProtocolResult::failure(code, message),
            })
            .await
    }

    async fn request_value(
        &self,
        request: ExtensionHostRequest,
        request_timeout: Duration,
        cancellation: Option<ExtensionCancellation>,
        abort: Option<AbortSignal>,
        on_update: Option<UpdateHandler>,
    ) -> Result<Value> {
        if !matches!(
            self.phase(),
            ExtensionPhase::Initializing | ExtensionPhase::Active
        ) {
            bail!(
                "extension {} generation {} is not active",
                self.id.extension_id,
                self.id.generation
            );
        }
        let request_id = Uuid::new_v4().to_string();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        self.pending.lock().insert(request_id.clone(), sender);
        if let Err(error) = self
            .transport
            .send(&ExtensionHostFrame::Request {
                id: request_id.clone(),
                generation: self.id.generation,
                request,
            })
            .await
        {
            self.pending.lock().remove(&request_id);
            return Err(with_diagnostics(error, self.transport.as_ref()))
                .context("sending extension request");
        }

        let cancellation_wait = async {
            if let Some(cancellation) = cancellation {
                cancellation.cancelled().await;
            } else {
                pending::<()>().await;
            }
        };
        let abort_wait = async {
            if let Some(abort) = abort {
                abort.cancelled().await;
            } else {
                pending::<()>().await;
            }
        };
        tokio::pin!(cancellation_wait);
        tokio::pin!(abort_wait);
        let deadline = sleep(request_timeout);
        tokio::pin!(deadline);

        let mut must_cancel = false;
        let result = loop {
            tokio::select! {
                event = receiver.recv() => {
                    match event {
                        Some(PendingRequestEvent::Update(value)) => {
                            if let Some(handler) = &on_update {
                                if let Err(error) = handler(value) {
                                    must_cancel = true;
                                    break Err(error);
                                }
                            }
                        }
                        Some(PendingRequestEvent::Finished(result)) => {
                            break result.into_result();
                        }
                        Some(PendingRequestEvent::ConnectionFailed(message)) => {
                            break Err(anyhow!(message));
                        }
                        None => break Err(anyhow!("extension response channel closed")),
                    }
                }
                () = self.invalidation.cancelled() => {
                    must_cancel = true;
                    break Err(anyhow!("extension generation was invalidated"));
                }
                () = &mut cancellation_wait => {
                    must_cancel = true;
                    break Err(anyhow!("extension request was cancelled"));
                }
                () = &mut abort_wait => {
                    must_cancel = true;
                    break Err(anyhow!("extension request was aborted"));
                }
                () = &mut deadline => {
                    must_cancel = true;
                    break Err(anyhow!("extension request timed out after {} ms", request_timeout.as_millis()));
                }
            }
        };
        self.pending.lock().remove(&request_id);
        if must_cancel {
            let _ = self
                .transport
                .send(&ExtensionHostFrame::Cancel { id: request_id })
                .await;
        }
        result.map_err(|error| with_diagnostics(error, self.transport.as_ref()))
    }

    fn invalidate_now(&self, reason: &str) {
        let previous = self
            .phase
            .swap(ExtensionPhase::Invalidated as u8, Ordering::AcqRel);
        if ExtensionPhase::from_byte(previous) == ExtensionPhase::Invalidated {
            return;
        }
        self.invalidation.cancel();
        self.fail_pending(format!(
            "extension {} generation {} was invalidated: {reason}",
            self.id.extension_id, self.id.generation
        ));
        for cancellation in self.inbound.lock().values() {
            cancellation.cancel();
        }
        let _ = self.events.send(ExtensionRuntimeEvent::Invalidated {
            instance: self.id.clone(),
            reason: reason.to_owned(),
        });
    }

    async fn finish_invalidate(&self, reason: &str) {
        let _ = self
            .transport
            .send(&ExtensionHostFrame::Shutdown {
                reason: reason.to_owned(),
            })
            .await;
        let _ = timeout(self.options.shutdown_timeout, self.transport.terminate()).await;
        if let Some(ui) = &self.ui {
            let _ = ui.clear_extension(self.id.clone()).await;
        }
    }

    fn mark_crashed(&self, message: &str) {
        let previous = self
            .phase
            .swap(ExtensionPhase::Crashed as u8, Ordering::AcqRel);
        if matches!(
            ExtensionPhase::from_byte(previous),
            ExtensionPhase::Invalidated | ExtensionPhase::Crashed
        ) {
            return;
        }
        self.invalidation.cancel();
        let contextual = if self.transport.diagnostic_context().is_empty() {
            message.to_owned()
        } else {
            format!(
                "{message}; child stderr: {}",
                self.transport.diagnostic_context()
            )
        };
        self.fail_pending(contextual.clone());
        for cancellation in self.inbound.lock().values() {
            cancellation.cancel();
        }
        let _ = self.events.send(ExtensionRuntimeEvent::Crashed {
            instance: self.id.clone(),
            message: contextual,
        });
    }

    fn fail_pending(&self, message: String) {
        let pending = self.pending.lock().drain().collect::<Vec<_>>();
        for (_, sender) in pending {
            let _ = sender.send(PendingRequestEvent::ConnectionFailed(message.clone()));
        }
    }
}

impl Drop for ExtensionInstance {
    fn drop(&mut self) {
        self.invalidation.cancel();
    }
}

fn validate_request_id(id: &str) -> Result<()> {
    validate_text(id, "request id", 128)
}

fn validate_command_name(name: &str) -> Result<()> {
    validate_identifier(name, "command name")?;
    if name.starts_with('/') {
        bail!("extension command names must not include the leading slash");
    }
    Ok(())
}

fn validate_event_name(name: &str) -> Result<()> {
    validate_text(name, "event name", 128)?;
    if name
        .chars()
        .any(|character| !(character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | ':' | '.')))
    {
        bail!("event name {name:?} contains unsupported characters");
    }
    Ok(())
}

fn validate_identifier(value: &str, field: &str) -> Result<()> {
    validate_text(value, field, 128)?;
    if value
        .chars()
        .any(|character| !(character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')))
    {
        bail!("{field} {value:?} contains unsupported characters");
    }
    Ok(())
}

fn validate_text(value: &str, field: &str, max_len: usize) -> Result<()> {
    if value.is_empty() {
        bail!("{field} must not be empty");
    }
    if value.len() > max_len {
        bail!("{field} exceeds {max_len} bytes");
    }
    if value.chars().any(char::is_control) {
        bail!("{field} contains control characters");
    }
    Ok(())
}

fn extension_candidate_error(report: &ExtensionLoadReport) -> anyhow::Error {
    anyhow!(
        "extension reload candidate rejected {} extension(s): {}",
        report.failures.len(),
        report
            .failures
            .iter()
            .map(|failure| failure.message.as_str())
            .collect::<Vec<_>>()
            .join("; ")
    )
}

fn with_diagnostics(error: anyhow::Error, transport: &dyn ExtensionTransport) -> anyhow::Error {
    let diagnostics = transport.diagnostic_context();
    if diagnostics.is_empty() {
        error
    } else {
        anyhow!("{error}; child stderr: {diagnostics}")
    }
}


#[cfg(test)]
mod tool_capability_tests {
    use super::*;
    use serde_json::json;

    fn tool_registration(capability: Option<&str>) -> Value {
        let mut tool = json!({
            "name": "custom_read",
            "label": "Custom Read",
            "description": "Reads custom data",
            "parameters": { "type": "object", "properties": {} }
        });
        if let Some(capability) = capability {
            tool["capability"] = json!(capability);
        }
        json!({ "kind": "tool", "tool": tool })
    }

    #[test]
    fn extension_tool_capabilities_round_trip_lowercase() {
        for capability in [
            ToolCapability::Read,
            ToolCapability::Write,
            ToolCapability::Exec,
        ] {
            let wire = serde_json::to_value(ExtensionRegistration::Tool {
                tool: ExtensionToolDescriptor {
                    name: "custom".to_owned(),
                    label: "Custom".to_owned(),
                    description: "Custom extension tool".to_owned(),
                    parameters: Schema::default(),
                    capability,
                    execution_mode: ToolExecutionMode::Default,
                    prompt_guidelines: Vec::new(),
                },
            })
            .unwrap();
            assert_eq!(wire["tool"]["capability"], serde_json::to_value(capability).unwrap());
            let decoded: ExtensionRegistration = serde_json::from_value(wire).unwrap();
            let ExtensionRegistration::Tool { tool } = decoded else {
                panic!("expected tool registration");
            };
            assert_eq!(tool.capability, capability);
        }
    }

    #[test]
    fn legacy_extension_tools_default_to_exec_and_invalid_values_fail() {
        let decoded: ExtensionRegistration =
            serde_json::from_value(tool_registration(None)).expect("legacy tool frame");
        let ExtensionRegistration::Tool { tool } = decoded else {
            panic!("expected tool registration");
        };
        assert_eq!(tool.capability, ToolCapability::Exec);
        assert!(serde_json::from_value::<ExtensionRegistration>(tool_registration(Some("network"))).is_err());
    }
}

#[cfg(test)]
mod trust_decision_hook_tests {
    use super::*;
    use serde_json::json;

    /// Registers the `trust_decision` handler (which only loads if the event
    /// is allow-listed) and asserts the payload contract before recommending.
    const TRUST_DECISION_SOURCE: &str = r#"export default function (pi) {
        pi.on("trust_decision", (event) => {
            if (typeof event.path !== "string" || event.path.length === 0) {
                throw new Error("missing path");
            }
            if (!["trusted", "untrusted", "ask"].includes(event.decision)) {
                throw new Error("unexpected decision: " + event.decision);
            }
            if (typeof event.isNew !== "boolean") {
                throw new Error("missing isNew");
            }
            return { approve: event.decision === "ask" };
        });
    }"#;

    async fn trust_decision_runtime(source: &str) -> (ExtensionRuntime, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("extension dir");
        let entry = dir.path().join("trust-decision.mjs");
        std::fs::write(&entry, source).expect("write extension source");
        let permissions = ExtensionPermissionSet {
            capabilities: BTreeSet::from([ExtensionCapability::EventHooks]),
            ui_capabilities: BTreeSet::new(),
        };
        let spec = ExtensionSpec::new_runtime(
            "trust-decision",
            ExtensionSpecRuntime::QuickJs { entry: entry.clone() },
            dir.path(),
            ExtensionOrigin::Project,
            true,
            permissions,
        );
        let runtime = ExtensionRuntime::new(
            Arc::new(QuickJsExtensionHost),
            None,
            ExtensionRuntimeOptions {
                mode: ExtensionMode::Tui,
                hook_timeout: Duration::from_secs(10),
                ..ExtensionRuntimeOptions::default()
            },
        );
        let report = runtime.load(vec![spec]).await;
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        (runtime, dir)
    }

    #[tokio::test]
    async fn trust_decision_event_is_allow_listed_and_delivers_payload() {
        let (runtime, _dir) = trust_decision_runtime(TRUST_DECISION_SOURCE).await;
        // Loading the extension registers `pi.on("trust_decision", ...)`. If
        // the event were missing from the allow-list, the load would fail
        // with "unsupported extension event trust_decision".
        let reduction = runtime
            .reduce_trust_decision(json!({
                "path": "/tmp/project",
                "decision": "ask",
                "isNew": true,
            }))
            .await
            .expect("trust_decision hook runs");
        assert_eq!(
            reduction,
            Some(ExtensionTrustDecisionReduction { approve: true })
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn trust_decision_approval_never_approves_a_denial_or_a_trust() {
        let (runtime, _dir) = trust_decision_runtime(TRUST_DECISION_SOURCE).await;
        // The fixture approves only `ask`; a denial (or an existing trust)
        // must come back without a recommendation so the host cannot weaken it.
        for decision in ["trusted", "untrusted"] {
            let reduction = runtime
                .reduce_trust_decision(json!({
                    "path": "/tmp/project",
                    "decision": decision,
                    "isNew": false,
                }))
                .await
                .expect("trust_decision hook runs");
            assert_eq!(reduction, None, "approval must not apply to {decision}");
        }
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn trust_decision_without_handlers_returns_none() {
        let (runtime, _dir) = trust_decision_runtime("export default function (pi) {}").await;
        let reduction = runtime
            .reduce_trust_decision(json!({
                "path": "/tmp/project",
                "decision": "ask",
                "isNew": true,
            }))
            .await
            .expect("no handlers is fine");
        assert_eq!(reduction, None);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn trust_decision_wire_rejects_unknown_fields() {
        let wire: std::result::Result<TrustDecisionWire, serde_json::Error> =
            serde_json::from_value(json!({
                "approve": true,
                "deny": false,
            }));
        assert!(wire.is_err(), "extensions cannot veto via unknown fields");
        let wire: TrustDecisionWire =
            serde_json::from_value(json!({ "approve": true })).expect("approve wire");
        assert!(wire.approve);
        let wire: TrustDecisionWire = serde_json::from_value(json!({})).expect("empty wire");
        assert!(!wire.approve);
    }
}

#[cfg(test)]
mod context_snapshot_failure_tests {
    use super::*;

    /// An action host whose context snapshot always fails: the invocation
    /// path must surface the host error instead of silently invoking with a
    /// None context.
    struct FailingSnapshotHost;

    impl ExtensionActionHost for FailingSnapshotHost {
        fn context_snapshot(&self) -> ExtensionFuture<'_, Result<ExtensionContextSnapshot>> {
            Box::pin(async { Err(anyhow!("host context snapshot exploded")) })
        }

        fn request(
            &self,
            _instance: ExtensionInstanceId,
            _action: ExtensionRuntimeAction,
            _cancellation: ExtensionCancellation,
        ) -> ExtensionFuture<'_, Result<Value>> {
            Box::pin(async { Ok(Value::Null) })
        }
    }

    #[tokio::test]
    async fn context_snapshot_error_reaches_invocation_caller() {
        let runtime = ExtensionRuntime::new(
            Arc::new(ProcessExtensionHost),
            None,
            ExtensionRuntimeOptions::default(),
        );
        runtime
            .set_action_host(Arc::new(FailingSnapshotHost))
            .expect("action host configured");
        let error = runtime
            .invocation_context()
            .await
            .expect_err("host snapshot errors must reach the caller, not become None");
        // `to_string()` shows only the top-level context; the host's error
        // must survive in the full chain (`{:#}`).
        let message = format!("{error:#}");
        assert!(
            message.contains("host context snapshot exploded"),
            "got: {message}"
        );
        assert!(
            message.contains("capturing extension context snapshot"),
            "got: {message}"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn context_snapshot_without_action_host_is_none() {
        let runtime = ExtensionRuntime::new(
            Arc::new(ProcessExtensionHost),
            None,
            ExtensionRuntimeOptions::default(),
        );
        let context = runtime
            .invocation_context()
            .await
            .expect("missing action host is not an error");
        assert!(context.is_none(), "no action host yields no context");
        runtime.shutdown().await;
    }
}

#[cfg(test)]
mod cleanup_runtime_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// Sets a flag when dropped: proves the runtime-build-failure fallback
    /// drops the future's captured cleanup resources (best-available
    /// synchronous cleanup) instead of panicking the process.
    struct SetOnDrop(Arc<AtomicBool>);

    impl Drop for SetOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn cleanup_runtime_build_failure_drops_resources_without_panic() {
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = SetOnDrop(dropped.clone());
        let cleanup = async move {
            let _kept_alive = guard;
            pending::<()>().await
        };
        run_cleanup_future(cleanup, || {
            Err(std::io::Error::new(
                std::io::ErrorKind::OutOfMemory,
                "injected runtime exhaustion",
            ))
        });
        assert!(
            dropped.load(Ordering::SeqCst),
            "captured cleanup resources must drop on runtime-build failure"
        );
    }

    #[test]
    fn cleanup_runtime_build_success_runs_future_on_fresh_thread() {
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        let cleanup = async move { flag.store(true, Ordering::SeqCst) };
        run_cleanup_future(cleanup, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !ran.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            ran.load(Ordering::SeqCst),
            "cleanup future must run on the fresh runtime"
        );
    }
}