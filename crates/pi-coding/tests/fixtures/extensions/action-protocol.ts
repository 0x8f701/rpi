interface ExtensionApi {
  registerCommand(name: string, options: { handler: () => unknown }): void;
  getActiveTools(): string[];
  getAllTools(): unknown[];
  getCommands(): unknown[];
  getThinkingLevel(): string;
  getSessionName(): string | undefined;
  setActiveTools(names: string[]): void;
  setThinkingLevel(level: string): void;
  setSessionName(name: string): void;
  sendMessage(message: Record<string, unknown>, options?: Record<string, unknown>): void;
  sendUserMessage(content: string, options?: Record<string, unknown>): void;
  appendEntry(customType: string, data?: unknown): void;
  setModel(model: Record<string, unknown>): Promise<boolean>;
}

export default function (pi: ExtensionApi): void {
  pi.registerCommand("snapshot", {
    handler: () => ({
      activeTools: pi.getActiveTools(),
      allTools: pi.getAllTools(),
      commands: pi.getCommands(),
      thinkingLevel: pi.getThinkingLevel(),
      sessionName: pi.getSessionName(),
    }),
  });

  pi.registerCommand("actions", {
    handler: async () => {
      pi.setActiveTools(["fixture_tool"]);
      pi.setThinkingLevel("high");
      pi.setSessionName("fixture session");
      pi.sendMessage(
        { customType: "fixture-message", content: "visible", display: true },
        { triggerTurn: false },
      );
      pi.sendMessage(
        { customType: "fixture-hidden", content: "hidden", display: false },
        { deliverAs: "nextTurn" },
      );
      pi.sendUserMessage("fixture user", { deliverAs: "followUp" });
      pi.appendEntry("fixture-entry", { persisted: true });
      return pi.setModel({
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
    },
  });
}
