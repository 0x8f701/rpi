import { contextBridge, ipcRenderer } from 'electron';

const kinds = ['new-workspace', 'new-session', 'settings', 'attach', 'stop'] as const;
type ActionKind = typeof kinds[number];
const origin = process.argv.find((argument) => argument.startsWith('--rpi-desktop-origin='))?.slice('--rpi-desktop-origin='.length);
const trusted = () => process.isMainFrame && window.top === window && !!origin && window.location.origin === origin;

function isKind(value: unknown): value is ActionKind {
  return typeof value === 'string' && kinds.includes(value as ActionKind);
}

if (trusted()) {
  contextBridge.exposeInMainWorld('rpiDesktop', {
    onAction(handler: (action: { kind: ActionKind }) => void): () => void {
      if (typeof handler !== 'function') throw new TypeError('Action handler must be a function.');
      const listener = (_event: Electron.IpcRendererEvent, action: unknown) => {
        if (!trusted() || !action || typeof action !== 'object' || !('kind' in action) || !isKind(action.kind)) return;
        handler({ kind: action.kind });
      };
      const attach = () => {
        if (trusted()) handler({ kind: 'attach' });
      };
      window.addEventListener('rpi-desktop:attach', attach);
      ipcRenderer.on('rpi-desktop:action', listener);
      return () => {
        ipcRenderer.removeListener('rpi-desktop:action', listener);
        window.removeEventListener('rpi-desktop:attach', attach);
      };
    },
    setActions(actions: unknown): void {
      if (!trusted() || !Array.isArray(actions) || actions.length > kinds.length || !actions.every(isKind)) return;
      ipcRenderer.send('rpi-desktop:actions', actions);
    },
  });
}
