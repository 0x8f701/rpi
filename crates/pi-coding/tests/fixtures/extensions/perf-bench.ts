// Perf benchmark fixture: minimal, hot-path handlers.
export default function (pi: any) {
  pi.registerCommand("noop", {
    description: "no-op command",
    handler: async (_args: string) => "ok",
  });

  pi.registerTool({
    name: "noop_tool",
    description: "no-op tool",
    parameters: { type: "object", properties: {} },
    execute: async () => ({ content: [{ type: "text", text: "ok" }] }),
  });

  pi.on("turn_start", async () => "ok");
}
