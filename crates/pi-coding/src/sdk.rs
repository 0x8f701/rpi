use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use pi_agent::{AfterToolCallFn, AgentTool, BeforeToolCallFn, StreamFn, ThinkingLevel};
use pi_ai::{Model, SimpleStreamOptions};

use crate::{
    Application, ExtensionLoadReport, ExtensionPermissionSet, ExtensionRuntime, ResourceDiscovery,
    ResourceManager, ResourceManagerOptions, Session, SessionAuthResolver, SessionOptions,
    SessionRecorder, Settings, ToolSelection,
};

#[derive(Clone)]
pub struct PreparedExtensionRuntime {
    pub runtime: ExtensionRuntime,
    pub permissions: ExtensionPermissionSet,
    pub load_report: Option<ExtensionLoadReport>,
}

impl PreparedExtensionRuntime {
    #[must_use]
    pub fn new(runtime: ExtensionRuntime, permissions: ExtensionPermissionSet) -> Self {
        Self {
            runtime,
            permissions,
            load_report: None,
        }
    }

    #[must_use]
    pub fn with_load_report(mut self, report: ExtensionLoadReport) -> Self {
        self.load_report = Some(report);
        self
    }
}

#[derive(Clone)]
pub struct AgentSession {
    pub session: Session,
    pub application: Application,
    pub extensions_result: Option<ExtensionLoadReport>,
    pub model_fallback_message: Option<String>,
}

pub struct AgentSessionBuilder {
    model: Model,
    cwd: PathBuf,
    system_prompt: String,
    thinking_level: ThinkingLevel,
    api_key: String,
    compaction: Option<crate::CompactionSettings>,
    stream_options: SimpleStreamOptions,
    tools: Option<Vec<AgentTool>>,
    additional_tools: Vec<AgentTool>,
    before_tool_call: Option<BeforeToolCallFn>,
    after_tool_call: Option<AfterToolCallFn>,
    stream_fn: Option<StreamFn>,
    auth_resolver: Option<SessionAuthResolver>,
    resources: Option<ResourceManager>,
    recorder: Option<SessionRecorder>,
    restore_recorder: bool,
    prefer_saved_model: bool,
    thinking_level_overridden: bool,
    settings: Option<Settings>,
    extensions: Option<PreparedExtensionRuntime>,
    tool_selection: ToolSelection,
    resource_discovery: ResourceDiscovery,
}

impl AgentSessionBuilder {
    #[must_use]
    pub fn new(model: Model, cwd: impl Into<PathBuf>) -> Self {
        Self {
            model,
            cwd: cwd.into(),
            system_prompt: String::new(),
            thinking_level: crate::DEFAULT_THINKING_LEVEL,
            api_key: String::new(),
            compaction: Some(crate::DEFAULT_COMPACTION_SETTINGS),
            stream_options: SimpleStreamOptions::default(),
            tools: None,
            additional_tools: Vec::new(),
            before_tool_call: None,
            after_tool_call: None,
            stream_fn: None,
            auth_resolver: None,
            resources: None,
            recorder: None,
            restore_recorder: false,
            prefer_saved_model: true,
            thinking_level_overridden: false,
            settings: None,
            extensions: None,
            tool_selection: ToolSelection::default(),
            resource_discovery: ResourceDiscovery::Disabled,
        }
    }

    #[must_use]
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    #[must_use]
    pub fn thinking_level(mut self, level: ThinkingLevel) -> Self {
        self.thinking_level = level;
        self.thinking_level_overridden = true;
        self
    }

    #[must_use]
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = api_key.into();
        self
    }

    /// Replaces the fallback model and prevents a resumed recorder from selecting its saved model.
    #[must_use]
    pub fn model_override(mut self, model: Model) -> Self {
        self.model = model;
        self.prefer_saved_model = false;
        self
    }

    #[must_use]
    pub fn compaction(mut self, compaction: Option<crate::CompactionSettings>) -> Self {
        self.compaction = compaction;
        self
    }

    #[must_use]
    pub fn stream_options(mut self, options: SimpleStreamOptions) -> Self {
        self.stream_options = options;
        self
    }

    #[must_use]
    pub fn tools(mut self, tools: Vec<AgentTool>) -> Self {
        self.tools = Some(tools);
        self
    }

    #[must_use]
    pub fn additional_tools(mut self, tools: Vec<AgentTool>) -> Self {
        self.additional_tools = tools;
        self
    }

    #[must_use]
    pub fn before_tool_call(mut self, hook: BeforeToolCallFn) -> Self {
        self.before_tool_call = Some(hook);
        self
    }

    #[must_use]
    pub fn after_tool_call(mut self, hook: AfterToolCallFn) -> Self {
        self.after_tool_call = Some(hook);
        self
    }

    #[must_use]
    pub fn stream_fn(mut self, stream_fn: StreamFn) -> Self {
        self.stream_fn = Some(stream_fn);
        self
    }

    #[must_use]
    pub fn auth_resolver(mut self, resolver: SessionAuthResolver) -> Self {
        self.auth_resolver = Some(resolver);
        self
    }

    #[must_use]
    pub fn resource_manager(mut self, resources: ResourceManager) -> Self {
        self.resources = Some(resources);
        self
    }

    /// Attaches a recorder for future entries without hydrating its existing branch.
    #[must_use]
    pub fn recorder(mut self, recorder: SessionRecorder) -> Self {
        self.recorder = Some(recorder);
        self.restore_recorder = false;
        self
    }

    /// Resumes the recorder's active branch, including history and saved session settings.
    #[must_use]
    pub fn resume_recorder(mut self, recorder: SessionRecorder) -> Self {
        self.recorder = Some(recorder);
        self.restore_recorder = true;
        self
    }

    #[must_use]
    pub fn settings(mut self, settings: Settings) -> Self {
        self.settings = Some(settings);
        self
    }

    #[must_use]
    pub fn extensions(mut self, extensions: PreparedExtensionRuntime) -> Self {
        self.extensions = Some(extensions);
        self
    }

    #[must_use]
    pub fn tool_selection(mut self, selection: ToolSelection) -> Self {
        self.tool_selection = selection;
        self
    }

    #[must_use]
    pub fn resource_discovery(mut self, discovery: ResourceDiscovery) -> Self {
        self.resource_discovery = discovery;
        self
    }

    pub fn discover_resources(mut self, options: ResourceManagerOptions) -> Result<Self> {
        self.resource_discovery = if options.project_trust_override == Some(false) {
            ResourceDiscovery::Global
        } else {
            ResourceDiscovery::TrustedProject
        };
        self.resources = Some(ResourceManager::new(options)?);
        Ok(self)
    }

    pub async fn build(mut self) -> Result<AgentSession> {
        let resume_state = if self.restore_recorder {
            let recorder = self
                .recorder
                .as_ref()
                .expect("resume recorder is set with its restore flag");
            let tree = recorder.tree()?;
            validate_resume_cwd(&self.cwd, &tree.header.cwd)?;
            if self.stream_options.stream.session_id.is_none() {
                self.stream_options.stream.session_id = Some(recorder.id());
            }
            let has_thinking_entry = tree.has_thinking_entry();
            Some((tree.build_context(None), has_thinking_entry))
        } else {
            None
        };
        let extensions_result = self
            .extensions
            .as_ref()
            .and_then(|prepared| prepared.load_report.clone());
        let extension_tools = self
            .extensions
            .as_ref()
            .map_or_else(Vec::new, |prepared| prepared.runtime.agent_tools());
        let mut additional_tools = self.additional_tools;
        additional_tools.extend(extension_tools);

        let mut options = SessionOptions {
            model: self.model,
            cwd: self.cwd,
            system_prompt: self.system_prompt,
            thinking_level: self.thinking_level,
            api_key: self.api_key,
            compaction: self.compaction,
            stream_options: self.stream_options,
            tools: self.tools,
            before_tool_call: self.before_tool_call,
            after_tool_call: self.after_tool_call,
            stream_fn: self.stream_fn,
            auth_resolver: self.auth_resolver,
        };
        let todo_enabled = self
            .settings
            .as_ref()
            .is_some_and(Settings::todo_tool_enabled);
        if let Some(settings) = &self.settings {
            settings.apply_session_options(&mut options)?;
            if !self.thinking_level_overridden
                && let Some(level) = settings.default_thinking_level
            {
                options.thinking_level = level;
            }
            if settings.process_tool_enabled() {
                self.tool_selection.enable_process = true;
            }
        }

        let mut model_fallback_message = None;
        if let Some((context, _)) = resume_state.as_ref()
            && !context.messages.is_empty()
        {
            if self.prefer_saved_model
                && let (Some(provider), Some(model_id)) =
                    (context.provider.as_deref(), context.model_id.as_deref())
                && (options.model.provider != provider || options.model.id != model_id)
            {
                let restored = restore_recorded_model(
                    provider,
                    model_id,
                    options.auth_resolver.clone(),
                )
                .await;
                if let Some((model, api_key)) = restored {
                    options.model = model;
                    options.api_key = api_key;
                } else {
                    model_fallback_message = Some(format!(
                        "Could not restore model {provider}/{model_id}. Using {}/{}",
                        options.model.provider, options.model.id
                    ));
                }
            }
            if !self.thinking_level_overridden
                && let Some((_, true)) = resume_state.as_ref()
            {
                options.thinking_level = parse_recorded_thinking_level(&context.thinking_level);
            }
        }

        let session = if todo_enabled {
            Session::new_with_todo_and_additional_tools_filtered_and_discovery(
                options,
                additional_tools,
                self.tool_selection,
                self.resource_discovery,
            )?
        } else {
            Session::new_with_additional_tools_filtered_and_discovery(
                options,
                additional_tools,
                self.tool_selection,
                self.resource_discovery,
            )?
        };
        if let Some(resources) = self.resources {
            session.attach_resources(resources).await?;
        }
        if let Some((context, has_thinking_entry)) = resume_state
            && !context.messages.is_empty()
        {
            session.load_history(context.messages).await?;
            if !has_thinking_entry
                && let Some(recorder) = self.recorder.as_ref()
            {
                recorder.record_thinking_level(thinking_level_name(session.thinking_level()))?;
            }
        }
        if let Some(recorder) = self.recorder {
            session.record(recorder);
        }
        let application = match self.extensions {
            Some(prepared) => {
                Application::new_with_extensions(
                    session.clone(),
                    prepared.runtime,
                    prepared.permissions,
                )
                .await
            }
            None => Application::new(session.clone()).await,
        };
        Ok(AgentSession {
            session,
            application,
            extensions_result,
            model_fallback_message,
        })
    }
}

async fn restore_recorded_model(
    provider: &str,
    model_id: &str,
    auth_resolver: Option<SessionAuthResolver>,
) -> Option<(Model, String)> {
    let model = pi_ai::get_model(provider, model_id)?;
    let resolver = auth_resolver?;
    let auth = resolver(model.clone()).await.ok()?;
    pi_ai::model_is_available_for_credential(&model, auth.available_model_ids.as_deref())
        .then_some((model, auth.api_key))
}

fn validate_resume_cwd(requested: &Path, recorded: &Path) -> Result<()> {
    if recorded.as_os_str().is_empty() {
        return Ok(());
    }
    let requested = if requested.as_os_str().is_empty() {
        std::env::current_dir()?
    } else {
        requested.to_path_buf()
    };
    let requested = requested.canonicalize().unwrap_or(requested);
    let recorded = recorded
        .canonicalize()
        .unwrap_or_else(|_| recorded.to_path_buf());
    if requested != recorded {
        bail!(
            "session working directory {} does not match {}",
            recorded.display(),
            requested.display()
        );
    }
    Ok(())
}

fn parse_recorded_thinking_level(level: &str) -> ThinkingLevel {
    match level {
        "minimal" => ThinkingLevel::Minimal,
        "low" => ThinkingLevel::Low,
        "medium" => ThinkingLevel::Medium,
        "high" => ThinkingLevel::High,
        "xhigh" | "max" => ThinkingLevel::Xhigh,
        _ => ThinkingLevel::Off,
    }
}

fn thinking_level_name(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh => "xhigh",
    }
}

pub async fn create_agent_session(builder: AgentSessionBuilder) -> Result<AgentSession> {
    builder.build().await
}

#[must_use]
pub fn discover_resource_options(cwd: impl AsRef<Path>) -> ResourceManagerOptions {
    ResourceManagerOptions::new(cwd.as_ref())
}
