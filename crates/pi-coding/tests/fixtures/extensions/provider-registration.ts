interface ExtensionApi {
  registerProvider(name: string, config: Record<string, unknown>): void;
}

export default function (pi: ExtensionApi): void {
  pi.registerProvider("fixture-provider", {
    name: "Fixture Provider",
    baseUrl: "https://fixture.invalid/v1",
    apiKey: "$FIXTURE_PROVIDER_KEY",
    api: "openai-completions",
    headers: { "x-fixture": "provider" },
    authHeader: true,
    models: [{
      id: "fixture-model",
      name: "Fixture Model",
      reasoning: true,
      input: ["text"],
      cost: { input: 1, output: 2, cacheRead: 3, cacheWrite: 4 },
      contextWindow: 8192,
      maxTokens: 1024,
    }],
  });
}
