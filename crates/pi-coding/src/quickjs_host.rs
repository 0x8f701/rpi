//! In-process QuickJS extension runtime (Phases 1-4).
//!
//! # OS isolation
//!
//! In-process QuickJS extensions are **not OS-isolated**: they share the host
//! process (thread-confined per extension, but same process, same user, same
//! filesystem and network access). The trust boundary for these extensions is
//! the extension trust/approval path — only trusted, approved extensions are
//! loaded at all. This is in deliberate contrast to process-hosted extensions,
//! which may be run inside the filesystem sandbox (`settings.sandbox`) via
//! [`crate::sandbox`]; an in-process extension cannot be sandboxed that way
//! without being re-hosted as a process extension.
//!
//! Each QuickJS extension runs in its own dedicated OS thread owning exactly one
//! [`AsyncRuntime`] + [`AsyncContext`] pair (rquickjs 0.12 runtimes are `!Send`
//! without the experimental `parallel` feature, so thread confinement is the
//! design). The thread speaks the standard extension protocol over in-memory
//! tokio channels, so the existing host-side machinery in [`crate::extensions`]
//! (handshake, load, invocation, cancellation, shutdown) works unchanged.
//!
//! Phase 1 surface:
//! - `pi.registerCommand` / `pi.registerTool` — synchronous, emit `Register`
//!   frames during the load phase only.
//! - `pi.setSessionName` — returns a Promise; the action is forwarded to the
//!   host action channel and the main loop settles the promise from the host's
//!   response frame.
//! - Everything else throws `unavailable in the quickjs runtime`.
//! - Per-invocation `ctx`: `{ mode, hasUI, cwd, model, signal }`.
//!
//! Phase 2 surface:
//! - `pi.on(event, handler)` — real event registration: allow-lists the 32
//!   authoritative events (the [`SUPPORTED_EVENTS`] allow-list), requires the
//!   `event_hooks` capability, emits one `Register` event_hook frame per event
//!   on first registration, and records the handler in the hooks registry.
//! - Hook dispatch with return reductions: `ExtensionInvocation::Event`
//!   invocations run every registered handler with a frozen
//!   `{ type, ...data, signal? }` event object and reduce the return values
//!   exactly like the host-side reduction machinery in [`crate::extensions`]:
//!   `before_agent_start` (systemPrompt/messages merge), `context`,
//!   `before_provider_request` / `before_provider_headers` (payload/headers
//!   mutation), `tool_call` (block), `tool_result`
//!   (content/details/isError/usage), `message_end` (message),
//!   `input` (text/images + handled), plus the `cancel`/`user_bash`
//!   early-break conditions and the trailing mutation merge.
//!   `session_before_compact` / `session_before_tree` receive the live signal
//!   object in `event.signal`.
//!
//! Phase 3 surface (`ctx.ui.*`):
//! - Interactive dialogs `select`/`confirm`/`input`/`editor` — Promise-returning,
//!   resolving to the host's answer and mapping a `Cancelled` response to
//!   `undefined` (or `false` for `confirm`).
//! - Queued/query methods `notify`/`setStatus`/`setWidget`/`setTitle`/
//!   `setEditorText`/`pasteToEditor`/`setWorkingMessage`/`setWorkingVisible`/
//!   `setWorkingIndicator`/`setHiddenThinkingLabel`/`setToolsExpanded` and the
//!   queries `getEditorText`/`getAllThemes`/`getTheme`/`setTheme`/
//!   `getToolsExpanded` — each issues an `ExtensionFrame::Request { Ui, ... }`
//!   and settles a JS promise from the host's response frame (the same
//!   action-channel round trip as `pi.setSessionName`).
//! - The `ui` capability is required synchronously; per-capability grants, the
//!   interactive-mode gate, and the no-UI-host gate are enforced host-side, so
//!   the promise rejects with the host's actionable failure (e.g.
//!   `ui_unavailable`).
//!
//! Phase 4 surface (the remaining extension API):
//! - Session actions on `pi` — `sendMessage`/`sendUserMessage`/`appendEntry`/
//!   `setLabel`/`setActiveTools`/`setThinkingLevel`/`setModel` (plus the
//!   Phase 1 `setSessionName`) and on `ctx` — `abort`/`compact`/`shutdown`/
//!   `waitForIdle`/`reload`. Each maps to the matching
//!   [`ExtensionRuntimeAction`] and returns a Promise settled from the host's
//!   response frame through the same action channel as `pi.setSessionName` /
//!   `ctx.ui.*` (the [`issue_runtime_request`] + [`route_action_response`]
//!   round trip). Argument coercion and the `session_actions` capability gate
//!   mirror the host's action-channel semantics: the capability is required
//!   synchronously, while host-side failures (permission_denied,
//!   action_failed, a rejected action host) reject the promise. Unlike
//!   process-hosted extensions, the in-process runtime carries no
//!   per-invocation idle snapshot, so the `deliverAs` default for
//!   `sendMessage`/`sendUserMessage` is pinned to `"steer"` (process hosts
//!   pick `"followUp"` when the session is idle); callers that need
//!   `followUp`/`nextTurn` pass `{ deliverAs }` explicitly.
//! - Tool streaming: `execute(callId, args, signal, onUpdate, ctx)` receives a
//!   real `onUpdate` callback that forwards each call to the host as an
//!   `ExtensionFrame::Update` carrying the invocation's request id, so the
//!   host's `PendingRequestEvent::Update` path delivers it to the caller's
//!   `on_update` handler in order, before the `Response` frame. `undefined` is
//!   forwarded as `{}`; a value that cannot round-trip to JSON throws
//!   synchronously.
//! - Cancellation shim: `ctx.signal` is a fresh, per-invocation
//!   AbortSignal-like object created by [`SIGNAL_BOOTSTRAP_SOURCE`]'s
//!   `AbortController` shim. When the host sends `Cancel` for the active
//!   invocation the main loop flips the shared abort atomic, fires the JS
//!   `abort` listeners (so handler-side awaits like
//!   `new Promise((_, reject) => ctx.signal.addEventListener("abort", ...))`
//!   settle), and rejects every pending JS -> host promise while sending the
//!   host a `Cancel` frame for each in-flight request (mirroring the process
//!   bridge's per-request `onAbort`). An invocation that was cancelled never
//!   succeeds: `run_invoke` maps the outcome to `cancelled` once the abort
//!   atomic is set, matching the `Promise.race([operation,
//!   cancelled(controller)])` cancellation semantics.
//!
//! Phase 5 surface (`pi.registerProvider` / `pi.unregisterProvider`):
//! - `pi.registerProvider({ id, label?, api?, capabilities?, stream })` —
//!   load-phase-only and `provider` capability-gated like `registerTool`.
//!   `stream` is an async JS function `(sessionId, messages, options)`
//!   returning an async iterator/generator (or a sync iterable) of compact
//!   events (`start`/`text`/`text_delta`/`thinking`/`tool_call`/`done`/
//!   `error`). The options object (holding the stream function) is stored in
//!   the shared providers registry; the host receives a `Register` provider
//!   descriptor frame and publishes the provider into the shared pi-ai
//!   registry under its `api` (defaults to the provider `id`), so models with
//!   `api: <provider-id>` route to the extension's stream.
//! - `pi.unregisterProvider(id)` — load-phase-only; removes the JS callable
//!   and emits an `UnregisterProvider` frame so resolution fails actionably.
//!   Re-registering the same id replaces it (last registration wins within a
//!   load phase; across reloads the new generation replaces the old one).
//! - Provider invocations arrive as `ExtensionInvocation::Provider`; the
//!   driver (`PROVIDER_STREAM_DRIVER_SOURCE`) iterates the JS stream and
//!   forwards each event to the host as an `ExtensionFrame::Update` (the same
//!   ordered path as tool `onUpdate`), then the final response completes the
//!   invocation. JS stream errors surface as typed stream errors host-side
//!   (see `crate::extensions::extension_provider_stream`).
//!
//! Phase 1 guards:
//! - [`QUICKJS_MEMORY_LIMIT_BYTES`] via `set_memory_limit` (libc allocator; the
//!   `rust-alloc` feature is intentionally not enabled because it would make the
//!   limit a no-op).
//! - A host-timeout deadline enforced by `set_interrupt_handler`; runaway
//!   bytecode is interrupted, and `ctx.signal.aborted` flips when the host sends
//!   `Cancel`.
//! - [`crate::extensions::DEFAULT_MAX_EXTENSION_FRAME_BYTES`] is enforced on
//!   outbound frames (responses, registrations, updates, actions).
//!
//! # Phase 4 cancellation bounds
//!
//! The shim mirrors the DOM AbortController surface extensions actually use
//! (`signal.aborted`, `signal.addEventListener`/`removeEventListener`, and
//! `controller.abort()`), not the full WHATWG spec. Documented bounds:
//! - A handler that awaits a promise with **no** abort listener can still only
//!   be bounded by the host-side request timeout (the extension thread cannot
//!   preempt an arbitrary pending promise). Cancellation-aware handlers settle
//!   promptly through the listener path.
//! - The abort atomic is shared per runtime and reset at every invocation
//!   start; a `Cancel` frame with no active invocation leaves the flag set
//!   until the next `run_invoke` clears it (harmless: the shim's signal is
//!   per-invocation and `run_invoke` rejects the outcome once cancelled).
//! - `event.signal` (the live signal injected into `session_before_compact` /
//!   `session_before_tree`) is the same per-invocation signal; a listener
//!   registered there fires on the same `Cancel`.
//! - `signal.reason` is `undefined` until aborted, then the reason passed to
//!   `abort(reason)` (or a generic `Error`); `dispatchEvent` is a no-op stub.
//!
//! # rquickjs teardown constraint (load-bearing)
//!
//! Rust closures installed on JS functions via [`Function::new`] must NEVER
//! capture rquickjs handles (`Ctx`, `Value`, `Object`, `Function`, ...). The
//! closure data is released when the owning JS function object is freed, which
//! during context teardown can happen *after* the captured object was already
//! freed — `JS_FreeRuntime` then aborts on a non-empty gc list (verified
//! empirically: a closure capturing an `Object` clone or a `Ctx` leaks;
//! releasing the function reference while the context is alive does not). The
//! pi methods therefore reach shared state through `ctx.globals()` lookups at
//! call time (see the `*_REGISTRY_GLOBAL` constants) and store JS handles only
//! in Rust-owned maps owned by the main loop, never in closure captures.

use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, anyhow, bail};
use pi_agent::{ToolCapability, ToolExecutionMode};
use pi_ai::{Model, Schema};
use rquickjs::{
    Array, AsyncContext, AsyncRuntime, Ctx, Exception, Function, Module, Object,
    Result as JsResult, String as JsString, Value as JsValue,
    prelude::*,
    promise::MaybePromise,
};
use serde_json::Value as JsonValue;
use serde_json::json;
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use uuid::Uuid;

/// serde_json object map with the standard key/value types.
type JsonMap = serde_json::Map<String, JsonValue>;

use crate::extensions::{
    EXTENSION_PROTOCOL_VERSION, ExtensionCapability, ExtensionCapabilityManifest,
    ExtensionCommandDescriptor, ExtensionEvent, ExtensionEventHookDescriptor, ExtensionFrame,
    ExtensionHost, ExtensionHostFrame, ExtensionHostRequest, ExtensionInvocation,
    ExtensionLaunch, ExtensionMode, ExtensionOverlayDescriptor, ExtensionProviderDescriptor,
    ExtensionRegistration, ExtensionRuntimeAction, ExtensionRuntimeOptions,
    ExtensionRuntimeRequest, ExtensionSpecRuntime, ExtensionToolDescriptor,
    ExtensionTransport, ProtocolResult, ExtensionFuture, ExtensionUiCapability,
    ExtensionUiRequest, OverlayEvent, OverlayInputDeclaration, RuntimeUiRequest,
};

/// The authoritative event allow-list. `pi.on` rejects any other name.
///
/// Every name has a production producer: either a session/agent event
/// forwarded by `session_extension_event` / `agent_extension_event`, a
/// direct emit/reduction in `application.rs`, or `extensions.rs`
/// `finish_reload` / `shutdown_with_reason`. Names that were once
/// allow-listed but had no producer (`project_trust`, `resources_discover`,
/// `overlay_open`, `overlay_close`) are deliberately rejected so extensions
/// fail fast at registration instead of silently never firing.
const SUPPORTED_EVENTS: [&str; 32] = [
    "trust_decision", "session_start",
    "session_info_changed",
    "session_before_switch", "session_before_fork", "session_before_compact", "session_compact",
    "session_shutdown", "session_before_tree", "session_tree", "context",
    "before_provider_request", "before_provider_headers", "after_provider_response",
    "before_agent_start", "agent_start", "agent_end", "agent_settled", "turn_start", "turn_end",
    "message_start", "message_update", "message_end", "tool_execution_start",
    "tool_execution_update", "tool_execution_end", "model_select", "thinking_level_select",
    "tool_call", "tool_result", "user_bash", "input",
];

/// Memory ceiling for a single in-process QuickJS extension runtime.
const QUICKJS_MEMORY_LIMIT_BYTES: usize = 64 * 1024 * 1024;
/// Maximum nesting depth for the JSON <-> JS value bridge (cyclic values fail
/// closed instead of recursing forever).
const VALUE_BRIDGE_MAX_DEPTH: usize = 128;
/// Bounded wait for the extension thread to exit during `terminate`.
const QUICKJS_TERMINATE_JOIN_TIMEOUT: Duration = Duration::from_secs(2);
/// Per-invocation AbortController/AbortSignal shim (Phase 4). `ctx.signal` is
/// a fresh signal created by `__piBeginInvocation()` at the start of every
/// invocation. Aborting (either from JS `controller.abort()` or from the host
/// `Cancel` frame via `__piAbortInvocation()`) flips the shared abort atomic
/// (so `signal.aborted` and `run_invoke`'s outcome mapping agree) and fires
/// the registered listeners synchronously, which is how handler-side awaits
/// reject. The signal's `aborted` getter also reads the live atomic so a host
/// `Cancel` is visible immediately, and pending action promises are rejected
/// by the main loop right after the listeners fire (see
/// [`cancel_pending_actions`]).
///
/// The shim implements the AbortSignal surface extensions actually use
/// (`aborted`, `reason`, `addEventListener`/`removeEventListener`, and the
/// controller's `abort`); `dispatchEvent` is a no-op stub. It is not a full
/// WHATWG implementation: `onabort` handlers, `AbortSignal.timeout`, and
/// `signal.throwIfAborted` are absent (documented bound).
const SIGNAL_BOOTSTRAP_SOURCE: &str = r#"
(() => {
  class AbortSignalShim {
    constructor() {
      this._aborted = false;
      this._reason = undefined;
      this._listeners = [];
    }
    get aborted() {
      // The live atomic mirrors the host's Cancel frames, which fire even
      // when no JS listener was registered yet.
      return this._aborted || __piSignalAborted();
    }
    get reason() {
      return this._aborted ? this._reason : undefined;
    }
    addEventListener(type, listener) {
      if (type === "abort" && typeof listener === "function" && !this.aborted) {
        this._listeners.push(listener);
      }
    }
    removeEventListener(type, listener) {
      if (type !== "abort") return;
      const index = this._listeners.indexOf(listener);
      if (index >= 0) this._listeners.splice(index, 1);
    }
    dispatchEvent() { return false; }
  }
  class AbortControllerShim {
    constructor() {
      this.signal = new AbortSignalShim();
    }
    abort(reason) {
      const signal = this.signal;
      if (signal._aborted) return;
      signal._aborted = true;
      signal._reason = reason === undefined ? new Error("This operation was aborted") : reason;
      // Flip the shared atomic so `run_invoke` maps the outcome to
      // "cancelled" and `signal.aborted` reads true from any context.
      __piAbortFlag();
      const listeners = signal._listeners;
      signal._listeners = [];
      for (const listener of listeners) {
        try { listener.call(signal, { type: "abort", target: signal }); } catch (error) {}
      }
    }
  }
  let currentController = null;
  globalThis.__piBeginInvocation = () => {
    currentController = new AbortControllerShim();
    return currentController.signal;
  };
  globalThis.__piAbortInvocation = () => {
    if (currentController) currentController.abort();
  };
})()
"#;
/// JS bootstrap for `ctx.ui` (Phase 3): argument coercion,
/// response mapping, cancel semantics, and the `{ timeout }` option. The
/// low-level `__piUiRequest` global (a Rust closure) issues the
/// `ExtensionFrame::Request { Ui, ... }` and returns a Promise of the raw
/// `ExtensionUiResponse` JSON object; the mapping to the observable JS value
/// happens here. Fire-and-forget methods
/// still return the underlying Promise so failures (e.g. a missing UI host)
/// are observable and actionable; awaiting one yields `undefined`, matching
/// the process bridge's observable return value.
const UI_BOOTSTRAP_SOURCE: &str = r#"
(() => {
  function timeoutMs(options) {
    const value = options && options.timeout;
    return Number.isFinite(value) && value >= 0 ? Math.floor(value) : undefined;
  }
  function unavailable(method) {
    throw new Error(`${method} is unavailable in the quickjs runtime`);
  }
  return Object.freeze({
    async select(title, options, opts) {
      const normalized = options.map(option => typeof option === "string"
        ? { value: option, label: option }
        : {
            value: String(option.value === undefined ? option.label : option.value),
            label: String(option.label),
            ...(option.description === undefined ? {} : { description: String(option.description) }),
          });
      const response = await __piUiRequest({
        type: "select",
        title: String(title),
        options: normalized,
      }, timeoutMs(opts));
      return response && response.type === "selected" ? response.value : undefined;
    },
    async confirm(title, message, opts) {
      const response = await __piUiRequest({
        type: "confirm",
        title: String(title),
        message: String(message),
      }, timeoutMs(opts));
      return response && response.type === "confirmed" ? response.confirmed : false;
    },
    async input(title, placeholder, opts) {
      const response = await __piUiRequest({
        type: "input",
        title: String(title),
        ...(placeholder === undefined ? {} : { placeholder: String(placeholder) }),
      }, timeoutMs(opts));
      return response && response.type === "input" ? response.value : undefined;
    },
    async editor(title, prefill, opts) {
      const response = await __piUiRequest({
        type: "editor",
        title: String(title),
        ...(prefill === undefined ? {} : { prefill: String(prefill) }),
      }, timeoutMs(opts));
      return response && response.type === "edited" ? response.value : undefined;
    },
    notify(message, level = "info") {
      return __piUiRequest({ type: "notify", message: String(message), level }, undefined);
    },
    setStatus(key, text) {
      return __piUiRequest({
        type: "status",
        key: String(key),
        ...(text === undefined ? {} : { text: String(text) }),
      }, undefined);
    },
    setWidget(key, content, options) {
      if (content !== undefined && (!Array.isArray(content) || content.some(line => typeof line !== "string"))) {
        unavailable("ui.setWidget component factories");
      }
      return __piUiRequest({
        type: "widget",
        key: String(key),
        ...(content === undefined ? {} : { lines: content }),
        placement: options && options.placement === "belowEditor" ? "below_editor" : "above_editor",
      }, undefined);
    },
    setTitle(title) {
      return __piUiRequest({ type: "title", title: String(title) }, undefined);
    },
    setEditorText(text) {
      return __piUiRequest({ type: "set_editor_text", text: String(text) }, undefined);
    },
    onTerminalInput: () => unavailable("ui.onTerminalInput"),
    setWorkingMessage(message) {
      return __piUiRequest({
        type: "set_working_message",
        ...(message === undefined ? {} : { message: String(message) }),
      }, undefined);
    },
    setWorkingVisible(visible) {
      return __piUiRequest({ type: "set_working_visible", visible: Boolean(visible) }, undefined);
    },
    setWorkingIndicator(options) {
      if (options !== undefined && (options === null || typeof options !== "object" || Array.isArray(options))) {
        throw new Error("ui.setWorkingIndicator requires an options object");
      }
      const normalized = options === undefined ? undefined : {
        ...(options.frames === undefined ? {} : {
          frames: Array.isArray(options.frames) ? options.frames.map(String) : (() => { throw new Error("ui.setWorkingIndicator frames must be an array"); })(),
        }),
        ...(options.intervalMs === undefined ? {} : {
          intervalMs: Number.isSafeInteger(options.intervalMs) && options.intervalMs >= 0
            ? options.intervalMs
            : (() => { throw new Error("ui.setWorkingIndicator intervalMs must be a non-negative integer"); })(),
        }),
      };
      return __piUiRequest({
        type: "set_working_indicator",
        ...(normalized === undefined ? {} : { options: normalized }),
      }, undefined);
    },
    setHiddenThinkingLabel(label) {
      return __piUiRequest({
        type: "set_hidden_thinking_label",
        ...(label === undefined ? {} : { label: String(label) }),
      }, undefined);
    },
    setFooter: () => unavailable("ui.setFooter component factories"),
    setHeader: () => unavailable("ui.setHeader component factories"),
    custom: () => unavailable("ui.custom component factories"),
    pasteToEditor(text) {
      return __piUiRequest({ type: "paste_to_editor", text: String(text) }, undefined);
    },
    getEditorText() {
      return __piUiRequest({ type: "get_editor_text" }, undefined)
        .then(response => response && response.value !== undefined ? response.value : "");
    },
    addAutocompleteProvider: () => unavailable("ui.addAutocompleteProvider"),
    setEditorComponent: () => unavailable("ui.setEditorComponent factories"),
    getEditorComponent: () => unavailable("ui.getEditorComponent"),
    // Process hosts read the active theme from the invocation snapshot; the
    // in-process runtime carries no theme snapshot, so the getter is null
    // (matching their fallback when the snapshot has no theme).
    get theme() { return null; },
    getAllThemes() {
      return __piUiRequest({ type: "get_all_themes" }, undefined)
        .then(response => Array.isArray(response && response.themes) ? response.themes : []);
    },
    getTheme(name) {
      return __piUiRequest({ type: "get_theme", name: String(name) }, undefined)
        .then(response => response && response.theme);
    },
    setTheme(name) {
      const requestedName = typeof name === "string" ? name : name && name.name;
      if (typeof requestedName !== "string" || !requestedName) {
        return Promise.resolve({ success: false, error: "process-hosted extensions can only set themes by name" });
      }
      return __piUiRequest({ type: "set_theme", name: requestedName }, undefined)
        .then(response => {
          const error = response && response.error;
          return {
            success: Boolean(response && response.success === true),
            ...(error === undefined ? {} : { error: String(error) }),
          };
        });
    },
    getToolsExpanded() {
      return __piUiRequest({ type: "get_tools_expanded" }, undefined)
        .then(response => Boolean(response && response.expanded));
    },
    setToolsExpanded(expanded) {
      return __piUiRequest({ type: "set_tools_expanded", expanded: Boolean(expanded) }, undefined);
    },
    // Extension-rendered overlays (SAFE surface): the extension supplies
    // content rows only; the host draws the border and owns focus/key
    // routing. `setRows` pushes dynamic rows into the overlay with `id`
    // (sanitized host-side: ≤100 rows × ≤200 chars, redacted); `open` opens
    // the overlay panel (auto-open from an event handler).
    setRows(id, rows) {
      if (typeof id !== "string" || !id) {
        return Promise.reject(new Error("overlay.setRows requires a non-empty overlay id"));
      }
      if (!Array.isArray(rows)) {
        return Promise.reject(new Error("overlay.setRows requires an array of rows"));
      }
      const normalized = rows.map(row => {
        if (typeof row === "string") return { text: row };
        if (row !== null && typeof row === "object" && typeof row.text === "string") {
          return {
            text: row.text,
            ...(row.style === undefined ? {} : { style: String(row.style) }),
          };
        }
        throw new Error("overlay rows must be strings or { text, style? } objects");
      });
      return __piUiRequest({ type: "overlay_set_rows", id: String(id), rows: normalized }, undefined);
    },
    open(id, options) {
      if (typeof id !== "string" || !id) {
        return Promise.reject(new Error("overlay.open requires a non-empty overlay id"));
      }
      if (options !== undefined && (options === null || typeof options !== "object" || Array.isArray(options))) {
        return Promise.reject(new Error("overlay.open options must be an object"));
      }
      const nonCapturing = Boolean(options && options.nonCapturing);
      return __piUiRequest({ type: "overlay_open", id: String(id), nonCapturing }, undefined)
        .then(response => response && response.type === "overlay_opened" ? undefined : undefined);
    },
  });
})()
"#;
/// Session-action methods (Phase 4), shared by `pi` (sendMessage/
/// sendUserMessage/appendEntry/setLabel/setActiveTools/setThinkingLevel/
/// setModel) and `ctx` (abort/compact/shutdown/waitForIdle/reload), mirroring
/// the process host's createApi/createContext surface for argument coercion. Each
/// method issues the matching [`ExtensionRuntimeAction`] through the low-level
/// `__piActionRequest` global (a Rust closure) and returns the settling
/// Promise. The in-process runtime has no per-invocation idle snapshot, so
/// the `deliverAs` default is pinned to "steer" (process hosts pick "followUp"
/// for an idle session); callers pass `{ deliverAs }` explicitly for the
/// others. Fire-and-forget methods still return the underlying
/// Promise so failures are observable and actionable, matching the Phase 3
/// `ctx.ui` convention.
const SESSION_ACTIONS_BOOTSTRAP_SOURCE: &str = r#"
(() => {
  function request(action) {
    return __piActionRequest(action);
  }
  return Object.freeze({
    sendMessage(message, options = {}) {
      return request({
        kind: "send_message",
        message,
        delivery: options.deliverAs ?? "steer",
        triggerTurn: options.triggerTurn ?? false,
      });
    },
    sendUserMessage(content, options = {}) {
      return request({
        kind: "send_user_message",
        content,
        delivery: options.deliverAs ?? "steer",
      });
    },
    appendEntry(customType, data) {
      return request({
        kind: "append_entry",
        customType: String(customType),
        ...(data === undefined ? {} : { data }),
      });
    },
    setLabel(entryId, label) {
      return request({
        kind: "set_label",
        entryId: String(entryId),
        ...(label === undefined ? {} : { label }),
      });
    },
    setActiveTools(toolNames) {
      return request({ kind: "set_active_tools", toolNames: toolNames.map(String) });
    },
    setThinkingLevel(level) {
      return request({ kind: "set_thinking_level", level });
    },
    setModel(model) {
      return request({ kind: "set_model", model });
    },
    abort() { return request({ kind: "abort" }); },
    shutdown() { return request({ kind: "shutdown" }); },
    compact(instructionsOrOptions) {
      const customInstructions = typeof instructionsOrOptions === "string"
        ? instructionsOrOptions
        : instructionsOrOptions?.customInstructions;
      return request({
        kind: "compact",
        ...(customInstructions === undefined ? {} : { customInstructions }),
      });
    },
    waitForIdle() { return request({ kind: "wait_for_idle" }); },
    reload() { return request({ kind: "reload" }); },
  });
})()
"#;
/// Provider stream driver (registerProvider): calls the registered stream
/// function with `(sessionId, messages, options)` and forwards every event it
/// yields through the `onEvent` bridge (a Rust closure that emits an
/// `ExtensionFrame::Update`). Both async iterators and plain sync iterables
/// are accepted; anything else throws an actionable error that surfaces as a
/// typed stream error.
const PROVIDER_STREAM_DRIVER_SOURCE: &str = r#"
(() => {
  globalThis.__piRunProviderStream = async (streamFn, sessionId, messages, options, onEvent) => {
    if (typeof streamFn !== "function") {
      throw new Error("provider stream handler is not a function");
    }
    const result = await streamFn(sessionId, messages, options);
    let iterable = null;
    if (result != null) {
      if (typeof result[Symbol.asyncIterator] === "function") {
        iterable = result;
      } else if (typeof result[Symbol.iterator] === "function") {
        iterable = result;
      }
    }
    if (!iterable) {
      throw new Error("provider stream must return an async iterator or an iterable");
    }
    for await (const event of iterable) {
      onEvent(event);
    }
  };
})()
"#;
/// pi methods that do not exist yet in the in-process runtime. Calling any of
/// them throws an actionable error instead of silently no-op'ing.
const UNAVAILABLE_PI_METHODS: &[&str] = &[
    "getThinkingLevel",
    "getActiveTools",
    "getAllTools",
    "getCommands",
    "getFlag",
    "registerShortcut",
    "registerFlag",
    "registerMessageRenderer",
    "registerEntryRenderer",
    "exec",
    "events",
];

/// Extension host that launches each QuickJS extension inside its own
/// thread-confined [`AsyncRuntime`] + [`AsyncContext`].
#[derive(Clone, Debug, Default)]
pub struct QuickJsExtensionHost;

impl ExtensionHost for QuickJsExtensionHost {
    fn launch(
        &self,
        launch: ExtensionLaunch,
    ) -> ExtensionFuture<'_, Result<Arc<dyn ExtensionTransport>>> {
        Box::pin(async move {
            let spec = launch.spec;
            spec.validate_before_launch()?;
            let entry = match &spec.runtime {
                ExtensionSpecRuntime::QuickJs { entry } => entry.clone(),
                runtime => bail!("QuickJsExtensionHost cannot launch {runtime:?} runtime"),
            };
            let working_directory = spec
                .working_directory
                .canonicalize()
                .with_context(|| format!("resolving working directory for extension {}", spec.id))?;
            let entry = entry
                .canonicalize()
                .with_context(|| format!("resolving QuickJS extension entry for {}", spec.id))?;
            if !entry.starts_with(&working_directory) || !entry.is_file() {
                bail!("QuickJS extension entry must remain inside its working directory");
            }
            let source = std::fs::read_to_string(&entry)
                .with_context(|| format!("reading QuickJS extension entry {}", entry.display()))?;
            let entry_leaf = entry
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("extension")
                .to_owned();
            let thread_name = format!("pi-quickjs-{}", spec.id);
            let diagnostic = format!("quickjs extension {} (entry: {entry_leaf})", spec.id);

            let (to_js_tx, to_js_rx) = mpsc::unbounded_channel::<ExtensionHostFrame>();
            let (from_js_tx, from_js_rx) = mpsc::unbounded_channel::<ExtensionFrame>();
            let interrupt_deadline = Arc::new(AtomicU64::new(0));
            let shutdown = Arc::new(AtomicBool::new(false));
            let abort_flag = Arc::new(AtomicBool::new(false));
            let last_error = Arc::new(Mutex::new(None));

            let thread = std::thread::Builder::new()
                .name(thread_name)
                .spawn({
                    let spec_id = spec.id.clone();
                    let working_directory = spec.working_directory.clone();
                    let capabilities = spec.permissions.capabilities.clone();
                    let ui_capabilities = spec.permissions.ui_capabilities.clone();
                    let timeouts = launch.timeouts.clone();
                    let last_error = last_error.clone();
                    let interrupt_deadline = interrupt_deadline.clone();
                    let shutdown = shutdown.clone();
                    let abort_flag = abort_flag.clone();
                    move || {
                        run_extension_thread(
                            spec_id,
                            working_directory.display().to_string(),
                            source,
                            entry_leaf,
                            capabilities,
                            ui_capabilities,
                            timeouts,
                            to_js_rx,
                            from_js_tx,
                            interrupt_deadline,
                            shutdown,
                            abort_flag,
                            last_error,
                        );
                    }
                })
                .with_context(|| format!("spawning quickjs extension thread for {}", spec.id))?;

            Ok(Arc::new(QuickJsTransport {
                to_js: to_js_tx,
                from_js: AsyncMutex::new(from_js_rx),
                join: Mutex::new(Some(thread)),
                shutdown_sent: AtomicBool::new(false),
                diagnostic,
                last_error,
            }) as Arc<dyn ExtensionTransport>)
        })
    }
}

struct QuickJsTransport {
    to_js: mpsc::UnboundedSender<ExtensionHostFrame>,
    from_js: AsyncMutex<mpsc::UnboundedReceiver<ExtensionFrame>>,
    join: Mutex<Option<JoinHandle<()>>>,
    shutdown_sent: AtomicBool,
    diagnostic: String,
    last_error: Arc<Mutex<Option<String>>>,
}

impl ExtensionTransport for QuickJsTransport {
    fn send(&self, frame: &ExtensionHostFrame) -> ExtensionFuture<'_, Result<()>> {
        let frame = frame.clone();
        Box::pin(async move {
            self.to_js
                .send(frame)
                .map_err(|_| anyhow!("quickjs extension transport closed"))
        })
    }

    fn receive(&self) -> ExtensionFuture<'_, Result<Option<ExtensionFrame>>> {
        Box::pin(async move {
            let mut receiver = self.from_js.lock().await;
            Ok(receiver.recv().await)
        })
    }

    fn terminate(&self) -> ExtensionFuture<'_, Result<()>> {
        Box::pin(async move {
            if !self.shutdown_sent.swap(true, Ordering::AcqRel) {
                let _ = self.to_js.send(ExtensionHostFrame::Shutdown {
                    reason: "terminate".to_owned(),
                });
            }
            let join = self
                .join
                .lock()
                .expect("quickjs join mutex poisoned")
                .take();
            let Some(join) = join else {
                return Ok(());
            };
            let joined = tokio::time::timeout(
                QUICKJS_TERMINATE_JOIN_TIMEOUT,
                tokio::task::spawn_blocking(move || join.join()),
            )
            .await;
            match joined {
                Ok(Ok(Ok(()))) => Ok(()),
                Ok(Ok(Err(_))) => Err(anyhow!("quickjs extension thread panicked")),
                Ok(Err(_)) => Err(anyhow!("joining quickjs extension thread failed")),
                Err(_) => Err(anyhow!(
                    "quickjs extension thread did not exit within the shutdown deadline"
                )),
            }
        })
    }

    fn diagnostic_context(&self) -> String {
        // `last_error` is diagnostics-only with trivial critical sections, so
        // a poisoned lock (a panic in the worker thread while it held the
        // guard) must never take down the host's diagnostic path: recover the
        // value instead of panicking.
        let last_error = self
            .last_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        match last_error {
            Some(error) => format!("{}: {error}", self.diagnostic),
            None => self.diagnostic.clone(),
        }
    }
}

fn run_extension_thread(
    spec_id: String,
    working_directory: String,
    entry_source: String,
    entry_leaf: String,
    capabilities: BTreeSet<ExtensionCapability>,
    ui_capabilities: BTreeSet<ExtensionUiCapability>,
    timeouts: ExtensionRuntimeOptions,
    mut to_js_rx: mpsc::UnboundedReceiver<ExtensionHostFrame>,
    from_js_tx: mpsc::UnboundedSender<ExtensionFrame>,
    interrupt_deadline: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    abort_flag: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
) {
    let tokio_runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            *last_error.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some(format!("building tokio runtime: {error}"));
            return;
        }
    };
    let loop_last_error = last_error.clone();
    let outcome = tokio_runtime.block_on(async {
        let js_runtime =
            AsyncRuntime::new().map_err(|error| format!("creating QuickJS runtime: {error}"))?;
        js_runtime.set_memory_limit(QUICKJS_MEMORY_LIMIT_BYTES).await;
        {
            let deadline = interrupt_deadline.clone();
            let shutdown = shutdown.clone();
            js_runtime
                .set_interrupt_handler(Some(Box::new(move || {
                    shutdown.load(Ordering::Relaxed)
                        || {
                            // Paired with the Release stores of
                            // `interrupt_deadline` (set before invoking JS and
                            // cleared after), so the deadline written by the
                            // worker is visible to the interrupt handler.
                            let deadline = deadline.load(Ordering::Acquire);
                            deadline != 0 && now_millis() >= deadline
                        }
                })))
                .await;
        }
        let ctx = AsyncContext::full(&js_runtime)
            .await
            .map_err(|error| format!("creating QuickJS context: {error}"))?;
        ctx.async_with(async move |ctx| {
            main_loop(
                ctx,
                spec_id,
                working_directory,
                entry_source,
                entry_leaf,
                capabilities,
                ui_capabilities,
                timeouts,
                &mut to_js_rx,
                from_js_tx,
                interrupt_deadline,
                shutdown,
                abort_flag,
                loop_last_error,
            )
            .await
        })
        .await
    });
    if let Err(error) = outcome {
        *last_error.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error);
    }
}

#[derive(Clone)]
struct HelloState {
    mode: ExtensionMode,
    cwd: String,
    has_ui: bool,
}

async fn main_loop<'js>(
    ctx: Ctx<'js>,
    spec_id: String,
    working_directory: String,
    entry_source: String,
    entry_leaf: String,
    capabilities: BTreeSet<ExtensionCapability>,
    ui_capabilities: BTreeSet<ExtensionUiCapability>,
    timeouts: ExtensionRuntimeOptions,
    to_js_rx: &mut mpsc::UnboundedReceiver<ExtensionHostFrame>,
    from_js_tx: mpsc::UnboundedSender<ExtensionFrame>,
    interrupt_deadline: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    abort_flag: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
) -> Result<(), String> {
    let commands_registry = Object::new(ctx.clone()).map_err(|error| js_err_text(&error))?;
    let tools_registry = Object::new(ctx.clone()).map_err(|error| js_err_text(&error))?;
    let hooks_registry = Object::new(ctx.clone()).map_err(|error| js_err_text(&error))?;
    let providers_registry = Object::new(ctx.clone()).map_err(|error| js_err_text(&error))?;
    let overlays_registry = Object::new(ctx.clone()).map_err(|error| js_err_text(&error))?;
    // Registries live on the global object so the pi methods (Rust closures
    // stored on JS functions) can reach them through ctx.globals() at call time
    // instead of capturing JS handles.
    ctx.globals()
        .set(COMMANDS_REGISTRY_GLOBAL, commands_registry.clone())
        .map_err(|error| js_err_text(&error))?;
    ctx.globals()
        .set(TOOLS_REGISTRY_GLOBAL, tools_registry.clone())
        .map_err(|error| js_err_text(&error))?;
    ctx.globals()
        .set(HOOKS_REGISTRY_GLOBAL, hooks_registry.clone())
        .map_err(|error| js_err_text(&error))?;
    ctx.globals()
        .set(PROVIDERS_REGISTRY_GLOBAL, providers_registry.clone())
        .map_err(|error| js_err_text(&error))?;
    ctx.globals()
        .set(OVERLAYS_REGISTRY_GLOBAL, overlays_registry.clone())
        .map_err(|error| js_err_text(&error))?;
    ctx.eval::<(), _>(PROVIDER_STREAM_DRIVER_SOURCE)
        .map_err(|error| format!("bootstrapping extension provider stream: {}", js_err_text(&error)))?;
    let in_load = Arc::new(AtomicBool::new(false));
    let pending_actions: PendingActions<'js> = Arc::new(Mutex::new(HashMap::new()));

    // AbortController/AbortSignal shim (Phase 4): `ctx.signal.aborted` is a
    // live getter backed by an atomic the host flips when it sends `Cancel`,
    // and each invocation gets a fresh signal from `__piBeginInvocation` (the
    // current controller is stored JS-side so `__piAbortInvocation` can fire
    // its listeners without any Rust-held JS handle).
    let aborted_fn = Function::new(ctx.clone(), {
        let abort_flag = abort_flag.clone();
        move || -> bool { abort_flag.load(Ordering::Acquire) }
    })
    .map_err(|error| js_err_text(&error))?;
    ctx.globals()
        .set(SIGNAL_ABORTED_GLOBAL, aborted_fn)
        .map_err(|error| js_err_text(&error))?;
    let abort_flag_fn = Function::new(ctx.clone(), {
        let abort_flag = abort_flag.clone();
        move || abort_flag.store(true, Ordering::Release)
    })
    .map_err(|error| js_err_text(&error))?;
    ctx.globals()
        .set(ABORT_FLAG_GLOBAL, abort_flag_fn)
        .map_err(|error| js_err_text(&error))?;
    ctx.eval::<(), _>(SIGNAL_BOOTSTRAP_SOURCE)
        .map_err(|error| format!("bootstrapping extension signal: {}", js_err_text(&error)))?;
    // `globalThis` assignments inside the bootstrap create global-object
    // properties, so both entry points are reachable through globals().get.
    let begin_invocation: Function = ctx
        .globals()
        .get("__piBeginInvocation")
        .map_err(|error| js_err_text(&error))?;
    let abort_invocation: Function = ctx
        .globals()
        .get("__piAbortInvocation")
        .map_err(|error| js_err_text(&error))?;

    // `ctx.ui` (Phase 3): the low-level request bridge is a Rust closure
    // installed as a global property; the frozen ui object built by
    // UI_BOOTSTRAP_SOURCE lives under UI_GLOBAL so `build_context_object` can
    // attach it to every invocation context without any closure capture.
    let ui_request = build_ui_request_fn(
        &ctx,
        &pending_actions,
        &from_js_tx,
        timeouts.max_frame_bytes,
        &capabilities,
    )
    .map_err(|error| js_err_text(&error))?;
    ctx.globals()
        .set(UI_REQUEST_GLOBAL, ui_request)
        .map_err(|error| js_err_text(&error))?;
    let ui: Object = ctx
        .eval(UI_BOOTSTRAP_SOURCE)
        .map_err(|error| format!("bootstrapping extension ui: {}", js_err_text(&error)))?;
    ctx.globals()
        .set(UI_GLOBAL, ui)
        .map_err(|error| js_err_text(&error))?;

    // Session actions (Phase 4): the low-level bridge is a Rust closure under
    // ACTION_REQUEST_GLOBAL; the frozen method object built by
    // SESSION_ACTIONS_BOOTSTRAP_SOURCE lives under SESSION_ACTIONS_GLOBAL so
    // both `build_pi` and `build_context_object` can attach the pi-visible
    // and ctx-visible subsets without any closure capture.
    let action_request = build_action_request_fn(
        &ctx,
        &pending_actions,
        &from_js_tx,
        timeouts.max_frame_bytes,
        &capabilities,
    )
    .map_err(|error| js_err_text(&error))?;
    ctx.globals()
        .set(ACTION_REQUEST_GLOBAL, action_request)
        .map_err(|error| js_err_text(&error))?;
    let session_actions: Object = ctx
        .eval(SESSION_ACTIONS_BOOTSTRAP_SOURCE)
        .map_err(|error| format!("bootstrapping extension session actions: {}", js_err_text(&error)))?;
    ctx.globals()
        .set(SESSION_ACTIONS_GLOBAL, session_actions)
        .map_err(|error| js_err_text(&error))?;

    let pi = build_pi(
        &ctx,
        &in_load,
        &pending_actions,
        &from_js_tx,
        timeouts.max_frame_bytes,
        &capabilities,
    )
    .map_err(|error| js_err_text(&error))?;

    let mut active: Option<(String, std::pin::Pin<Box<dyn Future<Output = ProtocolResult> + 'js>>)> =
        None;
    // Host frames received while an invocation is settling are buffered here
    // (arrival order) instead of being dropped; the idle branch drains the
    // buffer before blocking on the channel.
    let mut pending_frames: VecDeque<ExtensionHostFrame> = VecDeque::new();
    let mut hello: Option<HelloState> = None;
    let mut shutting_down = false;

    while !shutting_down {
        // An invocation is in flight: keep driving it while still routing host
        // frames (cancel notifications, action responses, shutdown).
        if let Some((id, mut future)) = active.take() {
            let outcome = loop {
                tokio::select! {
                    biased;
                    outcome = &mut future => break outcome,
                    frame = to_js_rx.recv() => {
                        match frame {
                            Some(ExtensionHostFrame::Cancel { id: cancel_id }) if cancel_id == id => {
                                abort_flag.store(true, Ordering::Release);
                                // Fire the JS abort listeners so handler-side
                                // awaits settle, then reject every pending JS
                                // -> host promise (and cancel those requests
                                // host-side), mirroring the process bridge's
                                // per-request onAbort.
                                let _ = abort_invocation.call::<_, ()>(());
                                cancel_pending_actions(
                                    &ctx,
                                    &pending_actions,
                                    &from_js_tx,
                                    timeouts.max_frame_bytes,
                                    "extension invocation was cancelled",
                                );
                            }
                            Some(ExtensionHostFrame::Shutdown { reason }) => {
                                shutdown.store(true, Ordering::Release);
                                // Mirrors the process bridge's shutdown path:
                                // every in-flight invocation is aborted so its
                                // handler-side awaits settle promptly instead
                                // of waiting for the interrupt handler.
                                let _ = abort_invocation.call::<_, ()>(());
                                reject_pending_actions(
                                    &ctx,
                                    &pending_actions,
                                    &format!("extension is shutting down: {reason}"),
                                );
                                break ProtocolResult::failure(
                                    "shutdown",
                                    format!("extension is shutting down: {reason}"),
                                );
                            }
                            Some(ExtensionHostFrame::Response { id: action_id, result }) => {
                                route_action_response(&ctx, &pending_actions, action_id, result)
                                    .await;
                            }
                            // Any other frame (a new host Request, a Cancel for
                            // a stale id, ...) that arrives while the
                            // invocation future is still settling is buffered
                            // in arrival order and processed by the idle
                            // branch below. Dropping it here would lose the
                            // frame: the future can take several JS-job
                            // iterations to finish after a Cancel, and a
                            // follow-up Request in that window must not
                            // vanish (it would wedge the runtime until the
                            // host-side request timeout).
                            Some(frame) => pending_frames.push_back(frame),
                            None => {
                                reject_pending_actions(
                                    &ctx,
                                    &pending_actions,
                                    "extension host channel closed",
                                );
                                break ProtocolResult::failure(
                                    "transport_closed",
                                    "extension host channel closed",
                                );
                            }
                        }
                    }
                }
            };
            let _ = send_frame(
                &from_js_tx,
                ExtensionFrame::Response { id, result: outcome },
                timeouts.max_frame_bytes,
            );
            continue;
        }

        // Drain frames buffered while the previous invocation was settling
        // before blocking on the channel, preserving arrival order.
        let frame = match pending_frames.pop_front() {
            Some(frame) => Some(frame),
            None => to_js_rx.recv().await,
        };
        match frame {
            Some(ExtensionHostFrame::Hello {
                protocol_version,
                cwd,
                mode,
                ..
            }) => {
                if protocol_version != EXTENSION_PROTOCOL_VERSION {
                    *last_error.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(
                        format!(
                            "unsupported extension protocol version {protocol_version}; host requires {EXTENSION_PROTOCOL_VERSION}"
                        ),
                    );
                    break;
                }
                hello = Some(HelloState {
                    mode,
                    cwd,
                    has_ui: !ui_capabilities.is_empty(),
                });
                let frame = ExtensionFrame::Hello {
                    protocol_version: EXTENSION_PROTOCOL_VERSION,
                    manifest: ExtensionCapabilityManifest {
                        id: spec_id.clone(),
                        name: format!("QuickJS extension {spec_id}"),
                        version: "1.0.0".to_owned(),
                        capabilities: capabilities.clone(),
                        ui_capabilities: ui_capabilities.clone(),
                    },
                };
                if send_frame(&from_js_tx, frame, timeouts.max_frame_bytes).is_err() {
                    break;
                }
            }
            Some(ExtensionHostFrame::Request { id, request, .. }) => match request {
                ExtensionHostRequest::Load => {
                    in_load.store(true, Ordering::Release);
                    interrupt_deadline.store(
                        now_millis().saturating_add(deadline_millis(timeouts.load_timeout)),
                        Ordering::Release,
                    );
                    let load_result =
                        load_extension(&ctx, &entry_source, &entry_leaf, pi.clone()).await;
                    interrupt_deadline.store(0, Ordering::Release);
                    in_load.store(false, Ordering::Release);
                    let result = match load_result {
                        Ok(()) => ProtocolResult::success(JsonValue::Null),
                        Err(message) => ProtocolResult::failure("load_failed", message),
                    };
                    if send_frame(
                        &from_js_tx,
                        ExtensionFrame::Response { id, result },
                        timeouts.max_frame_bytes,
                    )
                    .is_err()
                    {
                        break;
                    }
                }
                ExtensionHostRequest::Initialize => {
                    if send_frame(
                        &from_js_tx,
                        ExtensionFrame::Response {
                            id,
                            result: ProtocolResult::success(JsonValue::Null),
                        },
                        timeouts.max_frame_bytes,
                    )
                    .is_err()
                    {
                        break;
                    }
                }
                ExtensionHostRequest::Invoke { invocation, context } => {
                    let model = context.as_ref().and_then(|context| context.model.clone());
                    let hello = hello.clone().unwrap_or(HelloState {
                        mode: ExtensionMode::Tui,
                        cwd: working_directory.clone(),
                        has_ui: false,
                    });
                    // A fresh AbortController per invocation: `ctx.signal` and
                    // `event.signal` are this signal, so a host `Cancel` fires
                    // exactly this invocation's listeners.
                    let signal = begin_invocation
                        .call::<_, JsValue<'js>>(())
                        .map_err(|error| {
                            format!("creating invocation signal: {}", js_err_text(&error))
                        })?;
                    // The host routes Update frames by the request id (its
                    // pending map is keyed by it), so the tool onUpdate
                    // callback must carry the request id, not the tool call id.
                    let invocation_id = id.clone();
                    let future = Box::pin(run_invoke(
                        ctx.clone(),
                        invocation_id,
                        commands_registry.clone(),
                        tools_registry.clone(),
                        hooks_registry.clone(),
                        providers_registry.clone(),
                        overlays_registry.clone(),
                        invocation,
                        model,
                        hello,
                        signal,
                        abort_flag.clone(),
                        interrupt_deadline.clone(),
                        timeouts.invocation_timeout,
                        from_js_tx.clone(),
                        timeouts.max_frame_bytes,
                    ));
                    active = Some((id, future));
                }
            },
            Some(ExtensionHostFrame::Cancel { .. }) => {
                abort_flag.store(true, Ordering::Release);
            }
            Some(ExtensionHostFrame::Response { id, result }) => {
                // A UI/action response can arrive after the invocation that
                // issued the request has completed (fire-and-forget ctx.ui
                // calls are not awaited by every extension). Route it
                // regardless of invocation state; unknown ids are ignored.
                route_action_response(&ctx, &pending_actions, id, result).await;
            }
            Some(ExtensionHostFrame::Shutdown { reason }) => {
                shutdown.store(true, Ordering::Release);
                reject_pending_actions(
                    &ctx,
                    &pending_actions,
                    &format!("extension is shutting down: {reason}"),
                );
                *last_error.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(reason);
                break;
            }
            None => {
                reject_pending_actions(&ctx, &pending_actions, "extension host channel closed");
                break;
            }
        }
    }
    Ok(())
}

async fn route_action_response<'js>(
    ctx: &Ctx<'js>,
    pending_actions: &PendingActions<'js>,
    id: String,
    result: ProtocolResult,
) {
    let (resolve, reject) = match pending_actions
        .lock()
        .expect("quickjs action mutex poisoned")
        .remove(&id)
    {
        Some(pair) => pair,
        None => return,
    };
    match result {
        ProtocolResult::Success { value } => match json_to_js(ctx, &value, 0) {
            Ok(value) => {
                let _ = resolve.call::<_, ()>((value,));
            }
            Err(error) => {
                let _ = reject.call::<_, ()>((throw_value(
                    ctx,
                    &format!("cannot convert extension action result to JS: {error}"),
                ),));
            }
        },
        ProtocolResult::Failure { error } => {
            let _ = reject.call::<_, ()>((throw_value(ctx, &error.message),));
        }
    }
}

fn throw_value<'js>(ctx: &Ctx<'js>, message: &str) -> JsValue<'js> {
    JsString::from_str(ctx.clone(), message)
        .map(|string| string.into_value())
        .unwrap_or_else(|_| JsValue::new_undefined(ctx.clone()))
}

/// Shared JS -> host request bridge (Phase 1 `pi.setSessionName` + Phase 3
/// `ctx.ui.*`): create a Promise, store its resolve/reject pair Rust-side,
/// emit the request frame, and hand the Promise to JS. The main loop settles
/// it when the host's response frame arrives ([`route_action_response`]).
fn issue_runtime_request<'js>(
    ctx: &Ctx<'js>,
    pending_actions: &PendingActions<'js>,
    from_js_tx: &mpsc::UnboundedSender<ExtensionFrame>,
    max_frame_bytes: usize,
    request: ExtensionRuntimeRequest,
) -> JsResult<JsValue<'js>> {
    let (promise, resolve, reject) = ctx.promise()?;
    let id = Uuid::new_v4().to_string();
    pending_actions
        .lock()
        .expect("quickjs action mutex poisoned")
        .insert(id.clone(), (resolve, reject));
    let frame = ExtensionFrame::Request { id: id.clone(), request };
    if send_frame(from_js_tx, frame, max_frame_bytes).is_err() {
        pending_actions
            .lock()
            .expect("quickjs action mutex poisoned")
            .remove(&id);
        return Err(Exception::throw_message(
            ctx,
            "quickjs extension transport closed",
        ));
    }
    Ok(promise.into_value())
}

/// Settle every in-flight JS -> host promise with a rejection. Mirrors the
/// process bridge's `rejectPending` on shutdown and
/// transport-close so pending `ctx.ui` / action promises never dangle.
fn reject_pending_actions<'js>(
    ctx: &Ctx<'js>,
    pending_actions: &PendingActions<'js>,
    message: &str,
) {
    let pending = pending_actions
        .lock()
        .expect("quickjs action mutex poisoned")
        .drain()
        .collect::<Vec<_>>();
    for (_, (_, reject)) in pending {
        let _ = reject.call::<_, ()>((throw_value(ctx, message),));
    }
}

/// Settle every in-flight JS -> host promise with a rejection AND tell the
/// host to cancel the underlying request. Mirrors the process bridge's
/// per-request `onAbort`: when an invocation is cancelled,
/// every pending `pi.setModel` / `ctx.ui` / session-action promise rejects so
/// the handler's awaits fail fast, and a `Cancel` frame is sent per in-flight
/// request so the host cancels its side (action host / UI adapter) instead of
/// answering a promise nobody is listening to anymore. Late host responses
/// arrive with an unknown id and are dropped by [`route_action_response`].
fn cancel_pending_actions<'js>(
    ctx: &Ctx<'js>,
    pending_actions: &PendingActions<'js>,
    from_js_tx: &mpsc::UnboundedSender<ExtensionFrame>,
    max_frame_bytes: usize,
    message: &str,
) {
    let pending = pending_actions
        .lock()
        .expect("quickjs action mutex poisoned")
        .drain()
        .collect::<Vec<_>>();
    for (id, (_, reject)) in pending {
        let _ = send_frame(
            from_js_tx,
            ExtensionFrame::Cancel { id: id.clone() },
            max_frame_bytes,
        );
        let _ = reject.call::<_, ()>((throw_value(ctx, message),));
    }
}

/// The low-level `ctx.ui` request bridge installed under [`UI_REQUEST_GLOBAL`].
/// Mirrors the process bridge's `requestUi`: the
/// `ui` capability is required synchronously, the request JSON is validated,
/// and the request frame is emitted with the Promise returned to JS.
fn build_ui_request_fn<'js>(
    ctx: &Ctx<'js>,
    pending_actions: &PendingActions<'js>,
    from_js_tx: &mpsc::UnboundedSender<ExtensionFrame>,
    max_frame_bytes: usize,
    capabilities: &BTreeSet<ExtensionCapability>,
) -> JsResult<Function<'js>> {
    Function::new(
        ctx.clone(),
        {
            let pending_actions = pending_actions.clone();
            let from_js_tx = from_js_tx.clone();
            let capabilities = capabilities.clone();
            move |ctx: Ctx<'js>, request: Object<'js>, timeout_ms: JsValue<'js>| -> JsResult<JsValue<'js>> {
                // Mirror the process bridge's requireCapability("ui", "UI action")
                // gate: the ui capability is required synchronously.
                // Per-capability grants are enforced host-side and surface as
                // a promise rejection ("permission_denied").
                if !capabilities.contains(&ExtensionCapability::Ui) {
                    return Err(Exception::throw_message(
                        &ctx,
                        "UI action requires the ui capability",
                    ));
                }
                let request_json = js_to_json(&ctx, request.into_value(), 0).map_err(|error| {
                    Exception::throw_message(&ctx, &format!("invalid ui request: {error}"))
                })?;
                let ui_request: ExtensionUiRequest =
                    serde_json::from_value(request_json).map_err(|error| {
                        Exception::throw_message(&ctx, &format!("invalid ui request: {error}"))
                    })?;
                let timeout_ms = ui_timeout_ms(&ctx, timeout_ms)?;
                issue_runtime_request(
                    &ctx,
                    &pending_actions,
                    &from_js_tx,
                    max_frame_bytes,
                    ExtensionRuntimeRequest::Ui {
                        ui: RuntimeUiRequest {
                            request: ui_request,
                            timeout_ms,
                        },
                    },
                )
            }
        },
    )
}

/// The low-level session-action bridge installed under [`ACTION_REQUEST_GLOBAL`].
/// Mirrors the process bridge's `requestAction`:
/// the `session_actions` capability is required synchronously, the action JSON
/// is validated, and the request frame is emitted with the Promise returned to
/// JS. The capability gate throws synchronously (like the process bridge's
/// `requireCapability`); a malformed action (an object that does not
/// deserialize into an [`ExtensionRuntimeAction`]) rejects the returned
/// Promise instead, mirroring the host's `action_failed` rejection.
fn build_action_request_fn<'js>(
    ctx: &Ctx<'js>,
    pending_actions: &PendingActions<'js>,
    from_js_tx: &mpsc::UnboundedSender<ExtensionFrame>,
    max_frame_bytes: usize,
    capabilities: &BTreeSet<ExtensionCapability>,
) -> JsResult<Function<'js>> {
    Function::new(
        ctx.clone(),
        {
            let pending_actions = pending_actions.clone();
            let from_js_tx = from_js_tx.clone();
            let capabilities = capabilities.clone();
            move |ctx: Ctx<'js>, action: Object<'js>| -> JsResult<JsValue<'js>> {
                // Mirror the process bridge's requireCapability("session_actions",
                // "session action") gate. Per-capability grants are enforced
                // host-side and surface as a promise rejection
                // ("permission_denied").
                if !capabilities.contains(&ExtensionCapability::SessionActions) {
                    return Err(Exception::throw_message(
                        &ctx,
                        "session action requires the session_actions capability",
                    ));
                }
                let action_json = js_to_json(&ctx, action.into_value(), 0).map_err(|error| {
                    Exception::throw_message(&ctx, &format!("invalid session action: {error}"))
                })?;
                let action: ExtensionRuntimeAction = match serde_json::from_value(action_json) {
                    Ok(action) => action,
                    Err(error) => {
                        // Async rejection (the process bridge's host-side action_failed)
                        // instead of a synchronous throw: the method call
                        // itself succeeded, the payload was malformed.
                        let (promise, _resolve, reject) = ctx.promise()?;
                        let _ = reject.call::<_, ()>((throw_value(
                            &ctx,
                            &format!("invalid session action: {error}"),
                        ),));
                        return Ok(promise.into_value());
                    }
                };
                issue_runtime_request(
                    &ctx,
                    &pending_actions,
                    &from_js_tx,
                    max_frame_bytes,
                    ExtensionRuntimeRequest::Action { action },
                )
            }
        },
    )
}

/// Parse the JS `{ timeout }` option into the host-side `RuntimeUiRequest`
/// timeout. The bootstrap mirrors the process bridge's `timeoutMs()`, so
/// callers pass a non-negative integer
/// (or `undefined`); anything else fails closed.
fn ui_timeout_ms<'js>(ctx: &Ctx<'js>, value: JsValue<'js>) -> JsResult<Option<u64>> {
    if value.is_null() || value.is_undefined() {
        return Ok(None);
    }
    if let Some(integer) = value.as_int() {
        if integer >= 0 {
            return Ok(Some(integer as u64));
        }
    }
    if let Some(float) = value.as_float() {
        if float.is_finite() && float >= 0.0 && float.fract() == 0.0 {
            return Ok(Some(float as u64));
        }
    }
    Err(Exception::throw_message(
        ctx,
        "ui request timeout must be a non-negative integer",
    ))
}

async fn load_extension<'js>(
    ctx: &Ctx<'js>,
    entry_source: &str,
    entry_leaf: &str,
    pi: Object<'js>,
) -> Result<(), String> {
    let module = Module::declare(ctx.clone(), entry_leaf, entry_source)
        .map_err(|error| format!("declaring quickjs extension entry: {}", js_err_text(&error)))?;
    let (module, eval_promise) = module
        .eval()
        .map_err(|error| format!("evaluating quickjs extension entry: {}", js_err_text(&error)))?;
    eval_promise.into_future::<()>().await.map_err(|error| {
        format!(
            "evaluating quickjs extension entry: {}",
            js_err_text(&error)
        )
    })?;
    let factory: Function = module.get("default").map_err(|error| {
        format!(
            "reading quickjs extension default export: {}",
            js_err_text(&error)
        )
    })?;
    factory
        .call::<_, ()>((pi,))
        .map_err(|error| format!("initializing quickjs extension: {}", exception_message(ctx, &error)))
}

async fn run_invoke<'js>(
    ctx: Ctx<'js>,
    request_id: String,
    commands_registry: Object<'js>,
    tools_registry: Object<'js>,
    hooks_registry: Object<'js>,
    providers_registry: Object<'js>,
    overlays_registry: Object<'js>,
    invocation: ExtensionInvocation,
    model: Option<Model>,
    hello: HelloState,
    signal: JsValue<'js>,
    abort_flag: Arc<AtomicBool>,
    interrupt_deadline: Arc<AtomicU64>,
    timeout: Duration,
    from_js_tx: mpsc::UnboundedSender<ExtensionFrame>,
    max_frame_bytes: usize,
) -> ProtocolResult {
    interrupt_deadline.store(
        now_millis().saturating_add(deadline_millis(timeout)),
        Ordering::Release,
    );
    abort_flag.store(false, Ordering::Release);
    let result = dispatch_invoke(
        &ctx,
        &commands_registry,
        &tools_registry,
        &hooks_registry,
        &providers_registry,
        &overlays_registry,
        &invocation,
        &model,
        &hello,
        &signal,
        &from_js_tx,
        max_frame_bytes,
        &request_id,
    )
    .await;
    interrupt_deadline.store(0, Ordering::Release);
    // An invocation that was cancelled never reports success: once the abort
    // atomic is set the outcome maps to `cancelled` regardless of what the
    // handler did afterwards, matching the process bridge's
    // `Promise.race([operation, cancelled(controller)])`.
    let result = match result {
        Ok(value) if !abort_flag.load(Ordering::Acquire) => match js_to_json(&ctx, value, 0) {
            Ok(json) => ProtocolResult::success(json),
            Err(error) => ProtocolResult::failure(
                "extension_error",
                format!("cannot convert extension result to JSON: {error}"),
            ),
        },
        Ok(_) => ProtocolResult::failure("cancelled", "extension invocation was cancelled"),
        Err(message) => {
            if abort_flag.load(Ordering::Acquire) {
                ProtocolResult::failure("cancelled", "extension invocation was cancelled")
            } else {
                ProtocolResult::failure("extension_error", message)
            }
        }
    };
    if frame_bytes(&result) > max_frame_bytes {
        ProtocolResult::failure(
            "frame_too_large",
            format!("extension protocol frame exceeds {max_frame_bytes} bytes"),
        )
    } else {
        result
    }
}

async fn dispatch_invoke<'js>(
    ctx: &Ctx<'js>,
    commands_registry: &Object<'js>,
    tools_registry: &Object<'js>,
    hooks_registry: &Object<'js>,
    providers_registry: &Object<'js>,
    overlays_registry: &Object<'js>,
    invocation: &ExtensionInvocation,
    model: &Option<Model>,
    hello: &HelloState,
    signal: &JsValue<'js>,
    from_js_tx: &mpsc::UnboundedSender<ExtensionFrame>,
    max_frame_bytes: usize,
    request_id: &str,
) -> Result<JsValue<'js>, String> {
    match invocation {
        ExtensionInvocation::Command { name, arguments } => {
            let handler: Option<Function> = commands_registry.get(name.clone()).map_err(|error| js_err_text(&error))?;
            let Some(handler) = handler else {
                return Err(format!("unknown command {name}"));
            };
            let args = json_to_js(ctx, &JsonValue::String(arguments.clone()), 0)
                .map_err(|error| js_err_text(&error))?;
            let context_object =
                build_context_object(ctx, hello, signal.clone(), model.as_ref()).map_err(|error| js_err_text(&error))?;
            let call = handler.call::<_, MaybePromise<'js>>((args, context_object));
            await_call(ctx, call).await
        }
        ExtensionInvocation::Tool {
            name,
            call_id,
            arguments,
        } => {
            let tool: Option<Object> = tools_registry.get(name.clone()).map_err(|error| js_err_text(&error))?;
            let Some(tool) = tool else {
                return Err(format!("unknown tool {name}"));
            };
            let execute: Function = tool.get("execute").map_err(|error| js_err_text(&error))?;
            let args = json_to_js(ctx, arguments, 0).map_err(|error| js_err_text(&error))?;
            // The host routes Update frames by the *request* id (its pending
            // map is keyed by it), so the onUpdate callback must carry
            // `request_id` rather than the tool call id.
            let on_update = build_on_update(ctx, from_js_tx.clone(), request_id.to_owned(), max_frame_bytes)
                .map_err(|error| js_err_text(&error))?;
            let context_object =
                build_context_object(ctx, hello, signal.clone(), model.as_ref()).map_err(|error| js_err_text(&error))?;
            let call = execute.call::<_, MaybePromise<'js>>((
                call_id.clone(),
                args,
                signal.clone(),
                on_update,
                context_object,
            ));
            await_call(ctx, call).await
        }
        ExtensionInvocation::Event { event } => {
            dispatch_event(ctx, hooks_registry, event, hello, signal, model).await
        }
        ExtensionInvocation::Provider {
            provider_id,
            session_id,
            messages,
            options,
        } => {
            let provider: Option<Object> = providers_registry
                .get(provider_id.clone())
                .map_err(|error| js_err_text(&error))?;
            let Some(provider) = provider else {
                return Err(format!("unknown provider {provider_id}"));
            };
            let stream_fn: Function = provider.get("stream").map_err(|error| js_err_text(&error))?;
            let session_id_value = match session_id {
                Some(session_id) => JsString::from_str(ctx.clone(), session_id)
                    .map_err(|error| js_err_text(&error))?
                    .into_value(),
                None => JsValue::new_null(ctx.clone()),
            };
            let messages_json = serde_json::to_value(messages)
                .map_err(|error| format!("serializing provider messages: {error}"))?;
            let messages_value =
                json_to_js(ctx, &messages_json, 0).map_err(|error| js_err_text(&error))?;
            let options_value = json_to_js(ctx, options, 0).map_err(|error| js_err_text(&error))?;
            // Each yielded event is forwarded to the host as an
            // `ExtensionFrame::Update` (same path as tool onUpdate), in order,
            // before the invocation's final response.
            let on_event = build_on_update(ctx, from_js_tx.clone(), request_id.to_owned(), max_frame_bytes)
                .map_err(|error| js_err_text(&error))?;
            let driver: Function = ctx
                .globals()
                .get("__piRunProviderStream")
                .map_err(|error| js_err_text(&error))?;
            let call = driver.call::<_, MaybePromise<'js>>((
                stream_fn,
                session_id_value,
                messages_value,
                options_value,
                on_event,
            ));
            await_call(ctx, call).await
        }
        ExtensionInvocation::OverlayRender { id } => {
            let overlay: Option<Object> = overlays_registry
                .get(id.clone())
                .map_err(|error| js_err_text(&error))?;
            let Some(overlay) = overlay else {
                return Err(format!("unknown overlay {id}"));
            };
            let render: Function = overlay.get("render").map_err(|error| js_err_text(&error))?;
            let context_object =
                build_context_object(ctx, hello, signal.clone(), model.as_ref())
                    .map_err(|error| js_err_text(&error))?;
            let call = render.call::<_, MaybePromise<'js>>((context_object,));
            let value = await_call(ctx, call).await?;
            // The original contract let `render` return a bare rows array;
            // the interactive contract returns `{ rows, input? }`. Normalize
            // the array form so the host always receives the object shape.
            if value.is_array() {
                let rows = js_to_json(ctx, value, 0).map_err(|error| error.to_string())?;
                let mut normalized = JsonMap::new();
                normalized.insert("rows".to_owned(), rows);
                Ok(json_to_js(ctx, &JsonValue::Object(normalized), 0)
                    .map_err(|error| error.to_string())?)
            } else {
                Ok(value)
            }
        }
        ExtensionInvocation::OverlayEvent { id, event } => {
            let overlay: Option<Object> = overlays_registry
                .get(id.clone())
                .map_err(|error| js_err_text(&error))?;
            let Some(overlay) = overlay else {
                return Err(format!("unknown overlay {id}"));
            };
            let (callback, payload) = match event {
                OverlayEvent::Submit { text } => {
                    let callback: Option<Function> =
                        overlay.get("onSubmit").map_err(|error| js_err_text(&error))?;
                    let callback = callback.ok_or_else(|| {
                        format!("overlay {id} has no onSubmit callback for a submit event")
                    })?;
                    (
                        callback,
                        JsString::from_str(ctx.clone(), &text)
                            .map_err(|error| js_err_text(&error))?
                            .into_value(),
                    )
                }
                OverlayEvent::Key { action } => {
                    let callback: Option<Function> =
                        overlay.get("onKey").map_err(|error| js_err_text(&error))?;
                    let callback = callback.ok_or_else(|| {
                        format!("overlay {id} has no onKey callback for a key action")
                    })?;
                    let serialized = serde_json::to_value(action)
                        .map_err(|error| format!("serializing overlay key action: {error}"))?;
                    (callback, json_to_js(ctx, &serialized, 0).map_err(|e| e.to_string())?)
                }
            };
            let context_object =
                build_context_object(ctx, hello, signal.clone(), model.as_ref())
                    .map_err(|error| js_err_text(&error))?;
            let call = callback.call::<_, MaybePromise<'js>>((payload, context_object));
            await_call(ctx, call).await
        }
        other => Err(format!(
            "{other:?} invocation is unavailable in the quickjs runtime"
        )),
    }
}

async fn await_call<'js>(
    ctx: &Ctx<'js>,
    call: JsResult<MaybePromise<'js>>,
) -> Result<JsValue<'js>, String> {
    let maybe = call.map_err(|error| exception_message(ctx, &error))?;
    match maybe.into_future::<JsValue<'js>>().await {
        Ok(value) => Ok(value),
        Err(error) => Err(exception_message(ctx, &error)),
    }
}

/// Dispatch an event invocation to the registered `pi.on` handlers and reduce
/// their return values. This mirrors the process bridge's `invoke()` event
/// branch field for field: event data shaping,
/// per-event merge branches, early-break conditions, the `signal` injection
/// for cancellation-capable events, and the trailing mutation merge.
///
/// The return value is the *reduction* the host applies through its
/// `reduce_*` / `emit` machinery; `null` means "no reduction" (handlers either
/// returned `undefined` or no handler is registered), exactly like the process
/// bridge's `success(id, jsonValue(value, null))`.
async fn dispatch_event<'js>(
    ctx: &Ctx<'js>,
    hooks_registry: &Object<'js>,
    event: &ExtensionEvent,
    hello: &HelloState,
    signal: &JsValue<'js>,
    model: &Option<Model>,
) -> Result<JsValue<'js>, String> {
    let name = event.name.as_str();
    let handlers: Vec<Function<'js>> = hooks_registry
        .get::<_, Option<Vec<Function<'js>>>>(name)
        .map_err(|error| js_err_text(&error))?
        .unwrap_or_default();

    // `eventData`: a shallow copy when the payload is an object, otherwise a
    // `{ data }` wrapper (mirrors the process bridge's `{ ...data } : { data }`).
    let mut event_data: JsonMap = match &event.data {
        JsonValue::Object(fields) => fields.clone(),
        data => {
            let mut wrapped = JsonMap::new();
            wrapped.insert("data".to_owned(), data.clone());
            wrapped
        }
    };
    // `jsonValue(...)` deep copies captured before any handler runs.
    let original_input = event_data.get("input").cloned();
    let original_headers = event_data.get("headers").cloned();
    let original_message = event_data.get("message").cloned();
    let original_payload = event_data.get("payload").cloned();
    let needs_signal = matches!(name, "session_before_compact" | "session_before_tree");

    let mut value: JsonValue = JsonValue::Null;
    for handler in handlers {
        // A fresh frozen `{ type: name, ...eventData, signal? }` per handler;
        // the handler's own mutations of nested objects are synced back after
        // the call (top-level writes fail closed on the frozen object).
        let event_object = build_event_object(ctx, name, &event_data, needs_signal, signal)
            .map_err(|error| js_err_text(&error))?;
        let context_object = build_context_object(ctx, hello, signal.clone(), model.as_ref())
            .map_err(|error| js_err_text(&error))?;
        let call = handler.call::<_, MaybePromise<'js>>((
            JsValue::from(event_object.clone()),
            context_object,
        ));
        let next = await_call(ctx, call).await?;
        sync_event_data(ctx, &event_object, &mut event_data, needs_signal)
            .map_err(|error| js_err_text(&error))?;
        if next.is_undefined() {
            continue;
        }
        let next: JsonValue =
            js_to_json(ctx, next, 0).map_err(|error| js_err_text(&error))?;

        // Per-event merge branches.
        if name == "context"
            && next
                .as_object()
                .is_some_and(|fields| fields.contains_key("messages"))
        {
            let messages = next
                .get("messages")
                .cloned()
                .unwrap_or(JsonValue::Null);
            event_data.insert("messages".to_owned(), messages.clone());
            value = json!({ "messages": messages });
        } else if name == "before_provider_request" {
            event_data.insert("payload".to_owned(), next.clone());
            value = next.clone();
        } else if name == "before_agent_start" && next.is_object() {
            if next
                .as_object()
                .is_some_and(|fields| fields.contains_key("systemPrompt"))
            {
                if let Some(system_prompt) = next.get("systemPrompt") {
                    event_data.insert("systemPrompt".to_owned(), system_prompt.clone());
                }
            }
            let prior = value.as_object().cloned().unwrap_or_default();
            let mut messages: Vec<JsonValue> = prior
                .get("messages")
                .and_then(JsonValue::as_array)
                .cloned()
                .unwrap_or_default();
            if let Some(message) = next.get("message") {
                messages.push(message.clone());
            }
            let mut merged = prior;
            if let Some(fields) = next.as_object() {
                for (key, field) in fields {
                    merged.insert(key.clone(), field.clone());
                }
            }
            if !messages.is_empty() {
                merged.insert("messages".to_owned(), JsonValue::Array(messages));
            }
            value = JsonValue::Object(merged);
        } else if name == "before_provider_headers" && next.is_object() {
            if let Some(headers) = next.get("headers") {
                event_data.insert("headers".to_owned(), headers.clone());
            }
            value = merge_into_object(&value, &next);
        } else if name == "tool_result" && next.is_object() {
            for field in ["content", "details", "isError", "usage"] {
                if next.as_object().is_some_and(|fields| fields.contains_key(field)) {
                    if let Some(item) = next.get(field) {
                        event_data.insert(field.to_owned(), item.clone());
                    }
                }
            }
            value = merge_into_object(&value, &next);
        } else if name == "message_end" && next.is_object() {
            if let Some(message) = next.get("message") {
                event_data.insert("message".to_owned(), message.clone());
            }
            value = merge_into_object(&value, &next);
        } else if name == "input" && next.is_object() {
            if let Some(text) = next.get("text") {
                event_data.insert("text".to_owned(), text.clone());
            }
            if let Some(images) = next.get("images") {
                event_data.insert("images".to_owned(), images.clone());
            }
            value = merge_into_object(&value, &next);
            if next.get("action").and_then(JsonValue::as_str) == Some("handled") {
                break;
            }
        } else {
            value = next.clone();
        }

        // Early-break conditions.
        let should_break = next.is_object()
            && (next.get("cancel").and_then(JsonValue::as_bool) == Some(true)
                || (name == "tool_call"
                    && next.get("block").and_then(JsonValue::as_bool) == Some(true))
                || name == "user_bash");
        if should_break {
            break;
        }
    }

    // Trailing mutation merge.
    if matches!(name, "tool_call" | "tool_result") {
        merge_mutation("input", &original_input, &event_data, &mut value);
    }
    if name == "before_provider_headers" {
        merge_mutation("headers", &original_headers, &event_data, &mut value);
    }
    if name == "message_end" {
        merge_mutation("message", &original_message, &event_data, &mut value);
    }
    if name == "before_provider_request" {
        let payload_changed = match (event_data.get("payload"), &original_payload) {
            (Some(current), Some(original)) => current != original,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if payload_changed {
            if let Some(payload) = event_data.get("payload") {
                value = payload.clone();
            }
        }
    }

    json_to_js(ctx, &value, 0).map_err(|error| js_err_text(&error))
}

/// Build the per-handler event object: `Object.freeze({ type, ...data })`,
/// with the live signal injected for cancellation-capable events before the
/// freeze (top-level writes then fail closed).
fn build_event_object<'js>(
    ctx: &Ctx<'js>,
    name: &str,
    event_data: &JsonMap,
    needs_signal: bool,
    signal: &JsValue<'js>,
) -> JsResult<Object<'js>> {
    let mut fields = JsonMap::new();
    fields.insert("type".to_owned(), JsonValue::String(name.to_owned()));
    for (key, field) in event_data {
        fields.insert(key.clone(), field.clone());
    }
    let object = json_to_js(ctx, &JsonValue::Object(fields), 0)?
        .as_object()
        .ok_or_else(|| rquickjs::Error::FromJs {
            from: "json value",
            to: "object",
            message: Some("event data must convert to an object".to_owned()),
        })?
        .clone();
    if needs_signal {
        object.set("signal", signal.clone())?;
    }
    let global: Object = ctx.globals().get("Object")?;
    let freeze: Function = global.get("freeze")?;
    freeze.call::<_, ()>((object.clone(),))?;
    Ok(object)
}

/// Copy handler-side mutations of the event object back into the Rust-side
/// `event_data` map. The event object is frozen, so only deep mutations of
/// nested values (e.g. `Object.assign(event.headers, ...)`) are observable;
/// values that cannot round-trip to JSON (functions, symbols) are dropped the
/// way `JSON.stringify` drops them. `type` and `signal` are
/// never part of the payload.
fn sync_event_data<'js>(
    ctx: &Ctx<'js>,
    event_object: &Object<'js>,
    event_data: &mut JsonMap,
    needs_signal: bool,
) -> JsResult<()> {
    for entry in event_object.clone().into_iter() {
        let (key, item) = entry?;
        let key: String = key.to_string()?;
        if key == "type" || (needs_signal && key == "signal") {
            continue;
        }
        if let Ok(value) = js_to_json(ctx, item, 0) {
            event_data.insert(key, value);
        }
    }
    Ok(())
}

/// `{ ...(value && typeof value === "object" ? value : {}), ...next }`
/// (spread merge for object-valued reductions).
fn merge_into_object(value: &JsonValue, next: &JsonValue) -> JsonValue {
    let mut merged = match value {
        JsonValue::Object(fields) => fields.clone(),
        _ => JsonMap::new(),
    };
    if let JsonValue::Object(fields) = next {
        for (key, field) in fields {
            merged.insert(key.clone(), field.clone());
        }
    }
    JsonValue::Object(merged)
}

/// The process bridge's `mergeMutation(field, original)`: if the field is
/// present in the (post-handler) event data and
/// differs from its pre-handler snapshot, fold it into the reduction.
fn merge_mutation(
    field: &str,
    original: &Option<JsonValue>,
    event_data: &JsonMap,
    value: &mut JsonValue,
) {
    let Some(current) = event_data.get(field) else {
        return;
    };
    let changed = match original {
        None => true,
        Some(original) => current != original,
    };
    if !changed {
        return;
    }
    match value {
        JsonValue::Object(fields) => {
            fields.insert(field.to_owned(), current.clone());
        }
        _ => {
            let mut fields = JsonMap::new();
            fields.insert(field.to_owned(), current.clone());
            *value = JsonValue::Object(fields);
        }
    }
}

/// Globals under which the shared JS registries live. Rust closures stored in
/// JS functions MUST NOT capture rquickjs handles (the closure data is released
/// when the function object is freed, which can happen after the captured
/// object was already torn down — leaking at JS_FreeRuntime). The register
/// closures therefore look the registries up through `ctx.globals()` at call
/// time instead of capturing them.
const COMMANDS_REGISTRY_GLOBAL: &str = "__piCommandsRegistry";
const TOOLS_REGISTRY_GLOBAL: &str = "__piToolsRegistry";
const HOOKS_REGISTRY_GLOBAL: &str = "__piHooksRegistry";
const PROVIDERS_REGISTRY_GLOBAL: &str = "__piProvidersRegistry";
const OVERLAYS_REGISTRY_GLOBAL: &str = "__piOverlaysRegistry";
const SIGNAL_ABORTED_GLOBAL: &str = "__piSignalAborted";
/// Global under which the Phase 4 abort-flag closure lives: storing `true` to
/// the shared invocation abort atomic (called by the JS `AbortController`
/// shim's `abort()`).
const ABORT_FLAG_GLOBAL: &str = "__piAbortFlag";
/// Global under which the low-level `ctx.ui` request bridge lives: a Rust
/// closure that issues `ExtensionFrame::Request { Ui, ... }` frames and
/// returns the settling Promise (see [`build_ui_request_fn`]).
const UI_REQUEST_GLOBAL: &str = "__piUiRequest";
/// Global under which the low-level session-action bridge lives: a Rust
/// closure that issues `ExtensionFrame::Request { Action, ... }` frames and
/// returns the settling Promise (see [`build_action_request_fn`]).
const ACTION_REQUEST_GLOBAL: &str = "__piActionRequest";
/// Global under which the frozen `ctx.ui` object built by
/// [`UI_BOOTSTRAP_SOURCE`] lives; `build_context_object` reads it at
/// invocation time.
const UI_GLOBAL: &str = "__piUi";
/// Global under which the frozen session-action method object built by
/// [`SESSION_ACTIONS_BOOTSTRAP_SOURCE`] lives; `build_pi` and
/// `build_context_object` attach their subsets at call time.
const SESSION_ACTIONS_GLOBAL: &str = "__piSessionActions";

/// Session actions exposed on `pi` (`setSessionName` is a dedicated Rust
/// closure). The remaining action-channel surface
/// (`abort`/`compact`/`shutdown`/`waitForIdle`/`reload`) lives on `ctx`
/// exactly like the process bridge's `createContext`.
const PI_SESSION_METHODS: [&str; 7] = [
    "sendMessage",
    "sendUserMessage",
    "appendEntry",
    "setLabel",
    "setActiveTools",
    "setThinkingLevel",
    "setModel",
];
/// Session actions exposed on `ctx`.
const CTX_SESSION_METHODS: [&str; 5] =
    ["abort", "compact", "shutdown", "waitForIdle", "reload"];

/// A pending JS -> host action: resolve/reject pair stored Rust-side and
/// settled by the main loop when the host's response frame arrives.
type PendingActions<'js> =
    Arc<Mutex<HashMap<String, (Function<'js>, Function<'js>)>>>;

fn build_pi<'js>(
    ctx: &Ctx<'js>,
    in_load: &Arc<AtomicBool>,
    pending_actions: &PendingActions<'js>,
    from_js_tx: &mpsc::UnboundedSender<ExtensionFrame>,
    max_frame_bytes: usize,
    capabilities: &BTreeSet<ExtensionCapability>,
) -> JsResult<Object<'js>> {
    let register_command = Function::new(
        ctx.clone(),
        {
            let in_load = in_load.clone();
            let from_js_tx = from_js_tx.clone();
            move |ctx: Ctx<'js>, name: String, options: Object<'js>| -> JsResult<()> {
                if !in_load.load(Ordering::Acquire) {
                    return Err(Exception::throw_message(
                        &ctx,
                        "command registration is only allowed during the load phase",
                    ));
                }
                if options.get::<_, Option<Function>>("handler")?.is_none() {
                    return Err(Exception::throw_message(
                        &ctx,
                        "registerCommand requires a handler function",
                    ));
                }
                let registry: Object = ctx.globals().get(COMMANDS_REGISTRY_GLOBAL)?;
                if registry.contains_key(&name)? {
                    return Err(Exception::throw_message(
                        &ctx,
                        &format!("duplicate command {name}"),
                    ));
                }
                let handler: Function = options.get("handler")?;
                registry.set(name.clone(), handler)?;
                let description: Option<String> = options.get("description")?;
                let frame = ExtensionFrame::Register {
                    registration: ExtensionRegistration::Command {
                        command: ExtensionCommandDescriptor { name, description },
                    },
                };
                send_frame(&from_js_tx, frame, max_frame_bytes)
                    .map_err(|message| Exception::throw_message(&ctx, &message))
            }
        },
    )?;

    let register_tool = Function::new(
        ctx.clone(),
        {
            let in_load = in_load.clone();
            let from_js_tx = from_js_tx.clone();
            move |ctx: Ctx<'js>, tool: Object<'js>| -> JsResult<()> {
                if !in_load.load(Ordering::Acquire) {
                    return Err(Exception::throw_message(
                        &ctx,
                        "tool registration is only allowed during the load phase",
                    ));
                }
                for field in [
                    "constrainedSampling",
                    "renderShell",
                    "prepareArguments",
                    "renderCall",
                    "renderResult",
                ] {
                    if tool.get::<_, Option<JsValue>>(field)?.is_some() {
                        return Err(Exception::throw_message(
                            &ctx,
                            &format!("registerTool.{field} is unavailable in the quickjs runtime"),
                        ));
                    }
                }
                let name: String = tool.get("name")?;
                if tool.get::<_, Option<Function>>("execute")?.is_none() {
                    return Err(Exception::throw_message(
                        &ctx,
                        "registerTool requires an execute function",
                    ));
                }
                let registry: Object = ctx.globals().get(TOOLS_REGISTRY_GLOBAL)?;
                if registry.contains_key(&name)? {
                    return Err(Exception::throw_message(
                        &ctx,
                        &format!("duplicate tool {name}"),
                    ));
                }
                registry.set(name.clone(), tool.clone())?;
                let label: String = tool
                    .get::<_, Option<String>>("label")?
                    .unwrap_or_else(|| name.clone());
                let description: String = tool
                    .get::<_, Option<String>>("description")?
                    .unwrap_or_else(|| "Extension tool".to_owned());
                let capability = match tool.get::<_, Option<String>>("capability")?.as_deref() {
                    Some("read") => ToolCapability::Read,
                    Some("write") => ToolCapability::Write,
                    Some("exec") | None => ToolCapability::Exec,
                    Some(other) => {
                        return Err(Exception::throw_message(
                            &ctx,
                            &format!(
                                "registerTool capability must be read, write, or exec, got {other}"
                            ),
                        ));
                    }
                };
                let execution_mode =
                    match tool.get::<_, Option<String>>("executionMode")?.as_deref() {
                        None | Some("default") => ToolExecutionMode::Default,
                        Some(other) => {
                            return Err(Exception::throw_message(
                                &ctx,
                                &format!("registerTool executionMode must be \"default\", got {other:?}"),
                            ));
                        }
                    };
                let prompt_guidelines: Vec<String> = tool
                    .get::<_, Option<Vec<String>>>("promptGuidelines")?
                    .unwrap_or_default();
                let parameters = match tool.get::<_, Option<JsValue>>("parameters")? {
                    Some(value) => js_to_json(&ctx, value, 0).map_err(|error| {
                        Exception::throw_message(
                            &ctx,
                            &format!("invalid registerTool parameters: {error}"),
                        )
                    })?,
                    None => json!({ "type": "object", "properties": {} }),
                };
                let parameters: Schema = serde_json::from_value(parameters).map_err(|error| {
                    Exception::throw_message(
                        &ctx,
                        &format!("invalid registerTool parameters schema: {error}"),
                    )
                })?;
                let frame = ExtensionFrame::Register {
                    registration: ExtensionRegistration::Tool {
                        tool: ExtensionToolDescriptor {
                            name,
                            label,
                            description,
                            parameters,
                            capability,
                            execution_mode,
                            prompt_guidelines,
                        },
                    },
                };
                send_frame(&from_js_tx, frame, max_frame_bytes)
                    .map_err(|message| Exception::throw_message(&ctx, &message))
            }
        },
    )?;

    // `pi.registerProvider({ id, label?, api?, capabilities?, stream })`:
    // load-phase-only, `provider` capability-gated, like registerTool. The
    // options object (holding the JS stream function) is stored in the shared
    // providers registry so provider invocations can reach it without any
    // closure capture; the host receives the descriptor frame for resolution.
    let register_provider = Function::new(
        ctx.clone(),
        {
            let in_load = in_load.clone();
            let from_js_tx = from_js_tx.clone();
            let capabilities = capabilities.clone();
            move |ctx: Ctx<'js>, options: Object<'js>| -> JsResult<()> {
                if !in_load.load(Ordering::Acquire) {
                    return Err(Exception::throw_message(
                        &ctx,
                        "provider registration is only allowed during the load phase",
                    ));
                }
                if !capabilities.contains(&ExtensionCapability::Provider) {
                    return Err(Exception::throw_message(
                        &ctx,
                        "provider registration requires the provider capability",
                    ));
                }
                let id: String = options.get("id")?;
                if let Err(message) = validate_js_identifier(&id, "provider id") {
                    return Err(Exception::throw_message(&ctx, &message));
                }
                if options.get::<_, Option<Function>>("stream")?.is_none() {
                    return Err(Exception::throw_message(
                        &ctx,
                        "registerProvider requires a stream function",
                    ));
                }
                let api: String = match options.get::<_, Option<String>>("api")? {
                    Some(api) => api,
                    None => id.clone(),
                };
                if let Err(message) = validate_js_identifier(&api, "provider api") {
                    return Err(Exception::throw_message(&ctx, &message));
                }
                let label: Option<String> = options.get("label")?;
                if let Some(label) = &label {
                    if label.is_empty() || label.len() > 256 || label.chars().any(char::is_control)
                    {
                        return Err(Exception::throw_message(
                            &ctx,
                            "provider label must be 1-256 printable characters",
                        ));
                    }
                }
                let provider_capabilities: Vec<String> =
                    options.get::<_, Option<Vec<String>>>("capabilities")?.unwrap_or_default();
                for capability in &provider_capabilities {
                    if let Err(message) =
                        validate_js_identifier(capability, "provider capability")
                    {
                        return Err(Exception::throw_message(&ctx, &message));
                    }
                }
                let registry: Object = ctx.globals().get(PROVIDERS_REGISTRY_GLOBAL)?;
                if registry.contains_key(&id)? {
                    return Err(Exception::throw_message(
                        &ctx,
                        &format!("duplicate provider {id}"),
                    ));
                }
                registry.set(id.clone(), options)?;
                let frame = ExtensionFrame::Register {
                    registration: ExtensionRegistration::Provider {
                        provider: ExtensionProviderDescriptor {
                            id,
                            label,
                            api,
                            capabilities: provider_capabilities,
                        },
                    },
                };
                send_frame(&from_js_tx, frame, max_frame_bytes)
                    .map_err(|message| Exception::throw_message(&ctx, &message))
            }
        },
    )?;

    // `pi.unregisterProvider(id)`: load-phase-only; drops the JS-side
    // callable and tells the host to drop the descriptor so resolution fails
    // actionably. Re-registering the same id afterwards replaces it.
    let unregister_provider = Function::new(
        ctx.clone(),
        {
            let in_load = in_load.clone();
            let from_js_tx = from_js_tx.clone();
            move |ctx: Ctx<'js>, id: String| -> JsResult<()> {
                if !in_load.load(Ordering::Acquire) {
                    return Err(Exception::throw_message(
                        &ctx,
                        "provider unregistration is only allowed during the load phase",
                    ));
                }
                let registry: Object = ctx.globals().get(PROVIDERS_REGISTRY_GLOBAL)?;
                if !registry.contains_key(&id)? {
                    return Err(Exception::throw_message(
                        &ctx,
                        &format!("cannot unregister unknown provider {id}"),
                    ));
                }
                registry.remove(&id)?;
                send_frame(
                    &from_js_tx,
                    ExtensionFrame::UnregisterProvider { id },
                    max_frame_bytes,
                )
                .map_err(|message| Exception::throw_message(&ctx, &message))
            }
        },
    )?;

    // `pi.registerOverlay({ id, title, render })`: load-phase-only,
    // `overlays` capability-gated. The options object (holding the JS render
    // function) is stored in the shared overlays registry so render
    // invocations can reach it without any closure capture; the host receives
    // the descriptor frame for `/overlay <id>` resolution.
    let register_overlay = Function::new(
        ctx.clone(),
        {
            let in_load = in_load.clone();
            let from_js_tx = from_js_tx.clone();
            let capabilities = capabilities.clone();
            move |ctx: Ctx<'js>, options: Object<'js>| -> JsResult<()> {
                if !in_load.load(Ordering::Acquire) {
                    return Err(Exception::throw_message(
                        &ctx,
                        "overlay registration is only allowed during the load phase",
                    ));
                }
                if !capabilities.contains(&ExtensionCapability::Overlays) {
                    return Err(Exception::throw_message(
                        &ctx,
                        "overlay registration requires the overlays capability",
                    ));
                }
                let id: String = options.get("id")?;
                if let Err(message) = validate_js_identifier(&id, "overlay id") {
                    return Err(Exception::throw_message(&ctx, &message));
                }
                let title: String = options
                    .get::<_, Option<String>>("title")?
                    .unwrap_or_else(|| id.clone());
                if title.is_empty() || title.len() > 256 || title.chars().any(char::is_control) {
                    return Err(Exception::throw_message(
                        &ctx,
                        "overlay title must be 1-256 printable characters",
                    ));
                }
                if options.get::<_, Option<Function>>("render")?.is_none() {
                    return Err(Exception::throw_message(
                        &ctx,
                        "registerOverlay requires a render function",
                    ));
                }
                // Interactive overlays may declare `onSubmit(text, ctx)` and
                // `onKey(action, ctx)` callbacks; the host owns the editor, so
                // only sanitized text and the limited action ids reach them.
                for field in ["onSubmit", "onKey"] {
                    if let Some(value) = options.get::<_, Option<JsValue>>(field)? {
                        if !value.is_function() {
                            return Err(Exception::throw_message(
                                &ctx,
                                &format!("registerOverlay {field} must be a function"),
                            ));
                        }
                    }
                }
                // Optional static editor declaration (`input: { placeholder?,
                // multiline? }`). Validated host-side; the host owns the
                // editor, so the declaration carries no mutable state.
                let input = match options.get::<_, Option<Object>>("input")? {
                    None => None,
                    Some(declaration) => {
                        let placeholder: Option<String> = declaration.get("placeholder")?;
                        if let Some(placeholder) = &placeholder {
                            if placeholder.is_empty()
                                || placeholder.len() > 256
                                || placeholder.chars().any(char::is_control)
                            {
                                return Err(Exception::throw_message(
                                    &ctx,
                                    "overlay input placeholder must be 1-256 printable characters",
                                ));
                            }
                        }
                        let multiline: bool = declaration
                            .get::<_, Option<bool>>("multiline")?
                            .unwrap_or(false);
                        Some(OverlayInputDeclaration { placeholder, multiline })
                    }
                };
                let registry: Object = ctx.globals().get(OVERLAYS_REGISTRY_GLOBAL)?;
                if registry.contains_key(&id)? {
                    return Err(Exception::throw_message(
                        &ctx,
                        &format!("duplicate overlay {id}"),
                    ));
                }
                registry.set(id.clone(), options)?;
                let frame = ExtensionFrame::Register {
                    registration: ExtensionRegistration::Overlay {
                        overlay: ExtensionOverlayDescriptor { id, title, input },
                    },
                };
                send_frame(&from_js_tx, frame, max_frame_bytes)
                    .map_err(|message| Exception::throw_message(&ctx, &message))
            }
        },
    )?;

    // Synchronous bridge that returns a Promise: the resolve/reject pair is
    // stored Rust-side and settled by the main loop when the host answers the
    // action. Keeping the JS handles in Rust-owned state (never captured by a
    // closure stored on a JS function) avoids the JS_FreeRuntime teardown leak.
    let set_session_name = Function::new(
        ctx.clone(),
        {
            let pending_actions = pending_actions.clone();
            let from_js_tx = from_js_tx.clone();
            move |ctx: Ctx<'js>, name: String| -> JsResult<JsValue<'js>> {
                issue_runtime_request(
                    &ctx,
                    &pending_actions,
                    &from_js_tx,
                    max_frame_bytes,
                    ExtensionRuntimeRequest::Action {
                        action: ExtensionRuntimeAction::SetSessionName { name },
                    },
                )
            }
        },
    )?;

    let pi_on = Function::new(
        ctx.clone(),
        {
            let capabilities = capabilities.clone();
            let from_js_tx = from_js_tx.clone();
            move |ctx: Ctx<'js>, event: String, handler: Function<'js>| -> JsResult<()> {
                // Mirror the process bridge's pi.on:
                // capability gate, authoritative allow-list, one event_hook
                // Register frame per event on first registration, then record
                // the handler for dispatch.
                if !capabilities.contains(&ExtensionCapability::EventHooks) {
                    return Err(Exception::throw_message(
                        &ctx,
                        "event registration requires the event_hooks capability",
                    ));
                }
                if !SUPPORTED_EVENTS.contains(&event.as_str()) {
                    return Err(Exception::throw_message(
                        &ctx,
                        &format!("unsupported extension event {event}"),
                    ));
                }
                let hooks: Object = ctx.globals().get(HOOKS_REGISTRY_GLOBAL)?;
                let mut handlers: Vec<Function> =
                    hooks.get::<_, Option<Vec<Function>>>(&event)?.unwrap_or_default();
                if handlers.is_empty() {
                    let frame = ExtensionFrame::Register {
                        registration: ExtensionRegistration::EventHook {
                            hook: ExtensionEventHookDescriptor { event: event.clone() },
                        },
                    };
                    send_frame(&from_js_tx, frame, max_frame_bytes)
                        .map_err(|message| Exception::throw_message(&ctx, &message))?;
                }
                handlers.push(handler);
                hooks.set(event, handlers)
            }
        },
    )?;

    let pi = Object::new(ctx.clone())?;
    pi.set("registerCommand", register_command)?;
    pi.set("registerTool", register_tool)?;
    pi.set("registerProvider", register_provider)?;
    pi.set("unregisterProvider", unregister_provider)?;
    pi.set("registerOverlay", register_overlay)?;
    pi.set("setSessionName", set_session_name)?;
    pi.set("on", pi_on)?;
    // Phase 4 session actions: plain JS functions from the shared bootstrap
    // (no Rust handles stored on JS functions), copied onto `pi` at build
    // time. Each issues the matching ExtensionRuntimeAction through the
    // `__piActionRequest` global and returns the settling Promise.
    let session_actions: Object = ctx.globals().get(SESSION_ACTIONS_GLOBAL)?;
    for method in PI_SESSION_METHODS {
        let handler: Function = session_actions.get(method)?;
        pi.set(method, handler)?;
    }
    for method in UNAVAILABLE_PI_METHODS {
        let name = (*method).to_owned();
        let unavailable = Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>| -> JsResult<()> {
                Err(Exception::throw_message(
                    &ctx,
                    &format!("{name} is unavailable in the quickjs runtime"),
                ))
            },
        )?;
        pi.set(*method, unavailable)?;
    }
    Ok(pi)
}

/// The tool `onUpdate` callback (Phase 4). Mirrors the process bridge's
/// `update => send({ type: "update", id, value: jsonValue(update, {}) })`:
/// each call forwards an `ExtensionFrame::Update`
/// carrying the invocation's *request id* (the host's pending map is keyed by
/// it), `undefined` maps to `{}`, and a value that cannot round-trip to JSON —
/// or a frame over the size limit — throws synchronously, exactly like the
/// process bridge's `send()`. Synchronous (no Promise): the frame send is
/// non-blocking, and a throw surfaces at the `onUpdate` call site inside the
/// tool's `execute`.
fn build_on_update<'js>(
    ctx: &Ctx<'js>,
    from_js_tx: mpsc::UnboundedSender<ExtensionFrame>,
    request_id: String,
    max_frame_bytes: usize,
) -> JsResult<Function<'js>> {
    Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, value: JsValue<'js>| -> JsResult<()> {
            let value = if value.is_undefined() {
                // The process bridge's `jsonValue(update, {})` fallback.
                JsonValue::Object(JsonMap::new())
            } else {
                js_to_json(&ctx, value, 0).map_err(|error| {
                    Exception::throw_message(
                        &ctx,
                        &format!("invalid tool update value: {error}"),
                    )
                })?
            };
            send_frame(
                &from_js_tx,
                ExtensionFrame::Update {
                    id: request_id.clone(),
                    value,
                },
                max_frame_bytes,
            )
            .map_err(|message| Exception::throw_message(&ctx, &message))
        },
    )
}

fn build_context_object<'js>(
    ctx: &Ctx<'js>,
    hello: &HelloState,
    signal: JsValue<'js>,
    model: Option<&Model>,
) -> JsResult<Object<'js>> {
    let object = Object::new(ctx.clone())?;
    object.set("mode", mode_string(hello.mode))?;
    object.set("hasUI", hello.has_ui)?;
    object.set("cwd", &hello.cwd)?;
    let model_value = match model {
        Some(model) => match serde_json::to_value(model) {
            Ok(value) => json_to_js(ctx, &value, 0).unwrap_or_else(|_| JsValue::new_null(ctx.clone())),
            Err(_) => JsValue::new_null(ctx.clone()),
        },
        None => JsValue::new_null(ctx.clone()),
    };
    object.set("model", model_value)?;
    object.set("signal", signal)?;
    let ui: Object = ctx.globals().get(UI_GLOBAL)?;
    object.set("ui", ui)?;
    // Phase 4 ctx session actions (`abort`/`compact`/`shutdown`/`waitForIdle`/
    // `reload`), mirroring the process bridge's createContext. The handler
    // functions are plain JS from the shared bootstrap; they reach the
    // `__piActionRequest` bridge through the global at call time.
    let session_actions: Object = ctx.globals().get(SESSION_ACTIONS_GLOBAL)?;
    for method in CTX_SESSION_METHODS {
        let handler: Function = session_actions.get(method)?;
        object.set(method, handler)?;
    }
    Ok(object)
}

fn mode_string(mode: ExtensionMode) -> &'static str {
    match mode {
        ExtensionMode::Tui => "tui",
        ExtensionMode::Rpc => "rpc",
        ExtensionMode::Json => "json",
        ExtensionMode::Print => "print",
    }
}

/// Mirror of the host-side `validate_identifier` grammar (`[A-Za-z0-9_-.]`,
/// 1-128 bytes, no control characters) for load-phase registration values.
fn validate_js_identifier(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if value.len() > 128 {
        return Err(format!("{field} exceeds 128 bytes"));
    }
    if value
        .chars()
        .any(|character| !(character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')))
    {
        return Err(format!("{field} {value:?} contains unsupported characters"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// JSON <-> JS value bridge (no rquickjs-serde; ~recursive hand conversion)
// ---------------------------------------------------------------------------

fn json_to_js<'js>(ctx: &Ctx<'js>, value: &JsonValue, depth: usize) -> JsResult<JsValue<'js>> {
    if depth > VALUE_BRIDGE_MAX_DEPTH {
        return Err(rquickjs::Error::FromJs {
            from: "json value",
            to: "javascript value",
            message: Some(format!("maximum nesting depth {VALUE_BRIDGE_MAX_DEPTH} exceeded")),
        });
    }
    Ok(match value {
        JsonValue::Null => JsValue::new_null(ctx.clone()),
        JsonValue::Bool(boolean) => JsValue::new_bool(ctx.clone(), *boolean),
        JsonValue::Number(number) => {
            if let Some(integer) = number.as_i64() {
                match i32::try_from(integer) {
                    Ok(integer) => JsValue::new_int(ctx.clone(), integer),
                    Err(_) => JsValue::new_float(ctx.clone(), integer as f64),
                }
            } else if let Some(integer) = number.as_u64() {
                match i64::try_from(integer) {
                    Ok(integer) => match i32::try_from(integer) {
                        Ok(integer) => JsValue::new_int(ctx.clone(), integer),
                        Err(_) => JsValue::new_float(ctx.clone(), integer as f64),
                    },
                    Err(_) => JsValue::new_float(ctx.clone(), integer as f64),
                }
            } else {
                JsValue::new_float(ctx.clone(), number.as_f64().unwrap_or_default())
            }
        }
        JsonValue::String(string) => JsString::from_str(ctx.clone(), string)?.into_value(),
        JsonValue::Array(items) => {
            let array = Array::new(ctx.clone())?;
            for (index, item) in items.iter().enumerate() {
                array.set(index, json_to_js(ctx, item, depth + 1)?)?;
            }
            array.into_value()
        }
        JsonValue::Object(fields) => {
            let object = Object::new(ctx.clone())?;
            for (key, item) in fields {
                object.set(key.as_str(), json_to_js(ctx, item, depth + 1)?)?;
            }
            object.into_value()
        }
    })
}

fn js_to_json<'js>(ctx: &Ctx<'js>, value: JsValue<'js>, depth: usize) -> JsResult<JsonValue> {
    if depth > VALUE_BRIDGE_MAX_DEPTH {
        return Err(rquickjs::Error::FromJs {
            from: "javascript value",
            to: "json value",
            message: Some(format!("maximum nesting depth {VALUE_BRIDGE_MAX_DEPTH} exceeded")),
        });
    }
    if value.is_null() || value.is_undefined() {
        return Ok(JsonValue::Null);
    }
    if let Some(boolean) = value.as_bool() {
        return Ok(JsonValue::Bool(boolean));
    }
    if let Some(integer) = value.as_int() {
        return Ok(JsonValue::from(integer));
    }
    if let Some(float) = value.as_float() {
        if float.is_finite() {
            return Ok(JsonValue::from(float));
        }
        return Err(rquickjs::Error::FromJs {
            from: "javascript number",
            to: "json value",
            message: Some(format!("non-finite number {float} is not representable in JSON")),
        });
    }
    if let Some(string) = value.as_string() {
        return Ok(JsonValue::String(string.to_string()?));
    }
    if value.is_array() {
        let array = value.as_array().ok_or_else(|| rquickjs::Error::FromJs {
            from: "javascript value",
            to: "json value",
            message: Some("expected an array".to_owned()),
        })?;
        let mut items = Vec::with_capacity(array.len());
        for item in array.clone().into_iter() {
            items.push(js_to_json(ctx, item?, depth + 1)?);
        }
        return Ok(JsonValue::Array(items));
    }
    if value.is_object() {
        let object = value.as_object().ok_or_else(|| rquickjs::Error::FromJs {
            from: "javascript value",
            to: "json value",
            message: Some("expected an object".to_owned()),
        })?;
        let mut fields = JsonMap::new();
        for entry in object.clone().into_iter() {
            let (key, item) = entry?;
            fields.insert(key.to_string()?, js_to_json(ctx, item, depth + 1)?);
        }
        return Ok(JsonValue::Object(fields));
    }
    Err(rquickjs::Error::FromJs {
        from: "javascript value",
        to: "json value",
        message: Some(
            "unsupported value type (function, symbol, bigint, or exotic object)".to_owned(),
        ),
    })
}

// ---------------------------------------------------------------------------
// Frame plumbing and diagnostics
// ---------------------------------------------------------------------------

fn send_frame(
    from_js_tx: &mpsc::UnboundedSender<ExtensionFrame>,
    frame: ExtensionFrame,
    max_frame_bytes: usize,
) -> Result<(), String> {
    if frame_bytes(&frame) > max_frame_bytes {
        return Err(format!("extension protocol frame exceeds {max_frame_bytes} bytes"));
    }
    from_js_tx
        .send(frame)
        .map_err(|_| "quickjs extension transport closed".to_owned())
}

fn frame_bytes<T: serde::Serialize>(value: &T) -> usize {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
}

fn js_err_text(error: &rquickjs::Error) -> String {
    error.to_string()
}

fn exception_message<'js>(ctx: &Ctx<'js>, error: &rquickjs::Error) -> String {
    if matches!(error, rquickjs::Error::Exception) {
        let value: JsValue = ctx.catch();
        if let Some(message) = exception_text(ctx, value) {
            return message;
        }
    }
    error.to_string()
}

fn exception_text<'js>(ctx: &Ctx<'js>, value: JsValue<'js>) -> Option<String> {
    if let Some(object) = value.as_object() {
        if let Ok(Some(message)) = object.get::<_, Option<String>>("message") {
            if !message.is_empty() {
                return Some(message);
            }
        }
    }
    JsString::from_js(ctx, value)
        .ok()
        .and_then(|string| string.to_string().ok())
        .filter(|message| !message.is_empty())
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn deadline_millis(duration: Duration) -> u64 {
    duration.as_millis() as u64
}
