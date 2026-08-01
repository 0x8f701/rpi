interface ExtensionApi {
  registerTool(tool: Record<string, unknown>): void;
}

export default function (pi: ExtensionApi): void {
  pi.registerTool({
    name: "unsupported_tool_fields",
    label: "Unsupported Tool Fields",
    description: "Must be rejected rather than silently dropping process-inexpressible callbacks",
    promptSnippet: "Unsupported fixture",
    parameters: { type: "object", properties: {} },
    constrainedSampling: { type: "json_schema", schema: { type: "object" } },
    prepareArguments: (value: unknown) => value,
    renderShell: "self",
    renderCall: () => ({ render: () => [] }),
    renderResult: () => ({ render: () => [] }),
    execute: async () => ({ content: [{ type: "text", text: "unreachable" }] }),
  });
}
