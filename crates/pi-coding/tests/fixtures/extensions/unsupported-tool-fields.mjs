// QuickJS twin of unsupported-tool-fields.ts: registerTool fields that the
// in-process runtime cannot express (constrainedSampling, renderShell,
// prepareArguments, renderCall, renderResult) must be rejected at
// registration, not silently dropped.
export default function (pi) {
  pi.registerTool({
    name: "unsupported_tool_fields",
    label: "Unsupported Tool Fields",
    description: "Must be rejected rather than silently dropping inexpressible callbacks",
    promptSnippet: "Unsupported fixture",
    parameters: { type: "object", properties: {} },
    constrainedSampling: { type: "json_schema", schema: { type: "object" } },
    prepareArguments: (value) => value,
    renderShell: "self",
    renderCall: () => ({ render: () => [] }),
    renderResult: () => ({ render: () => [] }),
    execute: async () => ({ content: [{ type: "text", text: "unreachable" }] }),
  });
}
