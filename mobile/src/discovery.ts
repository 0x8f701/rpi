import {
  discoveredAlreadyAdded,
  originAuthority,
  parseDiscoveredRunner,
  parseDiscoveredRunnerList,
  type DiscoveredRunner,
} from './protocol';
import type { RunnerRegistry, RunnerView } from './registry';

function upsert(list: DiscoveredRunner[], runner: DiscoveredRunner): DiscoveredRunner[] {
  const next = list.filter((candidate) => candidate.origin !== runner.origin);
  next.push(runner);
  next.sort((left, right) => left.origin.localeCompare(right.origin));
  return next;
}

export class LanDiscovery {
  private runners: DiscoveredRunner[] = [];
  private readonly listeners = new Set<(runners: DiscoveredRunner[]) => void>();
  private frameUnsubs: Array<() => void> = [];

  subscribe(listener: (runners: DiscoveredRunner[]) => void): () => void {
    this.listeners.add(listener);
    listener(this.runners);
    return () => {
      this.listeners.delete(listener);
    };
  }

  snapshot(): DiscoveredRunner[] {
    return this.runners.map((row) => ({ ...row }));
  }

  bind(registry: RunnerRegistry): () => void {
    const off = registry.subscribe((views) => {
      this.rewire(registry, views);
      void this.refresh(registry, views);
    });
    return () => {
      off();
      this.clearFrameSubs();
    };
  }

  private emit(): void {
    const snapshot = this.snapshot();
    for (const listener of this.listeners) listener(snapshot);
  }

  private clearFrameSubs(): void {
    for (const unsub of this.frameUnsubs) unsub();
    this.frameUnsubs = [];
  }

  private rewire(registry: RunnerRegistry, views: RunnerView[]): void {
    this.clearFrameSubs();
    for (const view of views) {
      if (view.state !== 'connected') continue;
      const client = registry.client(view.id);
      if (!client) continue;
      this.frameUnsubs.push(client.subscribeFrames((frame) => {
        this.applyFrame(frame);
      }));
    }
  }

  private applyFrame(frame: unknown): void {
    if (!frame || typeof frame !== 'object') return;
    const value = frame as Record<string, unknown>;
    if (value.type === 'runner_discovered') {
      const runner = parseDiscoveredRunner(value.runner ?? value);
      if (!runner) return;
      this.runners = upsert(this.runners, runner);
      this.emit();
      return;
    }
    if (value.type === 'runner_gone' && typeof value.origin === 'string') {
      const gone = originAuthority(value.origin);
      this.runners = this.runners.filter((row) => originAuthority(row.origin) !== gone);
      this.emit();
    }
  }

  private async refresh(registry: RunnerRegistry, views: RunnerView[]): Promise<void> {
    let merged: DiscoveredRunner[] = [];
    for (const view of views) {
      if (view.state !== 'connected') continue;
      const client = registry.client(view.id);
      if (!client) continue;
      try {
        const response = await client.command({ type: 'get_discovered_runners' });
        const parsed = response.success ? parseDiscoveredRunnerList(response.data) : null;
        if (!parsed) continue;
        for (const row of parsed) merged = upsert(merged, row);
      } catch {
        continue;
      }
    }
    this.runners = merged;
    this.emit();
  }
}

export function lanAddCandidates(discovered: DiscoveredRunner[], addedAuthorities: string[]): DiscoveredRunner[] {
  return discovered.filter((row) => !discoveredAlreadyAdded(row, addedAuthorities));
}
