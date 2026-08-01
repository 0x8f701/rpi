interface WorkingIndicatorOptions {
  frames?: string[];
  intervalMs?: number;
}

interface ThemeDescriptor {
  name: string;
  path?: string;
}

interface ExtensionContextView {
  ui: {
    setWorkingMessage(message?: string): void;
    setWorkingVisible(visible: boolean): void;
    setWorkingIndicator(options?: WorkingIndicatorOptions): void;
    setHiddenThinkingLabel(label?: string): void;
    pasteToEditor(text: string): void;
    getEditorText(): string | Promise<string>;
    getAllThemes(): ThemeDescriptor[] | Promise<ThemeDescriptor[]>;
    getTheme(name: string): ThemeDescriptor | undefined | Promise<ThemeDescriptor | undefined>;
    setTheme(name: string): { success: boolean; error?: string } | Promise<{ success: boolean; error?: string }>;
    getToolsExpanded(): boolean | Promise<boolean>;
    setToolsExpanded(expanded: boolean): void;
  };
}

interface ExtensionApi {
  registerCommand(
    name: string,
    options: { handler: (arguments_: string, context: ExtensionContextView) => unknown },
  ): void;
  registerShortcut(
    key: string,
    options: { description?: string; handler: (context: ExtensionContextView) => unknown },
  ): void;
  registerFlag(
    name: string,
    options: { description?: string; type: "boolean" | "string"; default?: boolean | string },
  ): void;
  getFlag(name: string): boolean | string | undefined;
}

export default function (pi: ExtensionApi): void {
  pi.registerShortcut("ctrl+k", {
    description: "Unsupported process shortcut",
    handler: () => "shortcut-fired",
  });
  pi.registerFlag("verbose", { description: "verbose mode", type: "boolean", default: true });

  pi.registerCommand("exercise-apis", {
    handler: async (_arguments, context) => {
      const ui = context.ui;
      ui.setWorkingMessage("working");
      ui.setWorkingVisible(true);
      ui.setWorkingIndicator({ frames: ["·", "●"], intervalMs: 120 });
      ui.setHiddenThinkingLabel("thinking…");
      ui.setToolsExpanded(true);
      ui.pasteToEditor("pasted");
      const editorText = await ui.getEditorText();
      const themes = await ui.getAllThemes();
      const theme = await ui.getTheme("dark");
      const setThemeResult = await ui.setTheme("dark");
      const missingTheme = await ui.getTheme("missing");
      const toolsExpanded = await ui.getToolsExpanded();
      let verboseError: string | null = null;
      try {
        pi.getFlag("verbose");
      } catch (error) {
        verboseError = error instanceof Error ? error.message : String(error);
      }
      const missing = pi.getFlag("missing");
      return {
        editorText,
        themes,
        theme,
        setThemeResult,
        missingTheme: missingTheme ?? null,
        toolsExpanded,
        verboseError,
        missing: missing ?? null,
      };
    },
  });

  pi.registerCommand("probe-model", {
    handler: (_arguments, context) => (context as { model?: unknown }).model ?? null,
  });

  pi.registerCommand("get-editor-text", {
    handler: (_arguments, context) => context.ui.getEditorText(),
  });
}