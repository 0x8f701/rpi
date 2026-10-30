export interface ChannelMentionMember {
  mappingId: string;
  mention: string;
}

export type ChannelRecipientPlan =
  | { kind: 'team' }
  | { kind: 'mentioned'; mappingIds: readonly string[] }
  | { kind: 'unmatched'; token: string }
  | { kind: 'ambiguous'; token: string };

const MENTION_TOKEN = /(?:^|\s)@([^\s@]+)/g;
const TRAILING_MENTION = /(?:^|\s)@([^\s@]*)$/;

export function runnerMentionToken(name: string): string {
  const compact = name.trim().replace(/\s+/g, '-');
  if (!compact) throw new Error('Runner mention token is blank.');
  return compact;
}

export function trailingMentionQuery(draft: string): string | null {
  const match = TRAILING_MENTION.exec(draft);
  if (!match) return null;
  return match[1] ?? '';
}

export function insertMentionToken(draft: string, mention: string): string {
  const query = trailingMentionQuery(draft);
  if (query === null) {
    const prefix = draft.length === 0 || /\s$/.test(draft) ? draft : `${draft} `;
    return `${prefix}@${mention} `;
  }
  return `${draft.slice(0, draft.length - query.length)}${mention} `;
}

export function resolveChannelRecipients(
  draft: string,
  members: readonly ChannelMentionMember[],
): ChannelRecipientPlan {
  const tokens: string[] = [];
  MENTION_TOKEN.lastIndex = 0;
  let match = MENTION_TOKEN.exec(draft);
  while (match) {
    const token = match[1];
    if (token) tokens.push(token);
    match = MENTION_TOKEN.exec(draft);
  }
  if (tokens.length === 0) return { kind: 'team' };

  const byToken = new Map<string, string[]>();
  for (const member of members) {
    const key = member.mention.toLowerCase();
    const current = byToken.get(key) ?? [];
    current.push(member.mappingId);
    byToken.set(key, current);
  }

  const mappingIds: string[] = [];
  const seen = new Set<string>();
  for (const token of tokens) {
    const hits = byToken.get(token.toLowerCase()) ?? [];
    if (hits.length === 0) return { kind: 'unmatched', token };
    if (hits.length > 1) return { kind: 'ambiguous', token };
    const mappingId = hits[0];
    if (!mappingId || seen.has(mappingId)) continue;
    seen.add(mappingId);
    mappingIds.push(mappingId);
  }
  if (mappingIds.length === 0) return { kind: 'team' };
  return { kind: 'mentioned', mappingIds };
}
