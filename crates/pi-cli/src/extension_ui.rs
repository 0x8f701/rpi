use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::Arc,
};

use anyhow::{Result, anyhow};
use parking_lot::Mutex;
use pi_coding::{
    ExtensionCancellation, ExtensionFuture, ExtensionInstanceId, ExtensionThemeDescriptor,
    ExtensionUiContext, ExtensionUiHost, ExtensionUiRequest, ExtensionUiResponse,
    UiNotificationLevel, UiWidgetPlacement, WorkingIndicatorOptions,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, oneshot};
use uuid::Uuid;

const UI_EVENT_BUFFER: usize = 256;
const MAX_RETAINED_NOTIFICATIONS: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionUiInteraction {
    pub id: String,
    pub context: ExtensionUiContext,
    pub request: ExtensionUiRequest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionStatusItem {
    pub instance: ExtensionInstanceId,
    pub key: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionWidgetItem {
    pub instance: ExtensionInstanceId,
    pub key: String,
    pub lines: Vec<String>,
    pub placement: UiWidgetPlacement,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionNotification {
    pub instance: ExtensionInstanceId,
    pub message: String,
    pub level: UiNotificationLevel,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionUiSnapshot {
    #[serde(default)]
    pub statuses: Vec<ExtensionStatusItem>,
    #[serde(default)]
    pub widgets: Vec<ExtensionWidgetItem>,
    #[serde(default)]
    pub notifications: Vec<ExtensionNotification>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub editor_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_message: Option<String>,
    #[serde(default)]
    pub working_visible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_indicator: Option<WorkingIndicatorOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden_thinking_label: Option<String>,
    #[serde(default)]
    pub themes: Vec<ExtensionThemeDescriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_theme: Option<String>,
    #[serde(default)]
    pub tools_expanded: bool,
    /// Latest sanitized content rows per `(instance, overlay id)`, as pushed
    /// by `ctx.overlay.setRows`. The TUI reads these when opening an overlay.
    #[serde(default)]
    pub overlay_rows: Vec<ExtensionOverlayRowItem>,
}

/// One overlay's sanitized content rows, keyed by owning instance + overlay
/// id. Serialized for RPC snapshots; the TUI consumes it directly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionOverlayRowItem {
    pub instance: ExtensionInstanceId,
    pub id: String,
    pub rows: Vec<pi_coding::OverlayRow>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExtensionUiEvent {
    InteractionRequested {
        interaction: ExtensionUiInteraction,
    },
    Notification {
        notification: ExtensionNotification,
    },
    StatusChanged {
        item: ExtensionStatusItem,
    },
    StatusCleared {
        instance: ExtensionInstanceId,
        key: String,
    },
    WidgetChanged {
        item: ExtensionWidgetItem,
    },
    WidgetCleared {
        instance: ExtensionInstanceId,
        key: String,
    },
    TitleChanged {
        instance: ExtensionInstanceId,
        title: String,
    },
    EditorTextChanged {
        instance: ExtensionInstanceId,
        text: String,
    },
    WorkingMessageChanged {
        instance: ExtensionInstanceId,
        message: Option<String>,
    },
    WorkingVisibilityChanged {
        instance: ExtensionInstanceId,
        visible: bool,
    },
    WorkingIndicatorChanged {
        instance: ExtensionInstanceId,
        options: Option<WorkingIndicatorOptions>,
    },
    HiddenThinkingLabelChanged {
        instance: ExtensionInstanceId,
        label: Option<String>,
    },
    ThemeChanged {
        instance: ExtensionInstanceId,
        name: String,
    },
    ToolsExpandedChanged {
        instance: ExtensionInstanceId,
        expanded: bool,
    },
    /// `ctx.overlay.open(id, { nonCapturing? })` succeeded: the host UI
    /// should open the overlay panel with the given (sanitized) content rows
    /// and the registered title. `nonCapturing` opens the overlay unfocused
    /// (drawn but not capturing keys; the focus-toggle action flips focus to
    /// it). `input` is the registration-time static editor declaration.
    OverlayOpenRequested {
        instance: ExtensionInstanceId,
        id: String,
        title: String,
        rows: Vec<pi_coding::OverlayRow>,
        non_capturing: bool,
        input: Option<pi_coding::OverlayInputDeclaration>,
    },
    /// `ctx.overlay.setRows(id, rows)` published the sanitized content rows
    /// for `(instance, id)`. The TUI applies them to the currently open
    /// overlay with the same `(instance, id)` (live repaint) and ignores them
    /// otherwise; the rows also land in the adapter state for the next open.
    OverlayRowsChanged {
        instance: ExtensionInstanceId,
        id: String,
        rows: Vec<pi_coding::OverlayRow>,
    },
    ExtensionCleared {
        instance: ExtensionInstanceId,
    },
}

#[derive(Clone, Default)]
pub struct NonInteractiveExtensionUiHost {
    adapter: ExtensionUiAdapter,
}

impl NonInteractiveExtensionUiHost {
    #[must_use]
    pub fn snapshot(&self) -> ExtensionUiSnapshot {
        self.adapter.snapshot()
    }
}

#[derive(Clone)]
pub struct ExtensionUiAdapter {
    inner: Arc<AdapterInner>,
}

struct AdapterInner {
    state: Mutex<AdapterState>,
    pending: Mutex<HashMap<String, PendingInteraction>>,
    events: broadcast::Sender<ExtensionUiEvent>,
    observer_events: broadcast::Sender<ExtensionUiEvent>,
}

#[derive(Default)]
struct AdapterState {
    statuses: BTreeMap<(ExtensionInstanceId, String), String>,
    widgets: BTreeMap<(ExtensionInstanceId, String), ExtensionWidgetItem>,
    notifications: VecDeque<ExtensionNotification>,
    title: Option<(ExtensionInstanceId, String)>,
    editor_text: Option<(ExtensionInstanceId, String)>,
    working_message: Option<(ExtensionInstanceId, Option<String>)>,
    working_visible: Option<(ExtensionInstanceId, bool)>,
    working_indicator: Option<(ExtensionInstanceId, Option<WorkingIndicatorOptions>)>,
    hidden_thinking_label: Option<(ExtensionInstanceId, Option<String>)>,
    themes: Vec<ExtensionThemeDescriptor>,
    active_theme: Option<(ExtensionInstanceId, String)>,
    tools_expanded: Option<(ExtensionInstanceId, bool)>,
    overlay_rows: BTreeMap<(ExtensionInstanceId, String), Vec<pi_coding::OverlayRow>>,
    canonical_queries_supported: bool,
}

struct PendingInteraction {
    context: ExtensionUiContext,
    request: ExtensionUiRequest,
    response: oneshot::Sender<Result<ExtensionUiResponse, String>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostToolConfirmation {
    Approved,
    Denied,
    Cancelled,
}

impl ExtensionUiAdapter {
    #[must_use]
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(UI_EVENT_BUFFER);
        let (observer_events, _) = broadcast::channel(UI_EVENT_BUFFER);
        let state = AdapterState {
            canonical_queries_supported: true,
            ..AdapterState::default()
        };
        Self {
            inner: Arc::new(AdapterInner {
                state: Mutex::new(state),
                pending: Mutex::new(HashMap::new()),
                events,
                observer_events,
            }),
        }
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<ExtensionUiEvent> {
        self.inner.events.subscribe()
    }

    /// Subscribes to extension UI state changes without claiming ownership of
    /// interactive requests. This stream never emits `InteractionRequested`.
    #[must_use]
    pub fn subscribe_non_interactive(&self) -> broadcast::Receiver<ExtensionUiEvent> {
        self.inner.observer_events.subscribe()
    }

    #[must_use]
    pub fn snapshot(&self) -> ExtensionUiSnapshot {
        let state = self.inner.state.lock();
        ExtensionUiSnapshot {
            statuses: state
                .statuses
                .iter()
                .map(|((instance, key), text)| ExtensionStatusItem {
                    instance: instance.clone(),
                    key: key.clone(),
                    text: text.clone(),
                })
                .collect(),
            widgets: state.widgets.values().cloned().collect(),
            notifications: state.notifications.iter().cloned().collect(),
            title: state.title.as_ref().map(|(_, title)| title.clone()),
            editor_text: state
                .editor_text
                .as_ref()
                .map_or_else(String::new, |(_, text)| text.clone()),
            working_message: state
                .working_message
                .as_ref()
                .and_then(|(_, message)| message.clone()),
            working_visible: state
                .working_visible
                .as_ref()
                .is_some_and(|(_, visible)| *visible),
            working_indicator: state
                .working_indicator
                .as_ref()
                .and_then(|(_, options)| options.clone()),
            hidden_thinking_label: state
                .hidden_thinking_label
                .as_ref()
                .and_then(|(_, label)| label.clone()),
            themes: state.themes.clone(),
            active_theme: state.active_theme.as_ref().map(|(_, name)| name.clone()),
            tools_expanded: state
                .tools_expanded
                .as_ref()
                .is_some_and(|(_, expanded)| *expanded),
            overlay_rows: state
                .overlay_rows
                .iter()
                .map(|((instance, id), rows)| ExtensionOverlayRowItem {
                    instance: instance.clone(),
                    id: id.clone(),
                    rows: rows.clone(),
                })
                .collect(),
        }
    }

    /// Replaces the canonical theme catalog supplied by the real host.
    pub fn set_themes(&self, themes: Vec<ExtensionThemeDescriptor>) {
        self.inner.state.lock().themes = themes;
    }

    /// Sets the canonical active theme supplied by the real host. Extension
    /// writes remain owner-scoped so cleanup cannot erase another extension's
    /// later theme selection.
    pub fn set_active_theme(&self, name: Option<String>) {
        let mut state = self.inner.state.lock();
        state.active_theme = name.map(|name| (host_instance(), name));
    }

    /// Publishes the host editor buffer so `GetEditorText` reads authoritative
    /// TUI state instead of a shadow default.
    pub fn set_host_editor_text(&self, text: impl Into<String>) {
        let mut state = self.inner.state.lock();
        state.editor_text = Some((host_instance(), text.into()));
    }

    /// Publishes the host tool-expansion flag so `GetToolsExpanded` reads the
    /// live TUI reducer value.
    pub fn set_host_tools_expanded(&self, expanded: bool) {
        let mut state = self.inner.state.lock();
        state.tools_expanded = Some((host_instance(), expanded));
    }

    /// Controls whether queries can be answered from state bound to the real
    /// host. RPC disables this because its adapter otherwise only sees its own
    /// extension-originated events.
    pub fn set_canonical_queries_supported(&self, supported: bool) {
        self.inner.state.lock().canonical_queries_supported = supported;
    }

    pub async fn confirm_host_tool(
        &self,
        mode: pi_coding::ExtensionMode,
        tool_name: &str,
        capability: pi_agent::ToolCapability,
    ) -> Result<HostToolConfirmation> {
        let response = self
            .request(
                ExtensionUiContext {
                    instance: host_instance(),
                    mode,
                },
                ExtensionUiRequest::Confirm {
                    title: "Approve tool call?".to_owned(),
                    message: format!("Tool: {tool_name}\nCapability: {}", capability_name(capability)),
                },
                ExtensionCancellation::new(),
            )
            .await?;
        match response {
            ExtensionUiResponse::Confirmed { confirmed: true } => Ok(HostToolConfirmation::Approved),
            ExtensionUiResponse::Confirmed { confirmed: false } => Ok(HostToolConfirmation::Denied),
            ExtensionUiResponse::Cancelled => Ok(HostToolConfirmation::Cancelled),
            _ => Err(anyhow!("host tool confirmation returned an invalid response")),
        }
    }

    #[must_use]
    pub fn pending_interactions(&self) -> Vec<ExtensionUiInteraction> {
        let pending = self.inner.pending.lock();
        let mut interactions = pending
            .iter()
            .map(|(id, pending)| ExtensionUiInteraction {
                id: id.clone(),
                context: pending.context.clone(),
                request: pending.request.clone(),
            })
            .collect::<Vec<_>>();
        interactions.sort_by(|left, right| left.id.cmp(&right.id));
        interactions
    }

    pub fn respond(&self, id: &str, response: ExtensionUiResponse) -> Result<()> {
        let pending = self
            .inner
            .pending
            .lock()
            .remove(id)
            .ok_or_else(|| anyhow!("unknown or completed extension UI interaction {id:?}"))?;
        if let Err(error) = response.validate_for(&pending.request) {
            let message = error.to_string();
            let _ = pending.response.send(Err(message.clone()));
            return Err(anyhow!(message));
        }
        pending
            .response
            .send(Ok(response))
            .map_err(|_| anyhow!("extension UI interaction {id:?} was already cancelled"))
    }

    /// Resolves a value-bearing RPC response using the pending request type.
    /// Select, input, and editor interactions share the upstream `{ value }`
    /// wire shape, so correlation must happen before constructing the typed
    /// extension response.
    pub fn respond_value(&self, id: &str, value: String) -> Result<()> {
        let request = self
            .inner
            .pending
            .lock()
            .get(id)
            .map(|pending| pending.request.clone())
            .ok_or_else(|| anyhow!("unknown or completed extension UI interaction {id:?}"))?;
        let response = match request {
            ExtensionUiRequest::Select { .. } => {
                ExtensionUiResponse::Selected { value: Some(value) }
            }
            ExtensionUiRequest::Input { .. } => ExtensionUiResponse::Input { value: Some(value) },
            ExtensionUiRequest::Editor { .. } => ExtensionUiResponse::Edited { value: Some(value) },
            _ => {
                return Err(anyhow!(
                    "UI value response does not match pending request type"
                ));
            }
        };
        self.respond(id, response)
    }

    pub fn respond_confirmed(&self, id: &str, confirmed: bool) -> Result<()> {
        self.respond(id, ExtensionUiResponse::Confirmed { confirmed })
    }

    pub fn cancel(&self, id: &str) -> Result<()> {
        self.respond(id, ExtensionUiResponse::Cancelled)
    }
}

impl Default for ExtensionUiAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtensionUiHost for NonInteractiveExtensionUiHost {
    fn request(
        &self,
        context: ExtensionUiContext,
        request: ExtensionUiRequest,
        cancellation: ExtensionCancellation,
    ) -> ExtensionFuture<'_, Result<ExtensionUiResponse>> {
        if request.is_interactive() {
            return Box::pin(async move {
                Err(anyhow!(
                    "interactive extension UI request {:?} is unavailable in noninteractive mode",
                    request.capability()
                ))
            });
        }
        if matches!(
            request,
            ExtensionUiRequest::GetEditorText
                | ExtensionUiRequest::GetAllThemes
                | ExtensionUiRequest::GetTheme { .. }
                | ExtensionUiRequest::SetTheme { .. }
                | ExtensionUiRequest::GetToolsExpanded
        ) {
            return Box::pin(async move {
                Err(anyhow!(
                    "extension UI request {:?} requires canonical interactive host state",
                    request.capability()
                ))
            });
        }
        self.adapter.request(context, request, cancellation)

    }
    fn clear_extension(&self, instance: ExtensionInstanceId) -> ExtensionFuture<'_, Result<()>> {
        self.adapter.clear_extension(instance)
    }
}

impl ExtensionUiHost for ExtensionUiAdapter {
    fn request(
        &self,
        context: ExtensionUiContext,
        request: ExtensionUiRequest,
        cancellation: ExtensionCancellation,
    ) -> ExtensionFuture<'_, Result<ExtensionUiResponse>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Ok(ExtensionUiResponse::Cancelled);
            }
            if !request.is_interactive() {
                let canonical_query = matches!(
                    request,
                    ExtensionUiRequest::GetEditorText
                        | ExtensionUiRequest::GetAllThemes
                        | ExtensionUiRequest::GetTheme { .. }
                        | ExtensionUiRequest::SetTheme { .. }
                        | ExtensionUiRequest::GetToolsExpanded
                );
                if canonical_query && !inner.state.lock().canonical_queries_supported {
                    return Err(anyhow!(
                        "extension UI request {:?} requires canonical host state",
                        request.capability()
                    ));
                }
                return apply_action(&inner, context, request);
            }

            let id = Uuid::new_v4().to_string();
            let (response, receiver) = oneshot::channel();
            let interaction = ExtensionUiInteraction {
                id: id.clone(),
                context: context.clone(),
                request: request.clone(),
            };
            inner.pending.lock().insert(
                id.clone(),
                PendingInteraction {
                    context,
                    request,
                    response,
                },
            );
            if inner
                .events
                .send(ExtensionUiEvent::InteractionRequested {
                    interaction: interaction.clone(),
                })
                .is_err()
            {
                inner.pending.lock().remove(&id);
                return Err(anyhow!(
                    "no active terminal UI consumer can answer extension interaction {id:?}"
                ));
            }

            tokio::select! {
                result = receiver => match result {
                    Ok(Ok(response)) => Ok(response),
                    Ok(Err(message)) => Err(anyhow!(message)),
                    Err(_) => Ok(ExtensionUiResponse::Cancelled),
                },
                () = cancellation.cancelled() => {
                    inner.pending.lock().remove(&id);
                    Ok(ExtensionUiResponse::Cancelled)
                }
            }
        })
    }

    fn clear_extension(&self, instance: ExtensionInstanceId) -> ExtensionFuture<'_, Result<()>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            let cancelled = {
                let mut pending = inner.pending.lock();
                let ids = pending
                    .iter()
                    .filter_map(|(id, interaction)| {
                        (interaction.context.instance == instance).then(|| id.clone())
                    })
                    .collect::<Vec<_>>();
                ids.into_iter()
                    .filter_map(|id| pending.remove(&id))
                    .collect::<Vec<_>>()
            };
            for interaction in cancelled {
                let _ = interaction
                    .response
                    .send(Ok(ExtensionUiResponse::Cancelled));
            }

            {
                let mut state = inner.state.lock();
                state.statuses.retain(|(owner, _), _| owner != &instance);
                state.widgets.retain(|(owner, _), _| owner != &instance);
                state
                    .notifications
                    .retain(|notification| notification.instance != instance);
                clear_if_owned(&mut state.title, &instance);
                clear_if_owned(&mut state.editor_text, &instance);
                clear_if_owned(&mut state.working_message, &instance);
                clear_if_owned(&mut state.working_visible, &instance);
                clear_if_owned(&mut state.working_indicator, &instance);
                clear_if_owned(&mut state.hidden_thinking_label, &instance);
                clear_if_owned(&mut state.active_theme, &instance);
                clear_if_owned(&mut state.tools_expanded, &instance);
                state.overlay_rows.retain(|(owner, _), _| owner != &instance);
            }
            publish_event(&inner, ExtensionUiEvent::ExtensionCleared { instance });
            Ok(())
        })
    }
}

fn publish_event(inner: &AdapterInner, event: ExtensionUiEvent) {
    let _ = inner.events.send(event.clone());
    let _ = inner.observer_events.send(event);
}

fn clear_if_owned<T>(
    value: &mut Option<(ExtensionInstanceId, T)>,
    instance: &ExtensionInstanceId,
) {
    if value
        .as_ref()
        .is_some_and(|(owner, _)| owner == instance)
    {
        *value = None;
    }
}

fn host_instance() -> ExtensionInstanceId {
    ExtensionInstanceId {
        extension_id: "host".to_owned(),
        generation: 0,
    }
}

fn capability_name(capability: pi_agent::ToolCapability) -> &'static str {
    match capability {
        pi_agent::ToolCapability::Read => "read",
        pi_agent::ToolCapability::Write => "write",
        pi_agent::ToolCapability::Exec => "exec",
    }
}

fn apply_action(
    inner: &AdapterInner,
    context: ExtensionUiContext,
    request: ExtensionUiRequest,
) -> Result<ExtensionUiResponse> {
    match request {
        ExtensionUiRequest::Notify { message, level } => {
            let notification = ExtensionNotification {
                instance: context.instance,
                message,
                level,
            };
            {
                let mut state = inner.state.lock();
                if state.notifications.len() == MAX_RETAINED_NOTIFICATIONS {
                    state.notifications.pop_front();
                }
                state.notifications.push_back(notification.clone());
            }
            publish_event(inner, ExtensionUiEvent::Notification { notification });
        }
        ExtensionUiRequest::Status { key, text } => {
            if let Some(text) = text {
                let item = ExtensionStatusItem {
                    instance: context.instance.clone(),
                    key: key.clone(),
                    text: text.clone(),
                };
                inner
                    .state
                    .lock()
                    .statuses
                    .insert((context.instance, key), text);
                publish_event(inner, ExtensionUiEvent::StatusChanged { item });
            } else {
                inner
                    .state
                    .lock()
                    .statuses
                    .remove(&(context.instance.clone(), key.clone()));
                publish_event(
                    inner,
                    ExtensionUiEvent::StatusCleared {
                        instance: context.instance,
                        key,
                    },
                );
            }
        }
        ExtensionUiRequest::Widget {
            key,
            lines,
            placement,
        } => {
            if let Some(lines) = lines {
                let item = ExtensionWidgetItem {
                    instance: context.instance.clone(),
                    key: key.clone(),
                    lines,
                    placement,
                };
                inner
                    .state
                    .lock()
                    .widgets
                    .insert((context.instance, key), item.clone());
                publish_event(inner, ExtensionUiEvent::WidgetChanged { item });
            } else {
                inner
                    .state
                    .lock()
                    .widgets
                    .remove(&(context.instance.clone(), key.clone()));
                publish_event(
                    inner,
                    ExtensionUiEvent::WidgetCleared {
                        instance: context.instance,
                        key,
                    },
                );
            }
        }
        ExtensionUiRequest::SetEditorText { text } => {
            inner
                .state
                .lock()
                .editor_text = Some((context.instance.clone(), text.clone()));
            publish_event(
                inner,
                ExtensionUiEvent::EditorTextChanged {
                    instance: context.instance,
                    text,
                },
            );
        }
        ExtensionUiRequest::GetEditorText => {
            let text = inner
                .state
                .lock()
                .editor_text
                .as_ref()
                .map_or_else(String::new, |(_, text)| text.clone());
            return Ok(ExtensionUiResponse::EditorText { value: text });
        }
        ExtensionUiRequest::PasteToEditor { text } => {
            let updated = {
                let mut state = inner.state.lock();
                let current = state
                    .editor_text
                    .as_ref()
                    .map_or_else(String::new, |(_, text)| text.clone());
                let mut updated = current;
                updated.push_str(&text);
                state.editor_text = Some((context.instance.clone(), updated.clone()));
                updated
            };
            publish_event(
                inner,
                ExtensionUiEvent::EditorTextChanged {
                    instance: context.instance,
                    text: updated,
                },
            );
        }
        ExtensionUiRequest::SetWorkingMessage { message } => {
            inner.state.lock().working_message = Some((context.instance.clone(), message.clone()));
            publish_event(
                inner,
                ExtensionUiEvent::WorkingMessageChanged {
                    instance: context.instance,
                    message,
                },
            );
        }
        ExtensionUiRequest::SetWorkingVisible { visible } => {
            inner.state.lock().working_visible = Some((context.instance.clone(), visible));
            publish_event(
                inner,
                ExtensionUiEvent::WorkingVisibilityChanged {
                    instance: context.instance,
                    visible,
                },
            );
        }
        ExtensionUiRequest::SetWorkingIndicator { options } => {
            inner.state.lock().working_indicator = Some((context.instance.clone(), options.clone()));
            publish_event(
                inner,
                ExtensionUiEvent::WorkingIndicatorChanged {
                    instance: context.instance,
                    options,
                },
            );
        }
        ExtensionUiRequest::SetHiddenThinkingLabel { label } => {
            inner.state.lock().hidden_thinking_label = Some((context.instance.clone(), label.clone()));
            publish_event(
                inner,
                ExtensionUiEvent::HiddenThinkingLabelChanged {
                    instance: context.instance,
                    label,
                },
            );
        }
        ExtensionUiRequest::GetAllThemes => {
            let themes = inner.state.lock().themes.clone();
            return Ok(ExtensionUiResponse::Themes { themes });
        }
        ExtensionUiRequest::GetTheme { name } => {
            let theme = inner
                .state
                .lock()
                .themes
                .iter()
                .find(|candidate| candidate.name == name)
                .cloned();
            return Ok(ExtensionUiResponse::Theme { theme });
        }
        ExtensionUiRequest::SetTheme { name } => {
            let accepted = inner
                .state
                .lock()
                .themes
                .iter()
                .any(|candidate| candidate.name == name);
            if !accepted {
                return Ok(ExtensionUiResponse::ThemeSet {
                    success: false,
                    error: Some(format!("unknown or unavailable theme {name:?}")),
                });
            }
            inner.state.lock().active_theme = Some((context.instance.clone(), name.clone()));
            publish_event(
                inner,
                ExtensionUiEvent::ThemeChanged {
                    instance: context.instance,
                    name,
                },
            );
            return Ok(ExtensionUiResponse::ThemeSet {
                success: true,
                error: None,
            });
        }
        ExtensionUiRequest::GetToolsExpanded => {
            let expanded = inner
                .state
                .lock()
                .tools_expanded
                .as_ref()
                .is_some_and(|(_, expanded)| *expanded);
            return Ok(ExtensionUiResponse::ToolsExpanded { expanded });
        }
        ExtensionUiRequest::SetToolsExpanded { expanded } => {
            inner.state.lock().tools_expanded = Some((context.instance.clone(), expanded));
            publish_event(
                inner,
                ExtensionUiEvent::ToolsExpandedChanged {
                    instance: context.instance,
                    expanded,
                },
            );
        }
        ExtensionUiRequest::Title { title } => {
            inner.state.lock().title = Some((context.instance.clone(), title.clone()));
            publish_event(
                inner,
                ExtensionUiEvent::TitleChanged {
                    instance: context.instance,
                    title,
                },
            );
        }
        ExtensionUiRequest::OverlaySetRows { id, rows } => {
            // Sanitize at the host boundary: bounds + redaction before the
            // rows become displayable anywhere.
            let rows = pi_coding::sanitize_overlay_rows(rows);
            inner
                .state
                .lock()
                .overlay_rows
                .insert((context.instance.clone(), id.clone()), rows.clone());
            // Publish the live rows so an open overlay with the same
            // (instance, id) repaints without a close/reopen cycle.
            publish_event(
                inner,
                ExtensionUiEvent::OverlayRowsChanged {
                    instance: context.instance,
                    id,
                    rows,
                },
            );
        }
        ExtensionUiRequest::OverlayOpen {
            id,
            title,
            non_capturing,
            input,
        } => {
            // The overlay must be registered by a loaded extension; the
            // adapter cannot know the registry, so the runtime resolves the
            // id (and the registered title/input declaration) before issuing
            // this request. The host UI opens the panel with the overlay's
            // current rows.
            let rows = inner
                .state
                .lock()
                .overlay_rows
                .get(&(context.instance.clone(), id.clone()))
                .cloned()
                .unwrap_or_default();
            publish_event(
                inner,
                ExtensionUiEvent::OverlayOpenRequested {
                    instance: context.instance,
                    id,
                    title: title.unwrap_or_default(),
                    rows,
                    non_capturing,
                    input,
                },
            );
            return Ok(ExtensionUiResponse::OverlayOpened);
        }
        ExtensionUiRequest::Select { .. }
        | ExtensionUiRequest::Confirm { .. }
        | ExtensionUiRequest::Input { .. }
        | ExtensionUiRequest::Editor { .. } => {
            return Err(anyhow!("interactive UI request reached action handler"));
        }
    }
    Ok(ExtensionUiResponse::Acknowledged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_coding::ExtensionMode;

    fn context_for(extension_id: &str) -> ExtensionUiContext {
        ExtensionUiContext {
            instance: ExtensionInstanceId {
                extension_id: extension_id.to_owned(),
                generation: 1,
            },
            mode: ExtensionMode::Print,
        }
    }

    fn context() -> ExtensionUiContext {
        context_for("test")
    }

    #[tokio::test]
    async fn noninteractive_host_applies_actions_and_rejects_prompts() {
        let host = NonInteractiveExtensionUiHost::default();
        let response = host
            .request(
                context(),
                ExtensionUiRequest::Status {
                    key: "phase".to_owned(),
                    text: Some("running".to_owned()),
                },
                ExtensionCancellation::new(),
            )
            .await
            .expect("noninteractive action succeeds");
        assert_eq!(response, ExtensionUiResponse::Acknowledged);
        assert_eq!(host.snapshot().statuses.len(), 1);

        let error = host
            .request(
                context(),
                ExtensionUiRequest::Confirm {
                    title: "Continue?".to_owned(),
                    message: "Choose interactively".to_owned(),
                },
                ExtensionCancellation::new(),
            )
            .await
            .expect_err("interactive prompt is rejected");
        assert!(
            error
                .to_string()
                .contains("unavailable in noninteractive mode")
        );
    }

    #[tokio::test]
    async fn host_tool_confirmation_uses_synthetic_host_context_and_safe_prompt() {
        for (confirmed, expected) in [
            (true, HostToolConfirmation::Approved),
            (false, HostToolConfirmation::Denied),
        ] {
            let adapter = ExtensionUiAdapter::new();
            let mut events = adapter.subscribe();
            let requester = adapter.clone();
            let pending = tokio::spawn(async move {
                requester
                    .confirm_host_tool(
                        ExtensionMode::Rpc,
                        "dangerous_tool",
                        pi_agent::ToolCapability::Exec,
                    )
                    .await
            });
            let ExtensionUiEvent::InteractionRequested { interaction } =
                events.recv().await.expect("host confirmation")
            else {
                panic!("expected host confirmation")
            };
            assert_eq!(interaction.context.instance.extension_id, "host");
            assert_eq!(interaction.context.instance.generation, 0);
            assert_eq!(interaction.context.mode, ExtensionMode::Rpc);
            let ExtensionUiRequest::Confirm { title, message } = &interaction.request else {
                panic!("expected confirm request")
            };
            assert_eq!(title, "Approve tool call?");
            assert!(message.contains("dangerous_tool"));
            assert!(message.contains("exec"));
            assert!(!message.contains('{'));
            adapter.respond_confirmed(&interaction.id, confirmed).unwrap();
            assert_eq!(pending.await.unwrap().unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn host_tool_confirmation_cancel_is_distinct() {
        let adapter = ExtensionUiAdapter::new();
        let mut events = adapter.subscribe();
        let requester = adapter.clone();
        let pending = tokio::spawn(async move {
            requester
                .confirm_host_tool(
                    ExtensionMode::Tui,
                    "write",
                    pi_agent::ToolCapability::Write,
                )
                .await
        });
        let ExtensionUiEvent::InteractionRequested { interaction } =
            events.recv().await.expect("host confirmation")
        else {
            panic!("expected host confirmation")
        };
        adapter.cancel(&interaction.id).unwrap();
        assert_eq!(
            pending.await.unwrap().unwrap(),
            HostToolConfirmation::Cancelled
        );
    }

    #[tokio::test]
    async fn rpc_value_response_correlates_to_pending_request_type() {
        let adapter = ExtensionUiAdapter::new();
        let mut events = adapter.subscribe();
        let responder = adapter.clone();
        let request = tokio::spawn(async move {
            responder
                .request(
                    context(),
                    ExtensionUiRequest::Input {
                        title: "Name".to_owned(),
                        placeholder: Some("Ada".to_owned()),
                        value: None,
                    },
                    ExtensionCancellation::new(),
                )
                .await
        });
        let ExtensionUiEvent::InteractionRequested { interaction } = events.recv().await.unwrap()
        else {
            panic!("expected interaction request")
        };
        adapter
            .respond_value(&interaction.id, "Lovelace".to_owned())
            .unwrap();
        assert_eq!(
            request.await.unwrap().unwrap(),
            ExtensionUiResponse::Input {
                value: Some("Lovelace".to_owned())
            }
        );
    }
    #[tokio::test]
    async fn cleanup_only_removes_state_owned_by_the_cleared_extension() {
        let adapter = ExtensionUiAdapter::new();
        let first = context_for("first");
        let second = context_for("second");
        adapter.set_themes(vec![ExtensionThemeDescriptor {
            name: "dark".to_owned(),
            path: Some("/themes/dark.json".to_owned()),
        }]);

        for request in [
            ExtensionUiRequest::SetEditorText {
                text: "first editor".to_owned(),
            },
            ExtensionUiRequest::SetWorkingMessage {
                message: Some("first working".to_owned()),
            },
            ExtensionUiRequest::SetWorkingVisible { visible: true },
            ExtensionUiRequest::SetWorkingIndicator {
                options: Some(WorkingIndicatorOptions {
                    frames: Some(vec!["first".to_owned()]),
                    interval_ms: Some(80),
                }),
            },
            ExtensionUiRequest::SetHiddenThinkingLabel {
                label: Some("first hidden".to_owned()),
            },
            ExtensionUiRequest::SetTheme {
                name: "dark".to_owned(),
            },
            ExtensionUiRequest::SetToolsExpanded { expanded: true },
        ] {
            adapter
                .request(first.clone(), request, ExtensionCancellation::new())
                .await
                .unwrap();
        }

        for request in [
            ExtensionUiRequest::SetEditorText {
                text: "second editor".to_owned(),
            },
            ExtensionUiRequest::SetWorkingMessage {
                message: Some("second working".to_owned()),
            },
            ExtensionUiRequest::SetWorkingVisible { visible: false },
            ExtensionUiRequest::SetWorkingIndicator {
                options: Some(WorkingIndicatorOptions {
                    frames: Some(vec!["second".to_owned()]),
                    interval_ms: Some(120),
                }),
            },
            ExtensionUiRequest::SetHiddenThinkingLabel {
                label: Some("second hidden".to_owned()),
            },
            ExtensionUiRequest::SetTheme {
                name: "dark".to_owned(),
            },
            ExtensionUiRequest::SetToolsExpanded { expanded: false },
        ] {
            adapter
                .request(second.clone(), request, ExtensionCancellation::new())
                .await
                .unwrap();
        }

        adapter
            .clear_extension(first.instance.clone())
            .await
            .unwrap();
        let snapshot = adapter.snapshot();
        assert_eq!(snapshot.editor_text, "second editor");
        assert_eq!(snapshot.working_message.as_deref(), Some("second working"));
        assert!(!snapshot.working_visible);
        assert_eq!(
            snapshot.working_indicator,
            Some(WorkingIndicatorOptions {
                frames: Some(vec!["second".to_owned()]),
                interval_ms: Some(120),
            })
        );
        assert_eq!(snapshot.hidden_thinking_label.as_deref(), Some("second hidden"));
        assert_eq!(snapshot.active_theme.as_deref(), Some("dark"));
        assert!(!snapshot.tools_expanded);

        adapter.clear_extension(second.instance).await.unwrap();
        let snapshot = adapter.snapshot();
        assert_eq!(snapshot.editor_text, "");
        assert_eq!(snapshot.working_message, None);
        assert!(!snapshot.working_visible);
        assert_eq!(snapshot.working_indicator, None);
        assert_eq!(snapshot.hidden_thinking_label, None);
        assert_eq!(snapshot.active_theme, None);
        assert!(!snapshot.tools_expanded);
        assert_eq!(snapshot.themes[0].name, "dark", "canonical catalog is host-owned");
    }

    #[tokio::test]
    async fn cleanup_drops_only_cleared_extension_notifications() {
        let adapter = ExtensionUiAdapter::new();
        let first = context_for("first");
        let second = context_for("second");

        adapter
            .request(
                first.clone(),
                ExtensionUiRequest::Notify {
                    message: "first note".to_owned(),
                    level: UiNotificationLevel::Info,
                },
                ExtensionCancellation::new(),
            )
            .await
            .unwrap();
        adapter
            .request(
                second.clone(),
                ExtensionUiRequest::Notify {
                    message: "second note".to_owned(),
                    level: UiNotificationLevel::Warning,
                },
                ExtensionCancellation::new(),
            )
            .await
            .unwrap();

        adapter
            .clear_extension(first.instance.clone())
            .await
            .unwrap();
        let snapshot = adapter.snapshot();
        assert_eq!(
            snapshot
                .notifications
                .iter()
                .map(|notification| notification.message.as_str())
                .collect::<Vec<_>>(),
            vec!["second note"],
            "clearing first must drop only first-owned notifications"
        );
        assert_eq!(
            snapshot.notifications[0].instance.extension_id, "second",
            "remaining notification must keep its owner identity"
        );

        adapter.clear_extension(second.instance).await.unwrap();
        assert!(
            adapter.snapshot().notifications.is_empty(),
            "clearing the remaining owner must empty retained notifications"
        );
    }

    #[tokio::test]
    async fn theme_queries_use_the_canonical_catalog_and_set_theme_reports_result() {
        let adapter = ExtensionUiAdapter::new();
        adapter.set_themes(vec![ExtensionThemeDescriptor {
            name: "dark".to_owned(),
            path: Some("/themes/dark.json".to_owned()),
        }]);
        assert_eq!(
            adapter
                .request(
                    context(),
                    ExtensionUiRequest::GetTheme {
                        name: "dark".to_owned(),
                    },
                    ExtensionCancellation::new(),
                )
                .await
                .unwrap(),
            ExtensionUiResponse::Theme {
                theme: Some(ExtensionThemeDescriptor {
                    name: "dark".to_owned(),
                    path: Some("/themes/dark.json".to_owned()),
                })
            }
        );
        assert_eq!(
            adapter
                .request(
                    context(),
                    ExtensionUiRequest::SetTheme {
                        name: "missing".to_owned(),
                    },
                    ExtensionCancellation::new(),
                )
                .await
                .unwrap(),
            ExtensionUiResponse::ThemeSet {
                success: false,
                error: Some("unknown or unavailable theme \"missing\"".to_owned()),
            }
        );
    }

    #[tokio::test]
    async fn host_bound_editor_theme_and_tools_queries_read_authoritative_state() {
        let adapter = ExtensionUiAdapter::new();
        adapter.set_host_editor_text("from-host-editor");
        adapter.set_host_tools_expanded(true);
        adapter.set_themes(vec![ExtensionThemeDescriptor {
            name: "light".to_owned(),
            path: None,
        }]);
        adapter.set_active_theme(Some("light".to_owned()));

        assert_eq!(
            adapter
                .request(
                    context(),
                    ExtensionUiRequest::GetEditorText,
                    ExtensionCancellation::new(),
                )
                .await
                .unwrap(),
            ExtensionUiResponse::EditorText {
                value: "from-host-editor".to_owned()
            }
        );
        assert_eq!(
            adapter
                .request(
                    context(),
                    ExtensionUiRequest::GetToolsExpanded,
                    ExtensionCancellation::new(),
                )
                .await
                .unwrap(),
            ExtensionUiResponse::ToolsExpanded { expanded: true }
        );
        assert_eq!(
            adapter
                .request(
                    context(),
                    ExtensionUiRequest::GetAllThemes,
                    ExtensionCancellation::new(),
                )
                .await
                .unwrap(),
            ExtensionUiResponse::Themes {
                themes: vec![ExtensionThemeDescriptor {
                    name: "light".to_owned(),
                    path: None,
                }]
            }
        );
        assert_eq!(adapter.snapshot().active_theme.as_deref(), Some("light"));

        // Unrelated owner cleanup must not erase host-owned bindings.
        adapter
            .clear_extension(context_for("other").instance)
            .await
            .unwrap();
        assert_eq!(adapter.snapshot().editor_text, "from-host-editor");
        assert!(adapter.snapshot().tools_expanded);
        assert_eq!(adapter.snapshot().active_theme.as_deref(), Some("light"));
    }

    #[tokio::test]
    async fn working_theme_tools_and_editor_mutations_update_authoritative_snapshot() {
        let adapter = ExtensionUiAdapter::new();
        adapter.set_themes(vec![
            ExtensionThemeDescriptor {
                name: "dark".to_owned(),
                path: None,
            },
            ExtensionThemeDescriptor {
                name: "light".to_owned(),
                path: None,
            },
        ]);

        adapter
            .request(
                context(),
                ExtensionUiRequest::SetEditorText {
                    text: "typed-by-extension".to_owned(),
                },
                ExtensionCancellation::new(),
            )
            .await
            .unwrap();
        adapter
            .request(
                context(),
                ExtensionUiRequest::SetWorkingMessage {
                    message: Some("compiling".to_owned()),
                },
                ExtensionCancellation::new(),
            )
            .await
            .unwrap();
        adapter
            .request(
                context(),
                ExtensionUiRequest::SetWorkingVisible { visible: true },
                ExtensionCancellation::new(),
            )
            .await
            .unwrap();
        adapter
            .request(
                context(),
                ExtensionUiRequest::SetHiddenThinkingLabel {
                    label: Some("thinking quietly".to_owned()),
                },
                ExtensionCancellation::new(),
            )
            .await
            .unwrap();
        adapter
            .request(
                context(),
                ExtensionUiRequest::SetToolsExpanded { expanded: true },
                ExtensionCancellation::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            adapter
                .request(
                    context(),
                    ExtensionUiRequest::SetTheme {
                        name: "light".to_owned(),
                    },
                    ExtensionCancellation::new(),
                )
                .await
                .unwrap(),
            ExtensionUiResponse::ThemeSet {
                success: true,
                error: None,
            }
        );

        let snapshot = adapter.snapshot();
        assert_eq!(snapshot.editor_text, "typed-by-extension");
        assert_eq!(snapshot.working_message.as_deref(), Some("compiling"));
        assert!(snapshot.working_visible);
        assert_eq!(
            snapshot.hidden_thinking_label.as_deref(),
            Some("thinking quietly")
        );
        assert!(snapshot.tools_expanded);
        assert_eq!(snapshot.active_theme.as_deref(), Some("light"));

        assert_eq!(
            adapter
                .request(
                    context(),
                    ExtensionUiRequest::GetEditorText,
                    ExtensionCancellation::new(),
                )
                .await
                .unwrap(),
            ExtensionUiResponse::EditorText {
                value: "typed-by-extension".to_owned()
            }
        );
        assert_eq!(
            adapter
                .request(
                    context(),
                    ExtensionUiRequest::GetToolsExpanded,
                    ExtensionCancellation::new(),
                )
                .await
                .unwrap(),
            ExtensionUiResponse::ToolsExpanded { expanded: true }
        );
    }

    #[tokio::test]
    async fn unsupported_canonical_queries_error_when_flag_disabled() {
        let adapter = ExtensionUiAdapter::new();
        adapter.set_canonical_queries_supported(false);
        adapter.set_host_editor_text("shadow-must-not-leak");
        adapter.set_host_tools_expanded(true);
        adapter.set_themes(vec![ExtensionThemeDescriptor {
            name: "dark".to_owned(),
            path: None,
        }]);

        for request in [
            ExtensionUiRequest::GetEditorText,
            ExtensionUiRequest::GetAllThemes,
            ExtensionUiRequest::GetTheme {
                name: "dark".to_owned(),
            },
            ExtensionUiRequest::SetTheme {
                name: "dark".to_owned(),
            },
            ExtensionUiRequest::GetToolsExpanded,
        ] {
            let error = adapter
                .request(context(), request, ExtensionCancellation::new())
                .await
                .expect_err("disabled canonical queries must fail closed");
            assert!(
                error.to_string().contains("canonical host state"),
                "{error:#}"
            );
        }
    }
}
