use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::Arc,
};

use anyhow::{Result, anyhow};
use parking_lot::Mutex;
use pi_coding::{
    ExtensionCancellation, ExtensionFuture, ExtensionInstanceId, ExtensionUiContext,
    ExtensionUiHost, ExtensionUiRequest, ExtensionUiResponse, UiNotificationLevel,
    UiWidgetPlacement,
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
}

#[derive(Default)]
struct AdapterState {
    statuses: BTreeMap<(ExtensionInstanceId, String), String>,
    widgets: BTreeMap<(ExtensionInstanceId, String), ExtensionWidgetItem>,
    notifications: VecDeque<ExtensionNotification>,
    title: Option<(ExtensionInstanceId, String)>,
    editor_text: String,
}

struct PendingInteraction {
    context: ExtensionUiContext,
    request: ExtensionUiRequest,
    response: oneshot::Sender<Result<ExtensionUiResponse, String>>,
}

impl ExtensionUiAdapter {
    #[must_use]
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(UI_EVENT_BUFFER);
        Self {
            inner: Arc::new(AdapterInner {
                state: Mutex::new(AdapterState::default()),
                pending: Mutex::new(HashMap::new()),
                events,
            }),
        }
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<ExtensionUiEvent> {
        self.inner.events.subscribe()
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
            editor_text: state.editor_text.clone(),
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
                if state
                    .title
                    .as_ref()
                    .is_some_and(|(owner, _)| owner == &instance)
                {
                    state.title = None;
                }
            }
            let _ = inner
                .events
                .send(ExtensionUiEvent::ExtensionCleared { instance });
            Ok(())
        })
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
            let _ = inner
                .events
                .send(ExtensionUiEvent::Notification { notification });
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
                let _ = inner.events.send(ExtensionUiEvent::StatusChanged { item });
            } else {
                inner
                    .state
                    .lock()
                    .statuses
                    .remove(&(context.instance.clone(), key.clone()));
                let _ = inner.events.send(ExtensionUiEvent::StatusCleared {
                    instance: context.instance,
                    key,
                });
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
                let _ = inner.events.send(ExtensionUiEvent::WidgetChanged { item });
            } else {
                inner
                    .state
                    .lock()
                    .widgets
                    .remove(&(context.instance.clone(), key.clone()));
                let _ = inner.events.send(ExtensionUiEvent::WidgetCleared {
                    instance: context.instance,
                    key,
                });
            }
        }
        ExtensionUiRequest::Title { title } => {
            inner.state.lock().title = Some((context.instance.clone(), title.clone()));
            let _ = inner.events.send(ExtensionUiEvent::TitleChanged {
                instance: context.instance,
                title,
            });
        }
        ExtensionUiRequest::SetEditorText { text } => {
            inner.state.lock().editor_text.clone_from(&text);
            let _ = inner.events.send(ExtensionUiEvent::EditorTextChanged {
                instance: context.instance,
                text,
            });
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

    fn context() -> ExtensionUiContext {
        ExtensionUiContext {
            instance: ExtensionInstanceId {
                extension_id: "test".to_owned(),
                generation: 1,
            },
            mode: ExtensionMode::Print,
        }
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
}
