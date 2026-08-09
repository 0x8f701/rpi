// QuickJS UI fixture (Phase 3): exercises the ctx.ui.* surface through the
// host round trip. The scripted test host auto-answers interactive dialogs
// and queries; cancellation/timeout/gate paths are exercised by dedicated
// commands.
export default function (pi) {
  pi.registerCommand("exercise-ui", {
    handler: async (_args, ctx) => {
      const ui = ctx.ui;
      const confirmed = await ui.confirm("Continue?", "Proceed?");
      const selected = await ui.select(
        "Pick",
        [
          { value: "one", label: "One", description: "first" },
          "two",
        ],
        { timeout: 5000 },
      );
      const typed = await ui.input("Name", "hint");
      const edited = await ui.editor("Edit", "prefill");
      ui.notify("hello from quickjs", "info");
      ui.notify("warning from quickjs", "warning");
      ui.setStatus("status-key", "running");
      ui.setWidget("widget-key", ["line 1", "line 2"], { placement: "belowEditor" });
      ui.setTitle("quickjs title");
      ui.setEditorText("editor text");
      ui.setWorkingMessage("working");
      ui.setWorkingVisible(true);
      ui.setWorkingIndicator({ frames: ["·", "●"], intervalMs: 120 });
      ui.setHiddenThinkingLabel("thinking…");
      ui.pasteToEditor("pasted");
      ui.setToolsExpanded(true);
      const editorText = await ui.getEditorText();
      const themes = await ui.getAllThemes();
      const theme = await ui.getTheme("dark");
      const setThemeResult = await ui.setTheme("dark");
      const missingTheme = await ui.getTheme("missing");
      const toolsExpanded = await ui.getToolsExpanded();
      return {
        confirmed,
        selected,
        typed,
        edited,
        editorText,
        themes,
        theme,
        setThemeResult,
        missingTheme: missingTheme === undefined ? null : missingTheme,
        toolsExpanded,
      };
    },
  });

  // Cancelled dialogs: select/input/editor cancellations map to undefined and
  // confirm cancellations to false.
  pi.registerCommand("confirm-no", {
    handler: async (_args, ctx) => ctx.ui.confirm("decline", "Proceed?"),
  });
  pi.registerCommand("select-cancel", {
    handler: async (_args, ctx) => ctx.ui.select("abort", ["one"]),
  });
  pi.registerCommand("input-cancel", {
    handler: async (_args, ctx) => ctx.ui.input("escape", "hint"),
  });
  pi.registerCommand("editor-cancel", {
    handler: async (_args, ctx) => ctx.ui.editor("quit", "prefill"),
  });

  // Per-request timeout: the host answers slowly, the { timeout } option must
  // fail the UI promise ("UI request timed out").
  pi.registerCommand("confirm-timeout", {
    handler: async (_args, ctx) => ctx.ui.confirm("slow", "Proceed?", { timeout: 1 }),
  });
}
