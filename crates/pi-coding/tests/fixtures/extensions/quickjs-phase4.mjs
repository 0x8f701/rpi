// QuickJS Phase 4 fixture: tool onUpdate streaming, session actions over the
// action channel (pi + ctx), and a cancellation-aware hang used to prove the
// AbortController shim settles a pending invocation on Cancel.
export default function (pi) {
  pi.registerCommand("probe", {
    description: "prove the runtime stays usable after cancellation",
    handler: async () => "phase4-probe-ok",
  });

  // Emits two onUpdate frames (in order) then returns the final result; the
  // host must observe both updates before the result frame.
  pi.registerTool({
    name: "phase4_stream",
    label: "Phase 4 Stream",
    description: "Emit two onUpdate frames then return the final result",
    parameters: {
      type: "object",
      properties: { value: { type: "string" } },
      required: ["value"],
    },
    execute: async (callId, params, signal, onUpdate) => {
      onUpdate?.({ content: [{ type: "text", text: "partial" }] });
      onUpdate?.({ content: [{ type: "text", text: "second" }], details: { step: 2 } });
      return { content: [{ type: "text", text: `final:${params.value}` }] };
    },
  });

  // pi.setModel must resolve with the host's boolean (the acceptance contract).
  pi.registerCommand("phase4_set_model", {
    description: "pi.setModel resolves with the host's boolean",
    handler: async () => {
      return await pi.setModel({
        id: "test-model",
        name: "Test Model",
        api: "openai-completions",
        provider: "fixture-provider",
        baseUrl: "https://fixture.invalid/v1",
        reasoning: true,
        input: ["text"],
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
        contextWindow: 8192,
        maxTokens: 1024,
      });
    },
  });

  // Every session action round-trips the host result back; ctx actions are the
  // process bridge's createContext surface (abort/compact/shutdown/waitForIdle/
  // reload), the rest live on pi.
  pi.registerCommand("phase4_session_actions", {
    description: "Every session action round-trips the host result back",
    handler: async (_args, ctx) => {
      const results = {};
      results.setThinkingLevel = await pi.setThinkingLevel("high");
      results.setActiveTools = await pi.setActiveTools(["tool-a", "tool-b"]);
      results.setLabel = await pi.setLabel("entry-1", "label-x");
      results.appendEntry = await pi.appendEntry("custom-type", { key: "value" });
      results.sendMessage = await pi.sendMessage(
        { customType: "custom", content: "hi", display: true },
        { deliverAs: "steer" },
      );
      results.sendUserMessage = await pi.sendUserMessage("hello", { deliverAs: "followUp" });
      results.setModel = await pi.setModel({
        id: "test-model",
        name: "Test Model",
        api: "openai-completions",
        provider: "fixture-provider",
        baseUrl: "https://fixture.invalid/v1",
        reasoning: true,
        input: ["text"],
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
        contextWindow: 8192,
        maxTokens: 1024,
      });
      results.abort = await ctx.abort();
      results.shutdown = await ctx.shutdown();
      results.compact = await ctx.compact("trim it");
      results.waitForIdle = await ctx.waitForIdle();
      results.reload = await ctx.reload();
      return results;
    },
  });

  // Await a promise that only settles when ctx.signal aborts. The abort
  // listener is registered *before* the readiness action, so a Cancel observed
  // after the test records the action can never race the await.
  pi.registerCommand("phase4_hang", {
    description: "Await a promise that only settles when ctx.signal aborts",
    handler: async (_args, ctx) => {
      const never = new Promise((resolve, reject) => {
        if (ctx.signal.aborted) {
          reject(new Error("extension invocation was cancelled"));
          return;
        }
        ctx.signal.addEventListener(
          "abort",
          () => reject(new Error("extension invocation was cancelled")),
          { once: true },
        );
      });
      await pi.setActiveTools([]);
      await never;
      return "unreachable";
    },
  });
}
