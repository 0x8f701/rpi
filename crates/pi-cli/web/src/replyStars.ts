/**
 * Session-scoped starred-reply store — persists ONLY authoritative transcript
 * entry ids (never bodies, previews, or snapshots) per listener authority and
 * session, and derives the right-rail minimap projection from the already
 * loaded transcript items.
 *
 * The key mirrors the existing host/session-scoped conventions
 * (`rpi-web-token:<encoded authority>`, `rpi-web-session:<encoded authority>`):
 * `rpi-web-reply-stars:<encoded authority>:<encoded sessionId>`. Encoding keeps
 * IPv6/port colons safe and keeps the exact trimmed identity (no origin-wide
 * or listener-wide fallback), so identical entry-id strings in two sessions —
 * or two listeners — never share stars.
 *
 * Failure policy (the reply-star contract is fail-loud, unlike the fail-soft
 * token/session preferences): a storage error or malformed stored payload
 * throws with scope context so the caller can keep its last-good stars and
 * report the failure. The single explicit exception is a null storage:
 * `loadReplyStars` reports no stars (nothing can be stored) and
 * `saveReplyStars` throws, because a silent no-op would fake persistence.
 */
import type { StorageLike } from './hostToken';
import { contentText, firstLinePreview, type Item } from './transcript';

const REPLY_STARS_PREFIX = 'rpi-web-reply-stars:';

/** One-glance minimap label budget; the full reply stays in the transcript. */
const PREVIEW_CHAR_LIMIT = 80;

/** Listener authority + session identity the starred ids are scoped to. */
export type ReplyStarScope = Readonly<{ authority: string; sessionId: string }>;

/** One starred main assistant reply, in transcript order. */
export type StarredReply = Readonly<{
  entryId: string;
  itemId: string;
  preview: string;
  transcriptIndex: number;
}>;

function scopeLabel(scope: ReplyStarScope): string {
  return `${scope.authority.trim()}/${scope.sessionId.trim()}`;
}

/** The exact session-scoped key for `scope`; an empty authority or session id
 *  is rejected rather than degrading to a shared bucket. */
function replyStarsKey(scope: ReplyStarScope): string {
  const authority = scope.authority.trim();
  const sessionId = scope.sessionId.trim();
  if (authority === '' || sessionId === '') {
    throw new Error('reply stars require a non-empty listener authority and session id');
  }
  return `${REPLY_STARS_PREFIX}${encodeURIComponent(authority)}:${encodeURIComponent(sessionId)}`;
}

/** Load the starred entry ids for `scope` from its scoped key only.
 *
 *  - `null` storage → an empty set (explicitly unavailable; the caller must
 *    still fail the star action because nothing can persist);
 *  - a storage read failure → throws with scope context;
 *  - a stored payload that is not a JSON array of non-empty strings → throws
 *    rather than silently treating corrupted stars as none. */
export function loadReplyStars(
  storage: StorageLike | null,
  scope: ReplyStarScope,
): ReadonlySet<string> {
  const key = replyStarsKey(scope);
  if (storage === null) return new Set<string>();
  let raw: string | null;
  try {
    raw = storage.getItem(key);
  } catch (error) {
    throw new Error(
      `reply stars read failed for ${scopeLabel(scope)}: ${error instanceof Error ? error.message : String(error)}`,
    );
  }
  if (raw === null || raw === '') return new Set<string>();
  const malformed = `reply stars for ${scopeLabel(scope)} are malformed: expected a JSON array of entry ids`;
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    throw new Error(malformed);
  }
  if (!Array.isArray(parsed)) throw new Error(malformed);
  const entryIds = new Set<string>();
  for (const value of parsed) {
    if (typeof value !== 'string' || value === '') throw new Error(malformed);
    entryIds.add(value);
  }
  return entryIds;
}

/** Persist the exact starred entry-id set for `scope` under its scoped key.
 *  An empty set removes the key (no empty bucket). Throws with context when
 *  storage is unavailable or the write fails — never a silent no-op. */
export function saveReplyStars(
  storage: StorageLike | null,
  scope: ReplyStarScope,
  entryIds: ReadonlySet<string>,
): void {
  const key = replyStarsKey(scope);
  if (storage === null) {
    throw new Error(`reply stars unavailable for ${scopeLabel(scope)}: browser storage is not available`);
  }
  const ids = [...entryIds];
  if (ids.some((id) => typeof id !== 'string' || id === '')) {
    throw new Error(`reply stars for ${scopeLabel(scope)} reject an empty or non-string entry id`);
  }
  // Sorted + Set-deduped so the stored bytes are deterministic.
  const payload = JSON.stringify(ids.sort());
  try {
    if (ids.length === 0) storage.removeItem(key);
    else storage.setItem(key, payload);
  } catch (error) {
    throw new Error(
      `reply stars save failed for ${scopeLabel(scope)}: ${error instanceof Error ? error.message : String(error)}`,
    );
  }
}

/** Project the currently loaded transcript onto the starred entry-id set.
 *  Pure: no storage, no caching of message content. Only settled assistant
 *  items carrying the authoritative `entryId` can appear; output preserves
 *  transcript order and records the item index for jump bookkeeping. A
 *  starred id whose row is not in `items` (hidden branch) stays stored but
 *  invisible here. */
export function deriveStarredReplies(
  items: readonly Item[],
  starredEntryIds: ReadonlySet<string>,
): StarredReply[] {
  const replies: StarredReply[] = [];
  items.forEach((item, transcriptIndex) => {
    if (item.kind !== 'assistant') return;
    if (item.status !== 'final' || !item.entryId) return;
    const entryId = item.entryId;
    if (!starredEntryIds.has(entryId)) return;
    replies.push({
      entryId,
      itemId: item.id,
      preview: firstLinePreview(contentText(item.blocks), PREVIEW_CHAR_LIMIT),
      transcriptIndex,
    });
  });
  return replies;
}
