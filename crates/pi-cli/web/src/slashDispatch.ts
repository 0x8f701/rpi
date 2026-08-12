/**
 * Pure slash-command dispatch decisions for the Web composer submit path.
 *
 * Takes a parseSupportedCommand result ({name, args}) and maps it to the
 * concrete RPC / panel action Main should take. No DOM/socket dependency —
 * scripts/slashParse.test.ts exercises the decision table.
 *
 * Supported surface (matches WEB_SUPPORTED_COMMANDS):
 *   /compact [--snap | instructions…]
 *   /skill <name>
 *   /code-review [from to]
 */

import { parseCodeReviewArgs } from './codeReview';

export type SlashAction =
  | { type: 'compact'; mode: 'snap' }
  | { type: 'compact'; mode: 'llm'; customInstructions: string }
  | { type: 'skill'; name: string }
  | { type: 'code-review'; from?: string; to?: string }
  | { type: 'error'; message: string };

/**
 * True when `/compact` arguments select the deterministic snap path.
 * Mirrors the TUI rule: `--snap` alone or as a leading flag; any trailing
 * text after `--snap` is ignored (snap has no summarization instructions).
 */
export function isSnapCompactArgs(args: string): boolean {
  const trimmed = args.trim();
  if (!trimmed) return false;
  if (trimmed === '--snap') return true;
  return trimmed.startsWith('--snap') && /^--snap(?:\s|$)/.test(trimmed);
}

/**
 * Map a supported slash command + argument tail to a dispatch action.
 * Unknown names never reach this helper (parseSupportedCommand already
 * filters the Web surface); an unexpected name yields an error rather than
 * silently falling through to a prompt.
 */
export function resolveSlashAction(name: string, args: string): SlashAction {
  switch (name) {
    case 'compact': {
      if (isSnapCompactArgs(args)) {
        return { type: 'compact', mode: 'snap' };
      }
      return {
        type: 'compact',
        mode: 'llm',
        customInstructions: args.trim(),
      };
    }
    case 'skill': {
      const skillName = args.trim();
      if (!skillName) {
        return { type: 'error', message: 'usage: /skill <name>' };
      }
      // Skill names are a single token on the catalog; take the first token
      // so a pasted description after the name does not poison the RPC.
      const first = skillName.split(/\s+/)[0] ?? skillName;
      return { type: 'skill', name: first };
    }
    case 'code-review': {
      const parsed = parseCodeReviewArgs(args);
      if (!parsed.ok) {
        return { type: 'error', message: parsed.error };
      }
      if (parsed.from && parsed.to) {
        return { type: 'code-review', from: parsed.from, to: parsed.to };
      }
      return { type: 'code-review' };
    }
    default:
      return { type: 'error', message: `unsupported command: /${name}` };
  }
}

/**
 * Format a compact/snapcompact RPC token report for a system-style bubble.
 * Defensive against missing fields so a partial backend never throws.
 */
export function formatCompactReport(data: unknown, label: string): string {
  const d = (data && typeof data === 'object' ? data : {}) as {
    tokensBefore?: unknown;
    estimatedTokensAfter?: unknown;
  };
  const before = d.tokensBefore;
  const after = d.estimatedTokensAfter;
  if (typeof before !== 'number') return `${label}: done`;
  const afterText = typeof after === 'number' ? String(after) : '?';
  const shrank = typeof after === 'number' && after < before ? ' (shrank)' : '';
  return `${label}: ${before} → ${afterText} estimated tokens${shrank}`;
}

/**
 * Format a skill RPC response (`{name, summary}`) for a visible bubble.
 * Falls back to stringifying summary alone when the wrapper is bare text.
 */
export function formatSkillResult(data: unknown, requestedName: string): string {
  if (typeof data === 'string') {
    return data.trim() ? data : `skill ${requestedName}: (empty)`;
  }
  const d = (data && typeof data === 'object' ? data : {}) as {
    name?: unknown;
    summary?: unknown;
  };
  const name = typeof d.name === 'string' && d.name ? d.name : requestedName;
  const summary = typeof d.summary === 'string' ? d.summary : '';
  if (summary) return summary;
  return `skill ${name}: (no summary)`;
}
