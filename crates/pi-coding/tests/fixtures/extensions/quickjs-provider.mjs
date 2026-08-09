// QuickJS provider fixture: registers runtime providers whose JS streams yield
// scripted events (text/thinking/tool-call), plus failure and edge-case
// providers exercising the typed stream-error and driver-validation paths.
export default function (pi) {
  pi.registerProvider({
    id: "fixture-llm",
    label: "Fixture LLM",
    api: "fixture-llm-api",
    capabilities: ["streaming"],
    stream: async function* (sessionId, messages, options) {
      yield { type: "start" };
      yield { type: "thinking", thinking: "hmm" };
      yield { type: "text", text: "hello" };
      yield { type: "text_delta", delta: " world" };
      yield { type: "tool_call", name: "lookup", arguments: { q: "rust" } };
      yield {
        type: "text",
        text: `session=${sessionId};messages=${messages.length};options=${JSON.stringify(options)}`,
      };
      yield { type: "done", stopReason: "stop" };
    },
  });

  // A plain sync iterable is accepted by the driver too.
  pi.registerProvider({
    id: "fixture-sync",
    api: "fixture-sync-api",
    stream: function () {
      return [{ type: "text", text: "sync" }];
    },
  });

  // Throws mid-stream: the bridge must surface a typed stream error with the
  // secret-looking text redacted, never a crashed session.
  pi.registerProvider({
    id: "fixture-failing",
    api: "fixture-failing-api",
    stream: async function* () {
      yield { type: "text", text: "partial" };
      throw new Error("boom: token=abc123");
    },
  });

  // Emits an explicit error event instead of throwing.
  pi.registerProvider({
    id: "fixture-error-event",
    api: "fixture-error-event-api",
    stream: async function* () {
      yield { type: "error", error: "provider says no" };
    },
  });

  // Returns a non-iterable: the driver must fail actionably.
  pi.registerProvider({
    id: "fixture-not-iterable",
    api: "fixture-not-iterable-api",
    stream: async function () {
      return 42;
    },
  });
}
