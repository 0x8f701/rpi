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
use pi_agent::{AbortSignal, AgentTool, AgentToolResult, ThinkingLevel, ToolExecutionMode};
use pi_ai::{ContentBlock, CustomMessageContent, Message, Model, Schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex as AsyncMutex, OwnedMutexGuard, broadcast, mpsc},
    task::JoinHandle,
    time::{sleep, timeout},
};
use uuid::Uuid;

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
    SessionActions,
    Ui,
}

impl ExtensionCapability {
    pub const ALL: [Self; 7] = [
        Self::Commands,
        Self::Tools,
        Self::EventHooks,
        Self::MessageRenderers,
        Self::ProviderMetadata,
        Self::SessionActions,
        Self::Ui,
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
}

impl ExtensionUiCapability {
    pub const ALL: [Self; 9] = [
        Self::Select,
        Self::Confirm,
        Self::Input,
        Self::Editor,
        Self::Notify,
        Self::Status,
        Self::Widget,
        Self::Title,
        Self::SetEditorText,
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
const BUN_EXTENSION_HOST_SOURCE: &str = include_str!("bun_extension_host.mjs");

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ManifestRuntimeTag {
    Process,
    Bun,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtensionManifestRuntime {
    Process {
        executable: PathBuf,
        arguments: Vec<String>,
    },
    Bun {
        entry: PathBuf,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessExtensionManifest {
    pub schema_version: u32,
    pub id: String,
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
            ManifestRuntimeTag::Bun => {
                if wire.executable.is_some() {
                    return Err(serde::de::Error::custom(
                        "Bun extension manifest must not set executable",
                    ));
                }
                if !wire.arguments.is_empty() {
                    return Err(serde::de::Error::custom(
                        "Bun extension manifest does not support arguments",
                    ));
                }
                let entry = wire.entry.ok_or_else(|| {
                    serde::de::Error::custom("Bun extension manifest requires entry")
                })?;
                ExtensionManifestRuntime::Bun { entry }
            }
        };
        Ok(Self {
            schema_version: wire.schema_version,
            id: wire.id,
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
            ExtensionManifestRuntime::Bun { entry } => (
                ManifestRuntimeTag::Bun,
                None,
                Some(entry.clone()),
                Vec::new(),
            ),
        };
        ProcessExtensionManifestWire {
            schema_version: self.schema_version,
            id: self.id.clone(),
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
    fn validate(&self, manifest_path: &Path) -> Result<()> {
        if self.schema_version != PROCESS_EXTENSION_MANIFEST_VERSION {
            bail!(
                "unsupported process extension manifest version {} in {}; expected {}",
                self.schema_version,
                manifest_path.display(),
                PROCESS_EXTENSION_MANIFEST_VERSION
            );
        }
        validate_identifier(&self.id, "process extension manifest id")?;
        let configured_path = match &self.runtime {
            ExtensionManifestRuntime::Process { executable, .. } => executable,
            ExtensionManifestRuntime::Bun { entry } => entry,
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
        if matches!(self.runtime, ExtensionManifestRuntime::Bun { .. }) {
            if self
                .capabilities
                .contains(&ExtensionCapability::MessageRenderers)
            {
                bail!("Bun extensions do not support the message_renderers capability");
            }
            if self
                .capabilities
                .contains(&ExtensionCapability::ProviderMetadata)
            {
                bail!("Bun extensions do not support the provider_metadata capability");
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
        ExtensionManifestRuntime::Bun { entry } => {
            validate_bun_entry_extension(&entry)?;
            (
                ExtensionSpecRuntime::Bun {
                    entry: resolve_manifest_path(root, &entry, "Bun extension entry")?,
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
fn validate_bun_entry_extension(entry: &Path) -> Result<()> {
    let supported = entry
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| matches!(extension, "ts" | "js" | "mjs" | "cjs"));
    if !supported {
        bail!("Bun extension entry must end in .ts, .js, .mjs, or .cjs");
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
    Bun { entry: PathBuf },
}

impl ExtensionSpecRuntime {
    fn path(&self) -> &Path {
        match self {
            Self::Process { executable } => executable,
            Self::Bun { entry } => entry,
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

    fn validate_before_launch(&self) -> Result<()> {
        validate_identifier(&self.id, "extension id")?;
        if self.origin == ExtensionOrigin::Project && !self.project_trusted {
            bail!("refusing to execute untrusted project extension {}", self.id);
        }
        if self.working_directory.as_os_str().is_empty() {
            bail!("extension {} has an empty working directory", self.id);
        }
        if matches!(self.runtime, ExtensionSpecRuntime::Bun { .. }) {
            if self
                .permissions
                .capabilities
                .contains(&ExtensionCapability::MessageRenderers)
            {
                bail!("Bun extensions do not support the message_renderers capability");
            }
            if self
                .permissions
                .capabilities
                .contains(&ExtensionCapability::ProviderMetadata)
            {
                bail!("Bun extensions do not support the provider_metadata capability");
            }
        }
        const RESERVED_ENV: [&str; 7] = [
            "PI_EXTENSION_PROTOCOL_VERSION",
            "PI_EXTENSION_ID",
            "PI_EXTENSION_ENTRY",
            "PI_EXTENSION_CAPABILITIES",
            "PI_EXTENSION_UI_CAPABILITIES",
            "PI_EXTENSION_MAX_FRAME_BYTES",
            "PI_BUN_EXECUTABLE",
        ];
        if let Some(name) = self
            .environment
            .keys()
            .find(|name| RESERVED_ENV.contains(&name.as_str()) && name.as_str() != "PI_BUN_EXECUTABLE")
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
}

impl ExtensionRegistration {
    const fn capability(&self) -> ExtensionCapability {
        match self {
            Self::Command { .. } => ExtensionCapability::Commands,
            Self::Tool { .. } => ExtensionCapability::Tools,
            Self::EventHook { .. } => ExtensionCapability::EventHooks,
            Self::MessageRenderer { .. } => ExtensionCapability::MessageRenderers,
            Self::ProviderMetadata { .. } => ExtensionCapability::ProviderMetadata,
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
    RenderMessage {
        request: MessageRenderRequest,
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
    fn success(value: impl Into<Value>) -> Self {
        Self::Success {
            value: value.into(),
        }
    }

    fn failure(code: impl Into<String>, message: impl Into<String>) -> Self {
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

#[derive(Clone, Debug)]
pub struct ExtensionLaunch {
    pub spec: ExtensionSpec,
    pub max_frame_bytes: usize,
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

fn resolve_bun_executable(spec: &ExtensionSpec) -> Option<PathBuf> {
    if let Some(configured) = spec.environment.get("PI_BUN_EXECUTABLE") {
        let configured = PathBuf::from(configured);
        return configured.is_file().then_some(configured);
    }
    if let Some(configured) = std::env::var_os("PI_BUN_EXECUTABLE") {
        let configured = PathBuf::from(configured);
        return configured.is_file().then_some(configured);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(if cfg!(windows) { "bun.exe" } else { "bun" }))
        .find(|candidate| candidate.is_file())
}

fn write_bun_bridge() -> Result<(PathBuf, PathBuf)> {
    let directory = std::env::temp_dir().join(format!(
        "pi-rs-extension-host-v1-{}",
        Uuid::new_v4()
    ));
    std::fs::create_dir(&directory)
        .context("preparing bundled Bun extension host directory")?;
    let path = directory.join("host.mjs");
    if let Err(error) = std::fs::write(&path, BUN_EXTENSION_HOST_SOURCE) {
        let _ = std::fs::remove_dir_all(&directory);
        return Err(error).context("writing bundled Bun extension host");
    }
    Ok((path, directory))
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
            let mut child_environment = spec.environment.clone();
            child_environment.remove("PI_BUN_EXECUTABLE");
            let working_directory = spec
                .working_directory
                .canonicalize()
                .with_context(|| format!("resolving working directory for extension {}", spec.id))?;
            let (mut command, cleanup_directory) = match &spec.runtime {
                ExtensionSpecRuntime::Process { executable } => {
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
                    let mut command = Command::new(executable);
                    command.env_clear();
                    command.args(&spec.arguments);
                    (command, None)
                }
                ExtensionSpecRuntime::Bun { entry } => {
                    validate_bun_entry_extension(entry)?;
                    let entry = entry.canonicalize().context("resolving Bun extension entry")?;
                    if !entry.starts_with(&working_directory) || !entry.is_file() {
                        bail!("Bun extension entry must remain inside its working directory");
                    }
                    let bun = resolve_bun_executable(&spec).ok_or_else(|| {
                        anyhow!(
                            "Bun runtime is unavailable; install Bun or set PI_BUN_EXECUTABLE"
                        )
                    })?;
                    let (bridge, directory) = write_bun_bridge()?;
                    let mut command = Command::new(bun);
                    command.env_clear();
                    command
                        .arg("run")
                        .arg(&bridge)
                        .env("PI_EXTENSION_ENTRY", &entry)
                        .env(
                            "PI_EXTENSION_CAPABILITIES",
                            serde_json::to_string(&spec.permissions.capabilities)
                                .context("encoding Bun extension capabilities")?,
                        )
                        .env(
                            "PI_EXTENSION_UI_CAPABILITIES",
                            serde_json::to_string(&spec.permissions.ui_capabilities)
                                .context("encoding Bun extension UI capabilities")?,
                        )
                        .env(
                            "PI_EXTENSION_MAX_FRAME_BYTES",
                            launch.max_frame_bytes.max(1024).to_string(),
                        );
                    (command, Some(directory))
                }
            };
            command
                .current_dir(&working_directory)
                .envs(&child_environment)
                .env(
                    "PI_EXTENSION_PROTOCOL_VERSION",
                    EXTENSION_PROTOCOL_VERSION.to_string(),
                )
                .env("PI_EXTENSION_ID", &spec.id)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                command.as_std_mut().process_group(0);
            }
            let mut child = command
                .spawn()
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
                cleanup_directory,
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

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtensionResourcePaths {
    pub skill_paths: Vec<String>,
    pub prompt_paths: Vec<String>,
    pub theme_paths: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtensionProjectTrustDecision { Yes, No, Undecided }

#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionProjectTrustReduction {
    pub trusted: ExtensionProjectTrustDecision,
    pub remember: bool,
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResourcesDiscoverWire {
    #[serde(default)] skill_paths: Vec<String>,
    #[serde(default)] prompt_paths: Vec<String>,
    #[serde(default)] theme_paths: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectTrustWire {
    trusted: ExtensionProjectTrustDecision,
    #[serde(default)] remember: bool,
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
            }),
        }
    }

    pub fn set_action_host(&self, host: Arc<dyn ExtensionActionHost>) -> Result<()> {
        let mut current = self.inner.action_host.lock();
        if current.is_some() {
            bail!("extension action host is already configured");
        }
        *current = Some(host);
        Ok(())
    }

    async fn context_snapshot(&self) -> Option<ExtensionContextSnapshot> {
        let host = self.inner.action_host.lock().clone()?;
        host.context_snapshot().await.ok()
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
                    message: error.to_string(),
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
        let mut staged = Vec::new();
        let mut failures = Vec::new();
        for spec in specs {
            let extension_id = spec.id.clone();
            let path = spec.executable.clone();
            match self.load_one(spec, next_generation).await {
                Ok(instance) => staged.push(instance),
                Err(error) => {
                    let message = error.to_string();
                    failures.push(ExtensionLoadFailure {
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
        let transport = self
            .inner
            .host
            .launch(ExtensionLaunch {
                spec: spec.clone(),
                max_frame_bytes: self.inner.options.max_frame_bytes,
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
                    context: self.context_snapshot().await,
                },
                timeout_override.unwrap_or(self.inner.options.invocation_timeout),
                cancellation,
                None,
                None,
            )
            .await
            .map_err(|error| anyhow!("running extension command {name}: {error:#}"))
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
                    context: self.context_snapshot().await,
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

    #[must_use]
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
                    context: self.context_snapshot().await,
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
                context: self.context_snapshot().await,
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
                context: self.context_snapshot().await,
            }, self.inner.options.hook_timeout, None, None, None).await
                .with_context(|| format!("extension {} generation {} failed {name} hook", instance.id.extension_id, instance.id.generation))?;
            if !result.is_null() && apply(state, result)
                .with_context(|| format!("extension {} generation {} returned invalid {name} hook result", instance.id.extension_id, instance.id.generation))? {
                break;
            }
        }
        Ok(())
    }


    pub async fn reduce_before_agent_start(&self, mut event: Value, system_prompt: String) -> Result<ExtensionBeforeAgentStartReduction> {
        let mut reduction = ExtensionBeforeAgentStartReduction { system_prompt, messages: Vec::new() };
        self.reduce_event("before_agent_start", &mut reduction, |state| {
            event.as_object_mut().ok_or_else(|| anyhow!("before_agent_start event data must be an object"))?.insert("systemPrompt".to_owned(), Value::String(state.system_prompt.clone()));
            Ok(event.clone())
        }, |state, value| {
            let wire: BeforeAgentStartWire = serde_json::from_value(value)?;
            if let Some(system_prompt) = wire.system_prompt { state.system_prompt = system_prompt; }
            if wire.messages.is_empty() {
                if let Some(message) = wire.message { state.messages.push(message); }
            } else {
                state.messages.extend(wire.messages);
            }
            Ok(())
        }).await?;
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
    pub async fn reduce_project_trust(&self, event: Value) -> Result<Option<ExtensionProjectTrustReduction>> {
        let mut decision = None;
        self.reduce_event_until("project_trust", &mut decision, |_| Ok(event.clone()), |decision, value| {
            let wire: ProjectTrustWire = serde_json::from_value(value)?;
            let decided = wire.trusted != ExtensionProjectTrustDecision::Undecided;
            if decided { *decision = Some(ExtensionProjectTrustReduction { trusted: wire.trusted, remember: wire.remember }); }
            Ok(decided)
        }).await?;
        Ok(decision)
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


    pub async fn reduce_resources_discover(&self, event: Value) -> Result<ExtensionResourcePaths> {
        let mut paths = ExtensionResourcePaths::default();
        self.reduce_event("resources_discover", &mut paths, |_| Ok(event.clone()), |paths, value| {
            let wire: ResourcesDiscoverWire = serde_json::from_value(value)?;
            paths.skill_paths.extend(wire.skill_paths);
            paths.prompt_paths.extend(wire.prompt_paths);
            paths.theme_paths.extend(wire.theme_paths);
            Ok(())
        }).await?;
        Ok(paths)
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
            let result = instance
                .request_value(
                    ExtensionHostRequest::Invoke {
                        invocation: ExtensionInvocation::Event {
                            event: event.clone(),
                        },
                        context: self.context_snapshot().await,
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
        for instance in instances {
            instance.finish_invalidate(reason).await;
        }
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
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(cleanup);
    } else {
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("building extension cleanup runtime");
            runtime.block_on(cleanup);
        });
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

#[derive(Clone, Default)]
struct ExtensionRegistrations {
    commands: Vec<ExtensionCommandDescriptor>,
    tools: Vec<ExtensionToolDescriptor>,
    hooks: Vec<ExtensionEventHookDescriptor>,
    renderers: Vec<ExtensionMessageRendererDescriptor>,
    providers: Vec<ExtensionProviderMetadata>,
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
        }
        Ok(())
    }

    fn finish(self) -> Result<ExtensionRegistrations> {
        Ok(self.registrations)
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
