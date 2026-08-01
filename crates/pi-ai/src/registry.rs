use crate::{AssistantMessageEventStream, Context, Model, SimpleStreamOptions, StreamOptions};
use futures_util::future::BoxFuture;
use std::{
    collections::BTreeMap,
    sync::{Arc, LazyLock, RwLock},
};

pub type StreamFn = Arc<
    dyn Fn(Model, Context, StreamOptions) -> BoxFuture<'static, AssistantMessageEventStream>
        + Send
        + Sync,
>;
pub type SimpleStreamFn = Arc<
    dyn Fn(Model, Context, SimpleStreamOptions) -> BoxFuture<'static, AssistantMessageEventStream>
        + Send
        + Sync,
>;
#[derive(Clone)]
pub struct ApiProvider {
    pub api: String,
    pub stream: StreamFn,
    pub stream_simple: SimpleStreamFn,
}
#[derive(Clone)]
struct Entry {
    provider: ApiProvider,
    source_id: Option<String>,
}
static REGISTRY: LazyLock<RwLock<BTreeMap<String, Entry>>> =
    LazyLock::new(|| RwLock::new(BTreeMap::new()));
// Registered stream functions are guarded so a model carrying a foreign `api`
// panics instead of silently dispatching through this provider.
pub fn register_api_provider(mut provider: ApiProvider, source_id: Option<String>) {
    let api = provider.api.clone();
    provider.stream = guard_stream(&api, provider.stream);
    provider.stream_simple = guard_simple_stream(&api, provider.stream_simple);
    REGISTRY.write().expect("registry lock").insert(
        provider.api.clone(),
        Entry {
            provider,
            source_id,
        },
    );
}
pub fn get_api_provider(api: &str) -> Option<ApiProvider> {
    REGISTRY
        .read()
        .expect("registry lock")
        .get(api)
        .map(|e| e.provider.clone())
}
pub fn get_api_providers() -> Vec<ApiProvider> {
    REGISTRY
        .read()
        .expect("registry lock")
        .values()
        .map(|e| e.provider.clone())
        .collect()
}
pub fn unregister_api_providers(source_id: &str) {
    REGISTRY
        .write()
        .expect("registry lock")
        .retain(|_, e| e.source_id.as_deref() != Some(source_id));
}
pub fn clear_api_providers() {
    REGISTRY.write().expect("registry lock").clear();
}

fn assert_api_matches(model_api: &str, provider_api: &str) {
    assert_eq!(
        model_api, provider_api,
        "api mismatch: model.api \"{model_api}\" does not match provider.api \"{provider_api}\""
    );
}
fn guard_stream(api: &str, inner: StreamFn) -> StreamFn {
    let api = api.to_string();
    Arc::new(move |model, context, options| {
        assert_api_matches(&model.api, &api);
        inner(model, context, options)
    })
}
fn guard_simple_stream(api: &str, inner: SimpleStreamFn) -> SimpleStreamFn {
    let api = api.to_string();
    Arc::new(move |model, context, options| {
        assert_api_matches(&model.api, &api);
        inner(model, context, options)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn test_provider(api: &str, invoked: Arc<AtomicBool>) -> ApiProvider {
        let invoked_stream = invoked.clone();
        let stream: StreamFn = Arc::new(move |_, _, _| {
            invoked_stream.store(true, Ordering::SeqCst);
            async { crate::new_assistant_message_event_stream() }.boxed()
        });
        let invoked_simple = invoked;
        let simple: SimpleStreamFn = Arc::new(move |_, _, _| {
            invoked_simple.store(true, Ordering::SeqCst);
            async { crate::new_assistant_message_event_stream() }.boxed()
        });
        ApiProvider {
            api: api.into(),
            stream,
            stream_simple: simple,
        }
    }

    fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
            .unwrap_or_else(|| format!("{payload:?}"))
    }

    #[test]
    fn registers_replaces_and_unregisters_by_source() {
        let stream: StreamFn =
            Arc::new(|_, _, _| async { crate::new_assistant_message_event_stream() }.boxed());
        let simple: SimpleStreamFn =
            Arc::new(|_, _, _| async { crate::new_assistant_message_event_stream() }.boxed());
        let provider = ApiProvider {
            api: "registry-test".into(),
            stream,
            stream_simple: simple,
        };
        register_api_provider(provider.clone(), Some("test-source".into()));
        assert!(get_api_provider("registry-test").is_some());
        register_api_provider(provider, Some("test-source".into()));
        assert_eq!(
            get_api_providers()
                .iter()
                .filter(|p| p.api == "registry-test")
                .count(),
            1
        );
        unregister_api_providers("test-source");
        assert!(get_api_provider("registry-test").is_none());
    }

    #[test]
    fn stream_rejects_api_mismatch_before_invoking_provider() {
        let invoked = Arc::new(AtomicBool::new(false));
        register_api_provider(
            test_provider("guard-stream-api", invoked.clone()),
            Some("guard-stream-source".into()),
        );
        let registered = get_api_provider("guard-stream-api").expect("provider registered");
        let mut model = Model::default();
        model.api = "guard-other-api".into();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _future = (registered.stream)(model, Context::default(), StreamOptions::default());
        }));
        let message = panic_message(result.expect_err("mismatched api must reject"));
        assert!(
            message.contains("api mismatch"),
            "unexpected panic message: {message}"
        );
        assert!(
            message.contains("guard-other-api"),
            "message must name the model api: {message}"
        );
        assert!(
            message.contains("guard-stream-api"),
            "message must name the provider api: {message}"
        );
        assert!(
            !invoked.load(Ordering::SeqCst),
            "provider must not run for a mismatched model"
        );
        unregister_api_providers("guard-stream-source");
    }

    #[test]
    fn simple_stream_rejects_api_mismatch_before_invoking_provider() {
        let invoked = Arc::new(AtomicBool::new(false));
        register_api_provider(
            test_provider("guard-simple-api", invoked.clone()),
            Some("guard-simple-source".into()),
        );
        let registered = get_api_provider("guard-simple-api").expect("provider registered");
        let mut model = Model::default();
        model.api = "guard-other-api".into();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _future = (registered.stream_simple)(
                model,
                Context::default(),
                SimpleStreamOptions::default(),
            );
        }));
        let message = panic_message(result.expect_err("mismatched api must reject"));
        assert!(
            message.contains("api mismatch"),
            "unexpected panic message: {message}"
        );
        assert!(
            message.contains("guard-other-api"),
            "message must name the model api: {message}"
        );
        assert!(
            message.contains("guard-simple-api"),
            "message must name the provider api: {message}"
        );
        assert!(
            !invoked.load(Ordering::SeqCst),
            "provider must not run for a mismatched model"
        );
        unregister_api_providers("guard-simple-source");
    }

    #[test]
    fn stream_invokes_provider_when_api_matches() {
        let invoked = Arc::new(AtomicBool::new(false));
        register_api_provider(
            test_provider("guard-stream-match", invoked.clone()),
            Some("guard-stream-match-source".into()),
        );
        let registered = get_api_provider("guard-stream-match").expect("provider registered");
        let mut model = Model::default();
        model.api = "guard-stream-match".into();
        let stream = (registered.stream)(model, Context::default(), StreamOptions::default());
        assert!(
            invoked.load(Ordering::SeqCst),
            "matching model must reach the provider"
        );
        assert!(
            stream.now_or_never().is_some(),
            "wrapped provider stream must resolve"
        );
        unregister_api_providers("guard-stream-match-source");
    }

    #[test]
    fn simple_stream_invokes_provider_when_api_matches() {
        let invoked = Arc::new(AtomicBool::new(false));
        register_api_provider(
            test_provider("guard-simple-match", invoked.clone()),
            Some("guard-simple-match-source".into()),
        );
        let registered = get_api_provider("guard-simple-match").expect("provider registered");
        let mut model = Model::default();
        model.api = "guard-simple-match".into();
        let stream =
            (registered.stream_simple)(model, Context::default(), SimpleStreamOptions::default());
        assert!(
            invoked.load(Ordering::SeqCst),
            "matching model must reach the provider"
        );
        assert!(
            stream.now_or_never().is_some(),
            "wrapped provider stream must resolve"
        );
        unregister_api_providers("guard-simple-match-source");
    }
}
