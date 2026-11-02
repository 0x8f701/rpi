#!/usr/bin/env node
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  CHAT_RAIL_EMPTY,
  CHAT_RUNNERS_EMPTY,
  CHAT_RUNNER_MAIN_EMPTY,
  CHAT_WORKSPACE_EMPTY_BODY,
} from '../src/chat/chatEmpty.ts';
import {
  CHAT_RAIL_FOCUS_STORAGE_PREFIX,
  chatRailFocusStorageKey,
  loadChatRailFocus,
  parseChatRailFocus,
  saveChatRailFocus,
  selectStoredRailFocus,
} from '../src/chat/chatRail.ts';

const failures: string[] = [];
let assertions = 0;

function check(name: string, condition: boolean, detail = ''): void {
  assertions += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

function makeStorage(): {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
  has(key: string): boolean;
} {
  const store = new Map<string, string>();
  return {
    getItem: (key) => (store.has(key) ? store.get(key) ?? null : null),
    setItem: (key, value) => { store.set(key, String(value)); },
    removeItem: (key) => { store.delete(key); },
    has: (key) => store.has(key),
  };
}

const here = dirname(fileURLToPath(import.meta.url));
const styles = readFileSync(join(here, '../src/chat/styles.css'), 'utf8');

{
  check('storage key uses CHAT_RAIL_FOCUS_STORAGE_PREFIX',
    CHAT_RAIL_FOCUS_STORAGE_PREFIX === 'rpi-web-chat-rail:v1:'
    && chatRailFocusStorageKey('host:8765') === `${CHAT_RAIL_FOCUS_STORAGE_PREFIX}${encodeURIComponent('host:8765')}`);
  check('storage key encodes the authority colon',
    chatRailFocusStorageKey('host:8765') === 'rpi-web-chat-rail:v1:host%3A8765');
}

{
  check('parse rejects garbage JSON', parseChatRailFocus('{') === null);
  check('parse rejects arrays', parseChatRailFocus('[]') === null);
  check('parse rejects null', parseChatRailFocus('null') === null);
  check('parse rejects empty', parseChatRailFocus('') === null && parseChatRailFocus('   ') === null);
  check('parse rejects missing kind', parseChatRailFocus('{"id":"abc"}') === null);
  check('parse rejects unknown kind', parseChatRailFocus('{"kind":"channel","id":"abc"}') === null);
  check('parse rejects path-like slash id', parseChatRailFocus('{"kind":"project","id":"foo/bar"}') === null);
  check('parse rejects path-like backslash id', parseChatRailFocus('{"kind":"runner","id":"foo\\\\bar"}') === null);
  check('parse rejects newline id', parseChatRailFocus('{"kind":"project","id":"foo\\nbar"}') === null);
  check('parse rejects extra-long payload', parseChatRailFocus(`x${'a'.repeat(256)}`) === null);
  check('parse rejects extra-long id', parseChatRailFocus(JSON.stringify({ kind: 'project', id: 'a'.repeat(129) })) === null);
  check('parse accepts project focus', (() => {
    const parsed = parseChatRailFocus('{"kind":"project","id":"p1"}');
    return parsed !== null && parsed.kind === 'project' && parsed.id === 'p1';
  })());
  check('parse accepts runner focus', (() => {
    const parsed = parseChatRailFocus('{"kind":"runner","id":"r1"}');
    return parsed !== null && parsed.kind === 'runner' && parsed.id === 'r1';
  })());
}

{
  const storage = makeStorage();
  const authority = 'host:8765';
  const key = chatRailFocusStorageKey(authority);
  saveChatRailFocus(storage, authority, { kind: 'project', id: 'proj-1' });
  const loadedProject = loadChatRailFocus(storage, authority);
  check('round-trip saves project focus',
    loadedProject !== null && loadedProject.kind === 'project' && loadedProject.id === 'proj-1'
    && storage.getItem(key) === '{"kind":"project","id":"proj-1"}');
  saveChatRailFocus(storage, authority, { kind: 'runner', id: 'run-1' });
  const loadedRunner = loadChatRailFocus(storage, authority);
  check('round-trip saves runner focus',
    loadedRunner !== null && loadedRunner.kind === 'runner' && loadedRunner.id === 'run-1');
  saveChatRailFocus(storage, authority, null);
  check('saveChatRailFocus(null) removes the key', !storage.has(key) && loadChatRailFocus(storage, authority) === null);
}

{
  const rows = [{ id: 'a', label: 'A' }, { id: 'b', label: 'B' }];
  check('selectStoredRailFocus returns the matching row',
    selectStoredRailFocus(rows, { kind: 'runner', id: 'b' }) === rows[1]);
  check('selectStoredRailFocus returns null when id is absent',
    selectStoredRailFocus(rows, { kind: 'project', id: 'missing' }) === null);
  check('selectStoredRailFocus returns null when saved is null',
    selectStoredRailFocus(rows, null) === null);
}

{
  check('CHAT_RAIL_EMPTY stays the Projects list copy',
    CHAT_RAIL_EMPTY === 'Create a project to open a channel.');
  check('CHAT_RUNNERS_EMPTY is exact',
    CHAT_RUNNERS_EMPTY === 'Add a runner to open its terminals.');
  check('CHAT_RUNNER_MAIN_EMPTY is exact',
    CHAT_RUNNER_MAIN_EMPTY === 'This runner is not connected.');

  check('workspace empty says runners are independent of projects',
    CHAT_WORKSPACE_EMPTY_BODY.includes('Add runners independently of projects')
    && CHAT_WORKSPACE_EMPTY_BODY.includes('pick already-added runners')
    && CHAT_WORKSPACE_EMPTY_BODY.includes('Conversations stay on the rpi server.')
    && CHAT_WORKSPACE_EMPTY_BODY.includes('@ a runner'));
}

{
  const projectsBlock = styles.slice(styles.indexOf('.chat-app--projects {'), styles.indexOf('.chat-workspace-bar {'));
  check('left projects column stays 260px', projectsBlock.includes('grid-template-columns: 260px minmax(0, 1fr)'));
  const composer = styles.slice(styles.indexOf('.chat-session__composer {'), styles.indexOf('.chat-session__composer textarea {'));
  check('composer z-index stays 15', composer.includes('z-index: 15'));
  check('gear and invite stay z-index 16', styles.includes('.chat-invite--overlay,')
    && styles.includes('.chat-gear-menu {')
    && styles.includes('z-index: 16'));
  const tmuxOverlay = styles.slice(styles.indexOf('.chat-tmux-terminal {'), styles.indexOf('.chat-tmux-terminal__head {'));
  check('tmux overlay z-index stays 20', tmuxOverlay.includes('z-index: 20'));
  check('tmux overlay starts below the 44px workspace bar', tmuxOverlay.includes('top: 44px') && !tmuxOverlay.includes('inset: 0'));
  check('tmux overlay draws a right border', tmuxOverlay.includes('border-right: 1px solid var(--border-strong)'));
  check('rail is a vertical stack of sections', styles.includes('.chat-projects-rail {')
    && styles.includes('flex-direction: column')
    && styles.includes('.chat-runners-rail,')
    && styles.includes('.chat-rail-section,')
    && styles.includes('.chat-projects-section {')
    && styles.includes('flex: 1 1 0'));
  check('rail section head matches projects head', styles.includes('.chat-projects-rail__head,')
    && styles.includes('.chat-rail-section__head {'));
  check('runner row mirrors selected project accent border', styles.includes('.chat-runner-row[data-selected="true"]')
    && styles.includes('.chat-project-row[data-selected="true"],')
    && styles.includes('border-color: var(--accent)'));
  check('runner row has hover and focus-visible', styles.includes('.chat-runner-row:hover')
    && styles.includes('.chat-runner-row:focus-visible {')
    && styles.includes('outline: 2px solid var(--accent)'));
  check('runner main fills the right column like project main', styles.includes('.chat-project-main,')
    && styles.includes('.chat-runner-main {')
    && styles.includes('.chat-runner-header {')
    && styles.includes('.chat-runner-header__identity')
    && styles.includes('.chat-runner-header__actions'));
}

{
  const preview = styles.slice(styles.indexOf('.chat-tmux-preview {'), styles.indexOf('.chat-tmux-preview__cards {'));
  check('tmux preview is a filling grid', preview.includes('display: grid')
    && preview.includes('gap: 8px')
    && preview.includes('minmax(0, 1fr)')
    && preview.includes('overflow: auto'));
  check('preview cards use the content-sized rail stack',
    styles.includes('.chat-tmux-preview__cards {')
    && styles.includes('grid-template-columns: minmax(0, 1fr)'));
  const cell = styles.slice(styles.indexOf('.chat-tmux-preview__cell {'), styles.indexOf('.chat-tmux-preview__delete'));
  const card = styles.slice(styles.indexOf('.chat-tmux-preview__card {'), styles.indexOf('.chat-tmux-preview__card:hover'));
  check('tmux preview card is a clickable terminal tile', cell.includes('border: 1px solid var(--border-strong)')
    && cell.includes('background: #111111')
    && card.includes('cursor: pointer')
    && !card.includes('border-radius')
    && !card.includes('box-shadow')
    && !card.includes('gradient'));
  check('tmux preview card hover uses accent border', styles.includes('.chat-tmux-preview__card:hover {')
    && styles.includes('.chat-tmux-preview__card:focus-visible {'));
  const pre = styles.slice(styles.indexOf('.chat-tmux-preview__pre {'), styles.indexOf('.chat-tmux-preview__pre::-webkit-scrollbar'));
  check('tmux preview pre is small monospace and clipped',
    pre.includes('font-size: 10px')
    && pre.includes('height: 132px')
    && pre.includes('overflow: hidden')
    && pre.includes('white-space: pre'));
  check('tmux preview placeholder uses dim', styles.includes('.chat-tmux-preview__pre--placeholder {')
    && styles.includes('color: var(--dim)'));
  check('tmux preview has no separate Open terminal button class', !styles.includes('.chat-tmux-preview__open {'));

}

{
  check('mobile keeps runners in the projects pane', styles.includes('.chat-app--mobile-projects .chat-runner-main')
    && !styles.includes('chat-app--mobile-runners')
    && styles.includes('@media (max-width: 719px)'));
}

console.log(`chatRail.test: ${assertions} assertions, ${failures.length} failure(s)`);
for (const failure of failures) console.error(`  FAIL ${failure}`);
process.exit(failures.length > 0 ? 1 : 0);
