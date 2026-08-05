use crate::*;
use futures_util::FutureExt;
use serde_json::Value;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone)]
pub struct FauxResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub error_message: Option<String>,
}
impl FauxResponse {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::text(text)],
            stop_reason: StopReason::Stop,
            error_message: None,
        }
    }
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: vec![],
            stop_reason: StopReason::Error,
            error_message: Some(message.into()),
        }
    }
}
#[derive(Debug, Clone)]
pub struct FauxProviderOptions {
    pub api: String,
    pub provider: String,
    pub models: Vec<Model>,
    pub chunk_size: usize,
}
impl Default for FauxProviderOptions {
    fn default() -> Self {
        let mut model = Model::default();
        model.id = "faux-1".into();
        model.name = "Faux Model".into();
        model.api = API_FAUX.into();
        model.provider = "faux".into();
        model.base_url = "http://localhost:0".into();
        Self {
            api: API_FAUX.into(),
            provider: "faux".into(),
            models: vec![model],
            chunk_size: 12,
        }
    }
}
#[derive(Clone)]
pub struct FauxProviderRegistration {
    api: String,
    models: Vec<Model>,
    queue: Arc<Mutex<VecDeque<FauxResponse>>>,
    source_id: String,
    chunk_size: usize,
}
impl FauxProviderRegistration {
    pub fn model(&self, id: Option<&str>) -> Option<Model> {
        id.map_or_else(
            || self.models.first().cloned(),
            |id| self.models.iter().find(|m| m.id == id).cloned(),
        )
    }
    pub fn set_responses(&self, r: Vec<FauxResponse>) {
        *self.queue.lock().expect("faux queue") = r.into();
    }
    pub fn append_response(&self, r: FauxResponse) {
        self.queue.lock().expect("faux queue").push_back(r);
    }
    pub fn unregister(&self) {
        unregister_api_providers(&self.source_id)
    }
}
pub fn register_faux_provider(options: FauxProviderOptions) -> FauxProviderRegistration {
    let queue = Arc::new(Mutex::new(VecDeque::new()));
    let source_id = format!("faux:{}", uuid::Uuid::now_v7());
    let reg = FauxProviderRegistration {
        api: options.api.clone(),
        models: options.models.clone(),
        queue: queue.clone(),
        source_id: source_id.clone(),
        chunk_size: options.chunk_size.max(1),
    };
    for m in &reg.models {
        register_model(m.clone())
    }
    let simple_queue = queue.clone();
    let chunk = reg.chunk_size;
    let simple: SimpleStreamFn = Arc::new(move |model, _ctx, opts| {
        let q = simple_queue.clone();
        async move { faux_stream(model, opts.stream, q, chunk).await }.boxed()
    });
    let native = simple.clone();
    register_api_provider(
        ApiProvider {
            api: reg.api.clone(),
            stream: Arc::new(move |m, c, o| {
                let native = native.clone();
                async move { native(m, c, o.into()).await }.boxed()
            }),
            stream_simple: simple,
        },
        Some(source_id),
    );
    reg
}
async fn faux_stream(
    model: Model,
    options: StreamOptions,
    queue: Arc<Mutex<VecDeque<FauxResponse>>>,
    chunk: usize,
) -> AssistantMessageEventStream {
    let stream = new_assistant_message_event_stream();
    let stream2 = stream.clone();
    tokio::spawn(async move {
        let mut out = AssistantMessage::pending(&model);
        if options
            .abort_signal
            .as_ref()
            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
        {
            super::common::fail(&stream2, out, "Request was aborted", true).await;
            return;
        }
        stream2
            .push(AssistantMessageEvent::Start {
                partial: out.clone(),
            })
            .await;
        let response = queue
            .lock()
            .expect("faux queue")
            .pop_front()
            .or_else(|| {
                // Binary black-box tests (and other CI lanes) can supply a
                // deterministic offline reply without registering a custom
                // provider. Empty/missing values keep the historical error path.
                std::env::var("PI_FAUX_RESPONSE")
                    .ok()
                    .map(|text| text.trim().to_owned())
                    .filter(|text| !text.is_empty())
                    .map(FauxResponse::text)
            })
            .unwrap_or_else(|| FauxResponse::error("No more faux responses queued"));
        for block in response.content.clone() {
            let index = out.content.len();
            match block.clone() {
                ContentBlock::Text { text, .. } => {
                    out.content.push(ContentBlock::text(""));
                    stream2
                        .push(AssistantMessageEvent::TextStart {
                            content_index: index,
                            partial: out.clone(),
                        })
                        .await;
                    let mut acc = String::new();
                    for piece in text.as_bytes().chunks(chunk) {
                        if options
                            .abort_signal
                            .as_ref()
                            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
                        {
                            super::common::fail(&stream2, out, "Request was aborted", true).await;
                            return;
                        }
                        let delta = String::from_utf8_lossy(piece).into_owned();
                        acc.push_str(&delta);
                        out.content[index] = ContentBlock::text(acc.clone());
                        stream2
                            .push(AssistantMessageEvent::TextDelta {
                                content_index: index,
                                delta,
                                partial: out.clone(),
                            })
                            .await;
                    }
                    stream2
                        .push(AssistantMessageEvent::TextEnd {
                            content_index: index,
                            content: text,
                            partial: out.clone(),
                        })
                        .await
                }
                ContentBlock::Thinking { thinking, .. } => {
                    out.content.push(ContentBlock::thinking(""));
                    stream2
                        .push(AssistantMessageEvent::ThinkingStart {
                            content_index: index,
                            partial: out.clone(),
                        })
                        .await;
                    out.content[index] = block;
                    stream2
                        .push(AssistantMessageEvent::ThinkingDelta {
                            content_index: index,
                            delta: thinking.clone(),
                            partial: out.clone(),
                        })
                        .await;
                    stream2
                        .push(AssistantMessageEvent::ThinkingEnd {
                            content_index: index,
                            content: thinking,
                            partial: out.clone(),
                        })
                        .await
                }
                ContentBlock::ToolCall(tc) => {
                    out.content.push(ContentBlock::ToolCall(tc.clone()));
                    stream2
                        .push(AssistantMessageEvent::ToolCallStart {
                            content_index: index,
                            partial: out.clone(),
                        })
                        .await;
                    let delta = tc.arguments.to_string();
                    stream2
                        .push(AssistantMessageEvent::ToolCallDelta {
                            content_index: index,
                            delta,
                            partial: out.clone(),
                        })
                        .await;
                    stream2
                        .push(AssistantMessageEvent::ToolCallEnd {
                            content_index: index,
                            tool_call: tc,
                            partial: out.clone(),
                        })
                        .await
                }
                ContentBlock::Image { .. } => {}
            }
        }
        out.content = response.content;
        out.stop_reason = response.stop_reason;
        out.error_message = response.error_message;
        if matches!(out.stop_reason, StopReason::Error | StopReason::Aborted) {
            stream2
                .push(AssistantMessageEvent::Error {
                    reason: out.stop_reason,
                    error: out.clone(),
                })
                .await
        } else {
            stream2
                .push(AssistantMessageEvent::Done {
                    reason: out.stop_reason,
                    message: out.clone(),
                })
                .await
        }
        stream2.end(Some(out)).await;
    });
    stream
}
static DEFAULT: std::sync::OnceLock<FauxProviderRegistration> = std::sync::OnceLock::new();
pub fn register_default_faux() {
    DEFAULT.get_or_init(|| register_faux_provider(FauxProviderOptions::default()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio_util::sync::CancellationToken;

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    fn unique_registration() -> FauxProviderRegistration {
        let suffix = NEXT_ID.fetch_add(1, Ordering::Relaxed).to_string();
        let api = format!("faux-test-{suffix}");
        let provider = format!("faux-provider-{suffix}");
        let mut model = Model::default();
        model.id = "test".into();
        model.name = "Test".into();
        model.api = api.clone();
        model.provider = provider.clone();
        register_faux_provider(FauxProviderOptions {
            api,
            provider,
            models: vec![model],
            chunk_size: 2,
        })
    }

    #[tokio::test]
    async fn streams_text_and_tool_call_in_order() {
        let registration = unique_registration();
        let model = registration.model(None).unwrap();
        registration.set_responses(vec![FauxResponse {
            content: vec![
                ContentBlock::text("hello"),
                ContentBlock::ToolCall(ToolCall {
                    id: "call-1".into(),
                    name: "read".into(),
                    arguments: json!({"path":"x"}),
                    thought_signature: None,
                }),
            ],
            stop_reason: StopReason::ToolUse,
            error_message: None,
        }]);
        let events = faux_stream(
            model,
            SimpleStreamOptions::default().stream,
            registration.queue.clone(),
            registration.chunk_size,
        )
        .await;
        let mut kinds = Vec::new();
        while let Some(event) = events.next().await {
            kinds.push(match event {
                AssistantMessageEvent::Start { .. } => "start",
                AssistantMessageEvent::TextStart { .. } => "text_start",
                AssistantMessageEvent::TextDelta { .. } => "text_delta",
                AssistantMessageEvent::TextEnd { .. } => "text_end",
                AssistantMessageEvent::ToolCallStart { .. } => "tool_start",
                AssistantMessageEvent::ToolCallDelta { .. } => "tool_delta",
                AssistantMessageEvent::ToolCallEnd { .. } => "tool_end",
                AssistantMessageEvent::Done { .. } => "done",
                _ => "other",
            });
        }
        assert_eq!(kinds.first(), Some(&"start"));
        assert_eq!(kinds.last(), Some(&"done"));
        assert!(
            kinds
                .windows(3)
                .any(|w| w == ["tool_start", "tool_delta", "tool_end"])
        );
        let result = events.result().await.unwrap();
        assert_eq!(result.stop_reason, StopReason::ToolUse);
        registration.unregister();
    }

    #[tokio::test]
    async fn pre_cancelled_stream_emits_aborted_error() {
        let registration = unique_registration();
        let model = registration.model(None).unwrap();
        registration.set_responses(vec![FauxResponse::text("never")]);
        let token = CancellationToken::new();
        token.cancel();
        let mut options = SimpleStreamOptions::default();
        options.stream.abort_signal = Some(token);
        let events = faux_stream(model, options.stream, registration.queue.clone(), registration.chunk_size).await;
        assert!(matches!(
            events.next().await,
            Some(AssistantMessageEvent::Error {
                reason: StopReason::Aborted,
                ..
            })
        ));
        assert_eq!(
            events.result().await.unwrap().stop_reason,
            StopReason::Aborted
        );
        registration.unregister();
    }
    #[test]
    fn concurrent_registrations_have_distinct_sources() {
        let registrations = std::thread::scope(|scope| {
            let first = scope.spawn(unique_registration);
            let second = scope.spawn(unique_registration);
            [first.join().expect("first registration"), second.join().expect("second registration")]
        });
        assert_ne!(registrations[0].source_id, registrations[1].source_id);
        for registration in registrations {
            registration.unregister();
        }
    }

}
