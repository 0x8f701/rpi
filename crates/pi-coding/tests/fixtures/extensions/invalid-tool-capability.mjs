// QuickJS twin of invalid-tool-capability.ts: an unknown tool capability
// must reject at registration.
export default function (pi) {
  pi.registerTool({
    name: "invalid_tool_capability",
    description: "Must reject unknown capability metadata",
    capability: "network",
    parameters: { type: "object", properties: {} },
    execute: async () => ({ content: [{ type: "text", text: "unreachable" }] }),
  });
}
