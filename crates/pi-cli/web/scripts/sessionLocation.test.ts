#!/usr/bin/env node
// Session panel location display — cwd / sessionFile from get_state
// (RpcSessionState camelCase). Proves present fields render safely with
// bounded truncation, missing fields show Unavailable (never guessed), and
// the New-session hint names project/cwd + storage directory. Run via
// `npm run build` (esbuild + node).
//
// Exit codes: 0 = every assertion held; 1 = a regression.
import {
  SESSION_PATH_DISPLAY_MAX,
  formatSessionPath,
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

console.log(`\nsessionLocation.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
