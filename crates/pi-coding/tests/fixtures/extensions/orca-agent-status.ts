interface SessionManagerView {
  getSessionId(): string;
  getSessionFile(): string | undefined;
}

interface UiView {
  notify(message: string, level?: "info" | "warning" | "error"): void;
  setTitle(title: string): void;
}

interface ExtensionContextView {
  sessionManager: SessionManagerView;
  isIdle(): boolean;
  ui: UiView;
}

interface ExtensionApi {
  getSessionName(): string | undefined;
  on(
    eventName: string,
    handler: (event: Record<string, unknown>, context: ExtensionContextView) => unknown,
  ): void;
}

const EVENTS = [
  "session_start",
  "before_agent_start",
  "agent_start",
  "tool_execution_start",
  "tool_call",
  "tool_execution_end",
  "message_end",
  "agent_settled",
  "agent_end",
  "session_shutdown",
] as const;

export default function (pi: ExtensionApi): void {
  for (const eventName of EVENTS) {
    pi.on(eventName, (event, context) => {
      if (eventName === "session_start") {
        context.ui.notify("orca fixture started", "info");
        context.ui.setTitle("orca fixture");
      }
      return {
        observedType: event.type,
        sessionId: context.sessionManager.getSessionId(),
        sessionFile: context.sessionManager.getSessionFile(),
        idle: context.isIdle(),
        sessionName: pi.getSessionName(),
      };
    });
  }
}
