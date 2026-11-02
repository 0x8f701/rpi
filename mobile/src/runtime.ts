import { MappingStore } from './store';
import { RunnerRegistry } from './registry';
import { LanDiscovery } from './discovery';

export const registry = new RunnerRegistry();
export const mappingStore = new MappingStore();
export const lanDiscovery = new LanDiscovery();
