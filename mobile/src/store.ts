// Project + mapping persistence, minimal port of crates/pi-cli/web/src/chat/mapping.ts.
// Mappings bind a project to a runner session created via new_session.
import AsyncStorage from '@react-native-async-storage/async-storage';
import { parseSessionRow, type SessionRow } from './protocol';
import type { RunnerRegistry } from './registry';

const MAPPING_STORAGE_KEY = 'rpi-mobile-chat-mapping:v1';

export interface ProjectRecord {
  id: string;
  label: string;
  cwd: string;
  createdAt: number;
  updatedAt: number;
}

export interface MappingRecord {
  id: string;
  projectId: string;
  runnerId: string;
  sessionId: string;
  sessionPath: string;
  sessionName: string;
  createdAt: number;
}

function newId(prefix: string): string {
  return `${prefix}-${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`;
}

function normalizeProject(row: unknown): ProjectRecord {
  const value = row && typeof row === 'object' ? row as Record<string, unknown> : {};
  const createdAt = typeof value.createdAt === 'number' ? value.createdAt : Date.now();
  return {
    id: typeof value.id === 'string' ? value.id : newId('project'),
    label: typeof value.label === 'string' ? value.label : '',
    cwd: typeof value.cwd === 'string' ? value.cwd : '',
    createdAt,
    updatedAt: typeof value.updatedAt === 'number' ? value.updatedAt : createdAt,
  };
}

function isMappingRecord(row: unknown): row is MappingRecord {
  if (!row || typeof row !== 'object') return false;
  const value = row as Record<string, unknown>;
  return typeof value.id === 'string'
    && typeof value.projectId === 'string'
    && typeof value.runnerId === 'string'
    && typeof value.sessionId === 'string'
    && typeof value.sessionPath === 'string';
}

export function parseStoredMappingDocument(document: unknown): { projects: ProjectRecord[]; mappings: MappingRecord[] } | null {
  if (!document || typeof document !== 'object') return null;
  const value = document as Record<string, unknown>;
  if (value.version !== 1) return null;
  if (!Array.isArray(value.projects) || !Array.isArray(value.mappings)) return null;
  const projects = value.projects.map(normalizeProject).filter((project) => project.id && project.cwd);
  const projectIds = new Set(projects.map((project) => project.id));
  const mappings: MappingRecord[] = [];
  for (const row of value.mappings) {
    if (!row || typeof row !== 'object') continue;
    const mapping = row as Record<string, unknown>;
    const sessionId = typeof mapping.sessionId === 'string' ? mapping.sessionId : '';
    const sessionPath = typeof mapping.sessionPath === 'string' ? mapping.sessionPath : '';
    const projectId = typeof mapping.projectId === 'string' ? mapping.projectId : '';
    const runnerId = typeof mapping.runnerId === 'string' ? mapping.runnerId : '';
    const id = typeof mapping.id === 'string' ? mapping.id : '';
    if (!sessionId || !sessionPath || !projectId || !runnerId || !id) continue;
    if (!projectIds.has(projectId)) continue;
    mappings.push({
      id,
      projectId,
      runnerId,
      sessionId,
      sessionPath,
      sessionName: typeof mapping.sessionName === 'string' ? mapping.sessionName : '',
      createdAt: typeof mapping.createdAt === 'number' ? mapping.createdAt : Date.now(),
    });
  }
  return { projects, mappings };
}

export class MappingStore {
  private projects: ProjectRecord[] = [];
  private mappings: MappingRecord[] = [];
  private readonly listeners = new Set<() => void>();

  private onPersist: (() => void) | null = null;
  private syncMuted = 0;

  setPersistHandler(handler: () => void): void {
    this.onPersist = handler;
  }

  async load(): Promise<void> {
    let raw: string | null = null;
    try {
      raw = await AsyncStorage.getItem(MAPPING_STORAGE_KEY);
    } catch {
      raw = null;
    }
    if (!raw) return;
    try {
      const parsed = JSON.parse(raw);
      if (Array.isArray(parsed?.projects) && Array.isArray(parsed?.mappings)) {
        this.projects = parsed.projects.map(normalizeProject);
        this.mappings = parsed.mappings.filter(isMappingRecord);
      }
    } catch {
      this.projects = [];
      this.mappings = [];
    }
  }

  private async persist(): Promise<void> {
    try {
      await AsyncStorage.setItem(
        MAPPING_STORAGE_KEY,
        JSON.stringify({ version: 1, projects: this.projects, mappings: this.mappings }),
      );
    } catch {
      // quota — state lives in memory this session
    }
    for (const listener of this.listeners) listener();
    if (this.syncMuted === 0) this.onPersist?.();
  }

  exportDocument(): { version: number; projects: ProjectRecord[]; mappings: unknown[] } {
    return {
      version: 1,
      projects: this.projects.map((project) => ({
        ...project,
        label: project.label || project.cwd,
        updatedAt: project.updatedAt ?? project.createdAt,
      })),
      mappings: this.mappings.map((mapping) => ({
        id: mapping.id,
        projectId: mapping.projectId,
        runnerId: mapping.runnerId,
        operationId: mapping.id,
        sourceSessionId: mapping.sessionId,
        canaryId: mapping.id,
        createId: mapping.id,
        sessionName: mapping.sessionName || mapping.sessionId,
        createdAt: mapping.createdAt,
        updatedAt: mapping.createdAt,
        state: 'active',
        sessionId: mapping.sessionId,
        sessionPath: mapping.sessionPath,
      })),
    };
  }

  adoptDocument(document: unknown): void {
    const parsed = parseStoredMappingDocument(document);
    if (!parsed) return;
    this.syncMuted += 1;
    this.projects = parsed.projects;
    this.mappings = parsed.mappings;
    void this.persist().finally(() => {
      this.syncMuted = Math.max(0, this.syncMuted - 1);
    });
  }

  subscribe(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  projectRows(): ProjectRecord[] {
    return [...this.projects].sort((left, right) => left.createdAt - right.createdAt);
  }

  mappingRows(): MappingRecord[] {
    return this.mappings;
  }

  watermark(): number {
    const stamps = [
      ...this.projects.map((project) => project.updatedAt || project.createdAt),
      ...this.mappings.map((mapping) => mapping.createdAt),
    ];
    if (stamps.length === 0) return 0;
    return Math.max(...stamps);
  }

  isEmpty(): boolean {
    return this.projects.length === 0 && this.mappings.length === 0;
  }

  mappingsForProject(projectId: string): MappingRecord[] {
    return this.mappings.filter((row) => row.projectId === projectId);
  }

  mappingsForRunner(runnerId: string): MappingRecord[] {
    return this.mappings.filter((row) => row.runnerId === runnerId);
  }

  async addProject(label: string, cwd: string): Promise<ProjectRecord> {
    const trimmedLabel = label.trim();
    const trimmedCwd = cwd.trim();
    if (!trimmedLabel) throw new Error('Project label is required.');
    if (!trimmedCwd.startsWith('/')) throw new Error('Project directory must be absolute.');
    if (this.projects.some((row) => row.cwd === trimmedCwd)) {
      throw new Error('A project already uses this directory.');
    }
    const project: ProjectRecord = {
      id: newId('project'),
      label: trimmedLabel,
      cwd: trimmedCwd,
      createdAt: Date.now(),
      updatedAt: Date.now(),
    };
    this.projects = [...this.projects, project];
    await this.persist();
    return project;
  }

  async mapRunner(registry: RunnerRegistry, projectId: string, runnerId: string): Promise<MappingRecord> {
    const project = this.projects.find((row) => row.id === projectId);
    if (!project) throw new Error('Project was not found.');
    const existing = this.mappings.find((row) => row.projectId === projectId && row.runnerId === runnerId);
    if (existing) return existing;
    const client = registry.client(runnerId);
    if (!client || client.state !== 'connected' || !client.probe?.sessionId) {
      throw new Error('Runner is not connected.');
    }
    const sourceSessionId = client.probe.sessionId;
    const created = await client.command({ type: 'new_session', cwd: project.cwd }, sourceSessionId);
    const row: SessionRow | null = created.success ? parseSessionRow(created.data) : null;
    if (!row) throw new Error(created.error || 'Mapped session was not created.');
    const mapping: MappingRecord = {
      id: `${projectId}:${runnerId}`,
      projectId,
      runnerId,
      sessionId: row.sessionId,
      sessionPath: row.path,
      sessionName: row.name ?? '',
      createdAt: Date.now(),
    };
    this.mappings = [...this.mappings, mapping];
    await this.persist();
    return mapping;
  }

  async unmapRunner(registry: RunnerRegistry, projectId: string, runnerId: string): Promise<void> {
    const mapping = this.mappings.find((row) => row.projectId === projectId && row.runnerId === runnerId);
    if (!mapping) return;
    const client = registry.client(runnerId);
    if (client && client.state === 'connected' && client.probe?.sessionId) {
      try {
        await client.command({ type: 'session_delete', sessionPath: mapping.sessionPath }, client.probe.sessionId);
      } catch {
        // user asked to unmap: a failed archive still drops the mapping
      }
    }
    this.mappings = this.mappings.filter((row) => row.id !== mapping.id);
    await this.persist();
  }

  async deleteProject(registry: RunnerRegistry, projectId: string): Promise<void> {
    for (const mapping of this.mappingsForProject(projectId)) {
      await this.unmapRunner(registry, projectId, mapping.runnerId);
    }
    this.projects = this.projects.filter((row) => row.id !== projectId);
    await this.persist();
  }

  async removeRunner(registry: RunnerRegistry, runnerId: string): Promise<void> {
    for (const mapping of this.mappingsForRunner(runnerId)) {
      await this.unmapRunner(registry, mapping.projectId, runnerId);
    }
    await registry.remove(runnerId);
  }
}
