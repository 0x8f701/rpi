interface ExtensionContextView {
  ui: Record<string, (...arguments_: unknown[]) => unknown>;
}

interface ExtensionApi {
  registerCommand(
    name: string,
    options: { handler: (arguments_: string, context: ExtensionContextView) => unknown },
  ): void;
}

export default function (pi: ExtensionApi): void {
  const unsupportedMethods = [
    "onTerminalInput",
    "setFooter",
    "setHeader",
    "custom",
    "addAutocompleteProvider",
    "setEditorComponent",
    "getEditorComponent",
  ] as const;

  for (const method of unsupportedMethods) {
    pi.registerCommand(`reject-${method}`, {
      handler: (_arguments, context) => context.ui[method](() => undefined),
    });
  }

  pi.registerCommand("reject-widget-factory", {
    handler: (_arguments, context) => context.ui.setWidget("fixture", () => ({ render: () => [] })),
  });
}
