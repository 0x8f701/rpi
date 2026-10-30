export const RUNNER_MENU_LONG_PRESS_MS = 500;

export type RunnerContextAction = 'terminal' | 'web' | 'settings' | 'copy-url';

export const RUNNER_CONTEXT_ACTIONS: ReadonlyArray<{ id: RunnerContextAction; label: string }> = [
  { id: 'terminal', label: 'Open terminal' },
  { id: 'web', label: 'Open /web' },
  { id: 'settings', label: 'Runner settings' },
  { id: 'copy-url', label: 'Copy web URL' },
];

export interface RunnerContextMenuTarget {
  runnerId: string;
  x: number;
  y: number;
}

export function runnerContextMenuFromPointer(
  runnerId: string,
  clientX: number,
  clientY: number,
): RunnerContextMenuTarget {
  return { runnerId, x: clientX, y: clientY };
}

export function openRunnerContextMenuFromEvent(
  runnerId: string,
  event: { preventDefault: () => void; clientX: number; clientY: number },
): RunnerContextMenuTarget {
  event.preventDefault();
  return runnerContextMenuFromPointer(runnerId, event.clientX, event.clientY);
}

export function runnerLongPressShouldArm(pointerType: string, button: number): boolean {
  if (button !== 0) return false;
  return pointerType === 'touch' || pointerType === 'pen';
}

export function scheduleRunnerLongPress(
  runnerId: string,
  clientX: number,
  clientY: number,
  setTimeoutFn: (handler: () => void, timeout: number) => number,
  onFire: (target: RunnerContextMenuTarget) => void,
): number {
  return setTimeoutFn(() => {
    onFire(runnerContextMenuFromPointer(runnerId, clientX, clientY));
  }, RUNNER_MENU_LONG_PRESS_MS);
}

export function applyRunnerContextAction(
  action: RunnerContextAction,
  webUrl: string,
  handlers: {
    openTerminal: () => void;
    openWeb: (url: string) => void;
    openSettings: () => void;
    copyWebUrl: (url: string) => void;
  },
): void {
  switch (action) {
    case 'terminal':
      handlers.openTerminal();
      return;
    case 'web':
      handlers.openWeb(webUrl);
      return;
    case 'settings':
      handlers.openSettings();
      return;
    case 'copy-url':
      handlers.copyWebUrl(webUrl);
      return;
  }
}
