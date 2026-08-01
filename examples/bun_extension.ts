export default function (pi: any) {
  pi.registerCommand("bun-hello", {
    description: "Show a notification from a Bun extension",
    handler: async (args: string, ctx: any) => {
      ctx.ui.notify(args || "Hello from Bun", "info");
    },
  });

  pi.registerTool({
    name: "bun_echo",
    label: "Bun Echo",
    description: "Echo text through the Bun extension host",
    parameters: {
      type: "object",
      properties: { text: { type: "string" } },
      required: ["text"],
    },
    execute: async (_callId: string, params: { text: string }) => ({
      content: [{ type: "text", text: params.text }],
    }),
  });

  pi.registerCommand("bun-session", {
    description: "Rename the current session through the process bridge",
    handler: async () => {
      pi.setSessionName("Bun extension session");
    },
  });

  pi.on("session_start", async (event: { reason?: string }, ctx: any) => {
    ctx.ui.setStatus("bun-example", `Bun extension: ${event.reason ?? "started"}`);
  });
}
