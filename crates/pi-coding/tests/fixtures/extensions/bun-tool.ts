interface ExtensionApi {
  registerTool(tool: Record<string, unknown>): void;
}

export default function (pi: ExtensionApi): void {
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
}
