export function userContentPreview(content: unknown): string {
  if (typeof content === 'string' && content.trim() !== '') {
    return content;
  }
  if (!Array.isArray(content)) {
    return '';
  }
  const texts: string[] = [];
  let hasImage = false;
  for (const block of content) {
    if (!block || typeof block !== 'object') continue;
    const row = block as { type?: unknown; text?: unknown };
    if (row.type === 'text' && typeof row.text === 'string' && row.text !== '') {
      texts.push(row.text);
    } else if (row.type === 'image') {
      hasImage = true;
    }
  }
  const joined = texts.join('\n');
  if (joined.trim() !== '') return joined;
  return hasImage ? '(image)' : '';
}

export function steeringPreviewsFromMessages(messages: unknown): string[] {
  if (!Array.isArray(messages)) return [];
  const previews: string[] = [];
  for (const message of messages) {
    if (!message || typeof message !== 'object') continue;
    const row = message as { role?: unknown; content?: unknown };
    if (row.role !== 'user') continue;
    const preview = userContentPreview(row.content);
    if (preview !== '') previews.push(preview);
  }
  return previews;
}

export function steeringPreviewsFromState(state: unknown): string[] | null {
  if (!state || typeof state !== 'object') return null;
  const row = state as { queuedSteering?: unknown };
  if (!Array.isArray(row.queuedSteering)) return null;
  return row.queuedSteering.filter((item): item is string => typeof item === 'string');
}

export function collapseSteeringPreview(text: string): string {
  return text.split(/\s+/).filter(Boolean).join(' ');
}

export function applyQueueUpdatePreviews(
  prev: Record<string, string[]>,
  sessionId: string,
  previews: string[],
): Record<string, string[]> {
  const current = prev[sessionId];
  if (current && current.length === previews.length && current.every((item, i) => item === previews[i])) {
    return prev;
  }
  return { ...prev, [sessionId]: previews };
}

export function applyStateQueuedSteering(
  prev: Record<string, string[]>,
  sessionId: string,
  queuedSteering: string[] | null,
  liveFromQueueUpdate: boolean,
  sessionStreaming = false,
  submitInFlight = false,
): Record<string, string[]> {
  if (queuedSteering === null || liveFromQueueUpdate) return prev;
  const current = prev[sessionId];
  // A stale get_state built BEFORE the optimistic row's submit reached the
  // server reports an empty queue; never let it wipe an un-acked row.
  if (
    (sessionStreaming || submitInFlight)
    && queuedSteering.length === 0
    && current
    && current.length > 0
  ) {
    return prev;
  }
  if (current && current.length === queuedSteering.length && current.every((item, i) => item === queuedSteering[i])) {
    return prev;
  }
  return { ...prev, [sessionId]: queuedSteering };
}

export function dropSteeringAt(previews: string[], index: number): string[] {
  if (index < 0 || index >= previews.length) return previews;
  return previews.filter((_, i) => i !== index);
}

export function dropLastMatchingPreview(previews: string[], text: string): string[] {
  const index = previews.lastIndexOf(text);
  if (index < 0) return previews;
  return dropSteeringAt(previews, index);
}

/** Drop the FIRST preview matching `text`: the queue is FIFO, so the oldest
 *  matching entry is the one the agent consumes first. Identity (same array)
 *  when nothing matches, so callers can keep the previous tray state. */
export function dropConsumedPreview(previews: string[], text: string): string[] {
  const index = previews.indexOf(text);
  if (index < 0) return previews;
  return dropSteeringAt(previews, index);
}

/** After a successful cancel ACK, drop the captured preview. Prefer the
 *  original index when that slot still matches; otherwise relocate by text
 *  so a concurrent consume cannot cancel the wrong row. Identity when the
 *  preview is already gone. */
export function dropQueuedPreviewAfterCancel(
  previews: string[],
  index: number,
  preview: string | undefined,
): string[] {
  const at = preview !== undefined && previews[index] === preview
    ? index
    : (preview === undefined ? -1 : previews.indexOf(preview));
  if (at < 0) return previews;
  return dropSteeringAt(previews, at);
}

/** Await a queue command. A throw reports through `onFailure` and never
 *  runs `afterSuccess` (the local row stays). A successful cancel may drop
 *  locally only when `currentGen()` still matches the generation captured
 *  before send — a newer queue_update owns the tray. */
export async function settleQueueCommand(
  send: () => Promise<unknown>,
  startedGen: number,
  currentGen: () => number,
  onFailure: (error: unknown) => void,
  afterSuccess?: () => void,
): Promise<void> {
  try {
    await send();
  } catch (error) {
    onFailure(error);
    return;
  }
  if (!afterSuccess || currentGen() !== startedGen) return;
  afterSuccess();
}

/** Exclusive lock for cancel/send-now. A second click before re-render is
 *  ignored. Visual busy is set synchronously; released in `finally` after
 *  `run` settles (success or throw). */
export async function runExclusiveQueueCommand(
  busyRef: { current: boolean },
  setBusy: (busy: boolean) => void,
  run: () => Promise<void>,
): Promise<void> {
  if (busyRef.current) return;
  busyRef.current = true;
  setBusy(true);
  try {
    await run();
  } finally {
    busyRef.current = false;
    setBusy(false);
  }
}
