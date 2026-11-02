import type { Model } from './types';

export interface ModelSelectOption {
  id: string;
  name: string;
  value: string;
  label: string;
}

export interface ModelProviderGroup {
  provider: string;
  models: ModelSelectOption[];
}

export type ModelSelectInput = Partial<Model> | null | undefined;

export class SessionModelMutationQueue {
  private readonly tails = new Map<string, Promise<void>>();

  enqueue(sessionId: string, mutation: () => Promise<void>): Promise<void> {
    const previous = this.tails.get(sessionId) ?? Promise.resolve();
    const current = previous.catch(() => undefined).then(mutation);
    this.tails.set(sessionId, current);
    const clear = () => {
      if (this.tails.get(sessionId) === current) this.tails.delete(sessionId);
    };
    void current.then(clear, clear);
    return current;
  }
}

/** Header select identity from a wire model row (get_state applyState and
 *  model_select events): provider/id, or '' when the row is incomplete so an
 *  unknown model can never masquerade as a selected key. */
export function wireModelKey(model: ModelSelectInput): string {
  if (!model || !model.provider || !model.id) return '';
  return `${model.provider}/${model.id}`;
}

export interface SessionModelRequest {
  readonly seq: number;
  readonly key: string;
}

export interface SessionModelState {
  readonly settled: Readonly<Record<string, string>>;
  readonly pending: ReadonlyMap<string, SessionModelRequest>;
  readonly nextSeq: number;
}

export const EMPTY_SESSION_MODEL_STATE: SessionModelState = {
  settled: {},
  pending: new Map(),
  nextSeq: 1,
};

export function sessionModelView(state: SessionModelState): Record<string, string> {
  const view: Record<string, string> = { ...state.settled };
  for (const [sid, request] of state.pending) view[sid] = request.key;
  return view;
}

export interface SessionModelDecision {
  state: SessionModelState;
  send: boolean;
}

export function requestSessionModel(
  state: SessionModelState,
  sid: string,
  key: string,
): SessionModelDecision {
  if (sid === '' || key === '') return { state, send: false };
  if (state.pending.has(sid)) return { state, send: false };
  if (state.settled[sid] === key) return { state, send: false };
  return {
    state: {
      settled: state.settled,
      pending: new Map(state.pending).set(sid, { seq: state.nextSeq, key }),
      nextSeq: state.nextSeq + 1,
    },
    send: true,
  };
}

export function settleSessionModel(
  state: SessionModelState,
  sid: string,
  seq: number,
  ok: boolean,
  effectiveKey?: string | null,
): SessionModelState {
  if (sid === '') return state;
  const pending = state.pending.get(sid);
  if (!pending || pending.seq !== seq) return state;
  const nextPending = new Map(state.pending);
  nextPending.delete(sid);
  if (ok && typeof effectiveKey === 'string' && effectiveKey !== '') {
    return {
      settled: { ...state.settled, [sid]: effectiveKey },
      pending: nextPending,
      nextSeq: state.nextSeq,
    };
  }
  return { settled: state.settled, pending: nextPending, nextSeq: state.nextSeq };
}

export function reconcileSessionModel(
  state: SessionModelState,
  sid: string,
  key: string,
): SessionModelState {
  if (sid === '' || key === '' || state.pending.has(sid)) return state;
  if (state.settled[sid] === key) return state;
  return {
    settled: { ...state.settled, [sid]: key },
    pending: state.pending,
    nextSeq: state.nextSeq,
  };
}

export function applySessionModelEvent(
  state: SessionModelState,
  sid: string,
  key: string,
): SessionModelState {
  if (sid === '' || key === '' || state.pending.has(sid)) return state;
  if (state.settled[sid] === key) return state;
  return {
    settled: { ...state.settled, [sid]: key },
    pending: state.pending,
    nextSeq: state.nextSeq,
  };
}

function compareText(left: string, right: string): number {
  const foldedOrder = left.toLocaleLowerCase().localeCompare(right.toLocaleLowerCase());
  return foldedOrder || left.localeCompare(right);
}

function sortModelOptions(options: ModelSelectOption[]): ModelSelectOption[] {
  return options.slice().sort((a, b) => (
    compareText(a.label, b.label) || compareText(a.id, b.id) || compareText(a.value, b.value)
  ));
}

export function groupModelsByProvider(models: readonly ModelSelectInput[]): ModelProviderGroup[] {
  const groups = new Map<string, ModelSelectOption[]>();
  const seen = new Set<string>();

  for (const raw of models) {
    if (!raw) continue;
    const provider = raw.provider;
    const id = raw.id;
    if (!provider || !id) continue;
    const value = `${provider}/${id}`;
    if (seen.has(value)) continue;
    seen.add(value);
    const name = raw.name ?? '';
    const option: ModelSelectOption = {
      id,
      name,
      value,
      label: raw.name || id,
    };
    const list = groups.get(provider);
    if (list) list.push(option);
    else groups.set(provider, [option]);
  }

  return [...groups.entries()]
    .sort((a, b) => compareText(a[0], b[0]))
    .map(([provider, options]) => ({
      provider,
      models: sortModelOptions(options),
    }));
}

export function keepSelectedModelOption(
  groups: readonly ModelProviderGroup[],
  value: string,
): ModelProviderGroup[] {
  if (!value) return groups.slice();
  const slash = value.indexOf('/');
  if (slash <= 0 || slash === value.length - 1) return groups.slice();
  for (const group of groups) {
    if (group.models.some((model) => model.value === value)) return groups.slice();
  }
  const provider = value.slice(0, slash);
  const id = value.slice(slash + 1);
  const option: ModelSelectOption = { id, name: id, value, label: id };
  const next = groups.map((group) => ({
    provider: group.provider,
    models: group.models.slice(),
  }));
  const existing = next.find((group) => group.provider === provider);
  if (existing) {
    existing.models = sortModelOptions([...existing.models, option]);
    return next;
  }
  next.push({ provider, models: [option] });
  next.sort((a, b) => compareText(a.provider, b.provider));
  return next;
}
