interface ExtensionApi {
  registerMessageRenderer(customType: string, renderer: (message: unknown) => unknown): void;
  registerEntryRenderer(customType: string, renderer: (entry: unknown) => unknown): void;
}

export default function (pi: ExtensionApi): void {
  pi.registerMessageRenderer("fixture-message", () => ({ render: () => ["fixture message"] }));
  pi.registerEntryRenderer("fixture-entry", () => ({ render: () => ["fixture entry"] }));
}
