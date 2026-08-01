import { AsyncLocalStorage } from "node:async_hooks";
import { pathToFileURL } from "node:url";

const protocolWrite = process.stdout.write.bind(process.stdout);
const PROTOCOL_VERSION = 1;
const extensionId = process.env.PI_EXTENSION_ID ?? "";
const entryPath = process.env.PI_EXTENSION_ENTRY ?? "";
const cwd = process.cwd();
const maxFrameBytes = Number(process.env.PI_EXTENSION_MAX_FRAME_BYTES ?? "1048576");
if (!Number.isSafeInteger(maxFrameBytes) || maxFrameBytes < 1024) {
  throw new Error("PI_EXTENSION_MAX_FRAME_BYTES must be an integer of at least 1024");
}
const inputDecoder = new TextDecoder("utf-8", { fatal: true });
const capabilities = parseJsonArray(process.env.PI_EXTENSION_CAPABILITIES);
const uiCapabilities = parseJsonArray(process.env.PI_EXTENSION_UI_CAPABILITIES);
const secrets = Object.entries(process.env)
  .filter(([key, value]) => value && value.length >= 4 && /token|key|secret|password|auth|credential/i.test(key))
  .map(([, value]) => value);
const supportedEvents = new Set([
  "project_trust", "resources_discover", "session_start", "session_info_changed",
  "session_before_switch", "session_before_fork", "session_before_compact", "session_compact",
  "session_shutdown", "session_before_tree", "session_tree", "context",
  "before_provider_request", "before_provider_headers", "after_provider_response",
  "before_agent_start", "agent_start", "agent_end", "agent_settled", "turn_start", "turn_end",
  "message_start", "message_update", "message_end", "tool_execution_start",
  "tool_execution_update", "tool_execution_end", "model_select", "thinking_level_select",
  "tool_call", "tool_result", "user_bash", "input",
]);

const commands = new Map();
const tools = new Map();
const hooks = new Map();
const invocations = new Map();
const runtimeRequests = new Map();
const uiRequests = new Map();
const bus = new Map();
const invocationStorage = new AsyncLocalStorage();
function invocationState() {
  return invocationStorage.getStore() ?? latestContext;
}

function queuedAction(action) {
  const state = invocationState();
  const promise = requestAction(action, state.signal);
  if (Array.isArray(state.actionQueue)) state.actionQueue.push(promise);
  return promise;
}

let loaded = false;
let hostContext;
let latestContext = {};
let nextRuntimeRequest = 0;
let nextUiRequest = 0;
function requestAction(action, signal) {
  requireCapability("session_actions", "session action");
  const id = `bun-action-${++nextRuntimeRequest}`;
  return new Promise((resolve, reject) => {
    const onAbort = () => {
      runtimeRequests.delete(id);
      send({ type: "cancel", id });
      reject(new Error("extension action was cancelled"));
    };
    if (signal?.aborted) { onAbort(); return; }
    signal?.addEventListener("abort", onAbort, { once: true });
    runtimeRequests.set(id, {
      resolve: value => { signal?.removeEventListener("abort", onAbort); resolve(value); },
      reject: error => { signal?.removeEventListener("abort", onAbort); reject(error); },
    });
    send({ type: "request", id, request: { kind: "action", action } });
  });
}


function parseJsonArray(encoded) {
  if (!encoded) return [];
  try {
    const value = JSON.parse(encoded);
    return Array.isArray(value) ? value : [];
  } catch {
    return [];
  }
}

function sanitizeText(value) {
  let text = String(value ?? "extension operation failed");
  if (entryPath) text = text.replaceAll(entryPath, "<entry>");
  if (cwd) text = text.replaceAll(cwd, "<cwd>");
  for (const secret of secrets) text = text.replaceAll(secret, "<redacted>");
  return text
    .replaceAll(/file:\/\/[^\s)]+/g, "<file>")
    .replaceAll(/[\u0000-\u001f\u007f]+/g, " ")
    .replaceAll(/\s+/g, " ")
    .trim()
    .slice(0, 2048) || "extension operation failed";
}

function diagnostic(level, values) {
  const text = values.map(value => {
    if (typeof value === "string") return value;
    try { return JSON.stringify(value); } catch { return String(value); }
  }).join(" ");
  process.stderr.write(`[bun-extension:${level}] ${sanitizeText(text)}\n`);
}

process.stdout.write = (chunk, encoding, callback) => {
  const text = typeof chunk === "string"
    ? chunk
    : Buffer.from(chunk).toString(typeof encoding === "string" ? encoding : undefined);
  diagnostic("stdout", [text]);
  if (typeof encoding === "function") encoding();
  if (typeof callback === "function") callback();
  return true;
};

globalThis.console = Object.freeze({
  log: (...values) => diagnostic("log", values),
  info: (...values) => diagnostic("info", values),
  warn: (...values) => diagnostic("warn", values),
  error: (...values) => diagnostic("error", values),
  debug: (...values) => diagnostic("debug", values),
});

function send(frame) {
  const encoded = JSON.stringify(frame);
  if (Buffer.byteLength(encoded, "utf8") > maxFrameBytes) {
    throw new Error(`extension protocol frame exceeds ${maxFrameBytes} bytes`);
  }
  protocolWrite(`${encoded}\n`);
}
function rejectPending(message) {
  for (const pending of uiRequests.values()) pending.reject(new Error(message));
  for (const pending of runtimeRequests.values()) pending.reject(new Error(message));
  uiRequests.clear();
  runtimeRequests.clear();
}


function success(id, value = null) {
  send({ type: "response", id, result: { status: "success", value } });
}

function failure(id, code, error) {
  const message = sanitizeText(error instanceof Error ? error.message : error);
  send({ type: "response", id, result: { status: "failure", error: { code, message } } });
}

function requireCapability(capability, operation) {
  if (!capabilities.includes(capability)) {
    throw new Error(`${operation} requires the ${capability} capability`);
  }
}

function unavailable(method) {
  throw new Error(`${method} is unavailable in the process-hosted ExtensionAPI`);
}

function jsonValue(value, fallback = null) {
  if (value === undefined) return fallback;
  return JSON.parse(JSON.stringify(value));
}

function eventBus() {
  return Object.freeze({
    emit(channel, data) {
      for (const handler of [...(bus.get(String(channel)) ?? [])]) handler(data);
    },
    on(channel, handler) {
      if (typeof handler !== "function") throw new Error("events.on requires a handler");
      const name = String(channel);
      const handlers = bus.get(name) ?? [];
      handlers.push(handler);
      bus.set(name, handlers);
      return () => {
        const current = bus.get(name);
        if (!current) return;
        const index = current.indexOf(handler);
        if (index >= 0) current.splice(index, 1);
        if (current.length === 0) bus.delete(name);
      };
    },
  });
}

function createApi() {
  return Object.freeze({
    events: eventBus(),
    on(event, handler) {
      requireCapability("event_hooks", "event registration");
      if (!supportedEvents.has(event)) throw new Error(`unsupported extension event ${String(event)}`);
      if (typeof handler !== "function") throw new Error("pi.on requires an event name and handler");
      let handlers = hooks.get(event);
      if (!handlers) {
        handlers = [];
        hooks.set(event, handlers);
        send({ type: "register", registration: { kind: "event_hook", hook: { event } } });
      }
      handlers.push(handler);
    },
    registerCommand(name, options) {
      requireCapability("commands", "command registration");
      if (typeof name !== "string" || !options || typeof options.handler !== "function") {
        throw new Error("registerCommand requires a name and handler");
      }
      if (options.getArgumentCompletions !== undefined) unavailable("registerCommand.getArgumentCompletions");
      if (commands.has(name)) throw new Error(`duplicate command ${name}`);
      commands.set(name, options.handler);
      send({
        type: "register",
        registration: {
          kind: "command",
          command: { name, ...(options.description === undefined ? {} : { description: String(options.description) }) },
        },
      });
    },
    registerTool(tool) {
      requireCapability("tools", "tool registration");
      if (!tool || typeof tool.name !== "string" || typeof tool.execute !== "function") {
        throw new Error("registerTool requires a name and execute function");
      }
      const unsupported = ["constrainedSampling", "renderShell", "prepareArguments", "renderCall", "renderResult"]
        .find(field => tool[field] !== undefined);
      if (unsupported) unavailable(`registerTool.${unsupported}`);
      if (tools.has(tool.name)) throw new Error(`duplicate tool ${tool.name}`);
      tools.set(tool.name, tool);
      send({
        type: "register",
        registration: {
          kind: "tool",
          tool: {
            name: tool.name,
            label: String(tool.label ?? tool.name),
            description: String(tool.description ?? "Extension tool"),
            parameters: jsonValue(tool.parameters, { type: "object", properties: {} }),
            executionMode: tool.executionMode ?? "default",
            promptGuidelines: Array.isArray(tool.promptGuidelines) ? tool.promptGuidelines.map(String) : [],
          },
        },
      });
    },
    getSessionName: () => invocationState().sessionName,
    getThinkingLevel: () => invocationState().thinkingLevel,
    getActiveTools: () => invocationState().activeTools ?? [],
    getAllTools: () => invocationState().allTools ?? [],
    getCommands: () => invocationState().commands ?? [],
    sendMessage(message, options = {}) {
      const state = invocationState();
      const delivery = options.deliverAs ?? (state.isIdle ? "followUp" : "steer");
      queuedAction({ kind: "send_message", message, delivery, triggerTurn: options.triggerTurn ?? false });
    },
    sendUserMessage(content, options = {}) {
      const state = invocationState();
      const delivery = options.deliverAs ?? (state.isIdle ? "followUp" : "steer");
      queuedAction({ kind: "send_user_message", content, delivery });
    },
    appendEntry(customType, data) {
      queuedAction({ kind: "append_entry", customType: String(customType), ...(data === undefined ? {} : { data }) });
    },
    setSessionName(name) { queuedAction({ kind: "set_session_name", name: String(name) }); },
    setLabel(entryId, label) { queuedAction({ kind: "set_label", entryId: String(entryId), ...(label === undefined ? {} : { label }) }); },
    setActiveTools(toolNames) { queuedAction({ kind: "set_active_tools", toolNames: toolNames.map(String) }); },
    setThinkingLevel(level) { queuedAction({ kind: "set_thinking_level", level }); },
    setModel(model) { const state = invocationState(); return requestAction({ kind: "set_model", model }, state.signal); },
    registerShortcut: () => unavailable("registerShortcut"),
    registerFlag: () => unavailable("registerFlag"),
    getFlag: () => unavailable("getFlag"),
    registerMessageRenderer: () => unavailable("registerMessageRenderer"),
    registerEntryRenderer: () => unavailable("registerEntryRenderer"),
    exec: () => unavailable("exec"),
    registerProvider: () => unavailable("registerProvider"),
    unregisterProvider: () => unavailable("unregisterProvider"),
  });
}

function timeoutMs(options) {
  const value = options?.timeout;
  return Number.isFinite(value) && value >= 0 ? Math.floor(value) : undefined;
}

function requestUi(request, signal) {
  requireCapability("ui", "UI action");
  const id = `bun-ui-${++nextUiRequest}`;
  return new Promise((resolve, reject) => {
    const onAbort = () => {
      uiRequests.delete(id);
      send({ type: "cancel", id });
      reject(new Error("UI request was cancelled"));
    };
    if (signal?.aborted) { onAbort(); return; }
    signal?.addEventListener("abort", onAbort, { once: true });
    uiRequests.set(id, {
      resolve: value => { signal?.removeEventListener("abort", onAbort); resolve(value); },
      reject: error => { signal?.removeEventListener("abort", onAbort); reject(error); },
    });
    send({ type: "request", id, request: { kind: "ui", ui: request } });
  });
}

function enqueueUi(queue, request, signal) {
  queue.push(requestUi({ request }, signal));
}

function createUi(controller, queue) {
  return Object.freeze({
    async select(title, options, opts) {
      const normalized = options.map(option => typeof option === "string"
        ? { value: option, label: option }
        : {
            value: String(option.value ?? option.label),
            label: String(option.label),
            ...(option.description === undefined ? {} : { description: String(option.description) }),
          });
      const response = await requestUi({
        request: { type: "select", title: String(title), options: normalized },
        ...(timeoutMs(opts) === undefined ? {} : { timeoutMs: timeoutMs(opts) }),
      }, controller.signal);
      return response?.type === "selected" ? response.value : undefined;
    },
    async confirm(title, message, opts) {
      const response = await requestUi({
        request: { type: "confirm", title: String(title), message: String(message) },
        ...(timeoutMs(opts) === undefined ? {} : { timeoutMs: timeoutMs(opts) }),
      }, controller.signal);
      return response?.type === "confirmed" ? response.confirmed : false;
    },
    async input(title, placeholder, opts) {
      const response = await requestUi({
        request: { type: "input", title: String(title), ...(placeholder === undefined ? {} : { placeholder: String(placeholder) }) },
        ...(timeoutMs(opts) === undefined ? {} : { timeoutMs: timeoutMs(opts) }),
      }, controller.signal);
      return response?.type === "input" ? response.value : undefined;
    },
    async editor(title, prefill, opts) {
      const response = await requestUi({
        request: { type: "editor", title: String(title), ...(prefill === undefined ? {} : { prefill: String(prefill) }) },
        ...(timeoutMs(opts) === undefined ? {} : { timeoutMs: timeoutMs(opts) }),
      }, controller.signal);
      return response?.type === "edited" ? response.value : undefined;
    },
    notify(message, level = "info") {
      enqueueUi(queue, { type: "notify", message: String(message), level }, controller.signal);
    },
    setStatus(key, text) {
      enqueueUi(queue, { type: "status", key: String(key), ...(text === undefined ? {} : { text: String(text) }) }, controller.signal);
    },
    setWidget(key, content, options) {
      if (content !== undefined && (!Array.isArray(content) || content.some(line => typeof line !== "string"))) {
        unavailable("ui.setWidget component factories");
      }
      enqueueUi(queue, {
        type: "widget",
        key: String(key),
        ...(content === undefined ? {} : { lines: content }),
        placement: options?.placement === "belowEditor" ? "below_editor" : "above_editor",
      }, controller.signal);
    },
    setTitle(title) { enqueueUi(queue, { type: "title", title: String(title) }, controller.signal); },
    setEditorText(text) { enqueueUi(queue, { type: "set_editor_text", text: String(text) }, controller.signal); },
    onTerminalInput: () => unavailable("ui.onTerminalInput"),
    setWorkingMessage: () => unavailable("ui.setWorkingMessage"),
    setWorkingVisible: () => unavailable("ui.setWorkingVisible"),
    setWorkingIndicator: () => unavailable("ui.setWorkingIndicator"),
    setHiddenThinkingLabel: () => unavailable("ui.setHiddenThinkingLabel"),
    setFooter: () => unavailable("ui.setFooter component factories"),
    setHeader: () => unavailable("ui.setHeader component factories"),
    custom: () => unavailable("ui.custom component factories"),
    pasteToEditor: () => unavailable("ui.pasteToEditor"),
    getEditorText: () => unavailable("ui.getEditorText"),
    addAutocompleteProvider: () => unavailable("ui.addAutocompleteProvider"),
    setEditorComponent: () => unavailable("ui.setEditorComponent factories"),
    getEditorComponent: () => unavailable("ui.getEditorComponent"),
    get theme() { return unavailable("ui.theme"); },
    getAllThemes: () => unavailable("ui.getAllThemes"),
    getTheme: () => unavailable("ui.getTheme"),
    setTheme: () => unavailable("ui.setTheme"),
    getToolsExpanded: () => unavailable("ui.getToolsExpanded"),
    setToolsExpanded: () => unavailable("ui.setToolsExpanded"),
  });
}

function createContext(controller, queue, snapshot) {
  return Object.freeze({
    ui: createUi(controller, queue),
    mode: hostContext?.mode,
    hasUI: hostContext?.mode === "tui" || hostContext?.mode === "rpc",
    cwd: hostContext?.cwd ?? cwd,
    sessionManager: Object.freeze({
      getSessionId: () => snapshot.sessionId ?? "",
      getSessionFile: () => snapshot.sessionFile,
    }),
    model: snapshot.model,
    thinkingLevel: snapshot.thinkingLevel,
    signal: controller.signal,
    isIdle: () => Boolean(snapshot.isIdle),
    isProjectTrusted: () => Boolean(snapshot.projectTrusted ?? hostContext?.projectTrusted),
    abort() { queuedAction({ kind: "abort" }); },
    hasPendingMessages: () => Boolean(snapshot.hasPendingMessages),
    getContextUsage: () => snapshot.contextUsage,
    getSystemPrompt: () => snapshot.systemPrompt ?? "",
    compact(instructionsOrOptions) {
      const customInstructions = typeof instructionsOrOptions === "string"
        ? instructionsOrOptions
        : instructionsOrOptions?.customInstructions;
      queuedAction({ kind: "compact", ...(customInstructions === undefined ? {} : { customInstructions }) });
    },
    shutdown() { queuedAction({ kind: "shutdown" }); },
    modelRegistry: new Proxy({}, { get: () => unavailable("ctx.modelRegistry") }),
    waitForIdle: () => { const state = invocationState(); return requestAction({ kind: "wait_for_idle" }, state.signal); },
    newSession: () => unavailable("ctx.newSession"),
    fork: () => unavailable("ctx.fork"),
    navigateTree: () => unavailable("ctx.navigateTree"),
    switchSession: () => unavailable("ctx.switchSession"),
    reload: () => { const state = invocationState(); return requestAction({ kind: "reload" }, state.signal); },
    getSystemPromptOptions: () => unavailable("ctx.getSystemPromptOptions"),
  });
}

async function settleUi(queue) {
  const results = await Promise.allSettled(queue);
  const rejected = results.find(result => result.status === "rejected");
  if (rejected) throw rejected.reason;
}

function cancelled(controller) {
  return new Promise((_, reject) => {
    if (controller.signal.aborted) { reject(new Error("extension invocation was cancelled")); return; }
    controller.signal.addEventListener("abort", () => reject(new Error("extension invocation was cancelled")), { once: true });
  });
}

async function invoke(id, invocation, context) {
  const snapshot = context && typeof context === "object" ? context : latestContext;
  latestContext = snapshot;
  const controller = new AbortController();
  const queue = [];
  const state = { ...snapshot, signal: controller.signal, actionQueue: queue };
  const contextValue = createContext(controller, queue, snapshot);
  invocations.set(id, controller);
  try {
    const operation = invocationStorage.run(state, async () => {
      if (invocation.kind === "command") {
        const handler = commands.get(invocation.name);
        if (!handler) throw new Error(`unknown command ${invocation.name}`);
        return handler(invocation.arguments, contextValue);
      }
      if (invocation.kind === "tool") {
        const tool = tools.get(invocation.name);
        if (!tool) throw new Error(`unknown tool ${invocation.name}`);
        const onUpdate = update => send({ type: "update", id, value: jsonValue(update, {}) });
        return tool.execute(invocation.callId, invocation.arguments, controller.signal, onUpdate, contextValue);
      }
      if (invocation.kind !== "event") unavailable(`invocation ${invocation.kind}`);
      const name = invocation.event.name;
      const data = invocation.event.data;
      const eventData = data && typeof data === "object" && !Array.isArray(data)
        ? { ...data }
        : { data };
      const originalInput = eventData.input === undefined ? undefined : jsonValue(eventData.input);
      const originalHeaders = eventData.headers === undefined ? undefined : jsonValue(eventData.headers);
      const originalMessage = eventData.message === undefined ? undefined : jsonValue(eventData.message);
      const originalPayload = eventData.payload === undefined ? undefined : jsonValue(eventData.payload);
      if (name === "session_before_compact" || name === "session_before_tree") eventData.signal = controller.signal;
      let value = null;
      for (const handler of hooks.get(name) ?? []) {
        const event = Object.freeze({ type: name, ...eventData });
        const next = await handler(event, contextValue);
        if (next === undefined) continue;
        if (name === "context" && next && typeof next === "object" && next.messages !== undefined) {
          eventData.messages = next.messages;
          value = { messages: eventData.messages };
        } else if (name === "before_provider_request") {
          eventData.payload = next;
          value = next;
        } else if (name === "before_agent_start" && next && typeof next === "object") {
          if (next.systemPrompt !== undefined) eventData.systemPrompt = next.systemPrompt;
          const prior = value && typeof value === "object" ? value : {};
          const messages = Array.isArray(prior.messages) ? [...prior.messages] : [];
          if (next.message !== undefined) messages.push(next.message);
          value = {
            ...prior,
            ...next,
            ...(messages.length === 0 ? {} : { messages }),
          };
        } else if (name === "before_provider_headers" && next && typeof next === "object") {
          if (next.headers !== undefined) eventData.headers = next.headers;
          value = { ...(value && typeof value === "object" ? value : {}), ...next };
        } else if (name === "tool_result" && next && typeof next === "object") {
          for (const field of ["content", "details", "isError", "usage"]) {
            if (next[field] !== undefined) eventData[field] = next[field];
          }
          value = { ...(value && typeof value === "object" ? value : {}), ...next };
        } else if (name === "message_end" && next && typeof next === "object") {
          if (next.message !== undefined) eventData.message = next.message;
          value = { ...(value && typeof value === "object" ? value : {}), ...next };
        } else if (name === "input" && next && typeof next === "object") {
          if (next.text !== undefined) eventData.text = next.text;
          if (next.images !== undefined) eventData.images = next.images;
          value = { ...(value && typeof value === "object" ? value : {}), ...next };
          if (next.action === "handled") break;
        } else {
          value = next;
        }
        if (next && typeof next === "object"
          && (next.cancel === true
            || (name === "tool_call" && next.block === true)
            || (name === "project_trust" && (next.trusted === "yes" || next.trusted === "no"))
            || name === "user_bash")) break;
      }
      const mergeMutation = (field, original) => {
        if (eventData[field] === undefined || JSON.stringify(eventData[field]) === JSON.stringify(original)) return;
        value = { ...(value && typeof value === "object" ? value : {}), [field]: eventData[field] };
      };
      if (name === "tool_call" || name === "tool_result") mergeMutation("input", originalInput);
      if (name === "before_provider_headers") mergeMutation("headers", originalHeaders);
      if (name === "message_end") mergeMutation("message", originalMessage);
      if (name === "before_provider_request" && JSON.stringify(eventData.payload) !== JSON.stringify(originalPayload)) value = eventData.payload;
      return value;
    });
    const value = await Promise.race([operation, cancelled(controller)]);
    await settleUi(queue);
    success(id, jsonValue(value, null));
  } catch (error) {
    failure(id, controller.signal.aborted ? "cancelled" : "extension_error", error);
  } finally {
    invocations.delete(id);
  }
}

async function loadFactory() {
  if (loaded) throw new Error("extension factory was already loaded");
  if (!entryPath) throw new Error("Bun extension entry is not configured");
  let module;
  try { module = await import(pathToFileURL(entryPath).href); }
  catch { throw new Error("failed to import Bun extension entry"); }
  if (typeof module.default !== "function") {
    throw new Error("Bun extension entry must default-export a factory function");
  }
  await module.default(createApi());
  loaded = true;
}

function finishUi(frame) {
  const pending = uiRequests.get(frame.id);
  if (!pending) return;
  uiRequests.delete(frame.id);
  if (frame.result.status === "success") pending.resolve(frame.result.value);
  else pending.reject(new Error(frame.result.error?.message ?? "UI request failed"));
}

async function handle(frame) {
  if (frame.type === "hello") {
    if (!extensionId) throw new Error("PI_EXTENSION_ID is required");
    if (frame.protocolVersion !== PROTOCOL_VERSION) throw new Error("unsupported extension protocol version");
    hostContext = frame;
    latestContext = { projectTrusted: frame.projectTrusted };
    send({
      type: "hello",
      protocolVersion: PROTOCOL_VERSION,
      manifest: { id: extensionId, name: `Bun extension ${extensionId}`, version: "1.0.0", capabilities, uiCapabilities },
    });
  } else if (frame.type === "request") {
    if (frame.request.kind === "load") {
      try { await loadFactory(); success(frame.id); }
      catch (error) { failure(frame.id, "load_failed", error); }
    } else if (frame.request.kind === "initialize") {
      success(frame.id);
    } else if (frame.request.kind === "invoke") {
      await invoke(frame.id, frame.request.invocation, frame.request.context);
    } else {
      failure(frame.id, "unsupported_request", "unsupported host request");
    }
  } else if (frame.type === "response") {
    const pending = runtimeRequests.get(frame.id);
    if (pending) {
      runtimeRequests.delete(frame.id);
      if (frame.result.status === "success") pending.resolve(frame.result.value);
      else pending.reject(new Error(frame.result.error?.message ?? "extension action failed"));
    } else {
      finishUi(frame);
    }
  } else if (frame.type === "cancel") {
    invocations.get(frame.id)?.abort();
  } else if (frame.type === "shutdown") {
    for (const controller of invocations.values()) controller.abort();
    rejectPending("extension is shutting down");
    setTimeout(() => process.exit(0), 0);
  } else {
    throw new Error("unsupported host frame");
  }
}

let inputBuffer = Buffer.alloc(0);
let inputClosed = false;

function protocolFailure(message) {
  if (inputClosed) return;
  inputClosed = true;
  process.stderr.write(`[bun-extension:error] ${message}\n`);
  process.stdin.destroy();
  process.exitCode = 1;
}

function processInputChunk(chunk) {
  if (inputClosed) return;
  let incoming = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
  while (incoming.length > 0) {
    const newline = incoming.indexOf(0x0a);
    if (newline < 0) {
      if (inputBuffer.length + incoming.length > maxFrameBytes) {
        protocolFailure(`extension protocol frame exceeds ${maxFrameBytes} bytes`);
        return;
      }
      inputBuffer = inputBuffer.length === 0
        ? Buffer.from(incoming)
        : Buffer.concat([inputBuffer, incoming], inputBuffer.length + incoming.length);
      return;
    }
    if (inputBuffer.length + newline > maxFrameBytes) {
      protocolFailure(`extension protocol frame exceeds ${maxFrameBytes} bytes`);
      return;
    }
    const prefix = incoming.subarray(0, newline);
    const encoded = inputBuffer.length === 0
      ? prefix
      : Buffer.concat([inputBuffer, prefix], inputBuffer.length + prefix.length);
    inputBuffer = Buffer.alloc(0);
    incoming = incoming.subarray(newline + 1);
    if (encoded.length === 0 || encoded.at(-1) === 0x0d) {
      protocolFailure("invalid LF JSONL frame");
      return;
    }
    let frame;
    try { frame = JSON.parse(inputDecoder.decode(encoded)); }
    catch {
      protocolFailure("invalid JSON frame");
      return;
    }
    void handle(frame).catch(error => protocolFailure(sanitizeText(error instanceof Error ? error.message : error)));
  }
}

process.stdin.on("data", processInputChunk);
process.stdin.on("end", () => {
  for (const controller of invocations.values()) controller.abort();
  rejectPending("extension input closed");
  if (!inputClosed && inputBuffer.length !== 0) protocolFailure("extension protocol ended with a non-LF-terminated frame");
});
process.stdin.on("error", error => {
  rejectPending("extension input failed");
  protocolFailure(sanitizeText(error.message));
});
