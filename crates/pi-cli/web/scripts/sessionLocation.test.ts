#!/usr/bin/env node
// Session panel location display — cwd / sessionFile from get_state
// (RpcSessionState camelCase). Proves present fields render safely with
// bounded truncation, missing fields show Unavailable (never guessed), and
// the New-session hint demands an explicitly selected directory while
// naming the current project/cwd + storage directory. Run via
// `npm run build` (esbuild + node).
//
// Exit codes: 0 = every assertion held; 1 = a regression.
import {
  SESSION_PATH_DISPLAY_MAX,
  formatSessionPath,
  parsePathCompleteResponse,
  sendPathCompleteRequest,
  sessionDirectoryOf,
  projectLabelOf,
  newSessionLocationHint,
} from '../src/panels/SessionPanel.tsx';

const failures: string[] = [];
let ran = 0;

function check(name: string, cond: boolean, detail?: string) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

// ---- present wire fields (get_state: cwd, sessionFile) ----
{
  const cwd = '<workspace>/pi-rs';
  const sessionFile = '<agent-base>/sessions/--workspace-very-long-project-directory-name-pi-rs--/abc123.jsonl';

  const cwdDisp = formatSessionPath(cwd);
  check('cwd present is available', cwdDisp.available);
  check('cwd text is full when short', cwdDisp.text === cwd, cwdDisp.text);
  check('cwd title is full path', cwdDisp.title === cwd, cwdDisp.title);

  const fileDisp = formatSessionPath(sessionFile);
  check('sessionFile present is available', fileDisp.available);
  check(
    'sessionFile truncated when long',
    fileDisp.text.includes('…') && fileDisp.text.length <= SESSION_PATH_DISPLAY_MAX + 2,
    fileDisp.text
  );
  check('sessionFile title keeps full path', fileDisp.title === sessionFile, fileDisp.title);
  check('sessionFile truncation keeps head', fileDisp.text.startsWith('<agent-base>/'), fileDisp.text);
  check('sessionFile truncation keeps file name', fileDisp.text.endsWith('abc123.jsonl'), fileDisp.text);

  check('projectLabelOf uses basename', projectLabelOf(cwd) === 'pi-rs', String(projectLabelOf(cwd)));
  check(
    'sessionDirectoryOf is parent dir',
    sessionDirectoryOf(sessionFile) === '<agent-base>/sessions/--workspace-very-long-project-directory-name-pi-rs--',
    String(sessionDirectoryOf(sessionFile))
  );

  const hint = newSessionLocationHint(cwd, sessionFile);
  check('hint names project', hint.includes('project pi-rs'), hint);
  check('hint names cwd', hint.includes('cwd <workspace>/pi-rs') || hint.includes('cwd <workspace>/'), hint);
  check('hint names storage dir', hint.includes('stores under'), hint);
  check('hint not Unavailable when present', !hint.includes('Unavailable'), hint);
  check('hint demands an explicit directory selection', hint.includes('explicitly select'), hint);
  check('hint names any existing local directory', hint.includes('any existing local directory'), hint);
  check('hint never suggests inherit or default semantics', !hint.includes('inherit') && !hint.includes('default') && !hint.includes('empty'), hint);
}

// ---- New-session hint copy stays explicit-selection only ----
{
  // Even with a fully populated current session, the hint must never imply
  // a default or inherited New-session directory.
  const populated = newSessionLocationHint('<workspace>/pi-rs', '<agent-base>/sessions/x/s.jsonl');
  check('populated hint never mentions defaults or inheritance', !populated.includes('inherit') && !populated.includes('default') && !populated.includes('leave empty'), populated);
  check('populated hint keeps the explicit-selection requirement', populated.includes('explicitly select'), populated);
}

// ---- missing / empty / wrong-type fields -> Unavailable, never guess ----
{
  for (const bad of [undefined, null, '', '   ', 42, {}, []] as unknown[]) {
    const disp = formatSessionPath(bad);
    check(
      `formatSessionPath(${JSON.stringify(bad)}) -> Unavailable`,
      disp.available === false && disp.text === 'Unavailable' && disp.title === '',
      JSON.stringify(disp)
    );
  }

  check('projectLabelOf missing -> null', projectLabelOf(undefined) === null);
  check('projectLabelOf empty -> null', projectLabelOf('') === null);
  check('projectLabelOf non-string -> null', projectLabelOf(1) === null);
  check('sessionDirectoryOf missing -> null', sessionDirectoryOf(undefined) === null);
  check('sessionDirectoryOf empty -> null', sessionDirectoryOf('') === null);
  check('sessionDirectoryOf bare name -> null', sessionDirectoryOf('only-name.jsonl') === null);

  const missingHint = newSessionLocationHint(undefined, null);
  check(
    'missing fields hint uses Unavailable',
    missingHint.includes('project Unavailable') &&
      missingHint.includes('cwd Unavailable') &&
      missingHint.includes('stores under Unavailable'),
    missingHint
  );

  const partialCwd = newSessionLocationHint('/tmp/proj', null);
  check(
    'partial: cwd present sessionFile missing',
    partialCwd.includes('project proj') &&
      partialCwd.includes('cwd /tmp/proj') &&
      partialCwd.includes('stores under Unavailable'),
    partialCwd
  );

  const partialFile = newSessionLocationHint(null, '/data/sessions/x.jsonl');
  check(
    'partial: sessionFile present cwd missing',
    partialFile.includes('project Unavailable') &&
      partialFile.includes('cwd Unavailable') &&
      partialFile.includes('stores under /data/sessions'),
    partialFile
  );
}

// ---- Windows-style separators still resolve project + directory ----
{
  const winCwd = 'C:\\Users\\user\\Projects\\pi-rs';
  const winFile = 'C:\\Users\\user\\AppData\\rpi\\sessions\\--C-Users-user-Projects-pi-rs--\\sess.jsonl';
  check('projectLabelOf windows cwd', projectLabelOf(winCwd) === 'pi-rs', String(projectLabelOf(winCwd)));
  check(
    'sessionDirectoryOf windows path',
    sessionDirectoryOf(winFile) === 'C:\\Users\\user\\AppData\\rpi\\sessions\\--C-Users-user-Projects-pi-rs--',
    String(sessionDirectoryOf(winFile))
  );
}

// ---- safeText still applied (credential-shaped path segments redacted) ----
{
  const dirty = '/tmp/sk-abcdefghijklmnopqrstuvwxyz/session.jsonl';
  const disp = formatSessionPath(dirty);
  check('path redacts secret-shaped segment', disp.text.includes('[REDACTED]'), disp.text);
  check('title also redacted', disp.title.includes('[REDACTED]'), disp.title);
}

// ---- path_complete reply validation (bounded, quiet on malformed) ----
{
  const ok = {
    base: '/ws',
    prefix: 'pro',
    entries: [
      { name: 'projects', path: '/ws/projects', kind: 'directory' },
      { name: 'notes.txt', path: '/ws/notes.txt', kind: 'file' },
    ],
    truncated: false,
  };
  const parsed = parsePathCompleteResponse(ok);
  check('valid path_complete reply parses', parsed !== null && parsed.entries.length === 2 && parsed.base === '/ws', JSON.stringify(parsed));

  const filtered = parsePathCompleteResponse({ ...ok, entries: [...ok.entries, { name: 5, path: '/ws/x', kind: 'file' }, null, 'nope'] });
  check('invalid entries dropped, valid kept', filtered !== null && filtered.entries.length === 2, JSON.stringify(filtered));

  for (const bad of [null, undefined, 42, 'err', {}, { base: 1, prefix: '', entries: [], truncated: false }, { base: '/ws', prefix: '', entries: {}, truncated: false }, { base: '/ws', prefix: '', entries: [], truncated: 'no' }]) {
    check(`malformed reply is null (${JSON.stringify(bad)})`, parsePathCompleteResponse(bad) === null, JSON.stringify(parsePathCompleteResponse(bad)));
  }

  // Regression: >100 valid entries in an untruncated reply must be capped
  // at PATH_COMPLETE_LIMIT with truncated forced true (never trust wire
  // unbounded lists — the overlay slices defensively but the parse is
  // authoritative). Malformed-entry filtering still applies within the cap.
  const over = parsePathCompleteResponse({
    ...ok,
    entries: Array.from({ length: 101 }, (_, i) => ({ name: `n${i}`, path: `/ws/n${i}`, kind: 'file' })),
    truncated: false,
  });
  check(
    'over-limit entries capped to PATH_COMPLETE_LIMIT with truncated true',
    over !== null && over.entries.length === 100 && over.truncated === true,
    `len=${over?.entries.length} truncated=${over?.truncated}`
  );
  check('capped entries keep first 100 in order', over !== null && over.entries[0].path === '/ws/n0' && over.entries[99].path === '/ws/n99', JSON.stringify(over?.entries.length));

  const mixed = parsePathCompleteResponse({
    ...ok,
    entries: [
      ...Array.from({ length: 101 }, (_, i) => ({ name: `n${i}`, path: `/ws/n${i}`, kind: 'file' })),
      { name: 5, path: '/ws/bad', kind: 'file' },
      null,
    ],
    truncated: false,
  });
  check('malformed entries filtered before the cap', mixed !== null && mixed.entries.length === 100 && mixed.truncated === true, `len=${mixed?.entries.length}`);

  const exactly = parsePathCompleteResponse({
    ...ok,
    entries: Array.from({ length: 100 }, (_, i) => ({ name: `n${i}`, path: `/ws/n${i}`, kind: 'file' })),
    truncated: false,
  });
  check('exactly-limit entries keep wire truncated flag', exactly !== null && exactly.entries.length === 100 && exactly.truncated === false, `truncated=${exactly?.truncated}`);
}

// ---- sendPathCompleteRequest behavior: wire shape + quiet failures ----
{
  const sent: Record<string, unknown>[] = [];
  const sendCommand = async (command: Record<string, unknown>): Promise<unknown> => {
    sent.push(command);
    return {
      base: '/ws',
      prefix: 'pro',
      entries: [{ name: 'projects', path: '/ws/projects', kind: 'directory' }],
      truncated: false,
    };
  };

  const ok = await sendPathCompleteRequest(sendCommand, 'sess-1', { type: 'path_complete', path: '/ws/pro', kind: 'any' });
  check(
    'sender pins path_complete/directory + sessionId',
    sent.length === 1 && sent[0].type === 'path_complete' && sent[0].kind === 'directory' && sent[0].sessionId === 'sess-1' && sent[0].path === '/ws/pro',
    JSON.stringify(sent[0])
  );
  check('sender parses valid reply', ok !== null && ok.entries.length === 1 && ok.entries[0].name === 'projects', JSON.stringify(ok));

  const noSession = await sendPathCompleteRequest(sendCommand, null, { type: 'path_complete', path: '/ws', kind: 'any' });
  check('sender omits sessionId when none', !Object.prototype.hasOwnProperty.call(sent[1], 'sessionId'), JSON.stringify(sent[1]));
  check('sender still parses without sessionId', noSession !== null, JSON.stringify(noSession));

  const failing = async (): Promise<unknown> => {
    throw new Error('socket gone');
  };
  check('sender swallows transport failure to null', await sendPathCompleteRequest(failing, 'sess-1', { type: 'path_complete', path: '/x', kind: 'any' }) === null);
  const malformed = async (): Promise<unknown> => 'protocol error text';
  check('sender maps malformed reply to null', await sendPathCompleteRequest(malformed, 'sess-1', { type: 'path_complete', path: '/x', kind: 'any' }) === null);
}
console.log(`\nsessionLocation.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
