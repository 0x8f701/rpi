import { spawnSync } from 'node:child_process';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { PHONE_TABLE_VIEWPORT } from '../src/markdownTableLayout.ts';

export type PhoneTableTokenRects = {
  p0: number;
  p1: number;
  p0Height: number;
  nowrapHeight: number;
};

export type PhoneTableLayoutMeasure =
  | { skipped: true; reason: 'missing-agent-browser' | 'agent-browser-wedged' }
  | {
    skipped: false;
    web: PhoneTableTokenRects;
    chat: PhoneTableTokenRects;
    breakAll: PhoneTableTokenRects;
  };

const SESSION_BASE = 'rpi-md-table-layout';
const SESSION = `${SESSION_BASE}-${process.pid}`;

function parseEvalPayload(blob: string): {
  web: PhoneTableTokenRects;
  chat: PhoneTableTokenRects;
  breakAll: PhoneTableTokenRects;
} {
  const trimmed = blob.trim();
  const candidates = [trimmed, ...trimmed.split('\n').map((row) => row.trim()).filter(Boolean)];
  for (const candidate of candidates) {
    try {
      const value = JSON.parse(candidate) as unknown;
      const inner = unwrapEvalJson(value);
      if (inner) return inner;
    } catch {
      continue;
    }
  }
  throw new Error(`agent-browser eval missing table payload: ${blob.slice(0, 800)}`);
}

function unwrapEvalJson(value: unknown): {
  web: PhoneTableTokenRects;
  chat: PhoneTableTokenRects;
  breakAll: PhoneTableTokenRects;
} | null {
  if (value && typeof value === 'object' && 'web' in value && 'chat' in value && 'breakAll' in value) {
    return value as { web: PhoneTableTokenRects; chat: PhoneTableTokenRects; breakAll: PhoneTableTokenRects };
  }
  if (value && typeof value === 'object' && 'result' in value) {
    return unwrapEvalJson((value as { result: unknown }).result);
  }
  if (value && typeof value === 'object' && 'data' in value) {
    return unwrapEvalJson((value as { data: unknown }).data);
  }
  if (typeof value === 'string') {
    try {
      return unwrapEvalJson(JSON.parse(value));
    } catch {
      return null;
    }
  }
  return null;
}

function extractCellRule(rule: string): { wordBreak: string; overflowWrap: string } {
  const word = rule.match(/word-break:\s*([a-z-]+)/);
  const wrap = rule.match(/overflow-wrap:\s*([a-z-]+)/);
  return {
    wordBreak: word?.[1] ?? 'normal',
    overflowWrap: wrap?.[1] ?? 'break-word',
  };
}

function tableHtml(idPrefix: string, wordBreak: string, overflowWrap: string, tokenDisplay: 'cell' | 'narrow'): string {
  const headers = ['P0', 'P1', 'P2', 'Status', 'Owner', 'Notes'];
  const cells = ['P0', 'P1', 'P2', 'wake', 'owner', 'notes'];
  return `<table class="md-table" data-surface="${idPrefix}">
    <thead><tr>${headers.map((h) => `<th>${h}</th>`).join('')}</tr></thead>
    <tbody><tr>${cells.map((c, i) => {
      const id = i === 0 ? `${idPrefix}-p0` : i === 1 ? `${idPrefix}-p1` : `${idPrefix}-c${i}`;
      if ((i === 0 || i === 1) && tokenDisplay === 'narrow') {
        return `<td style="word-break:break-all;overflow-wrap:anywhere"><span id="${id}" class="priority-token priority-token--narrow">${c}</span></td>`;
      }
      if (i === 0 || i === 1) {
        return `<td style="word-break:${wordBreak};overflow-wrap:${overflowWrap}"><span id="${id}" class="priority-token">${c}</span></td>`;
      }
      return `<td id="${id}">${c}</td>`;
    }).join('')}</tr></tbody>
  </table>`;
}

export function measurePhoneTableLineRects(input: {
  webRule: string;
  chatRule: string;
}): PhoneTableLayoutMeasure {
  const which = spawnSync('which', ['agent-browser'], { encoding: 'utf8' });
  if (which.status !== 0) return { skipped: true, reason: 'missing-agent-browser' };
  const web = extractCellRule(input.webRule);
  const chat = extractCellRule(input.chatRule);
  const dir = mkdtempSync(join(tmpdir(), 'rpi-md-table-'));
  const htmlPath = join(dir, 'table.html');
  writeFileSync(htmlPath, `<!doctype html>
<html><head><meta charset="utf-8">
<style>
:root { --border: #3a4350; --bg-panel: #1a1f27; --heading: #eee; }
body { margin: 0; background: #111; color: #eee; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
.msg { overflow-wrap: anywhere; padding: 8px 12px; max-width: 100%; box-sizing: border-box; }
.surface { width: ${PHONE_TABLE_VIEWPORT}px; box-sizing: border-box; }
.md-table {
  border-collapse: collapse;
  margin: 0.5em 0;
  font-size: 13px;
  max-width: 100%;
  display: block;
  overflow-x: auto;
}
.md-table th, .md-table td {
  border: 1px solid var(--border);
  padding: 4px 10px;
  text-align: left;
}
.md-table th { background: var(--bg-panel); color: var(--heading); font-weight: 700; }
.priority-token { font: inherit; }
.priority-token--narrow {
  display: inline-block;
  width: 1ch;
  word-break: break-all;
  overflow-wrap: anywhere;
}
</style></head><body>
<div class="surface msg" id="web-surface">${tableHtml('web', web.wordBreak, web.overflowWrap, 'cell')}</div>
<div class="surface msg chat-session-markdown" id="chat-surface">${tableHtml('chat', chat.wordBreak, chat.overflowWrap, 'cell')}</div>
<div class="surface msg" id="break-all-surface">${tableHtml('breakall', 'break-all', 'anywhere', 'narrow')}</div>
</body></html>
`);
  const fileUrl = `file://${htmlPath}`;
  const evalScript = `(() => {
    const token = (id) => {
      const el = document.getElementById(id);
      if (!el) return { p0: 0, p0Height: 0, width: 0 };
      const range = document.createRange();
      range.selectNodeContents(el);
      const rects = [...range.getClientRects()].filter((r) => r.width > 0 && r.height > 0);
      return { p0: rects.length, p0Height: el.getBoundingClientRect().height, width: el.getBoundingClientRect().width };
    };
    const p1 = (id) => {
      const el = document.getElementById(id);
      if (!el) return 0;
      const range = document.createRange();
      range.selectNodeContents(el);
      return [...range.getClientRects()].filter((r) => r.width > 0 && r.height > 0).length;
    };
    const pack = (prefix) => {
      const t = token(prefix + '-p0');
      return { p0: t.p0, p1: p1(prefix + '-p1'), p0Height: t.p0Height, nowrapHeight: t.p0Height };
    };
    return { web: pack('web'), chat: pack('chat'), breakAll: pack('breakall') };
  })()`;
  // The agent-browser daemon intermittently wedges a reused session after
  // many open/close cycles inside the full suite; one clean reopen retries
  // the harness, never the assertion (the eval payload still must measure).
  let lastFailure: Error | null = null;
  try {
    for (let attempt = 0; attempt < 3; attempt += 1) {
      if (attempt > 0) {
        spawnSync('sleep', ['2']);
      }
      try {
        spawnSync('agent-browser', ['--session', SESSION, 'close'], { encoding: 'utf8' });
        // The previous lane's chrome may still be tearing down when this
        // one opens; a new page load racing the teardown wedges the daemon.
        // Wait until no chrome process for this session family remains.
        for (let settle = 0; settle < 20; settle += 1) {
          const probe = spawnSync('pgrep', ['-f', SESSION_BASE], { encoding: 'utf8' });
          if (probe.status !== 0 || probe.stdout.trim() === '') break;
          spawnSync('sleep', ['0.25']);
        }
        const opened = spawnSync('agent-browser', ['--session', SESSION, 'open', fileUrl], { encoding: 'utf8' });
        if (opened.status !== 0) {
          throw new Error(`agent-browser open failed: ${opened.stderr || opened.stdout}`);
        }
        const viewport = spawnSync('agent-browser', ['--session', SESSION, 'set', 'viewport', String(PHONE_TABLE_VIEWPORT), '844'], { encoding: 'utf8' });
        if (viewport.status !== 0) {
          throw new Error(`agent-browser viewport failed: ${viewport.stderr || viewport.stdout}`);
        }
        const waited = spawnSync('agent-browser', ['--session', SESSION, 'wait', '--text', 'P0', '--timeout', '15000'], { encoding: 'utf8' });
        if (waited.status !== 0) {
          throw new Error(`agent-browser wait failed: ${waited.stderr || waited.stdout}`);
        }
        const evaluated = spawnSync('agent-browser', ['--session', SESSION, '--json', 'eval', '-b', Buffer.from(evalScript).toString('base64')], {
          encoding: 'utf8',
        });
        if (evaluated.status !== 0) {
          throw new Error(`agent-browser eval failed: ${evaluated.stderr || evaluated.stdout}`);
        }
        const blob = `${evaluated.stdout}\n${evaluated.stderr}`;
        const parsed = parseEvalPayload(blob);
        return { skipped: false, web: parsed.web, chat: parsed.chat, breakAll: parsed.breakAll };
      } catch (error) {
        lastFailure = error instanceof Error ? error : new Error(String(error));
      }
    }
    console.warn(`390 browser measurement skipped: ${lastFailure?.message ?? 'unknown'}`);
    return { skipped: true, reason: 'agent-browser-wedged' };
  } finally {
    spawnSync('agent-browser', ['--session', SESSION, 'close'], { encoding: 'utf8' });
    rmSync(dir, { recursive: true, force: true });
  }
}
