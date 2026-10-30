import { useLayoutEffect, useRef } from 'react';

export const desktopActionKinds = ['new-workspace', 'new-session', 'settings', 'attach', 'stop'] as const;
export type DesktopActionKind = typeof desktopActionKinds[number];
export type DesktopActionHandlers = Partial<Record<DesktopActionKind, () => void>>;
export interface DesktopBridge {
  onAction(handler: (action: { kind: DesktopActionKind }) => void): () => void;
  setActions(kinds: DesktopActionKind[]): void;
}

declare global {
  interface Window { rpiDesktop?: DesktopBridge }
}

export interface DesktopActionRegistration {
  update(handlers: DesktopActionHandlers): void;
  unsubscribe(): void;
}
export interface DesktopActionRegistry {
  register(handlers: DesktopActionHandlers): DesktopActionRegistration;
}

export function createDesktopActionRegistry(bridge: DesktopBridge, onCapabilities?: (kinds: DesktopActionKind[]) => void) {
  const owners = new Map<symbol, DesktopActionHandlers>();
  let unsubscribe: (() => void) | undefined;
  let capabilities: string | undefined;
  const publish = () => {
    const kinds = desktopActionKinds.filter((kind) => {
      for (const handlers of owners.values()) if (typeof handlers[kind] === 'function') return true;
      return false;
    });
    const next = kinds.join(' ');
    if (next === capabilities) return;
    capabilities = next;
    bridge.setActions(kinds);
    onCapabilities?.(kinds);
  };
  return {
    register(handlers: DesktopActionHandlers) {
      const owner = Symbol();
      owners.set(owner, handlers);
      unsubscribe ??= bridge.onAction((action) => {
        if (!action || !desktopActionKinds.includes(action.kind)) return;
        let handler: (() => void) | undefined;
        for (const registered of owners.values()) handler = registered[action.kind] ?? handler;
        handler?.();
      });
      publish();
      return {
        update(next: DesktopActionHandlers) {
          if (!owners.has(owner)) return;
          owners.set(owner, next);
          publish();
        },
        unsubscribe() {
          if (!owners.delete(owner)) return;
          publish();
          if (owners.size === 0) {
            unsubscribe?.();
            unsubscribe = undefined;
          }
        },
      };
    },
  };
}

const registries = new WeakMap<DesktopBridge, DesktopActionRegistry>();

export function useDesktopActions(handlers: DesktopActionHandlers): void {
  const registration = useRef<DesktopActionRegistration>();
  const bridge = typeof window === 'undefined' ? undefined : window.rpiDesktop;
  useLayoutEffect(() => {
    if (!bridge) return;
    let registry = registries.get(bridge);
    if (!registry) {
      registry = createDesktopActionRegistry(bridge, (kinds) => {
        document.documentElement.dataset.desktopActions = kinds.join(' ');
      });
      registries.set(bridge, registry);
    }
    document.documentElement.dataset.desktop = 'true';
    registration.current = registry.register(handlers);
    return () => {
      registration.current?.unsubscribe();
      registration.current = undefined;
    };
  }, [bridge]);
  useLayoutEffect(() => registration.current?.update(handlers));
}
