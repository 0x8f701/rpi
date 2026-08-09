// QuickJS extension example: commands, a tool, an event hook, session
// actions, and UI actions through the in-process QuickJS runtime.
//
// Install with a pi-extension.json manifest:
// {
//   "schemaVersion": 1,
//   "id": "quickjs-example",
//   "runtime": "quickjs",
//   "entry": "./quickjs_extension.mjs",
//   "capabilities": ["commands", "tools", "event_hooks", "session_actions", "ui"],
//   "uiCapabilities": ["notify", "status"]
// }
export default function (pi) {
  pi.registerCommand("quickjs-hello", {
    description: "Show a notification from a QuickJS extension",
    handler: async (args, ctx) => {
      ctx.ui.notify(args || "Hello from QuickJS", "info");
    },
  });

  pi.registerTool({
    name: "quickjs_echo",
    label: "QuickJS Echo",
    description: "Echo text through the in-process QuickJS runtime",
    parameters: {
      type: "object",
      properties: { text: { type: "string" } },
      required: ["text"],
    },
    execute: async (_callId, params) => ({
      content: [{ type: "text", text: `echo:${params.text}` }],
    }),
  });

  pi.registerCommand("quickjs-session", {
    description: "Rename the current session through the action channel",
    handler: async () => {
      pi.setSessionName("QuickJS extension session");
    },
  });

  pi.on("session_start", async (event, ctx) => {
    ctx.ui.setStatus("quickjs-example", `QuickJS extension: ${event.reason ?? "started"}`);
  });
}
