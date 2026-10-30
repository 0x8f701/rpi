import { readFileSync } from 'fs';
import { join } from 'path';
import { parseAnsi, ansiToPlainText } from '../src/ansi';
import { parseGoalArgs, parseLoopArgs, parseSlashLine, resolveSlashAction, WEB_COMPOSER_COMMANDS } from '../src/slash';
import { applySessionEvent, emptyTranscript, messagesToItems } from '../src/transcriptItems';
import {
  DEFAULT_COMMAND_TIMEOUT_MS,
  buildCodeReviewStopPayload,
  buildCodeReviewTldrPayload,
  commandTimeoutMs,
  emptyReviewSnapshot,
  parseCodeReviewTldrAck,
  parseReviewSnapshot,
  parseRunnerInviteLink,
  parseSessionCatalog,
  parseTmuxTerminalAttach,
} from '../src/protocol';
import { runnerRecordWithoutToken, TOKEN_STORAGE_KEY, RUNNER_STORAGE_KEY } from '../src/registry';
import { redactSecrets } from '../src/redact';
import { normalizeList, normalizeRead } from '../src/fileExplorer';
import { applyJobUpdated, parseJobList } from '../src/jobCard';
import { normalizeCodeReviewSnapshot, parseAvailableModels, parseThinkingLevels } from '../src/codeReviewSnapshot';
import { parseMarkdown, parseInlineMarkdown, safeMarkdownHref } from '../src/markdown';
import { applyStickyCtrl, decodeTmuxB64, encodeTmuxB64 } from '../src/tmuxCtrl';
import { formatTlsFingerprint, parseTlsInspect, tlsPinHost } from '../src/tlsPin';
import {
  MAX_MERMAID_SOURCE_BYTES,
  mermaidSourceWithinBound,
  mermaidViewerHtml,
  sanitizeMermaidSvg,
} from '../src/mermaidSvg';

describe('invite link', () => {
  it('keeps the token out of origin', () => {
    const parsed = parseRunnerInviteLink('https://192.0.2.15:8765/chat#listen-hash');
    expect(parsed.endpoint.origin).toBe('https://192.0.2.15:8765');
    expect(parsed.endpoint.origin.includes('#')).toBe(false);
    expect(parsed.endpoint.webSocketUrl).toBe('wss://192.0.2.15:8765/ws');
    expect(parsed.token).toBe('listen-hash');
  });
});

describe('runner persist shape', () => {
  it('strips token from the origin-bearing record', () => {
    const stored = runnerRecordWithoutToken({
      id: 'r1',
      label: 'psy',
      origin: 'https://192.0.2.15:8765',
      authority: '192.0.2.15:8765',
      token: 'listen-hash',
      codename: 'psy',
      createdAt: 1,
    });
    expect(stored.origin.includes('#')).toBe(false);
    expect('token' in stored).toBe(false);
    expect(TOKEN_STORAGE_KEY).toBe('rpi-mobile-chat-tokens:v1');
    expect(RUNNER_STORAGE_KEY).toBe('rpi-mobile-chat-runners:v1');
  });
});

describe('ansi pane text', () => {
  it('parses SGR color runs and redacts secrets in plain text', () => {
    const colored = '\u001b[32mok\u001b[0m';
    expect(ansiToPlainText(colored)).toBe('ok');
    expect(parseAnsi(colored)[0]?.fg).toBe(2);
    expect(redactSecrets('export GITHUB_TOKEN=ghp_aaaaaaaaaaaaaaaaaaaa')).toBe('export GITHUB_TOKEN=[REDACTED]');
  });
});

describe('source contracts', () => {
  const app = readFileSync(join(__dirname, '../src/App.tsx'), 'utf8');
  const detail = readFileSync(join(__dirname, '../src/screens/RunnerDetailScreen.tsx'), 'utf8');
  const runners = readFileSync(join(__dirname, '../src/screens/RunnersScreen.tsx'), 'utf8');
  const projects = readFileSync(join(__dirname, '../src/screens/ProjectsScreen.tsx'), 'utf8');

  it('has Runners and Projects tabs and no Team tab', () => {
    expect(app.includes("'runners' | 'projects'")).toBe(true);
    expect(app.includes("'Team'")).toBe(false);
    expect(app.includes('Team')).toBe(false);
    expect(runners.includes('title="Runners"')).toBe(true);
    expect(projects.includes('title="Projects"')).toBe(true);
  });

  it('has no pi-zig copy', () => {
    expect(app.includes('pi-zig')).toBe(false);
    expect(runners.includes('pi-zig')).toBe(false);
    expect(projects.includes('pi-zig')).toBe(false);
    expect(detail.includes('pi-zig')).toBe(false);
  });

  it('lists sessions with 24-line ANSI snapshots and two-click delete', () => {
    expect(detail.includes('tmux_list_sessions')).toBe(true);
    expect(detail.includes('tmux_pane_snapshot')).toBe(true);
    expect(detail.includes('TMUX_PREVIEW_LINES = 24')).toBe(true);
    expect(detail.includes('<AnsiText')).toBe(true);
    expect(detail.includes('tmux_session_create')).toBe(true);
    expect(detail.includes('tmux_session_delete')).toBe(true);
    expect(detail.includes('pendingDelete')).toBe(true);
    expect(detail.includes('Confirm delete')).toBe(true);
    expect(detail.includes('Delete?')).toBe(true);
  });

  it('shows LAN discovery add rows', () => {
    expect(runners.includes('lanCandidates')).toBe(true);
    expect(runners.includes('accessibilityLabel="Add"')).toBe(true);
  });

  it('new project lists connected runners only', () => {
    expect(projects.includes("filter((runner) => runner.state === 'connected')")).toBe(true);
  });

  it('opens a Session workspace from the runner', () => {
    expect(detail.includes("accessibilityLabel=\"Session\"")).toBe(true);
    expect(detail.includes('SessionWorkspaceScreen')).toBe(true);
  });
});

describe('web slash and transcript', () => {
  it('parses composer slashes onto the /ws command types', () => {
    expect(parseSlashLine('/goal build it')?.name).toBe('goal');
    expect(parseGoalArgs('').ok && parseGoalArgs('').ok ? parseGoalArgs('').action : null).toEqual({ op: 'show' });
    const created = parseGoalArgs('ship apk');
    expect(created.ok && created.action.op === 'create' ? created.action.objective : '').toBe('ship apk');
    const loop = parseLoopArgs('5m ping');
    expect(loop.ok && loop.action.op === 'create' ? loop.action.interval : '').toBe('5m');
    expect(resolveSlashAction('compact', '--snap').type).toBe('compact');
    expect(resolveSlashAction('skill', 'research').type).toBe('skill');
    expect(resolveSlashAction('model', 'anthropic/opus').type).toBe('model');
    expect('goal' in WEB_COMPOSER_COMMANDS).toBe(true);
  });

  it('restores user and assistant rows and streams thinking deltas', () => {
    const items = messagesToItems([
      { id: 'u1', message: { role: 'user', content: [{ type: 'text', text: 'hi' }] } },
      { id: 'a1', message: { role: 'assistant', content: [{ type: 'text', text: 'ok' }] } },
    ]);
    expect(items[0]).toMatchObject({ kind: 'user', text: 'hi' });
    expect(items[1]).toMatchObject({ kind: 'assistant', status: 'final' });
    const streamed = applySessionEvent(emptyTranscript(), {
      type: 'message_start',
      itemId: 'a2',
      message: { role: 'assistant' },
    });
    const withThink = applySessionEvent(streamed, {
      type: 'message_update',
      itemId: 'a2',
      assistantMessageEvent: { type: 'thinking_delta', delta: 'plan' },
    });
    const assistant = withThink.items.find((item) => item.id === 'a2');
    expect(assistant && assistant.kind === 'assistant' ? assistant.blocks[0]?.thinking : '').toBe('plan');
  });

  it('parses session catalog rows for switch/fork', () => {
    const rows = parseSessionCatalog({
      sessions: [{
        sessionId: 's1',
        path: '/sessions/s1.jsonl',
        cwd: '/work',
        name: 'main',
        loaded: true,
        runtimeIncarnation: 'inc-1',
        status: 'native',
      }],
    });
    expect(rows?.[0]?.sessionId).toBe('s1');
    expect(rows?.[0]?.runtimeIncarnation).toBe('inc-1');
  });
});

describe('file explorer wire', () => {
  it('keeps directory and file paths for a tree, not an entry count', () => {
    const listed = normalizeList({
      entries: [
        { name: 'src', path: 'src', kind: 'dir' },
        { name: 'app.ts', path: 'src/app.ts', kind: 'file', sizeBytes: 12 },
      ],
      truncated: false,
    });
    expect(listed?.entries.map((entry) => entry.path)).toEqual(['src', 'src/app.ts']);
    const read = normalizeRead({
      path: 'src/app.ts',
      lines: ['export const n = 1;'],
      totalLines: 1,
      sizeBytes: 12,
      truncated: false,
    });
    expect(read?.lines).toEqual(['export const n = 1;']);
    expect(normalizeList({ files: [] })).toBeNull();
  });
});

describe('jobs and review wire', () => {
  it('parses job_list empty as no jobs and applies job_updated', () => {
    const empty = parseJobList({ enabled: true, jobs: [] });
    expect(empty).toEqual({ enabled: true, jobs: [] });
    const next = applyJobUpdated([], {
      type: 'job_updated',
      job: { id: 'j1', agentId: 'a', agent: 'explore', parentId: '', status: 'running', createdAt: 1 },
    });
    expect(next[0]?.id).toBe('j1');
    expect(next[0]?.status).toBe('running');
  });

  it('parses review files and hunk lines', () => {
    const snapshot = normalizeCodeReviewSnapshot({
      comparisonLabel: 'HEAD',
      files: [{
        path: 'src/a.ts',
        status: 'modified',
        insertions: 1,
        deletions: 0,
        hunks: [{
          header: '@@ -1 +1 @@',
          contentHash: 'h1',
          lines: [{ kind: 'addition', text: '+ok' }],
        }],
      }],
      threads: [],
    });
    expect(snapshot.files[0]?.hunks[0]?.lines[0]?.text).toBe('+ok');
    expect(snapshot.threads).toEqual([]);
    expect(snapshot.autoReviewState).toBe('idle');
    expect(snapshot.reviewFindings).toEqual([]);
  });

  it('parses strict per-hunk review findings and rejects a malformed one for the whole snapshot', () => {
    const identity = { snapshotId: 'snap', path: 'src/a.ts', oldStart: 1, oldCount: 1, newStart: 1, newCount: 1, contentHash: 'h1' };
    const finding = { category: 'naming', side: 'new', startLine: 1, endLine: 1, summary: 'Rename the verb', evidence: 'The helper is named read but writes.' };
    const accepted = normalizeCodeReviewSnapshot({
      autoReviewState: 'triaging',
      threads: [{ identity: { path: 'src/a.ts' } }],
      reviewFindings: [{ identity, findings: [finding] }],
    });
    expect(accepted.error).toBeNull();
    expect(accepted.threads[0]?.comments).toEqual([]);
    expect(accepted.reviewFindings[0]?.identity.snapshotId).toBe('snap');
    expect(accepted.reviewFindings[0]?.findings).toEqual([finding]);
    expect(normalizeCodeReviewSnapshot({ threads: [{ path: 'src/a.ts' }] }).reviewFindings).toEqual([]);
    const invalid = 'Invalid code-review snapshot response';
    for (const payload of [
      { reviewFindings: [{ identity, findings: [{ ...finding, category: 'clean' }] }] },
      { reviewFindings: [{ identity, findings: [{ ...finding, severity: 'warning' }] }] },
      { reviewFindings: [{ identity, findings: [{ ...finding, category: 'constructor' }] }] },
      { reviewFindings: [{ identity, findings: [{ ...finding, side: 'added' }] }] },
      { reviewFindings: [{ identity, findings: [{ ...finding, startLine: 0 }] }] },
      { reviewFindings: [{ identity, findings: [{ ...finding, startLine: 3, endLine: 2 }] }] },
      { reviewFindings: [{ identity, findings: [{ ...finding, endLine: 4294967296 }] }] },
      { reviewFindings: [{ identity, findings: [{ ...finding, summary: '   ' }] }] },
      { reviewFindings: [{ identity, findings: [{ ...finding, summary: 'a'.repeat(513) }] }] },
      { reviewFindings: [{ identity, findings: [{ ...finding, evidence: '' }] }] },
      { reviewFindings: [{ identity, findings: [{ ...finding, evidence: 'a'.repeat(8193) }] }] },
      { reviewFindings: [{ identity, findings: [{ ...finding, extra: 1 }] }] },
      { reviewFindings: [{ identity: { ...identity, snapshotId: '' }, findings: [finding] }] },
      { reviewFindings: [{ identity: { ...identity, oldStart: -1 }, findings: [finding] }] },
      { reviewFindings: [{ identity, findings: [finding] }, { identity, findings: [] }] },
      { reviewFindings: [{ identity, findings: [finding, finding] }] },
      { reviewFindings: [{ identity, findings: [finding], extra: true }] },
      { reviewFindings: 'nope' },
      { reviewFindings: [{ identity, findings: Array.from({ length: 21 }, (_, index) => ({ ...finding, endLine: index + 1 })) }] },
    ]) {
      expect(normalizeCodeReviewSnapshot(payload).error).toBe(invalid);
    }
  });

  it('parses model catalog and thinking levels', () => {
    expect(parseAvailableModels({
      models: [{ id: 'opus', name: 'Opus', provider: 'anthropic' }, { id: 'skip-me' }],
    })).toEqual([{ id: 'opus', name: 'Opus', provider: 'anthropic' }]);
    expect(parseThinkingLevels({ levels: ['off', 'high'] })).toEqual(['off', 'high']);
    expect(parseThinkingLevels({})).toBeNull();
  });
});

describe('code review tldr wire', () => {
  it('starts an optional-language summary and stops it through the typed operation', () => {
    expect(buildCodeReviewTldrPayload()).toEqual({ type: 'code_review_tldr' });
    expect(buildCodeReviewTldrPayload('de')).toEqual({ type: 'code_review_tldr', language: 'de' });
    expect(buildCodeReviewStopPayload('tldr')).toEqual({ type: 'code_review_stop', operation: 'tldr' });
    // Start and stop ack within the fast window: neither inherits the long bound.
    expect(commandTimeoutMs('code_review_tldr')).toBe(DEFAULT_COMMAND_TIMEOUT_MS);
    expect(commandTimeoutMs('code_review_stop')).toBe(DEFAULT_COMMAND_TIMEOUT_MS);
  });

  it('parses the running ack fail-closed', () => {
    expect(parseCodeReviewTldrAck({ ok: true, state: 'running', tldrGeneration: 4 }))
      .toEqual({ state: 'running', tldrGeneration: 4 });
    expect(parseCodeReviewTldrAck({ state: 'running' })).toBeNull();
    expect(parseCodeReviewTldrAck({ ok: true })).toBeNull();
    expect(parseCodeReviewTldrAck({ ok: true, state: 'done', tldrGeneration: 4 })).toBeNull();
    expect(parseCodeReviewTldrAck({ ok: true, state: 'running', tldrGeneration: -1 })).toBeNull();
    expect(parseCodeReviewTldrAck({ ok: true, state: 'running', tldrGeneration: 1.5 })).toBeNull();
    expect(parseCodeReviewTldrAck('nope')).toBeNull();
  });

  it('projects the exact tldr snapshot fields and defaults the absent ones', () => {
    const empty = parseReviewSnapshot({ snapshotId: 'snap-1', files: [] });
    expect(empty?.tldrState).toBe('idle');
    expect(empty?.tldrGeneration).toBe(0);
    expect(empty?.tldrSnapshotId).toBeNull();
    expect(empty?.tldrError).toBeNull();
    expect(empty?.tldrMarkdown).toBe('');
    const full = parseReviewSnapshot({
      snapshotId: 'snap-1',
      files: [],
      tldrState: 'done',
      tldrGeneration: 2,
      tldrSnapshotId: 'snap-1',
      tldrError: null,
      tldrMarkdown: '## Overview\nbody',
    });
    expect(full?.tldrState).toBe('done');
    expect(full?.tldrGeneration).toBe(2);
    expect(full?.tldrSnapshotId).toBe('snap-1');
    expect(full?.tldrMarkdown).toBe('## Overview\nbody');
    // A supplied-but-malformed field rejects the whole payload (literal state
    // membership, non-negative integer generation, string-or-null fields);
    // absent fields keep their neutral defaults.
    for (const payload of [
      { snapshotId: 'snap-1', files: [], tldrState: 'generating' },
      { snapshotId: 'snap-1', files: [], tldrState: 'constructor' },
      { snapshotId: 'snap-1', files: [], tldrState: 'toString' },
      { snapshotId: 'snap-1', files: [], tldrGeneration: '2' },
      { snapshotId: 'snap-1', files: [], tldrGeneration: -1 },
      { snapshotId: 'snap-1', files: [], tldrSnapshotId: 5 },
      { snapshotId: 'snap-1', files: [], tldrError: 3 },
      { snapshotId: 'snap-1', files: [], tldrMarkdown: 7 },
    ]) {
      expect(parseReviewSnapshot(payload)).toBeNull();
    }
    expect(emptyReviewSnapshot().tldrState).toBe('idle');
    expect(emptyReviewSnapshot().tldrMarkdown).toBe('');
  });

  it('mirrors the tldr defaults in the page-reader snapshot', () => {
    const normalized = normalizeCodeReviewSnapshot({
      files: [],
      tldrState: 'running',
      tldrGeneration: 3,
      tldrSnapshotId: 'snap-1',
      tldrError: 'boom',
      tldrMarkdown: 'x',
    });
    expect(normalized.tldrState).toBe('running');
    expect(normalized.tldrGeneration).toBe(3);
    expect(normalized.tldrSnapshotId).toBe('snap-1');
    expect(normalized.tldrError).toBe('boom');
    expect(normalized.tldrMarkdown).toBe('x');
    // The page-reader mirror rejects supplied malformed fields the same way.
    for (const payload of [
      { files: [], tldrState: 'paused' },
      { files: [], tldrState: 'toString' },
      { files: [], tldrGeneration: -2 },
      { files: [], tldrGeneration: '2' },
      { files: [], tldrSnapshotId: 5 },
      { files: [], tldrError: 3 },
      { files: [], tldrMarkdown: 7 },
    ]) {
      expect(normalizeCodeReviewSnapshot(payload).error).toBe('Invalid code-review snapshot response');
    }
  });
});

describe('mermaid svg', () => {
  it('fails closed on scripted svg and enforces the 64KiB source bound', () => {
    expect(sanitizeMermaidSvg('<svg><script>x</script></svg>')).toBe('');
    expect(sanitizeMermaidSvg('<svg><rect/></svg>')).toContain('<svg');
    expect(mermaidSourceWithinBound('graph TD; A-->B;')).toBe(true);
    expect(mermaidSourceWithinBound('x'.repeat(MAX_MERMAID_SOURCE_BYTES + 1))).toBe(false);
    const html = mermaidViewerHtml('graph TD; A-->B;');
    expect(html.includes('cdn.jsdelivr.net')).toBe(false);
    expect(html.includes('src="mermaid.min.js"')).toBe(true);
  });
});

describe('markdown', () => {
  it('renders strong, code, lists, and keeps hostile markup as text', () => {
    const inlines = parseInlineMarkdown('say **hi** and `code`');
    expect(inlines).toEqual([
      { type: 'text', text: 'say ' },
      { type: 'strong', text: 'hi' },
      { type: 'text', text: ' and ' },
      { type: 'code', text: 'code' },
    ]);
    expect(safeMarkdownHref('javascript:alert(1)')).toBe('');
    expect(safeMarkdownHref('https://192.0.2.15:8765/chat')).toBe('https://192.0.2.15:8765/chat');
    const blocks = parseMarkdown('# Title\n\n<script>x</script>\n\n- one\n\n```js\nok\n```');
    expect(blocks[0]).toMatchObject({ type: 'h', level: 1 });
    const script = blocks.find((block) => block.type === 'p');
    expect(script && script.type === 'p' ? script.inlines.some((node) => node.type === 'text' && node.text.includes('<script>')) : false).toBe(true);
    expect(blocks.some((block) => block.type === 'li')).toBe(true);
    expect(blocks.some((block) => block.type === 'code' && block.body.includes('ok'))).toBe(true);
  });
  it('routes mermaid fences to the native mermaid block the renderer hosts', () => {
    const blocks = parseMarkdown('```mermaid\ngraph TD\na-->b\n```');
    expect(blocks[0]).toEqual({ type: 'mermaid', body: 'graph TD\na-->b\n' });
  });
});

describe('web surfaces on runner session', () => {
  const workspace = readFileSync(join(__dirname, '../src/screens/SessionWorkspaceScreen.tsx'), 'utf8');
  const files = readFileSync(join(__dirname, '../src/screens/FileExplorerPanel.tsx'), 'utf8');
  const review = readFileSync(join(__dirname, '../src/screens/ReviewPanel.tsx'), 'utf8');
  const jobs = readFileSync(join(__dirname, '../src/screens/JobsPanel.tsx'), 'utf8');
  const settings = readFileSync(join(__dirname, '../src/screens/SessionSettingsPanel.tsx'), 'utf8');
  const transcript = readFileSync(join(__dirname, '../src/TranscriptItemView.tsx'), 'utf8');
  const markdownBody = readFileSync(join(__dirname, '../src/MarkdownBody.tsx'), 'utf8');

  it('mounts file tree, review hunks, jobs, and model catalog instead of count stubs', () => {
    expect(workspace.includes('filesText')).toBe(false);
    expect(workspace.includes('Snapshot loaded')).toBe(false);
    expect(files.includes("type: 'file_explorer_read'")).toBe(true);
    expect(files.includes('Open file ${entry.path}')).toBe(true);
    expect(files.includes('Load more')).toBe(true);
    expect(review.includes('Review file ${file.path}')).toBe(true);
    expect(jobs.includes('No live jobs')).toBe(true);
    expect(jobs.includes("type: 'job_cancel'")).toBe(true);
    expect(settings.includes("type: 'get_available_models'")).toBe(true);
    expect(settings.includes("type: 'get_available_thinking_levels'")).toBe(true);
  });

  it('exposes Mermaid View and Download SVG', () => {
    expect(markdownBody.includes('accessibilityLabel="View"')).toBe(true);
    expect(markdownBody.includes('accessibilityLabel="Download SVG"')).toBe(true);
    expect(markdownBody.includes('mermaidViewerHtml')).toBe(true);
    expect(transcript.includes('MarkdownBody')).toBe(true);
  });

  it('reloads session snapshot after reconnect without dropping the workspace', () => {
    expect(workspace.includes('subscribeReconnect')).toBe(true);
    expect(workspace.includes('Reconnecting')).toBe(true);
    expect(workspace.includes("AppState.addEventListener('change'")).toBe(true);
    expect(workspace.includes("next !== 'active'")).toBe(true);
    expect(workspace.includes('hasOpenSocket()')).toBe(true);
    expect(workspace.includes('resumeFromBackground()')).toBe(true);
    expect(workspace.includes('loadSession(identity, false)')).toBe(true);
  });
});

describe('mentions and reconnect', () => {
  const { runnerMentionToken, insertMentionToken, resolveChannelRecipients, trailingMentionQuery } = require('../src/chatMentions');
  const { nextReconnectDelay, RUNNER_RECONNECT_INITIAL_MS, RUNNER_RECONNECT_MAX_MS } = require('../src/reconnect');
  const { parseTransportFailure, RUNNER_CERT_TRUST_HINT, transportFailureMessage } = require('../src/transportError');
  const { parseImportedRunner } = require('../src/chatSync');
  const { parseStoredMappingDocument } = require('../src/store');
  const { appendChannelLog, channelLogsFor, parseChannelLogs, serializeChannelLogs, replaceChannelLogs } = require('../src/channelLog');

  it('parses team, unmatched, and ambiguous mention plans', () => {
    const members = [
      { mappingId: 'm1', mention: runnerMentionToken('psy') },
      { mappingId: 'm2', mention: runnerMentionToken('x3') },
      { mappingId: 'm3', mention: 'lab' },
      { mappingId: 'm4', mention: 'lab' },
    ];
    expect(resolveChannelRecipients('hello team', members)).toEqual({ kind: 'team' });
    expect(resolveChannelRecipients('hi @psy', members)).toEqual({ kind: 'mentioned', mappingIds: ['m1'] });
    expect(resolveChannelRecipients('hi @nope', members)).toEqual({ kind: 'unmatched', token: 'nope' });
    expect(resolveChannelRecipients('hi @lab', members)).toEqual({ kind: 'ambiguous', token: 'lab' });
    expect(trailingMentionQuery('hello @ps')).toBe('ps');
    expect(insertMentionToken('hello @ps', 'psy')).toBe('hello @psy ');
  });

  it('doubles reconnect delay until the 15s cap', () => {
    expect(nextReconnectDelay(RUNNER_RECONNECT_INITIAL_MS)).toBe(1000);
    expect(nextReconnectDelay(1000)).toBe(2000);
    expect(nextReconnectDelay(8000)).toBe(RUNNER_RECONNECT_MAX_MS);
    expect(nextReconnectDelay(RUNNER_RECONNECT_MAX_MS)).toBe(RUNNER_RECONNECT_MAX_MS);
  });

  it('classifies cert failures onto the trust hint', () => {
    expect(parseTransportFailure('SSL handshake failed')).toBe('cert');
    expect(parseTransportFailure('Trust anchor for certification path not found')).toBe('cert');
    expect(parseTransportFailure('unauthorized token')).toBe('auth');
    expect(parseTransportFailure('Runner connection closed.')).toBe('closed');
    expect(transportFailureMessage('cert', 'ssl')).toBe(RUNNER_CERT_TRUST_HINT);
    expect(RUNNER_CERT_TRUST_HINT.includes('/chat')).toBe(false);
    expect(transportFailureMessage('cert', 'ssl').includes('Trust certificate')).toBe(true);
  });

  it('rejects mapping sync runner origins that embed a token hash', () => {
    expect(parseImportedRunner({
      id: 'r1',
      label: 'psy',
      origin: 'https://192.0.2.15:8765#secret',
      authority: '192.0.2.15:8765',
      createdAt: 1,
    })).toBeNull();
    expect(parseImportedRunner({
      id: 'r1',
      label: 'psy',
      origin: 'https://192.0.2.15:8765',
      authority: '192.0.2.15:8765',
      createdAt: 1,
    })?.origin.includes('#')).toBe(false);
  });

  it('adopts version-1 mapping documents and keeps channel logs across replace', () => {
    const parsed = parseStoredMappingDocument({
      version: 1,
      projects: [{ id: 'p1', label: 'app', cwd: '/work', createdAt: 1, updatedAt: 2 }],
      mappings: [{
        id: 'm1',
        projectId: 'p1',
        runnerId: 'r1',
        sessionId: 's1',
        sessionPath: '/s',
        sessionName: 'app',
        createdAt: 1,
      }],
    });
    expect(parsed?.mappings[0]?.sessionId).toBe('s1');
    expect(parseStoredMappingDocument({ version: 2, projects: [], mappings: [] })).toBeNull();
    replaceChannelLogs('p1', []);
    appendChannelLog('p1', { id: 'h1', at: 1, kind: 'human', mappingId: null, author: 'You', text: 'stay' });
    expect(channelLogsFor('p1')[0]?.text).toBe('stay');
    const raw = serializeChannelLogs();
    expect(parseChannelLogs(raw).p1[0]?.text).toBe('stay');
  });
});

describe('channel streaming and local xterm', () => {
  const channel = readFileSync(join(__dirname, '../src/screens/ProjectChannelScreen.tsx'), 'utf8');
  const tmuxHtml = readFileSync(join(__dirname, '../src/tmuxTerminalHtml.ts'), 'utf8');
  const tmuxScreen = readFileSync(join(__dirname, '../src/screens/TmuxTerminalScreen.tsx'), 'utf8');
  const goal = readFileSync(join(__dirname, '../src/screens/GoalPanel.tsx'), 'utf8');
  const todo = readFileSync(join(__dirname, '../src/screens/TodoPanel.tsx'), 'utf8');
  const gradle = readFileSync(join(__dirname, '../android/app/build.gradle'), 'utf8');
  const runners = readFileSync(join(__dirname, '../src/screens/RunnersScreen.tsx'), 'utf8');
  const client = readFileSync(join(__dirname, '../src/runnerClient.ts'), 'utf8');

  it('subscribes mapped sessions and restores get_messages into run cards', () => {
    expect(channel.includes("type: 'session_list'")).toBe(true);
    expect(channel.includes('client.subscribe(identity)')).toBe(true);
    expect(channel.includes("type: 'get_messages'")).toBe(true);
    expect(channel.includes('applySessionEvent')).toBe(true);
    expect(channel.includes('appendChannelLog')).toBe(true);
    expect(channel.includes('resolveChannelRecipients')).toBe(true);
    expect(channel.includes('accessibilityLabel="Stop"')).toBe(true);
    expect(channel.includes('MarkdownBody')).toBe(true);
    expect(channel.includes('subscribeReconnect')).toBe(true);
    expect(channel.includes("AppState.addEventListener('change'")).toBe(true);
    expect(channel.includes('hasOpenSocket()')).toBe(true);
    expect(channel.includes('resumeFromBackground()')).toBe(true);
  });

  it('bundles xterm locally with FitAddon fit and a soft-key row', () => {
    expect(tmuxHtml.includes('cdn.jsdelivr.net')).toBe(false);
    expect(tmuxHtml.includes('fitAddon.fit()')).toBe(true);
    expect(tmuxHtml.includes('addon-fit.js')).toBe(true);
    expect(tmuxScreen.includes("accessibilityLabel={key.label}")).toBe(true);
    expect(tmuxScreen.includes("label: 'Esc'")).toBe(true);
    expect(tmuxScreen.includes("label: 'Ctrl'")).toBe(true);
    expect(tmuxScreen.includes("label: 'Tab'")).toBe(true);
    expect(gradle.includes('copyXtermAssets')).toBe(true);
    expect(gradle.includes('mermaid.min.js')).toBe(true);
    expect(gradle.includes('versionName "1.0.0"')).toBe(true);
    expect(gradle.includes('versionCode 5')).toBe(true);
    expect(gradle.includes('rpi-chat-${defaultConfig.versionName}.apk')).toBe(true);
    const pkg = JSON.parse(readFileSync(join(__dirname, '../package.json'), 'utf8')) as { version: string };
    expect(pkg.version).toBe('1.0.0');
    const readme = readFileSync(join(__dirname, '../README.md'), 'utf8');
    expect(readme.includes('rpi-chat-1.0.0.apk')).toBe(true);
    expect(readme.includes('rpi-chat-1.2.apk')).toBe(false);
    expect(readme.includes('rpi-chat-1.1.apk')).toBe(false);
    expect(tmuxScreen.includes('mapTmuxInputB64')).toBe(true);
    expect(tmuxScreen.includes('ctrlHeldRef')).toBe(true);
    expect(tmuxScreen.includes('subscribeReconnect')).toBe(true);
    expect(tmuxScreen.includes("setStatus('Reconnecting')")).toBe(true);
    expect(tmuxScreen.includes('attachRetryUsedRef')).toBe(true);
    expect(tmuxScreen.includes('const leave = useCallback')).toBe(true);
    expect(tmuxScreen.includes("AppState.addEventListener('change'")).toBe(true);
    expect(tmuxScreen.includes("next !== 'active'")).toBe(true);
    expect(tmuxScreen.includes('hasOpenSocket()')).toBe(true);
    expect(tmuxScreen.includes("type: 'tmux_terminal_resize'")).toBe(true);
    expect(tmuxScreen.includes('fullHistory')).toBe(false);
    expect(tmuxScreen.includes('cachedHistoryKey')).toBe(false);
    const protocol = readFileSync(join(__dirname, '../src/protocol.ts'), 'utf8');
    expect(protocol.includes('fullHistory')).toBe(false);
    expect(protocol.includes("type: 'tmux_terminal_attach'; session: string; cols: number; rows: number }")).toBe(true);
    expect(parseTmuxTerminalAttach({
      terminalId: 'term-1',
      session: 'agentlo-pi-rs',
      cols: 80,
      rows: 24,
      paneKey: '%0',
      historyKey: '1:agentlo-pi-rs:%0',
      historyBytes: 0,
      historyComplete: false,
    })).toEqual({ terminalId: 'term-1', session: 'agentlo-pi-rs', cols: 80, rows: 24 });
  });

  it('refreshes goal and todo panels from updated frames', () => {
    expect(goal.includes("record.type !== 'goal_updated'")).toBe(true);
    expect(todo.includes("record.type !== 'todo_updated'")).toBe(true);
  });

  it('reconnects with cursor subscribe and surfaces cert trust', () => {
    expect(client.includes("'reconnecting'")).toBe(true);
    expect(client.includes('sessionCursors')).toBe(true);
    expect(client.includes('resubscribe()')).toBe(true);
    expect(client.includes('resumeFromBackground')).toBe(true);
    expect(client.includes("next === 'active'")).toBe(true);
    expect(client.includes('clearReconnectTimer')).toBe(true);
    expect(client.includes('shouldResumeFromBackground')).toBe(true);
    expect(runners.includes('TrustCertificate')).toBe(true);
    expect(runners.includes('OpenToTrust')).toBe(false);
    expect(runners.includes('Linking.openURL')).toBe(false);
    const trustUi = readFileSync(join(__dirname, '../src/TrustCertificate.tsx'), 'utf8');
    expect(trustUi.includes('accessibilityLabel="Trust certificate"')).toBe(true);
    expect(trustUi.includes('inspectRunnerTls')).toBe(true);
    expect(trustUi.includes('Linking.openURL')).toBe(false);
    const appNative = readFileSync(join(__dirname, '../android/app/src/main/java/com/rpichat/MainApplication.kt'), 'utf8');
    expect(appNative.includes('WebSocketModule.setCustomClientBuilder')).toBe(true);
    expect(appNative.includes('OkHttpClientProvider.setOkHttpClientFactory')).toBe(true);
    expect(appNative.includes('TlsPinPackage()')).toBe(true);
  });
});

describe('tls pin and sticky ctrl', () => {
  it('accepts a SHA-256 inspect payload and rejects a short digest', () => {
    const sha256 = 'a'.repeat(64);
    expect(tlsPinHost('https://192.0.2.15:8765/chat')).toBe('192.0.2.15');
    expect(parseTlsInspect({
      host: '192.0.2.15',
      sha256,
      subject: 'CN=psy',
      issuer: 'CN=psy',
      notAfterMs: 1,
    })).toEqual({
      host: '192.0.2.15',
      sha256,
      subject: 'CN=psy',
      issuer: 'CN=psy',
      notAfterMs: 1,
    });
    expect(parseTlsInspect({
      host: '192.0.2.15',
      sha256: 'abc',
      subject: 'CN=psy',
      issuer: 'CN=psy',
      notAfterMs: 1,
    })).toBeNull();
    expect(formatTlsFingerprint(sha256).startsWith('aaaa aaaa')).toBe(true);
  });

  it('maps sticky Ctrl+C from keyboard text and round-trips tmux b64', () => {
    expect(applyStickyCtrl('c')).toEqual({ text: '\u0003', consumed: true });
    expect(applyStickyCtrl('C')).toEqual({ text: '\u0003', consumed: true });
    expect(applyStickyCtrl('\u001b')).toEqual({ text: '\u001b', consumed: false });
    expect(applyStickyCtrl('cc')).toEqual({ text: 'cc', consumed: false });
    expect(decodeTmuxB64(encodeTmuxB64('c'))).toBe('c');
    expect(applyStickyCtrl(decodeTmuxB64(encodeTmuxB64('c'))).text).toBe('\u0003');
  });
});
