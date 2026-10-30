#!/usr/bin/env node
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const failures: string[] = [];
let ran = 0;
function check(name: string, cond: boolean, detail = ''): void {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

function sliceFn(src: string, start: string, end: string): string {
  const at = src.indexOf(start);
  if (at < 0) return '';
  const stop = src.indexOf(end, at + start.length);
  return stop > at ? src.slice(at, stop) : src.slice(at, at + 1800);
}

const here = dirname(fileURLToPath(import.meta.url));
const mobile = join(here, '..', '..', '..', '..', 'mobile', 'src');

const settings = readFileSync(join(mobile, 'screens', 'SessionSettingsPanel.tsx'), 'utf8');
const addRunner = readFileSync(join(mobile, 'screens', 'AddRunnerScreen.tsx'), 'utf8');
const runner = readFileSync(join(mobile, 'screens', 'RunnerDetailScreen.tsx'), 'utf8');
const trust = readFileSync(join(mobile, 'TrustCertificate.tsx'), 'utf8');
const projects = readFileSync(join(mobile, 'screens', 'ProjectsScreen.tsx'), 'utf8');

const settingsRun = sliceFn(settings, 'const run = async (fn: () => Promise<void>) => {', 'return (');
check('settings declares busyRef', settings.includes('const busyRef = useRef(false);'));
check(
  'settings run claims busyRef before fn()',
  settingsRun.includes('if (busyRef.current) return')
    && settingsRun.includes('busyRef.current = true')
    && settingsRun.indexOf('busyRef.current = true') < settingsRun.indexOf('await fn()'),
);
check(
  'settings queue onPress gates busyRef and queueReady',
  settings.includes('if (busyRef.current || !queueReady) return'),
);
check('settings keeps Model change failed', settings.includes("'Model change failed'"));
check('settings keeps Thinking change failed', settings.includes("'Thinking change failed'"));
check('settings keeps Queue mode failed', settings.includes("'Queue mode failed'"));

const submit = sliceFn(addRunner, 'const submit = async (raw: string) => {', 'const submitManual');
const submitManual = sliceFn(addRunner, 'const submitManual = async () => {', 'return (');
check('addRunner declares busyRef', addRunner.includes('const busyRef = useRef(false);'));
check(
  'addRunner submit claims busyRef before onAdd',
  submit.includes('if (busyRef.current) return')
    && submit.includes('busyRef.current = true')
    && submit.indexOf('busyRef.current = true') < submit.indexOf('await props.onAdd(raw)'),
);
check(
  'addRunner submitManual empty-address returns before claim',
  submitManual.includes('if (!trimmedAddress) return')
    && submitManual.indexOf('if (!trimmedAddress) return') < submitManual.indexOf('await submit(raw)'),
);

const createSession = sliceFn(runner, 'const createSession = async () => {', 'const deleteSession');
const deleteSession = sliceFn(runner, 'const deleteSession = async (name: string) => {', 'if (!runner)');
check('runner declares busyRef', runner.includes('const busyRef = useRef(false);'));
check(
  'createSession empty name returns before claim',
  createSession.includes("if (name === '') return")
    && createSession.indexOf("if (name === '') return") < createSession.indexOf('busyRef.current = true'),
);
check(
  'createSession claims busyRef before tmux_session_create',
  createSession.includes('if (busyRef.current) return')
    && createSession.includes('busyRef.current = true')
    && createSession.indexOf('busyRef.current = true') < createSession.indexOf("type: 'tmux_session_create'"),
);
check('createSession keeps Create failed', createSession.includes("'Create failed'"));
check(
  'deleteSession pendingDelete two-step returns before claim',
  deleteSession.includes('if (pendingDelete !== name)')
    && deleteSession.indexOf('if (pendingDelete !== name)') < deleteSession.indexOf('busyRef.current = true'),
);
check(
  'deleteSession claims busyRef before tmux_session_delete',
  deleteSession.includes('if (busyRef.current) return')
    && deleteSession.includes('busyRef.current = true')
    && deleteSession.indexOf('busyRef.current = true') < deleteSession.indexOf("type: 'tmux_session_delete'"),
);
check('deleteSession keeps Delete failed', deleteSession.includes("'Delete failed'"));
check('runner refreshSeqRef stays last-request-wins', runner.includes('const refreshSeqRef = useRef(0);'));

const tokenSave = sliceFn(runner, 'onSave={(token) => {', 'onCancel={() => setTokenOpen(false)}');
check(
  'token onSave claims busyRef before updateToken',
  tokenSave.includes('if (busyRef.current) return')
    && tokenSave.includes('busyRef.current = true')
    && tokenSave.indexOf('busyRef.current = true') < tokenSave.indexOf('registry.updateToken'),
);
check(
  'token onSave has no setBusy(true) without a preceding busyRef claim',
  (() => {
    let from = 0;
    while (true) {
      const at = tokenSave.indexOf('setBusy(true)', from);
      if (at < 0) return from > 0;
      const claim = tokenSave.lastIndexOf('busyRef.current = true', at);
      if (claim < 0 || claim > at) return false;
      from = at + 1;
    }
  })(),
);
check(
  'token onSave keeps setTokenOpen(false) on success',
  tokenSave.includes('setTokenOpen(false)')
    && tokenSave.indexOf('registry.updateToken') < tokenSave.indexOf('setTokenOpen(false)'),
);

const inspect = sliceFn(trust, 'const inspect = async () => {', 'const confirm = async () => {');
const confirm = sliceFn(trust, 'const confirm = async () => {', 'return (');
check('trust declares busyRef', trust.includes('const busyRef = useRef(false);'));
check(
  'trust inspect claims busyRef before inspectRunnerTls',
  inspect.includes('if (busyRef.current) return')
    && inspect.includes('busyRef.current = true')
    && inspect.indexOf('busyRef.current = true') < inspect.indexOf('inspectRunnerTls(props.origin)'),
);
check(
  'trust confirm preview === null returns before claim',
  confirm.includes('if (preview === null) return')
    && confirm.indexOf('if (preview === null) return') < confirm.indexOf('busyRef.current = true'),
);
check(
  'trust confirm claims busyRef before trustRunnerTls',
  confirm.includes('if (busyRef.current) return')
    && confirm.includes('busyRef.current = true')
    && confirm.indexOf('busyRef.current = true') < confirm.indexOf('trustRunnerTls(props.origin, preview.sha256)'),
);

const newProject = sliceFn(projects, 'export function NewProjectScreen', 'return (');
check('newProject declares busyRef', newProject.includes('const busyRef = useRef(false);'));
check(
  'newProject absolute-cwd check stays before claim',
  newProject.includes("'Project directory must be absolute.'")
    && newProject.indexOf("'Project directory must be absolute.'") < newProject.indexOf('busyRef.current = true'),
);
check(
  'newProject claims busyRef before mappingStore.addProject',
  newProject.includes('if (busyRef.current) return')
    && newProject.includes('busyRef.current = true')
    && newProject.indexOf('busyRef.current = true') < newProject.indexOf('mappingStore.addProject(label, cwd)'),
);
check('newProject keeps mappingStore.mapRunner', newProject.includes('mappingStore.mapRunner(registry, project.id, runnerId)'));

console.log(`\nandroidBusyLock.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
