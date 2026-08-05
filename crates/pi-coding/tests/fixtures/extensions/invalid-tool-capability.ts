interface ExtensionApi {
  registerTool(tool: Record<string, unknown>): void;
}

export default function (pi: ExtensionApi): void {
  pi.registerTool({
    name: "invalid_tool_capability",
    description: "Must reject unknown capability metadata",
    capability: "network",
    parameters: { type: "object", properties: {} },
    execute: async () => ({ content: [{ type: "text", text: "unreachable" }] }),
  });
}
