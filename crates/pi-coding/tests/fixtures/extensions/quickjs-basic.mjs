// QuickJS fixture: commands, a tool, a session action, and an event hook
// (pi.on registers a real event hook that returns no reduction).
export default function (pi) {
  pi.registerCommand("hello", {
    description: "Say hello through the in-process QuickJS runtime",
    handler: async (args, ctx) => `hello:${args || "world"}`,
  });
  pi.registerCommand("alpha", {
    description: "first chain step",
    handler: async (args) => `alpha:${args || "none"}`,
  });
  pi.registerCommand("beta", {
    description: "second chain step",
    handler: async (args) => `beta:${args || "none"}`,
  });
  pi.registerCommand("rename", {
    description: "Set the session name through the host action channel",
    handler: async (args) => {
      const result = await pi.setSessionName(args || "unnamed");
      return result;
    },
  });
  pi.registerTool({
    name: "echo_quickjs",
    label: "QuickJS Echo",
    description: "Echo a value through the in-process QuickJS runtime",
    parameters: {
      type: "object",
      properties: { value: { type: "string" } },
      required: ["value"],
    },
    execute: async (callId, params, signal, onUpdate) => {
      onUpdate?.({ content: [{ type: "text", text: "partial" }] });
      return { content: [{ type: "text", text: `qjs:${params.value}` }] };
    },
  });
  pi.on("session_start", () => null);
}
