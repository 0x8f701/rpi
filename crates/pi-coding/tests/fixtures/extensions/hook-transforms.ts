interface ExtensionApi {
  on(eventName: string, handler: (event: Record<string, unknown>) => unknown): void;
}

export default function (pi: ExtensionApi): void {
  pi.on("before_agent_start", (event) => ({
    systemPrompt: `${String(event.systemPrompt)}\nfixture-system`,
    message: {
      customType: "fixture.before-agent",
      content: "fixture message",
      display: true,
      details: { prompt: event.prompt },
    },
  }));

  pi.on("context", (event) => ({
    messages: [...(Array.isArray(event.messages) ? event.messages : []), {
      role: "user",
      content: [{ type: "text", text: "fixture-context" }],
      timestamp: 17,
    }],
  }));

  pi.on("before_provider_request", (event) => ({
    ...(typeof event.payload === "object" && event.payload !== null ? event.payload : {}),
    transformedByFixture: true,
  }));

  pi.on("before_provider_headers", (event) => {
    if (typeof event.headers === "object" && event.headers !== null) {
      Object.assign(event.headers, { "x-fixture": "yes", "x-remove": null });
    }
    return { headers: event.headers };
  });

  pi.on("tool_call", (event) => ({
    block: event.toolName === "danger",
    reason: "blocked by fixture",
  }));

  pi.on("tool_result", () => ({
    content: [{ type: "text", text: "fixture-result" }],
    details: { replaced: true },
    isError: false,
    usage: { input: 1, output: 2, cacheRead: 3, cacheWrite: 4 },
  }));

  pi.on("message_end", (event) => ({
    message: {
      ...(typeof event.message === "object" && event.message !== null ? event.message : {}),
      content: [{ type: "text", text: "fixture-message" }],
    },
  }));

  pi.on("session_before_switch", () => ({ cancel: true }));
  pi.on("session_before_fork", () => ({ cancel: true, skipConversationRestore: true }));
  pi.on("session_before_compact", () => ({ cancel: true }));
  pi.on("session_before_tree", () => ({
    cancel: false,
    summary: { summary: "fixture summary", details: { source: "fixture" } },
    customInstructions: "fixture instructions",
    replaceInstructions: true,
    label: "fixture-label",
  }));
  pi.on("input", () => ({ action: "transform", text: "fixture-input" }));
}
