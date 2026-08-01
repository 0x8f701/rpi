interface ExtensionApi {
  getActiveTools(): string[];
  getAllTools(): unknown[];
  setActiveTools(names: string[]): void;
  getCommands(): unknown[];
  setModel(model: Record<string, unknown>): Promise<boolean>;
  getThinkingLevel(): string;
  setThinkingLevel(level: string): void;
  sendMessage(message: Record<string, unknown>, options?: Record<string, unknown>): void;
  sendUserMessage(content: string, options?: Record<string, unknown>): void;
  appendEntry(customType: string, data?: unknown): void;
  getSessionName(): string | undefined;
  setSessionName(name: string): void;
  registerCommand(name: string, options: { handler: () => unknown }): void;
}

export default function (pi: ExtensionApi): void {
  pi.registerCommand("exercise-session-methods", {
    handler: async () => {
      const before = {
        activeTools: pi.getActiveTools(),
        allTools: pi.getAllTools(),
        commands: pi.getCommands(),
        thinkingLevel: pi.getThinkingLevel(),
        sessionName: pi.getSessionName(),
      };
      const actions = {
        activeTools: pi.setActiveTools(["fixture_tool"]),
        thinkingLevel: pi.setThinkingLevel("high"),
        sessionName: pi.setSessionName("fixture session"),
        visibleMessage: pi.sendMessage({ customType: "fixture-message", content: "visible", display: true }, { triggerTurn: false }),
        hiddenMessage: pi.sendMessage({ customType: "fixture-hidden", content: "hidden", display: false }, { deliverAs: "nextTurn" }),
        userMessage: pi.sendUserMessage("fixture user", { deliverAs: "followUp" }),
        entry: pi.appendEntry("fixture-entry", { persisted: true }),
      };
      const modelAccepted = await pi.setModel({
        id: "fixture-model",
        name: "Fixture Model",
        api: "openai-completions",
        provider: "fixture-provider",
        baseUrl: "https://fixture.invalid/v1",
        reasoning: true,
        input: ["text"],
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
        contextWindow: 8192,
        maxTokens: 1024,
      });
      return { before, modelAccepted, actions };
    },
  });
}
