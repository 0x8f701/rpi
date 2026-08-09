use crate::{
    AssistantMessageEventStream, Context, ImageGenerationOptions, ImageGenerationResult, Model,
    SimpleStreamOptions, StreamOptions,
};
use anyhow::Result;
use futures_util::future::BoxFuture;
use std::{
    collections::BTreeMap,
    fmt,
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
pub type ImageGenFn = Arc<
    dyn Fn(Model, ImageGenerationOptions) -> BoxFuture<'static, Result<ImageGenerationResult>>
        + Send
        + Sync,
>;
#[derive(Clone)]
pub struct ApiProvider {
    pub api: String,
    pub stream: StreamFn,
    pub stream_simple: SimpleStreamFn,
    /// Optional image-generation capability (`images/generations`). Absent for
    /// chat-only providers; the imagegen/openrouter-images providers set it.
    pub generate_image: Option<ImageGenFn>,
}
#[derive(Clone)]
struct Entry {
    provider: ApiProvider,
    /// Owner scope for unregistration. `Some` only for builtin entries (the
    /// source id passed to [`register_api_provider`]); extension entries are
    /// keyed by their runtime namespace instead.
    source_id: Option<String>,
}

/// Provider registry keyed by api, then by ownership namespace.
///
/// - The `None` slot of an api holds its global builtin provider.
/// - Each `Some(namespace)` slot holds one extension runtime's provider.
///
/// Extension runtimes may register the same api concurrently; every entry
/// coexists and session lookups resolve strictly within the caller's
/// namespace. Unregistration is namespace-scoped, so shutting down or
/// reloading one runtime never touches another runtime's entries.
static REGISTRY: LazyLock<RwLock<BTreeMap<String, BTreeMap<Option<String>, Entry>>>> =
    LazyLock::new(|| RwLock::new(BTreeMap::new()));
// Registered stream functions are guarded so a model carrying a foreign `api`
// panics instead of silently dispatching through this provider.
pub fn register_api_provider(mut provider: ApiProvider, source_id: Option<String>) {
    let api = provider.api.clone();
    provider.stream = guard_stream(&api, provider.stream);
    provider.stream_simple = guard_simple_stream(&api, provider.stream_simple);
    provider.generate_image = provider
        .generate_image
        .take()
        .map(|inner| guard_image_gen(&api, inner));
    REGISTRY
        .write()
        .expect("registry lock")
        .entry(provider.api.clone())
        .or_default()
        .insert(
            None,
            Entry {
                provider,
                source_id,
            },
        );
}

/// Register a provider owned by an extension runtime (see
/// `pi-coding::extensions`). `namespace` identifies the owning runtime, so
/// two runtimes registering the same api coexist and each session resolves
/// the entry of its own runtime (see [`resolve_extension_provider`]).
/// Re-registering within the same namespace replaces the previous entry.
/// The entry is extension-owned: credential resolution skips host-side API
/// keys because the extension streams itself.
pub fn register_extension_provider(mut provider: ApiProvider, namespace: String) {
    let api = provider.api.clone();
    provider.stream = guard_stream(&api, provider.stream);
    provider.stream_simple = guard_simple_stream(&api, provider.stream_simple);
    provider.generate_image = provider
        .generate_image
        .take()
        .map(|inner| guard_image_gen(&api, inner));
    REGISTRY
        .write()
        .expect("registry lock")
        .entry(provider.api.clone())
        .or_default()
        .insert(
            Some(namespace),
            Entry {
                provider,
                source_id: None,
            },
        );
}

/// Unregister every entry owned by an extension runtime namespace. Only the
/// owning runtime's entries are removed; other runtimes and builtins are
/// untouched, so shutting down or reloading one runtime never affects
/// another.
pub fn unregister_extension_providers(namespace: &str) {
    REGISTRY
        .write()
        .expect("registry lock")
        .retain(|_, slots| {
            slots.remove(&Some(namespace.to_owned()));
            !slots.is_empty()
        });
}

/// True when `api` is owned by at least one extension runtime.
#[must_use]
pub fn is_extension_provider(api: &str) -> bool {
    REGISTRY
        .read()
        .expect("registry lock")
        .get(api)
        .is_some_and(|slots| slots.keys().any(Option::is_some))
}

/// Unscoped provider lookup. The global builtin wins deterministically; an
/// api owned by exactly one extension runtime resolves to it; an api owned by
/// several runtimes resolves to `None` — fail closed, never an arbitrary
/// pick. Sessions carrying a runtime namespace must use
/// [`resolve_extension_provider`] instead.
pub fn get_api_provider(api: &str) -> Option<ApiProvider> {
    let registry = REGISTRY.read().expect("registry lock");
    let slots = registry.get(api)?;
    if let Some(entry) = slots.get(&None) {
        return Some(entry.provider.clone());
    }
    single_extension_owner(slots).map(|entry| entry.provider.clone())
}

/// Every globally resolvable provider: all builtins plus extension entries
/// whose api has exactly one owner (ambiguous apis are excluded).
pub fn get_api_providers() -> Vec<ApiProvider> {
    let registry = REGISTRY.read().expect("registry lock");
    let mut providers = Vec::new();
    for slots in registry.values() {
        if let Some(entry) = slots.get(&None) {
            providers.push(entry.provider.clone());
        } else if let Some(entry) = single_extension_owner(slots) {
            providers.push(entry.provider.clone());
        }
    }
    providers
}

/// Unregister builtin entries registered under `source_id`, plus extension
/// namespaces that exactly match it (unscoped callers that registered with a
/// plain source id). Namespace-scoped extension owners use
/// [`unregister_extension_providers`].
pub fn unregister_api_providers(source_id: &str) {
    REGISTRY
        .write()
        .expect("registry lock")
        .retain(|_, slots| {
            // Drop the global builtin entry registered under this source.
            if slots
                .get(&None)
                .is_some_and(|entry| entry.source_id.as_deref() == Some(source_id))
            {
                slots.remove(&None);
            }
            // Drop extension namespaces that exactly match this source id.
            slots.retain(|namespace, _| {
                !matches!(namespace, Some(namespace) if namespace == source_id)
            });
            !slots.is_empty()
        });
}
pub fn clear_api_providers() {
    REGISTRY.write().expect("registry lock").clear();
}

/// Why a namespaced extension-provider lookup failed closed instead of
/// resolving.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderScopeError {
    /// `api` is owned by one or more extension runtimes, but not by the
    /// namespace the lookup was scoped to. Resolving through another runtime
    /// would leak the session's stream across runtimes, so it fails instead.
    ExtensionNotInNamespace { api: String, namespace: String },
    /// `api` is owned by several extension runtimes and the lookup carried no
    /// namespace; any pick would be arbitrary.
    AmbiguousExtension { api: String, namespaces: Vec<String> },
}

impl fmt::Display for ProviderScopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExtensionNotInNamespace { api, namespace } => write!(
                f,
                "extension provider api {api:?} belongs to another extension runtime \
                 (namespace {namespace:?} does not provide it)"
            ),
            Self::AmbiguousExtension { api, namespaces } => write!(
                f,
                "extension provider api {api:?} is registered by multiple extension \
                 runtimes ({}) and cannot be resolved without a session namespace",
                namespaces.join(", ")
            ),
        }
    }
}

impl std::error::Error for ProviderScopeError {}

/// Resolve an extension-owned provider within `namespace` (fail closed).
///
/// Only extension entries are considered: builtin providers resolve through
/// the unscoped [`get_api_provider`] path so credential resolution still
/// applies, and an api with no extension owner yields `Ok(None)` so the
/// caller can fall through to the global path.
///
/// - `Some(namespace)`: the namespace's own entry wins. When the api is owned
///   by a different extension runtime, resolution fails with
///   [`ProviderScopeError::ExtensionNotInNamespace`] instead of leaking into
///   that runtime.
/// - `None` (unscoped): an api owned by exactly one extension runtime
///   resolves; several owners fail with
///   [`ProviderScopeError::AmbiguousExtension`].
pub fn resolve_extension_provider(
    api: &str,
    namespace: Option<&str>,
) -> Result<Option<ApiProvider>, ProviderScopeError> {
    let registry = REGISTRY.read().expect("registry lock");
    let Some(slots) = registry.get(api) else {
        return Ok(None);
    };
    if let Some(namespace) = namespace {
        if let Some(entry) = slots.get(&Some(namespace.to_owned())) {
            return Ok(Some(entry.provider.clone()));
        }
        if slots.keys().any(Option::is_some) {
            return Err(ProviderScopeError::ExtensionNotInNamespace {
                api: api.to_owned(),
                namespace: namespace.to_owned(),
            });
        }
        return Ok(None);
    }
    match single_extension_owner(slots) {
        Some(entry) => Ok(Some(entry.provider.clone())),
        None => {
            let owners = slots
                .keys()
                .filter_map(|namespace| namespace.clone())
                .collect::<Vec<_>>();
            if owners.is_empty() {
                Ok(None)
            } else {
                Err(ProviderScopeError::AmbiguousExtension {
                    api: api.to_owned(),
                    namespaces: owners,
                })
            }
        }
    }
}

/// The single extension entry of `slots`, or `None` when there is none or
/// several (ambiguous).
fn single_extension_owner(slots: &BTreeMap<Option<String>, Entry>) -> Option<&Entry> {
    let mut owners = slots.keys().filter(|namespace| namespace.is_some());
    let owner = owners.next()?;
    if owners.next().is_some() {
        return None;
    }
    slots.get(owner)
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
fn guard_image_gen(api: &str, inner: ImageGenFn) -> ImageGenFn {
    let api = api.to_string();
    Arc::new(move |model, options| {
        assert_api_matches(&model.api, &api);
        inner(model, options)
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
            generate_image: None,
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
            generate_image: None,
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

    fn matching_model(api: &str) -> Model {
        let mut model = Model::default();
        model.api = api.into();
        model
    }

    /// Invoke a resolved provider's guarded stream and report which provider
    /// flag fired.
    fn invoke_stream(provider: &ApiProvider, api: &str) {
        let _stream = (provider.stream)(matching_model(api), Context::default(), StreamOptions::default());
    }

    #[test]
    fn extension_namespace_unregisters_only_its_own_entries() {
        register_extension_provider(
            test_provider("ns-marked", Arc::new(AtomicBool::new(false))),
            "ns-marked-source".into(),
        );
        assert!(
            is_extension_provider("ns-marked"),
            "extension-registered apis must be flagged"
        );
        // A builtin registration of the same api coexists in the global slot:
        // it resolves unscoped (builtin wins) but never clears or shadows the
        // extension entry for namespaced lookups.
        register_api_provider(
            test_provider("ns-marked", Arc::new(AtomicBool::new(false))),
            Some("ns-marked-builtin-source".into()),
        );
        assert!(
            is_extension_provider("ns-marked"),
            "the extension entry must coexist with the builtin"
        );
        assert!(
            get_api_provider("ns-marked").is_some(),
            "the builtin remains globally resolvable"
        );
        // Unregistering the extension namespace leaves the builtin intact.
        unregister_extension_providers("ns-marked-source");
        assert!(!is_extension_provider("ns-marked"));
        assert!(get_api_provider("ns-marked").is_some());
        // Unregistering the builtin source removes the last entry.
        unregister_api_providers("ns-marked-builtin-source");
        assert!(get_api_provider("ns-marked").is_none());
    }

    #[test]
    fn unregister_by_source_id_also_drops_matching_extension_namespace() {
        register_extension_provider(
            test_provider("compat-api", Arc::new(AtomicBool::new(false))),
            "plain-source-id".into(),
        );
        assert!(is_extension_provider("compat-api"));
        unregister_api_providers("plain-source-id");
        assert!(!is_extension_provider("compat-api"));
        assert!(get_api_provider("compat-api").is_none());
    }

    #[test]
    fn same_api_extension_namespaces_resolve_independently() {
        let invoked_a = Arc::new(AtomicBool::new(false));
        let invoked_b = Arc::new(AtomicBool::new(false));
        register_extension_provider(
            test_provider("ns-shared-api", invoked_a.clone()),
            "namespace-a".into(),
        );
        register_extension_provider(
            test_provider("ns-shared-api", invoked_b.clone()),
            "namespace-b".into(),
        );

        // Each namespace resolves its own closure — never the other runtime's.
        let resolved_a = resolve_extension_provider("ns-shared-api", Some("namespace-a"))
            .expect("scoped lookup must resolve")
            .expect("namespace-a provides the api");
        let resolved_b = resolve_extension_provider("ns-shared-api", Some("namespace-b"))
            .expect("scoped lookup must resolve")
            .expect("namespace-b provides the api");
        invoke_stream(&resolved_a, "ns-shared-api");
        assert!(invoked_a.load(Ordering::SeqCst), "a's stream must run");
        assert!(!invoked_b.load(Ordering::SeqCst), "a must not run b's stream");
        invoke_stream(&resolved_b, "ns-shared-api");
        assert!(invoked_b.load(Ordering::SeqCst), "b's stream must run");

        // An unscoped lookup must fail closed: two owners, no arbitrary pick.
        assert!(get_api_provider("ns-shared-api").is_none());
        assert!(
            get_api_providers()
                .iter()
                .all(|p| p.api != "ns-shared-api"),
            "ambiguous apis must be excluded from unscoped listings"
        );
        assert!(is_extension_provider("ns-shared-api"));
        assert!(matches!(
            resolve_extension_provider("ns-shared-api", None),
            Err(ProviderScopeError::AmbiguousExtension { ref api, ref namespaces })
                if api == "ns-shared-api"
                    && namespaces == &vec!["namespace-a".to_owned(), "namespace-b".to_owned()]
        ));

        // Shutting down namespace-a leaves namespace-b fully functional and
        // never routes b's lookups through a's (now removed) entry.
        unregister_extension_providers("namespace-a");
        invoked_a.store(false, Ordering::SeqCst);
        invoked_b.store(false, Ordering::SeqCst);
        let survivor = resolve_extension_provider("ns-shared-api", Some("namespace-b"))
            .expect("scoped lookup must resolve")
            .expect("namespace-b still provides the api");
        invoke_stream(&survivor, "ns-shared-api");
        assert!(!invoked_a.load(Ordering::SeqCst), "b must never hit a's entry");
        assert!(matches!(
            resolve_extension_provider("ns-shared-api", Some("namespace-a")),
            Err(ProviderScopeError::ExtensionNotInNamespace { ref api, ref namespace })
                if api == "ns-shared-api" && namespace == "namespace-a"
        ));
        // Unscoped is now unambiguous and resolves to the survivor.
        let survivor = get_api_provider("ns-shared-api").expect("single owner resolves");
        invoke_stream(&survivor, "ns-shared-api");
        assert!(!invoked_a.load(Ordering::SeqCst));
        assert!(invoked_b.load(Ordering::SeqCst));

        unregister_extension_providers("namespace-b");
        assert!(get_api_provider("ns-shared-api").is_none());
    }

    #[test]
    fn scoped_lookup_falls_back_to_builtin_only_without_extension_owners() {
        let invoked_builtin = Arc::new(AtomicBool::new(false));
        register_api_provider(
            test_provider("builtin-fallback-api", invoked_builtin.clone()),
            Some("builtin-fallback-source".into()),
        );
        // No extension owns the api: a scoped session resolves through the
        // global path (`Ok(None)`), keeping builtin auth resolution intact.
        assert!(matches!(
            resolve_extension_provider("builtin-fallback-api", Some("any-namespace")),
            Ok(None)
        ));
        assert!(matches!(
            resolve_extension_provider("builtin-fallback-api", None),
            Ok(None)
        ));
        assert!(get_api_provider("builtin-fallback-api").is_some());
        unregister_api_providers("builtin-fallback-source");
    }

    #[test]
    fn builtin_and_extension_coexist_with_deterministic_priority() {
        let invoked_builtin = Arc::new(AtomicBool::new(false));
        let invoked_ext = Arc::new(AtomicBool::new(false));
        register_api_provider(
            test_provider("mixed-api", invoked_builtin.clone()),
            Some("mixed-builtin-source".into()),
        );
        register_extension_provider(
            test_provider("mixed-api", invoked_ext.clone()),
            "mixed-ext-namespace".into(),
        );

        // Unscoped: the builtin wins deterministically.
        let unscoped = get_api_provider("mixed-api").expect("builtin resolves globally");
        invoke_stream(&unscoped, "mixed-api");
        assert!(invoked_builtin.load(Ordering::SeqCst));
        assert!(!invoked_ext.load(Ordering::SeqCst), "unscoped must pick the builtin");

        // Scoped to the owning runtime: the extension's own entry wins.
        let scoped = resolve_extension_provider("mixed-api", Some("mixed-ext-namespace"))
            .expect("scoped lookup must resolve")
            .expect("the extension namespace provides the api");
        invoke_stream(&scoped, "mixed-api");
        assert!(invoked_ext.load(Ordering::SeqCst), "scoped must pick the extension");

        // Scoped to a foreign namespace: fail closed rather than serving the
        // builtin or leaking into the other runtime.
        assert!(matches!(
            resolve_extension_provider("mixed-api", Some("foreign-namespace")),
            Err(ProviderScopeError::ExtensionNotInNamespace { ref api, ref namespace })
                if api == "mixed-api" && namespace == "foreign-namespace"
        ));

        unregister_api_providers("mixed-builtin-source");
        unregister_extension_providers("mixed-ext-namespace");
    }
}
