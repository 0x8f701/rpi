#!/usr/bin/env node
// Projects-first /chat layout contract: left Projects rail, selected Project
// conversation, Project header with Add runner, direct vs Broadcast target
// selector, right Files/Jobs context pane, gear settings, and
// responsive mobile panes. Also audits the user-visible language: zero banned
// terms in the built surface and exact required labels.
//
// Run through `npm run build`. Exit codes: 0 = every assertion held; 1 = a
// regression.
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  boundedVisibleRunnerError,
  ChatProjectForm,
  CHAT_SELECTED_PROJECT_STORAGE_PREFIX,
  loadSelectedProjectId,
  MAX_RUNNER_ERROR_CHARS,
  saveSelectedProjectId,
  selectedProjectStorageKey,
  selectStoredProject,
} from '../src/chat/ChatApp.tsx';
import { ChatEmptyState } from '../src/chat/ChatEmptyState.tsx';
import {
  CHAT_INVITE_EMPTY,
  CHAT_INVITE_LAN_HINT,
  CHAT_LAN_NONE,
  CHAT_PROJECT_ATTACH_RUNNERS,
  CHAT_PROJECT_EMPTY_BODY,
  CHAT_PROJECT_EMPTY_TITLE,
  CHAT_PROJECT_FORM_HINT,
  CHAT_RAIL_EMPTY,
  CHAT_RUNNERS_EMPTY,
  CHAT_RUNNER_MAIN_EMPTY,
  CHAT_WORKSPACE_EMPTY_BODY,
  CHAT_WORKSPACE_EMPTY_TITLE,
} from '../src/chat/chatEmpty.ts';
import { RUNNER_UNREACHABLE_MESSAGE } from '../src/chat/auth.ts';
import { runnerDisplayName } from '../src/chat/runners.ts';
import {
  HostConnectionChip,
  HOST_RECONNECTING_STATUS,
  HOST_TOKEN_REQUIRED_ERROR,
  hostConnectionChip,
} from '../src/hostConnectionChip.tsx';
import { ReconnectOverlay } from '../src/reconnectOverlay.tsx';
import { RECONNECT_OVERLAY_TEXT, reconnectOverlayVisible } from '../src/reconnect.ts';
import {
  CJK_SHEET_TASK,
  phoneSheetRects,
  rectsIntersect,
  subagentSheetSectionRects,
  subagentSheetTaskShrinks,
} from '../src/subagentSheetLayout.ts';
import {
  CJK_TRANSCRIPT_TOOL,
  adjacentTranscriptRects,
  assistantIndentsThinking,
  outputFrameStealsRawArgs,
  phoneTranscriptAdjacentRects,
  rawArgsInsideCardBorder,
  rawArgsOutsideVisualCard,
  siblingLeftsAligned,
  transcriptBlocksCanOverlap,
} from '../src/transcriptBlockLayout.ts';
import {
  ASCII_PRIORITY_TOKENS,
  PHONE_TABLE_VIEWPORT,
  asciiTokenLineCount,
  cjkTokenLineCount,
  firstColumnKeepsCjkLabel,
  mdTableCellRule,
  mdTableFirstColumnRule,
  mdTableIsScrollWrap,
  phonePriorityColumnContentPx,
  shippedTableCellsKeepAsciiTokens,
  tableCellWrapPolicy,
} from '../src/markdownTableLayout.ts';
import { measurePhoneTableLineRects } from './markdownTableBrowser.ts';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server.browser';
import { ActivityPanel } from '../src/panels/ActivityPanel.tsx';
import {
  activityEntryMatchesPage,
  applyActivityEvent,
  type ActivitySessionSnapshot,
} from '../src/panels/todoActivity.ts';

const failures: string[] = [];
let assertions = 0;

function check(name: string, condition: boolean, detail = ''): void {
  assertions += 1;
  if (!condition) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

const here = dirname(fileURLToPath(import.meta.url));
const app = readFileSync(join(here, '../src/chat/ChatApp.tsx'), 'utf8');
const styles = readFileSync(join(here, '../src/chat/styles.css'), 'utf8');
const broadcast = readFileSync(join(here, '../src/chat/BroadcastConversation.tsx'), 'utf8');
const invite = readFileSync(join(here, '../src/chat/InviteRunnerDialog.tsx'), 'utf8');
const sessionView = readFileSync(join(here, '../src/chat/SessionView.tsx'), 'utf8');
const emptyState = readFileSync(join(here, '../src/chat/ChatEmptyState.tsx'), 'utf8');
const emptyCopy = readFileSync(join(here, '../src/chat/chatEmpty.ts'), 'utf8');
const team = readFileSync(join(here, '../src/chat/team.ts'), 'utf8');
const runners = readFileSync(join(here, '../src/chat/runners.ts'), 'utf8');
const webApp = readFileSync(join(here, '../src/App.tsx'), 'utf8');
const webStyles = readFileSync(join(here, '../src/styles.css'), 'utf8');
const subagentsPanel = readFileSync(join(here, '../src/panels/SubagentsPanel.tsx'), 'utf8');

// ---- Projects-first shell ----
{
  check('single persistent main shell', app.includes('<main ref={chatRootRef} className={shellClass} id="chat-projects">'));
  check('left rail stacks Runners then Projects', app.includes('id="chat-runners-rail"')
    && app.includes('aria-label="Runners"')
    && app.includes('className="chat-projects-rail"')
    && app.includes('aria-label="Runners and Projects"')
    && app.includes('>Runners</span>')
    && app.includes('>Projects</span>'));
  check('rail head labels Projects', app.includes('>Projects</span>'));
  check('New project button exists', app.includes('id="chat-project-add-btn"') && app.includes('aria-label="New project"') && app.includes('New project'));
  check('project rows carry data-project-id and selection', app.includes('data-project-id={project.id}')
    && app.includes('data-selected={railFocus?.kind === \'project\' && railFocus.id === project.id}'));
  check('project rows are ordered by createdAt then id', app.includes('left.createdAt - right.createdAt || left.id.localeCompare(right.id)'));
  check('empty rail copy is exact', CHAT_RAIL_EMPTY === 'Create a project to open a channel.'
    && app.includes('{CHAT_RAIL_EMPTY}'));
  check('runners rail add button and empty copy', app.includes('id="chat-runner-add-btn"')
    && app.includes('aria-label="Add runner"')
    && CHAT_RUNNERS_EMPTY === 'Add a runner to open its terminals.'
    && app.includes('{CHAT_RUNNERS_EMPTY}')
    && app.includes('className="chat-runner-row"'));
  check('ChatRailFocus persists runner and project selection', app.includes('loadChatRailFocus')
    && app.includes('saveChatRailFocus(storage, pageAuthority, { kind: \'runner\', id: runnerId })')
    && app.includes('saveChatRailFocus(storage, pageAuthority, { kind: \'project\', id: projectId })')
    && app.includes('useState<ChatRailFocus | null>(null)')
    && app.includes('const selectRunner = (runnerId: string) =>'));
  check('workspace bar spans the shell', app.includes('id="chat-workspace-bar"')
    && (() => {
      const block = styles.slice(styles.indexOf('.chat-app--projects {'), styles.indexOf('.chat-workspace-bar {'));
      return block.includes('grid-template-rows: auto minmax(0, 1fr)')
        && block.includes('height: 100%')
        && block.includes('max-height: 100%')
        && block.includes('height: 100svh')
        && !block.includes('min-height: 100vh');
    })()
    && /^\.chat-workspace-bar \{[\s\S]*?grid-column:\s*1\s*\/\s*-1;/m.test(styles));
  check('workspace bar uses the /web brand face', app.includes('className="brand"')
    && app.includes('<span className="brand-sub">chat</span>')
    && app.includes('rpi'));
  check('workspace bar uses the /web hamburger menu control', app.includes('id="chat-workspace-menu-btn"')
    && app.includes('className={workspaceMenuOpen ? \'panel-toggle panel-toggle--open\' : \'panel-toggle\'}')
    && app.includes('☰')
    && webApp.includes('id="sidebar-toggle-btn"')
    && webApp.includes('☰')
    && /#sidebar-toggle-btn,\s*#chat-workspace-menu-btn/.test(webStyles)
    && app.includes('aria-label={workspaceMenuOpen ? \'Close menu\' : \'Open menu\'}'));
  check('workspace menu contains Web as a menu item', app.includes('id="chat-workspace-menu"')
    && app.includes('href="/web"')
    && app.includes('aria-label="Open web"')
    && />\s*Web\s*</.test(app)
    && app.includes('{workspaceMenuOpen && (')
    && !app.includes('chat-workspace-bar__web')
    && /\.chat-workspace-menu\s*>\s*:last-child:nth-child\(odd\)\s*\{[^}]*grid-column:\s*1\s*\/\s*-1/.test(styles));
  check('workspace bar stays above the tmux overlay', (() => {
    const barStart = styles.indexOf('.chat-workspace-bar {\n');
    const bar = barStart >= 0 ? styles.slice(barStart, styles.indexOf('}', barStart)) : '';
    const overlayStart = styles.indexOf('.chat-tmux-terminal {\n');
    const overlay = overlayStart >= 0 ? styles.slice(overlayStart, styles.indexOf('}', overlayStart)) : '';
    const menuStart = styles.indexOf('.chat-workspace-menu {\n');
    const menu = menuStart >= 0 ? styles.slice(menuStart, styles.indexOf('}', menuStart)) : '';
    return bar.includes('z-index: 30') && bar.includes('min-height: 44px')
      && overlay.includes('z-index: 20') && overlay.includes('top: 44px') && !overlay.includes('inset: 0')
      && menu.includes('z-index: 31');
  })());
  check('workspace ☰ is 44px at 820 (721px media, not only 480)', (() => {
    const start = webStyles.indexOf('@media (min-width: 721px) {\n  #sidebar-toggle-btn,');
    const block = start >= 0 ? webStyles.slice(start, webStyles.indexOf('}', start + 40)) : '';
    return block.includes('#chat-workspace-menu-btn')
      && block.includes('min-width: 44px')
      && block.includes('min-height: 44px');
  })());
  check('workspace ☰ is 44px on the 720px drawer breakpoint', (() => {
    const start = webStyles.indexOf('@media (max-width: 720px) {\n  .app-layout {');
    const block = start >= 0 ? webStyles.slice(start, webStyles.indexOf('@media (max-width: 480px) {', start)) : '';
    const btn = block.indexOf('#chat-workspace-menu-btn {');
    const rule = btn >= 0 ? block.slice(btn, block.indexOf('}', btn)) : '';
    return rule.includes('min-width: 44px') && rule.includes('min-height: 44px');
  })());
  check('/web shell fills dvh so footer pins at 390/820/1280', (() => {
    const dvh = webStyles.indexOf('@supports (height: 100dvh)');
    const dvhBlock = dvh >= 0 ? webStyles.slice(dvh, webStyles.indexOf('}', dvh + 80)) : '';
    const layoutStart = webStyles.indexOf('.app-layout {\n  flex: 1 1 auto');
    const layout = layoutStart >= 0 ? webStyles.slice(layoutStart, webStyles.indexOf('}', layoutStart)) : '';
    return dvhBlock.includes('html, body, #root') && dvhBlock.includes('height: 100dvh')
      && layout.includes('height: 100%') && layout.includes('max-height: 100%') && layout.includes('min-height: 0')
      && webStyles.includes('html, body {\n  height: 100%;\n  overflow: hidden;');
  })());
  check('Escape closes the workspace ☰ before the tmux overlay trap', (() => {
    const start = app.indexOf('const onKeyDown = (event: KeyboardEvent) =>');
    const body = start >= 0 ? app.slice(start, app.indexOf('document.addEventListener', start)) : '';
    return body.includes('if (workspaceMenuOpen) {')
      && body.indexOf('if (workspaceMenuOpen) {') < body.indexOf('if (tmuxTerminal && !tmuxTerminal.hidden) return;')
      && app.includes("addEventListener('keydown', onKeyDown, true)")
      && app.includes("removeEventListener('keydown', onKeyDown, true)");
  })());
  check('pointer-down outside the workspace ☰ dismisses it', (() => {
    const start = app.indexOf('if (!workspaceMenuOpen) return;');
    const body = start >= 0 ? app.slice(start, app.indexOf('}, [workspaceMenuOpen]);', start)) : '';
    return body.includes('workspaceMenuRef.current')
      && body.includes('root.contains(event.target as Node)')
      && body.includes('setWorkspaceMenuOpen(false)')
      && body.includes("addEventListener('pointerdown', onPointerDown)")
      && body.includes("removeEventListener('pointerdown', onPointerDown)");
  })());
  check('primary chat actions use accent ink like /web send', (() => {
    const start = styles.indexOf('.chat-button--primary {');
    const hover = styles.indexOf('.chat-button--primary:hover');
    if (start < 0 || hover < 0 || hover <= start) return false;
    const block = styles.slice(start, hover);
    return block.includes('color: var(--accent)')
      && !block.includes('background: var(--accent)')
      && !block.includes('color: var(--bg)');
  })());
  check('workspace empty mounts from ChatEmptyState', app.includes('<ChatEmptyState')
    && !app.includes('kind="workspace"')
    && !app.includes('kind="project"')
    && emptyState.includes('id="chat-empty-workspace"')
    && emptyState.includes('id="chat-empty-new-project"')
    && !emptyState.includes('id="chat-empty-add-runner"')
    && !emptyState.includes('onAddRunner'));
  {
    const emptyHtml = renderToStaticMarkup(createElement(ChatEmptyState, { onAction: () => {} }));
    check('LAN Chat empty state has New project and no Add runner button',
      emptyHtml.includes('id="chat-empty-new-project"')
      && emptyHtml.includes('New project')
      && !emptyHtml.includes('id="chat-empty-add-runner"')
      && !/>Add runner</.test(emptyHtml));
  }
  check('workspace empty copy is exact', CHAT_WORKSPACE_EMPTY_TITLE === 'Projects are channels for people and rpi agents'
    && CHAT_WORKSPACE_EMPTY_BODY.includes('Conversations stay on the rpi server.')
    && CHAT_WORKSPACE_EMPTY_BODY.includes('@ a runner')
    && emptyState.includes('{CHAT_WORKSPACE_EMPTY_TITLE}')
    && emptyState.includes('{CHAT_WORKSPACE_EMPTY_BODY}'));
  check('empty project is the LAN lobby', app.includes('id="chat-empty-project"')
    && app.includes('className="chat-lobby"')
    && app.includes('{CHAT_PROJECT_EMPTY_TITLE}')
    && app.includes('{CHAT_PROJECT_EMPTY_BODY}')
    && app.includes('placement="lobby"')
    && CHAT_PROJECT_EMPTY_TITLE === 'Add runners to this channel'
    && CHAT_PROJECT_EMPTY_BODY.includes('Add a listener from Discover on LAN')
    && CHAT_PROJECT_EMPTY_BODY.includes('paste its runner link'));
  check('lobby intro is a div not a header', app.includes('className="chat-lobby__intro"')
    && !app.includes('<header className="chat-lobby__intro"'));
  check('project search filters label and cwd locally', app.includes('project.label.toLowerCase().includes(query)') && app.includes('project.cwd.toLowerCase().includes(query)'));
  {
    const addStart = app.indexOf('const addRunner = async');
    const addAndInviteStart = app.indexOf('const addAndInvite = async');
    const addBody = addStart >= 0 && addAndInviteStart > addStart ? app.slice(addStart, addAndInviteStart) : '';
    const tokenStart = app.indexOf('const saveIndependentToken = async');
    const tokenBody = tokenStart >= 0 && addAndInviteStart > tokenStart ? app.slice(tokenStart, addAndInviteStart) : '';
    check('independent addRunner is registry.add without mapProject', addBody.includes('registry.add(input.label, input.address, input.token)')
      && !addBody.includes('mapProject')
      && app.includes('<AddRunnerForm')
      && app.includes('onAdd={addRunner}'));
    check('independent saveToken updates the registry only', tokenBody.includes('registry.updateToken(runnerId, nextToken)')
      && !tokenBody.includes('mapProject'));
  }
  check('runner focus mounts TmuxPreviewBoard', app.includes('TmuxPreviewBoard')
    && app.includes('id="chat-runner-main"')
    && app.includes('className="chat-runner-main"')
    && app.includes('CHAT_RUNNER_MAIN_EMPTY')
    && CHAT_RUNNER_MAIN_EMPTY === 'This runner is not connected.');
  check('disconnected runner empty state offers Set token and Open to trust',
    app.includes('id="chat-runner-empty"')
    && app.includes('id="chat-runner-empty-set-token"')
    && app.includes('id="chat-runner-empty-trust"')
    && app.includes("focusedRunner.state === 'token_required'")
    && app.includes('runnerTrustHref(focusedRunner, storage)')
    && app.includes('setTokenFormRunnerId(focusedRunner.id)'));
  check('runner delete is a two-step confirm', app.includes('id="chat-runner-delete"')
    && app.includes("aria-label={confirmRunnerDelete ? 'Confirm delete runner' : 'Delete runner'}")
    && app.includes("{confirmRunnerDelete ? 'Delete?' : 'Delete runner'}"));
  check('project delete is a two-step confirm', app.includes("aria-label={confirmProjectDelete ? 'Confirm delete project' : 'Delete project'}")
    && app.includes("{confirmProjectDelete ? 'Delete?' : 'Delete project'}"));
  check('gear session name strips chat- prefix and jumps to the runner', app.includes("member.mapping.sessionName.replace(/^chat-/, '')")
    && app.includes('onClick={() => selectRunner(member.runner.id)}'));
  check('Escape disarms destructive confirms before any form dismissal', (() => {
    const start = app.indexOf('const onKeyDown = (event: KeyboardEvent) =>');
    const body = app.slice(start, app.indexOf('document.addEventListener', start));
    const runner = body.indexOf('if (confirmRunnerDelete)');
    const project = body.indexOf('if (confirmProjectDelete)');
    const form = body.indexOf("if (tokenFormRunnerId)");
    return runner >= 0 && project >= 0 && form > project && runner < form;
  })());
  check('select handlers cross-reset both confirms', app.includes("setConfirmRunnerDelete(false);\n    setConfirmProjectDelete(false);") || (app.split('setConfirmRunnerDelete(false)').length >= 4 && app.split('setConfirmProjectDelete(false)').length >= 4));
  check('successful delete does not toast leftover native sessions', !app.includes('archiveNote')
    && !app.includes('Sessions left on the runner (archive failed)')
    && !app.includes('Sessions left on runners (archive failed)'));
  check('runner page forms have no inline formError', app.includes("id=\"chat-runner-create-session-form\"") && app.includes("id=\"chat-runner-token-form\"") && !app.includes('{formError && <p className="chat-form-error" role="alert">{safeText(formError)}</p>}'));
  check('session toast is the single formError surface and stays visible with forms open', app.includes('const sessionToast = formError || team.channelLogError || mapping.storageError;')
    && app.includes('<ChatSessionToast')
    && app.includes('text={visibleSessionToast}')
    && app.includes('aria-label="Dismiss"')
    && !app.includes('!formOpen && !projectFormOpen && !inviteLobbyOpen && !runnerFormOpen'));
  check('session toast Dismiss hides formError immediately and persistent errors until the text changes', app.includes("const visibleSessionToast = formError !== ''")
    && app.includes("dismissedToast === sessionToast ? '' : sessionToast")
    && app.includes("if (formError !== '')")
    && app.includes('setDismissedToast(sessionToast)'));
  check('header delete reuses the busy-gated removeRunner helper', app.includes('void removeRunner(focusedRunner.id)') && app.includes("disabled={inviteBusyKey === `remove:${focusedRunner.id}`}"));

}

// ---- Runner rail rows, LAN discovery, runner page exclusivity ----
{
  const addForm = readFileSync(join(here, '../src/chat/AddRunnerForm.tsx'), 'utf8');
  check('runner rail rows show only dot + name', app.includes('chat-runner-row__label')
    && !app.includes('chat-runner-row__state'));
  check('runner rail dedupes by identity before paint',
    app.includes('const ordered = sortRunnersOnlineFirst(dedupeRunnersKeepHealthiest(runners));'));
  {
    const ctxStart = app.indexOf('onContextMenu={(event) => {');
    const ctxEnd = ctxStart >= 0 ? app.indexOf('}}', ctxStart) : -1;
    const ctx = ctxStart >= 0 && ctxEnd > ctxStart ? app.slice(ctxStart, ctxEnd) : '';
    const menuStart = styles.indexOf('.chat-runner-context-menu {');
    const menuBlock = menuStart >= 0 ? styles.slice(menuStart, styles.indexOf('}', menuStart)) : '';
    const itemStart = styles.indexOf('.chat-runner-context-menu__item {');
    const itemBlock = itemStart >= 0 ? styles.slice(itemStart, styles.indexOf('}', itemStart)) : '';
    check('runner right-click opens a context menu instead of /web',
      ctx.includes('openRunnerContextMenuFromEvent')
      && !ctx.includes('window.open')
      && app.includes('id="chat-runner-context-menu"')
      && app.includes('id={`chat-runner-menu-${action.id}`}')
      && app.includes('RUNNER_CONTEXT_ACTIONS.map')
      && app.includes('scheduleRunnerLongPress')
      && app.includes('runnerLongPressShouldArm'));
    check('runner context menu is a fixed square overlay with hover',
      menuBlock.includes('position: fixed')
      && menuBlock.includes('z-index: 24')
      && !menuBlock.includes('border-radius')
      && itemBlock.includes('min-height: 44px')
      && styles.includes('.chat-runner-context-menu__item:hover')
      && styles.includes('.chat-runner-context-menu__item:focus-visible'));
  }
  check('switching runners closes the tmux overlay', (() => {
    const start = app.indexOf('const selectRunner = (runnerId: string) =>');
    const end = app.indexOf('const openRunnerAdd = () =>', start);
    return start >= 0 && end > start && app.slice(start, end).includes('setTmuxTerminal(null);');
  })());
  check('phone Add runner overlay stays on the rail', (() => {
    const start = app.indexOf('const openRunnerAdd = () =>');
    const end = app.indexOf('const selectTarget', start);
    return start >= 0 && end > start && !app.slice(start, end).includes("setMobilePane('conversation')");
  })());
  check('Add runner overlay is hosted on the chat shell',
    app.includes('{runnerFormOpen && runners.length > 0 && independentAddForm}')
    && app.indexOf('{runnerFormOpen && runners.length > 0 && independentAddForm}') < app.indexOf('id="chat-projects-rail"')
    && app.indexOf('{runnerFormOpen && runners.length > 0 && independentAddForm}') === app.lastIndexOf('{runnerFormOpen && runners.length > 0 && independentAddForm}'));
  check('discovered LAN candidates live in the runners rail', app.includes('id="chat-rail-lan"')
    && app.includes('aria-label="Discovered on LAN"')
    && app.includes('lanRailCandidates')
    && app.includes('<details')
    && app.includes('open={lanRailOpen}'));
  {
    const railStart = styles.indexOf('.chat-runners-rail,\n.chat-rail-section,\n.chat-projects-section {');
    const railBlock = railStart >= 0 ? styles.slice(railStart, styles.indexOf('}', railStart)) : '';
    const projectsStart = styles.indexOf('.chat-projects-section {\n  position: relative;');
    const projectsBlock = projectsStart >= 0 ? styles.slice(projectsStart, styles.indexOf('}', projectsStart)) : '';
    const railHostStart = styles.indexOf('.chat-projects-rail {');
    const railHost = railHostStart >= 0 ? styles.slice(railHostStart, styles.indexOf('}', railHostStart)) : '';
    check('runners rail is an independent flexbox that scrolls its list', railBlock.includes('display: flex')
      && railBlock.includes('flex-direction: column')
      && railBlock.includes('overflow: hidden')
      && railBlock.includes('flex: 1 1 0')
      && railBlock.includes('min-height: 0'));
    check('Projects live in a sibling section so a runner row cannot cover them', app.includes('className="chat-projects-section"')
      && railBlock.includes('.chat-projects-section')
      && projectsBlock.includes('position: relative')
      && projectsBlock.includes('z-index: 1')
      && projectsBlock.includes('background: var(--bg)')
      && !projectsBlock.includes('max-height: 36vh')
      && !railBlock.includes('max-height: 36vh'));
    check('the projects rail fills its grid cell', railHost.includes('height: 100%')
      && railHost.includes('min-height: 0')
      && railHost.includes('overflow: hidden'));
    const lanStart = styles.indexOf('.chat-rail-lan {');
    const lanBlock = lanStart >= 0 ? styles.slice(lanStart, styles.indexOf('}', lanStart)) : '';
    const lanListStart = styles.indexOf('.chat-rail-lan__list {');
    const lanListBlock = lanListStart >= 0 ? styles.slice(lanListStart, styles.indexOf('}', lanListStart)) : '';
    check('LAN list pads and scrolls so the last row does not kiss Projects', lanBlock.includes('padding-bottom: 8px')
      && lanBlock.includes('overflow: hidden')
      && app.includes('className="chat-rail-lan__list"'));
    check('open LAN rows scroll inside a 28vh list; closed details keep native hide', lanListBlock.includes('display: grid')
      && lanListBlock.includes('overflow-y: auto')
      && lanListBlock.includes('padding-bottom: 8px')
      && lanListBlock.includes('max-height: calc(28vh - 40px)')
      && styles.includes('.chat-rail-lan[open] {')
      && /max-height:\s*28vh/.test(styles.slice(styles.indexOf('.chat-rail-lan[open] {'), styles.indexOf('}', styles.indexOf('.chat-rail-lan[open] {')))));
    const listStart = styles.indexOf('.chat-projects-list,\n.chat-runners-list {');
    const listBlock = listStart >= 0 ? styles.slice(listStart, styles.indexOf('}', listStart)) : '';
    check('both rail lists are flex:1 1 0 min-height 0 overflow-y auto',
      listBlock.includes('flex: 1 1 0')
      && listBlock.includes('min-height: 0')
      && listBlock.includes('overflow-y: auto'));
    check('runners rail is a flex column not a 4-row grid',
      railBlock.includes('display: flex')
      && railBlock.includes('flex-direction: column')
      && !railBlock.includes('display: grid')
      && !railBlock.includes('grid-template-rows'));
    check('projects and runners each take half the rail',
      railBlock.includes('flex: 1 1 0')
      && railBlock.includes('min-height: 0')
      && railBlock.includes('overflow: hidden')
      && !styles.includes('max-height: 36vh'));
    const rowStart = styles.indexOf('.chat-project-row,\n.chat-runner-row {');
    const rowBlock = rowStart >= 0 ? styles.slice(rowStart, styles.indexOf('}', rowStart)) : '';
    check('base runner rows are 36px tall', rowBlock.includes('min-height: 36px'));
    const phoneRailMq = styles.slice(
      styles.indexOf('@media (max-width: 719px) {'),
      styles.indexOf('\n.chat-tmux-terminal__close'),
    );
    const phoneRow = phoneRailMq.slice(
      phoneRailMq.indexOf('.chat-runner-row,'),
      phoneRailMq.indexOf('}', phoneRailMq.indexOf('.chat-runner-row,')),
    );
    check('mobile runner rows stay 36px tall', phoneRow.includes('min-height: 36px'));
    function railProjectsGeometry(opts: {
      viewportHeight: number;
      headerHeight: number;
      railPad: number;
      sectionGap: number;
      runnerRows: number;
      rowHeight: number;
      mode: 'half' | 'content';
    }): { runnersHeight: number; projectsTop: number; railHeight: number; pushed: boolean } {
      const railHeight = opts.viewportHeight - opts.headerHeight;
      const inner = railHeight - opts.railPad * 2;
      if (opts.mode === 'half') {
        const each = (inner - opts.sectionGap) / 2;
        return {
          runnersHeight: each,
          projectsTop: opts.headerHeight + opts.railPad + each + opts.sectionGap,
          railHeight,
          pushed: false,
        };
      }
      const runnersUsed = opts.runnerRows * opts.rowHeight;
      const projectsTop = opts.headerHeight + opts.railPad + runnersUsed + opts.sectionGap;
      return {
        runnersHeight: runnersUsed,
        projectsTop,
        railHeight,
        pushed: projectsTop >= opts.viewportHeight,
      };
    }
    const desktop = { viewportWidth: 1280, viewportHeight: 800, headerHeight: 48, railPad: 8, sectionGap: 6, runnerRows: 24, rowHeight: 36 };
    const phone = { viewportWidth: 390, viewportHeight: 844, headerHeight: 48, railPad: 8, sectionGap: 6, runnerRows: 24, rowHeight: 36 };
    const desktopContent = railProjectsGeometry({ ...desktop, mode: 'content' });
    const desktopHalf = railProjectsGeometry({ ...desktop, mode: 'half' });
    const phoneContent = railProjectsGeometry({ ...phone, mode: 'content' });
    const phoneHalf = railProjectsGeometry({ ...phone, mode: 'half' });
    check('1280 content-sized runners push Projects below the fold',
      desktop.viewportWidth === 1280 && desktop.viewportHeight === 800 && desktopContent.pushed
      && desktopContent.projectsTop > 800,
      `${desktopContent.projectsTop}`);
    check('1280 half-flex runners keep Projects inside the rail',
      !desktopHalf.pushed
      && desktopHalf.runnersHeight === desktopHalf.railHeight / 2 - desktop.railPad - desktop.sectionGap / 2
      && desktopHalf.projectsTop < 800
      && desktopHalf.runnersHeight === (desktopHalf.railHeight - desktop.railPad * 2 - desktop.sectionGap) / 2,
      `${desktopHalf.runnersHeight} ${desktopHalf.projectsTop}`);
    check('390 content-sized runners push Projects below the fold',
      phone.viewportWidth === 390 && phone.viewportHeight === 844 && phoneContent.pushed
      && phoneContent.projectsTop > 844,
      `${phoneContent.projectsTop}`);
    check('390 half-flex runners keep Projects inside the rail',
      !phoneHalf.pushed
      && phoneHalf.projectsTop < 844
      && phoneHalf.runnersHeight === (phoneHalf.railHeight - phone.railPad * 2 - phone.sectionGap) / 2,
      `${phoneHalf.runnersHeight} ${phoneHalf.projectsTop}`);
    check('half-flex is the shipped rail contract at 1280 and 390',
      railBlock.includes('flex: 1 1 0')
      && listBlock.includes('overflow-y: auto')
      && /\.chat-runners-rail,\s*\.chat-rail-section,\s*\.chat-projects-section \{[^}]*flex:\s*1 1 0/.test(phoneRailMq));
  }
  check('AddRunnerForm is the form only', !addForm.includes('Discovered on LAN')
    && !addForm.includes('candidates')
    && !addForm.includes('CertTrustLink')
    && addForm.includes('parseRunnerInviteLink')
    && !addForm.includes('chat-form-error')
    && addForm.includes('onError'));
  check('InviteRunnerDialog has no inline form error', !invite.includes('chat-form-error') && invite.includes('onError('));
  check('cert trust is typed connection state, not error prose', (() => {
    const start = app.indexOf('function shouldOfferCertTrust');
    const body = app.slice(start, app.indexOf('function runnerTrustHref'));
    return start >= 0
      && !body.includes('error.includes')
      && body.includes("case 'token_required':")
      && body.includes("case 'offline':")
      && body.includes("case 'reconnecting':");
  })());
  check('runner header offers Open to trust for unreachable or token-gated https runners', app.includes('shouldOfferCertTrust(focusedRunner)')
    && app.includes('runnerTrustHref(focusedRunner, storage)')
    && app.includes('target="_blank"')
    && app.includes('rel="noopener noreferrer"'));
  check('runner page and project page are mutually exclusive', app.includes('{!focusedRunner && (')
    && app.includes('{focusedRunner && (')
    && app.indexOf('{!focusedRunner && (') < app.indexOf('{focusedRunner && ('));
  check('rail LAN row and preview delete styles exist', styles.includes('.chat-rail-lan__row')
    && styles.includes('.chat-tmux-preview__delete')
    && /\.chat-tmux__pre \{[^}]*JetBrainsMono Nerd Font/s.test(styles)
    && /\.chat-tmux-preview__pre \{[^}]*JetBrainsMono Nerd Font/s.test(styles)
    && /\.chat-runner-form--project \{[^}]*margin-bottom:\s*12px/s.test(styles));
}

// ---- Project header: identity, Add runner, gear ----
{
  check('header shows the project label', app.includes('{safeText(selectedProject.label)}')
    && app.includes('chat-channel-hash'));
  check('header shows runner count without absolute directory', app.includes("{activeMappings.length} runner{activeMappings.length === 1 ? '' : 's'}") && !app.includes('title={selectedProject.cwd}'));
  check('header shows an aggregate connection state', app.includes('connectionSummary'));
  check('header Invite on an empty project focuses the lobby address', app.includes("document.getElementById('chat-runner-address')?.focus()")
    && app.includes('placement="overlay"'));
  check('gear settings button exists', app.includes('id="chat-project-gear-btn"') && app.includes('aria-label="Project settings"'));
  const headerActions = app.slice(
    app.indexOf('className="chat-project-header__actions"'),
    app.indexOf('className="chat-target-select"'),
  );
  const addAt = headerActions.indexOf('id="chat-invite-runner-btn"');
  const gearAt = headerActions.indexOf('id="chat-project-gear-btn"');
  const detailsAt = headerActions.indexOf('Details');
  check('project header gear is last, not between Add runner and Details',
    addAt >= 0 && gearAt >= 0 && detailsAt >= 0
    && gearAt > addAt && gearAt > detailsAt
    && detailsAt < addAt);
  check('gear menu is project-only Delete project', app.includes('id="chat-gear-delete-project"')
    && app.includes('aria-label="Project settings"')
    && !app.includes('chat-gear-set-token')
    && !app.includes('chat-gear-delete-runner')
    && !app.includes('chat-gear-unmap-runner')
    && !app.includes('chat-gear-reconnect')
    && !app.includes('registry.reconnect')
    && !app.includes('<TmuxPanel'));
  check('gear heading is the project label', app.includes('<h2>{safeText(selectedProject.label)}</h2>')
    && app.includes('aria-label="Runner settings"'));


}

// ---- New project modal ----
{
  check('rail add button is a chat-icon-button with visible +', app.includes('className="chat-icon-button"')
    && app.includes('id="chat-project-add-btn"')
    && app.includes('aria-label="New project"')
    && app.includes('title="New project"')
    && app.includes('>+'));
  check('rail add button carries the opener ref', app.includes('ref={projectAddButtonRef}'));
  check('project form renders outside the rail in a backdrop', app.includes('chat-project-modal-backdrop')
    && app.includes('id="chat-project-form"')
    && !app.includes('chat-runner-form--project" id="chat-project-form"'));
  check('project modal is a dialog with aria-modal and labelled title', app.includes('role="dialog"')
    && app.includes('aria-modal="true"')
    && app.includes('aria-labelledby="chat-project-form-title"')
    && app.includes('aria-describedby="chat-project-form-hint"')
    && app.includes('id="chat-project-form-title"')
    && app.includes('>New project</h2>'));
  check('project modal autofocuses the Label input', app.includes('id="chat-project-label"') && app.includes('autoFocus'));
  check('project modal close is a quiet × and one focus-restoring close helper', app.includes('aria-label="Close"')
    && app.includes('>×</button>')
    && app.includes('const closeProjectForm = useCallback')
    && app.includes('requestAnimationFrame(() => projectAddButtonRef.current?.focus())')
    && app.includes('onClick={closeProjectForm}'));
  check('project modal Path field uses the compact label', app.includes('ariaLabel="Path"') && !app.includes('ariaLabel="Absolute directory"'));
  check('Escape closes the project modal through the shared helper', app.includes("if (projectFormOpen) {")
    && app.includes('closeProjectForm();'));
  check('duplicate error surfaces in the session toast, not the modal', app.includes('const sessionToast = formError || team.channelLogError || mapping.storageError;')
    && app.includes('<ChatSessionToast')
    && app.includes('text={visibleSessionToast}')
    && !app.includes('{formError && <p className="chat-form-error" role="alert">{safeText(formError)}</p>}'));
  check('successful create selects the new project then closes', app.includes('setPendingProjectSelection(project.id)')
    && app.includes('teamStore.selectProject(pendingProjectSelection)')
    && app.includes('setPendingProjectSelection(null)'));
  check('project submit label is Confirm', app.includes('id="chat-project-add"')
    && /id="chat-project-add"[\s\S]*?\n\s*Confirm\n/.test(app)
    && !app.includes('Add project'));
  check('project form lists already-added runners', app.includes('id="chat-project-runners"')
    && app.includes('<legend>{CHAT_PROJECT_ATTACH_RUNNERS}</legend>')
    && app.includes('onToggleRunner')
    && app.includes('mappingStore.mapProject(project.id, runnerId)'));
  {
    const formHtml = renderToStaticMarkup(createElement(ChatProjectForm, {
      projectLabel: '',
      setProjectLabel: () => {},
      projectCwd: '',
      setProjectCwd: () => {},
      addProject: (event) => { event.preventDefault(); },
      closeProjectForm: () => {},
      sendProjectPathComplete: async () => null,
      isProjectBusy: false,
      storageError: '',
      runners: [{ id: 'r1', label: 'psy', connected: true }],
      selectedRunnerIds: [],
    }));
    check('new project dialog hints under the title and labels the runner list',
      formHtml.includes('id="chat-project-form-hint"')
      && formHtml.includes(CHAT_PROJECT_FORM_HINT)
      && formHtml.includes(CHAT_PROJECT_ATTACH_RUNNERS)
      && formHtml.includes('<legend>')
      && formHtml.includes('psy')
      && CHAT_PROJECT_ATTACH_RUNNERS === 'Attach runners to this project');
    check('new project dialog styles the hint and runner-list legend',
      styles.includes('.chat-project-modal__hint')
      && styles.includes('#chat-project-runners legend'));
  }
  const backdropBlock = styles.slice(styles.indexOf('.chat-project-modal-backdrop'), styles.indexOf('}', styles.indexOf('.chat-project-modal-backdrop')));
  check('modal backdrop is centered over the shell', styles.includes('.chat-project-modal-backdrop')
    && styles.includes('position: fixed')
    && styles.includes('place-items: center'));
  const shell = readFileSync(join(here, '../src/styles.css'), 'utf8');
  const backdropZ = Number(backdropBlock.match(/z-index:\s*(\d+)/)?.[1]);
  const panelZ = Number(shell.match(/\.panel,[^}]*?z-index:\s*(\d+)/s)?.[1]);
  check('project modal backdrop keeps its own 30 layer above chat surfaces', backdropZ === 30);
  check('project modal backdrop stays BELOW the App-side panels and modals', backdropZ < panelZ && panelZ === 46);
  check('invite runner overlay and gear menu stack at z-index 16', styles.includes('.chat-invite--overlay,')
    && styles.includes('.chat-gear-menu {')
    && styles.includes('z-index: 16'));
  check('backdrop click outside the dialog uses the shared close helper', app.includes('if (event.target === event.currentTarget)')
    && app.includes('closeProjectForm();'));
  check('clicking inside the dialog does not close', app.includes('onMouseDown={(event) => event.stopPropagation()}'));
  const modalBlock = styles.slice(styles.indexOf('.chat-project-modal {'), styles.indexOf('}', styles.indexOf('.chat-project-modal {')));
  check('project modal does not clip PathAutocomplete', modalBlock.includes('overflow: visible')
    && !modalBlock.includes('overflow: auto'));
  check('project modal backdrop may scroll', backdropBlock.includes('overflow: auto'));
  check('project modal PathAutocomplete fills the field', styles.includes('.chat-project-modal .path-autocomplete {')
    && /display:\s*block/.test(styles.slice(styles.indexOf('.chat-project-modal .path-autocomplete {'), styles.indexOf('}', styles.indexOf('.chat-project-modal .path-autocomplete {'))))
    && /width:\s*100%/.test(styles.slice(styles.indexOf('.chat-project-modal .path-autocomplete {'), styles.indexOf('}', styles.indexOf('.chat-project-modal .path-autocomplete {')))));
  const autocompleteMenu = styles.slice(styles.indexOf('.chat-project-modal .path-autocomplete__menu {'), styles.indexOf('}', styles.indexOf('.chat-project-modal .path-autocomplete__menu {')));
  check('project modal PathAutocomplete menu stacks above the dialog', autocompleteMenu.includes('z-index: 40')
    && autocompleteMenu.includes('max-height: min(240px, 50vh)'));
}

// ---- Target selector: direct runner chips + Broadcast ----
{
  check('target selector is labelled', app.includes('className="chat-target-select" aria-label="Conversation target"'));
  check('runner chips carry data-runner-id and active state', app.includes('data-runner-id={runner.id}') && app.includes('data-active={selected}'));
  check('conversation chips stay visible while mapping is deleting', app.includes("candidate.state === 'active' || candidate.state === 'deleting' || candidate.state === 'rollback_pending'")
    && app.includes('{removing && <span>{mappingSetupLabel(mappingRow.state)}</span>}')
    && app.includes('disabled={removing}'));

  check('Channel is the group conversation option', app.includes('id="chat-channel-target"')
    && app.includes('selectTarget({ kind: \'broadcast\' })')
    && app.includes('Channel'));
  check('zero active mappings shows the exact invite copy', CHAT_INVITE_EMPTY === 'Add a runner to join this channel.'
    && app.includes('CHAT_INVITE_EMPTY')
    && team.includes('throw new Error(CHAT_INVITE_EMPTY)'));
  check('direct SessionView mounts on the exact mapped session', app.includes('key={JSON.stringify([activeSession.runner.id, activeSession.mapping.sessionId])}')
    && app.includes('sessionId={activeSession.mapping.sessionId}'));
  check('direct SessionView receives injected events', app.includes('events={sessionEvents}'));
  check('Broadcast composer lock copy is exact', app.includes('This runner is responding to the current Broadcast.'));
}

// ---- Offline runner error (BUG-7) ----
{
  check('selected runner error renders compactly below the header', app.includes('className="chat-runner-error"')
    && app.includes('role="status"')
    && app.includes('{selectedRunnerError}'));
  check('runner chips expose bounded error via title and description', app.includes('title={boundedVisibleRunnerError(runner.error) || undefined}')
    && app.includes('aria-description={boundedVisibleRunnerError(runner.error) || undefined}'));
  check('inline error is gated on nonempty text', app.includes('selectedRunnerError !== \'\' && conversationTarget !== null'));
  check('cert-reach selected runner shows Open to trust instead of the header wall', app.includes('shouldOfferCertTrust(conversationTarget.runner)')
    && app.includes('runnerTrustHref(conversationTarget.runner, storage)'));
  check('error helper redacts secrets and paths', boundedVisibleRunnerError('token=sensitive-marker-value directory /workspace/secret failed') === '[REDACTED] directory [path] failed');
  check('empty runner error stays empty', boundedVisibleRunnerError('') === '' && boundedVisibleRunnerError('   ') === '');
  check('long runner error is bounded', boundedVisibleRunnerError('x'.repeat(MAX_RUNNER_ERROR_CHARS + 40)).length === MAX_RUNNER_ERROR_CHARS);
}

// ---- Selected project persistence (BUG-9) ----
{
  const authority = '192.0.2.15:8765';
  const other = '127.0.0.1:8765';
  const key = selectedProjectStorageKey(authority);
  check('selected project uses one versioned per-authority key', key === `${CHAT_SELECTED_PROJECT_STORAGE_PREFIX}${encodeURIComponent(authority)}`);
  check('authorities never share a selected-project key', selectedProjectStorageKey(authority) !== selectedProjectStorageKey(other));
  const store: Record<string, string> = {};
  const memory = {
    getItem: (name: string) => (name in store ? store[name] : null),
    setItem: (name: string, value: string) => { store[name] = value; },
    removeItem: (name: string) => { delete store[name]; },
  };
  saveSelectedProjectId(memory, authority, 'proj-b');
  check('save writes only the project id', memory.getItem(key) === 'proj-b');
  check('other authority stays empty', loadSelectedProjectId(memory, other) === '');
  const projects = [
    { id: 'proj-a', label: 'A', cwd: '/a', createdAt: 1, updatedAt: 1 },
    { id: 'proj-b', label: 'B', cwd: '/b', createdAt: 2, updatedAt: 2 },
  ];
  check('restore uses saved project when it still exists', selectStoredProject(projects, 'proj-b')?.id === 'proj-b');
  check('missing saved project falls back to first', selectStoredProject(projects, 'gone')?.id === 'proj-a');
  check('empty catalog restores nothing', selectStoredProject([], 'proj-b') === null);
  const throwing = {
    getItem: () => { throw new Error('blocked'); },
    setItem: () => { throw new Error('quota'); },
    removeItem: () => { throw new Error('quota'); },
  };
  check('unavailable storage load is nonfatal', loadSelectedProjectId(throwing, authority) === '');
  let threw = false;
  try { saveSelectedProjectId(throwing, authority, 'proj-b'); } catch { threw = true; }
  check('unavailable storage save is nonfatal', !threw);
  check('corrupt path-like values are rejected', loadSelectedProjectId({
    getItem: () => '/tmp/project',
    setItem: () => {},
    removeItem: () => {},
  }, authority) === '');
  check('ChatApp restores from the page authority key', app.includes('loadSelectedProjectId(storage, pageAuthority)')
    && app.includes('saveSelectedProjectId(storage, pageAuthority, selectedProjectId)')
    && app.includes('saveSelectedProjectId(storage, pageAuthority, pendingProjectSelection)'));
  check('persistence never stores cwd or session ids', !app.includes('saveSelectedProjectId(storage, pageAuthority, selectedProject.cwd)')
    && !app.includes('saveSelectedProjectId(storage, pageAuthority, mapping.sessionId)')
    && !app.includes('saveSelectedProjectId(storage, pageAuthority, activeSession.mapping.sessionId)'));
  check('channel log restores from the same page authority', app.includes('pageAuthority: pageHostAuthority()')
    && team.includes('loadChannelLogs(this.storage, this.pageAuthority)')
    && team.includes('saveChannelLogs(this.storage, this.pageAuthority, logs)'));
}

// ---- Broadcast conversation ----
{
  check('broadcast empty copy is exact', broadcast.includes('Send one message to the agent team.'));
  check('channel composer has an @mention picker', broadcast.includes('id="chat-channel-mentions"')
    && broadcast.includes('insertMentionToken')
    && broadcast.includes('resolveChannelRecipients')
    && broadcast.includes('@ a runner to assign work'));
  check('broadcast group carries the run id', broadcast.includes('data-run-id={run.id}'));
  check('broadcast cards carry runner/session/turn provenance', broadcast.includes('data-runner-id={card.runnerId}')
    && broadcast.includes('data-session-id={card.sessionId}')
    && broadcast.includes('data-turn-id={attempt.turnId ?? \'\'}'));
  check('broadcast card key is run+mapping', broadcast.includes('key={JSON.stringify([run.id, card.mappingId])}'));
  check('broadcast transcript matches SessionView: no working line or run summary header', !broadcast.includes('workingLine')
    && !broadcast.includes('cardSummary')
    && !broadcast.includes('Cancel remaining')
    && !broadcast.includes('Retry failed')
    && !broadcast.includes('Sending to')
    && !broadcast.includes('recipientLine')
    && !broadcast.includes('chat-broadcast__recipients'));
  check('broadcast per-card Cancel/Retry/View conversation', broadcast.includes('View conversation')
    && broadcast.includes('onCancelTarget(run.id, card.mappingId)')
    && broadcast.includes('onRetryTarget(run.id, card.mappingId)'));
  check('broadcast composer uses the shared session composer row', broadcast.includes('className="chat-session__composer-row"'));
  check('channel send wakes with a per-runner brief', team.includes('this.wakeBriefFor(reference.projectId, card.mappingId)')
    && team.includes('composeWakeBrief')
    && !team.includes('message: run.prompt'));
  check('channel stream renders the shared log not the raw prompt', broadcast.includes('messages.map((message) =>')
    && broadcast.includes("message.kind === 'human'")
    && !broadcast.includes('{safeText(run.prompt)}')
    && app.includes('messages={channel?.channelMessages ?? []}'));
  check('live cards hide succeeded replies already in the log', broadcast.includes('{liveCards(run).map((card) =>'));
  check('broadcast never imports the registry or event registry', !broadcast.includes('RunnerRegistry') && !broadcast.includes('subscribeRunnerEvents') && !broadcast.includes('WebSocket'));
}

// ---- Invite runner dialog ----
{
  check('invite overlay is a modal section', invite.includes("placement === 'overlay'")
    && invite.includes('role={overlay ? \'dialog\' : undefined}')
    && invite.includes('id="chat-invite"'));
  check('invite overlay uses Add runner title', invite.includes('Add runner ·'));
  check('empty project lobby is not a modal', app.includes('placement="lobby"')
    && invite.includes('role={overlay ? \'dialog\' : undefined}')
    && invite.includes("'chat-empty-project-title'")
    && app.includes('id="chat-empty-project-title"')
    && styles.includes('.chat-invite--lobby')
    && styles.includes('.chat-lobby'));
  check('invite dialog drops token-target state when the project changes', invite.includes('}, [project.id]);')
    && app.includes('key={`${inviteFields.project.id}:lobby`}')
    && app.includes('key={`${inviteFields.project.id}:overlay`}'));
  check('invite dialog shows LAN candidates', invite.includes('Discovered on LAN')
    && invite.includes('{CHAT_INVITE_LAN_HINT}')
    && CHAT_INVITE_LAN_HINT.includes('Add one to join this channel')
    && invite.includes('{CHAT_LAN_NONE}')
    && CHAT_LAN_NONE === 'No runners discovered on LAN yet.');
  check('invite candidate states cover online/offline/connecting/token_required/invited', invite.includes("'online'") && invite.includes("'offline'") && invite.includes("'connecting'") && invite.includes("'token_required'") && invite.includes("'invited'"));
  check('invite dialog has Add runner form', invite.includes('<h3>Add runner</h3>')
    && invite.includes('Runner link')
    && invite.includes('id="chat-invite-form"')
    && invite.includes('id="chat-runner-address"')
    && invite.includes('/chat#token')
    && invite.includes('parseRunnerInviteLink'));
  check('invite form keeps fields unless add succeeds', invite.includes('const added = await onAddAndInvite({')
    && invite.includes('if (!added) return')
    && invite.includes("setLabel('')"));
  check('invite token form keeps fields unless save succeeds', invite.includes('if (!saved) return')
    && invite.includes("setToken('')"));
  check('invite callbacks return success to the dialog', invite.includes('Promise<boolean>'));
  check('token input appears only for token-required targets', invite.includes('Token required') && invite.includes('id="chat-runner-token"'));
  check('invite dialog never imports the registry', !invite.includes('RunnerRegistry') && !invite.includes('subscribeRunnerEvents'));
  check('candidate rows carry a bounded runner error line', invite.includes('candidate.error')
    && invite.includes('>{safeText(candidate.error)}')
    && invite.includes('chat-runner-error'));
  check('invited rows never show the row error', invite.includes("candidate.state !== 'invited' && candidate.error"));
  check('token form surfaces the same runner error', invite.includes('tokenTarget.error !== RUNNER_UNREACHABLE_MESSAGE')
    && invite.includes('role="status"'));
  check('saved runner rows offer Remove when offline or token-gated', invite.includes("(candidate.state === 'offline' || candidate.state === 'token_required') && candidate.runnerId !== null")
    && invite.includes("'Remove'")
    && invite.includes('chat-button--danger'));
  check('remove busy copy is Removing', invite.includes("'Removing…'"));
  check('pure discovered offline rows keep the Offline pill', invite.includes("candidate.state === 'offline' && candidate.runnerId === null")
    && invite.includes('chat-added-pill'));
  check('discovered offline LAN rows offer Add without a live /status', invite.includes("candidate.state === 'offline' && candidate.runnerId === null")
    && invite.includes('lanDiscoverAddPayload')
    && invite.includes('onAddAndInvite(lanDiscoverAddPayload(candidate))'));
  check('discovered connecting LAN rows offer Add', invite.includes("candidate.state === 'connecting'")
    && invite.includes('candidate.runnerId === null')
    && invite.includes('onAddAndInvite(lanDiscoverAddPayload(candidate))'));
  check('lanDiscoverAddPayload is defined', invite.includes('function lanDiscoverAddPayload')
    && invite.includes('candidate.codename || candidate.label'));
  {
    const offlineStart = invite.indexOf("candidate.state === 'offline' && candidate.runnerId === null");
    const offlineEnd = invite.indexOf("(candidate.state === 'offline' || candidate.state === 'token_required') && candidate.runnerId !== null");
    const offlineBlock = offlineStart >= 0 && offlineEnd > offlineStart ? invite.slice(offlineStart, offlineEnd) : '';
    check('offline discovered LAN row has Open to trust and Add', offlineBlock.includes('shouldOfferCertTrust')
      && offlineBlock.includes('<CertTrustLink address={candidate.address} />')
      && offlineBlock.includes(': \'Add\'')
      && offlineBlock.includes('onAddAndInvite(lanDiscoverAddPayload(candidate))'));
  }
  {
    const connectingStart = invite.indexOf("{candidate.state === 'connecting' && (");
    const connectingEnd = invite.indexOf('<h3>Add runner</h3>');
    const connectingBlock = connectingStart >= 0 && connectingEnd > connectingStart ? invite.slice(connectingStart, connectingEnd) : '';
    check('connecting discovered LAN row has Add', connectingBlock.includes('candidate.runnerId === null')
      && connectingBlock.includes(': \'Add\'')
      && connectingBlock.includes('onAddAndInvite(lanDiscoverAddPayload(candidate))'));
  }
  check('remove wires through the invite dialog prop', app.includes('onRemoveRunner: removeRunner')
    && app.includes('{...inviteFields}'));
  check('remove cleans mappings first via the mapping store', app.includes('mappingStore.removeRunner(runnerId, () => registry.delete(runnerId))'));
  check('remove carries busy and form-error handling', app.includes('setInviteBusyKey(`remove:${runnerId}`)')
    && app.includes('setFormError(safeText(error instanceof Error ? error.message : String(error)))'));
  check('saved candidates carry the bounded runner error', app.includes('error: boundedVisibleRunnerError(runner.error)'));
  check('discovered unreachable rows surface the cert-reach message', app.includes('RUNNER_UNREACHABLE_MESSAGE')
    && app.includes("typeof reach !== 'string' && !reach.reachable"));
  check('invite LAN cards keep Open to trust and omit the cert-reach wall', invite.includes('candidate.error !== RUNNER_UNREACHABLE_MESSAGE')
    && invite.includes('tokenTarget.error !== RUNNER_UNREACHABLE_MESSAGE'));
  check('https connecting and offline rows offer Open to trust', invite.includes('shouldOfferCertTrust')
    && invite.includes("candidate.state === 'offline' || candidate.state === 'connecting'")
    && invite.includes('CertTrustLink')
    && invite.includes('Open to trust')
    && invite.includes('target="_blank"')
    && invite.includes('rel="noopener noreferrer"')
    && styles.includes('a.chat-button:hover'));
  check('cert-reach copy has no HTML-tag placeholders', !RUNNER_UNREACHABLE_MESSAGE.includes('<')
    && !RUNNER_UNREACHABLE_MESSAGE.includes('>')
    && RUNNER_UNREACHABLE_MESSAGE.includes("/chat page"));
}

// ---- Context panes: Files / Jobs ----
{
  check('context pane tabs are exactly Files/Jobs', app.includes("(['files', 'jobs'] as const)")
    && app.includes("'Files'") && app.includes("'Jobs'")
    && !app.includes("(['team', 'jobs', 'goal', 'files'] as const)")
    && !app.includes("'Team'") && !app.includes("'Goal'"));
  check('Team pane is not mounted', !app.includes('chat-team-member') && !app.includes('chat-context__team'));
  check('Jobs pane uses runnerActivityJobs and teamJobKey', app.includes('runnerActivityJobs(member.session)') && app.includes('teamJobKey({'));
  check('Jobs note uses jobsTrustLabel closed set without last known',
    app.includes("function jobsTrustLabel(aggregate: ProjectAggregate): 'checking' | 'orchestration off' | 'idle'")
    && !app.includes('last known')
    && app.includes("return 'idle';"));
  check('Jobs note maps idle to No live jobs and otherwise live/failed counts',
    app.includes("if (trust === 'checking' || trust === 'orchestration off') return trust;")
    && app.includes("return 'No live jobs';")
    && app.includes('return `${aggregate.activeJobCount} live · ${aggregate.failedJobCount} failed`;')
    && app.includes('{jobsNoteText(teamAggregate)}'));
  check('empty runnerSessions Jobs label is checking', app.includes("if (sessions.length === 0) return 'checking';"));
  check('idle Jobs pane does not stack per-runner No jobs', !app.includes('<p className="chat-team-note">No jobs</p>')
    && app.includes('if (activityJobs.length === 0) return null'));

  check('Goal pane is not mounted', !app.includes('<ChatGoalPanel sendCommand={sendGoalCommand} events={goalEvents} />')
    && !app.includes('ChatGoalPanel'));
  check('context pane defaults to files', app.includes("channel?.contextPane ?? 'files'")
    && !app.includes("channel?.contextPane ?? 'team'"));
  check('Files pane has an explicit owner selector', app.includes('aria-label="Files owner"'));
  check('Files explorer carries the chat toggle id', app.includes('toggleId={CHAT_FILE_EXPLORER_TOGGLE_ID}')
    && app.includes("id={pane === 'files' ? CHAT_FILE_EXPLORER_TOGGLE_ID : undefined}")
    && !app.includes('{!fileExplorerOpen && ('));
  check('Files drawer mounts only while open with an owner', app.includes('{fileExplorerOpen && fileExplorerOwner && fileExplorerMappingId && fileExplorerSessionId && (')
    && app.includes('chat-context__files'));
  check('chat Files uses exclusive tree or file', styles.includes('.chat-app .chat-context .file-explorer__body:not(.file-explorer__body--tree-collapsed) .file-explorer__content-viewport')
    && styles.includes('.chat-app .chat-context .file-explorer__body--tree-collapsed .file-explorer__tree-viewport')
    && styles.includes('display: none'));
  check('chat Files exclusivePanes is true', app.includes('exclusivePanes={true}'));
  check('chat context does not page-scroll the Files pane', styles.includes('.chat-context {')
    && /overflow:\s*hidden/.test(styles.slice(styles.indexOf('.chat-context {'), styles.indexOf('}', styles.indexOf('.chat-context {')))));
  check('Files tab opens the explorer', app.includes("if (pane === 'files') openFileExplorer()")
    && app.includes('applyChatFileExplorerOpen')
    && app.includes("applyDrawerMode(slot, 'open')"));
  check('desktop hides context until a runner is invited', app.includes('hidden={!selectedProject || (mobile ? mobilePane !== \'context\' : compact ? !tabletContextOpen : activeMappings.length === 0)}')
    && styles.includes('.chat-project-body:not(:has(.chat-context:not([hidden])))')
    && styles.includes('.chat-context[hidden]')
    && styles.includes('display: none'));
}
{
  check('compact breakpoint is 1100px', styles.includes('@media (max-width: 1099px)'));
  check('mobile breakpoint is 720px', styles.includes('@media (max-width: 719px)'));
  {
    const toastStart = styles.indexOf('.chat-session-toast {');
    const toastBlock = toastStart >= 0 ? styles.slice(toastStart, styles.indexOf('}', toastStart)) : '';
    check('session toast is bottom-right fixed on all sizes', toastBlock.includes('position: fixed')
      && toastBlock.includes('bottom: 18px')
      && toastBlock.includes('right: 18px')
      && toastBlock.includes('top: auto')
      && toastBlock.includes('display: flex')
      && !toastBlock.includes('top: 18px'));
    check('session toast Dismiss uses the icon-button hover tokens', styles.includes('.chat-session-toast__dismiss')
      && app.includes('chat-icon-button chat-session-toast__dismiss')
      && styles.includes('.chat-icon-button:hover:not(:disabled)'));
    check('session toast has no mobile static override', !/\.chat-session-toast \{\s*position:\s*static/.test(styles));
  }
  check('tablet rail is 230px', styles.includes('grid-template-columns: 230px minmax(0, 1fr)'));
  check('mobile renders one pane at a time via visibility', styles.includes('.chat-app--mobile-projects .chat-project-main')
    && styles.includes('.chat-app--mobile-conversation .chat-projects-rail')
    && styles.includes('.chat-app--mobile-context .chat-projects-rail'));
  check('mobile context fills the viewport', styles.includes('.chat-app--mobile .chat-context') && styles.includes('width: 100%'));
  check('mobile conversation hides the context pane', styles.includes('.chat-app--mobile-conversation .chat-context'));
  check('mobile projects hides the context pane', styles.includes('.chat-app--mobile-projects .chat-context'));
  check('compact context is an overlay drawer', styles.includes('.chat-app--compact .chat-context') && styles.includes('position: absolute'));
  check('ChatApp drives mobilePane from TeamStore', app.includes('team.mobilePane ?? \'projects\'') && app.includes('teamStore.setMobilePane'));
  check('empty catalog on mobile shows the workspace hero', app.includes('mobile && projects.length === 0')
    && app.includes("? 'conversation'"));
  check('ChatApp has compact project Back and Details on mobile', app.includes("teamStore.setMobilePane('projects')") && app.includes('Details')
    && app.includes('id="chat-mobile-back-projects"')
    && app.includes('className="chat-project-header__row"')
    && app.includes('aria-label="Back to Projects"')
    && !app.includes('>Back to Projects</')
    && app.includes('chat-project-header__back'));
  const headerBackAt = app.indexOf('id="chat-mobile-back-projects"');
  const headerTitleAt = app.indexOf('className="chat-project-header__identity"');
  const headerActionsAt = app.indexOf('className="chat-project-header__actions"');
  check('project header back sits on the title row before identity',
    headerBackAt >= 0 && headerTitleAt > headerBackAt && headerActionsAt > headerTitleAt
    && app.includes('chat-project-header__row')
    && app.slice(app.indexOf('<header className="chat-project-header">'), headerActionsAt).includes('←'));
  check('context pane Back is a compact head-row icon',
    app.includes('className="chat-context__head"')
    && app.includes('chat-context__close')
    && app.includes('aria-label="Back"')
    && app.includes('className="chat-context__title"')
    && !/>\s*Back\s*<\/button>/.test(app.slice(app.indexOf('className="chat-context"'), app.indexOf('className="chat-context__tabs"'))));
  check('mobile Details expands the context pane with Files and Jobs tabs',
    app.includes("if (mobile) teamStore.setMobilePane('context')")
    && app.includes("hidden={!selectedProject || (mobile ? mobilePane !== 'context'")
    && app.includes('className="chat-context"')
    && app.includes('className="chat-context__tabs"')
    && app.includes("(['files', 'jobs'] as const)")
    && app.includes("'Files'")
    && app.includes("'Jobs'"));
  check('project selection uses live innerWidth for the conversation pane', app.includes('const selectProject = (projectId: string) =>')
    && app.includes("window.innerWidth < 720")
    && app.includes("teamStore.setMobilePane('conversation')"));
}

// ---- User-visible language audit (source) ----
{
  const banned = ['probe', 'mapping', 'provisioning', 'reconciling', 'source session', 'Add and probe', 'Probing…'];
  const surfaces = [app, styles, broadcast, invite, sessionView, emptyState, emptyCopy];
  for (const term of banned) {
    const hits = surfaces.filter((source) => {
      const literals = [...source.matchAll(/(['"])((?:\\.|(?!\1).)*)\1/g)].map((match) => match[2]);
      return literals.some((literal) => (
        !literal.startsWith('./')
        && !literal.startsWith('../')
        && literal.toLowerCase().includes(term.toLowerCase())
      ));
    }).length;
    check(`banned term "${term}" absent from user-visible chat strings`, hits === 0, String(hits));
  }
  const required = [
    'Add runner',
    'Online',
    'Offline',
    'Token required',
    CHAT_RAIL_EMPTY,
    CHAT_INVITE_EMPTY,
    CHAT_WORKSPACE_EMPTY_TITLE,
    CHAT_PROJECT_EMPTY_TITLE,
    'No messages yet. Send a prompt to start this conversation.',
    'Send one message to the agent team.',
    'Channel',
    'Jobs',
    'Files',
    'Confirm',
    'Project settings',

  ];
  for (const label of required) {
    const present = [app, broadcast, invite, sessionView, emptyState, emptyCopy].some((source) => source.includes(label));
    check(`required label "${label}" present`, present);
  }
}

{
  check('display names strip a trailing listen-port suffix',
    runnerDisplayName({ codename: 'x3-8765', label: 'box', authority: 'host:8765' }) === 'x3'
    && runnerDisplayName({ codename: null, label: 'box-9000', authority: 'host:8765' }) === 'box'
    && runnerDisplayName({ codename: 'alpha-one', label: 'My box', authority: 'host:8765' }) === 'alpha-one'
    && runnerDisplayName({ codename: null, label: '   ', authority: 'host:8765' }) === 'host'
    && runnerDisplayName({ codename: 'build-1', label: 'box', authority: 'host:8765' }) === 'build-1');
  const overlayBlock = styles.slice(styles.indexOf('.chat-invite--overlay {'), styles.indexOf('}', styles.indexOf('.chat-invite--overlay {')));
  check('invite overlay does not scroll the dialog page', overlayBlock.includes('overflow: hidden'));
  const overlayLan = styles.slice(styles.indexOf('.chat-invite--overlay .chat-invite__lan {'), styles.indexOf('}', styles.indexOf('.chat-invite--overlay .chat-invite__lan {')));
  check('invite LAN list may scroll', overlayLan.includes('overflow: auto'));
  check('invite overlay LAN list is height-capped so the form cannot cover cards', overlayLan.includes('max-height: min(40vh, 360px)'));
  const tokenForm = styles.slice(styles.indexOf('.chat-token-form {'), styles.indexOf('}', styles.indexOf('.chat-token-form {')));
  check('token form stays on one row', tokenForm.includes('flex-wrap: nowrap'));
  const identityTitle = styles.slice(styles.indexOf('.chat-project-header__identity h1,\n.chat-runner-header__identity h1 {'), styles.indexOf('}', styles.indexOf('.chat-project-header__identity h1,\n.chat-runner-header__identity h1 {')));
  check('project header title ellipsizes instead of wrapping', identityTitle.includes('white-space: nowrap') && identityTitle.includes('text-overflow: ellipsis'));
  check('runner and project header titles clamp so they never clip',
    identityTitle.includes('font-size: 15px')
    && identityTitle.includes('line-height: 1.25')
    && identityTitle.includes('max-height: 1.25em')
    && identityTitle.includes('overflow: hidden'));
  const railLan = styles.slice(styles.indexOf('.chat-rail-lan {'), styles.indexOf('}', styles.indexOf('.chat-rail-lan {')));
  const railLanOpen = styles.slice(styles.indexOf('.chat-rail-lan[open] {'), styles.indexOf('}', styles.indexOf('.chat-rail-lan[open] {')));
  const railLanList = styles.slice(styles.indexOf('.chat-rail-lan__list {'), styles.indexOf('}', styles.indexOf('.chat-rail-lan__list {')));
  check('expanded LAN rail is height-capped on every viewport', railLanOpen.includes('max-height: 28vh') && railLanList.includes('overflow-y: auto') && railLanList.includes('max-height: calc(28vh - 40px)') && railLan.includes('overflow: hidden'));
  const composerGrid = styles.slice(styles.indexOf('.chat-session__composer-row {'), styles.indexOf('}', styles.indexOf('.chat-session__composer-row {')));
  check('composer grid leaves a column for the command picker', composerGrid.includes('grid-template-columns: auto minmax(0, 1fr) auto auto'));
  const composerText = styles.slice(styles.indexOf('.chat-session__composer textarea {'), styles.indexOf('}', styles.indexOf('.chat-session__composer textarea {')));
  const composerBtn = styles.slice(styles.indexOf('.chat-session__composer .chat-button {'), styles.indexOf('}', styles.indexOf('.chat-session__composer .chat-button {')));
  check('composer idle textarea is fixed 44px and scrolls overflow', composerText.includes('height: 44px') && composerText.includes('min-height: 44px') && composerText.includes('max-height: 44px') && composerText.includes('overflow-y: auto') && composerText.includes('resize: none') && !composerText.includes('resize: vertical'));
  check('composer Send is 44px', composerBtn.includes('height: 44px') && composerBtn.includes('min-height: 44px'));
  const phoneComposerMq = styles.slice(
    styles.indexOf('@media (max-width: 719px) {'),
    styles.indexOf('\n.chat-tmux-terminal__close:hover'),
  );
  check('phone composer keeps textarea and Send on one row',
    !/\.chat-runner-form--project,\s*\.chat-session__composer,/.test(phoneComposerMq)
    && !/\.chat-session__composer \.chat-button \{\s*width:\s*100%/.test(phoneComposerMq)
    && /\.chat-session__composer textarea \{[^}]*flex:\s*1/.test(phoneComposerMq)
    && /\.chat-session__composer \.chat-button \{[^}]*width:\s*auto/.test(phoneComposerMq)
    && /\.chat-session__composer \{[^}]*flex-wrap:\s*nowrap/.test(phoneComposerMq));
  check('phone composer-row aligns to flex-end, not stretch',
    /\.chat-session__composer-row \{[^}]*align-items:\s*flex-end/.test(phoneComposerMq)
    && !/\.chat-session__composer-row \{[^}]*align-items:\s*stretch/.test(phoneComposerMq));
  check('phone project gear sits at the far right',
    /#chat-project-gear-btn \{[^}]*margin-left:\s*auto/.test(phoneComposerMq));
  check('phone project header hides the truncated runner-count origin',
    /\.chat-project-header__identity \.chat-runner-origin \{\s*display:\s*none/.test(phoneComposerMq));
  check('phone slash picker opens left from the / button',
    /\.chat-session__composer \.command-picker__popover \{[^}]*left:\s*auto/.test(phoneComposerMq)
    && /\.chat-session__composer \.command-picker__popover \{[^}]*right:\s*0/.test(phoneComposerMq));
  check('phone back controls stay compact on the header row',
    !phoneComposerMq.includes('#chat-mobile-back-projects {\n    width: 100%')
    && /\.chat-project-header__row,\s*\.chat-runner-header__row \{/.test(styles)
    && /\.chat-project-header__row \.chat-project-header__identity,\s*\.chat-runner-header__row \.chat-runner-header__identity \{[^}]*flex:\s*1 1 auto/.test(styles)
    && /\.chat-context__title \{[^}]*flex:\s*1/.test(styles));
  check('phone runners list scrolls; LAN is bounded',
    !/\.chat-projects-list,\s*\.chat-runners-list \{\s*overflow-y:\s*visible/.test(phoneComposerMq)
    && /\.chat-runners-rail,\s*\.chat-rail-section,\s*\.chat-projects-section \{[^}]*flex:\s*1 1 0/.test(phoneComposerMq)
    && !/\.chat-runners-rail,\s*\.chat-rail-section,\s*\.chat-projects-section \{[^}]*grid-template-rows/.test(phoneComposerMq)
    && /\.chat-rail-lan \{[^}]*max-height:\s*28vh/.test(phoneComposerMq)
    && /\.chat-runners-list \{[^}]*overflow-y:\s*auto/.test(phoneComposerMq));
  const pickerBtn = styles.slice(styles.indexOf('.chat-session__composer #command-btn {'), styles.indexOf('}', styles.indexOf('.chat-session__composer #command-btn {')));
  check('composer command picker is 44px', pickerBtn.includes('height: 44px') && pickerBtn.includes('min-height: 44px') && pickerBtn.includes('width: 44px') && pickerBtn.includes('min-width: 44px'));
  const transcriptBlock = styles.slice(styles.indexOf('.chat-session__transcript,\n.chat-broadcast__transcript {'), styles.indexOf('}', styles.indexOf('.chat-session__transcript,\n.chat-broadcast__transcript {')));
  check('session transcript reserves scrollbar gutter so the thumb cannot cover markdown', transcriptBlock.includes('scrollbar-gutter: stable'));
  const sessionPos = styles.slice(styles.indexOf('.chat-session {\n  min-height: 0;'), styles.indexOf('}', styles.indexOf('.chat-session {\n  min-height: 0;')));
  check('session positions the goal overlay', sessionPos.includes('position: relative'));
  const stageBlock = styles.slice(styles.indexOf('.chat-session__stage {'), styles.indexOf('}', styles.indexOf('.chat-session__stage {')));
  check('session stage is the overlay containing block', stageBlock.includes('position: relative') && stageBlock.includes('min-height: 0'));
  check('session composer mounts the slash picker', sessionView.includes('<CommandPicker') && sessionView.includes('chat-session__stage') && sessionView.includes('chat-goal-overlay'));
  const goalOverlay = styles.slice(styles.indexOf('.chat-goal-overlay {'), styles.indexOf('}', styles.indexOf('.chat-goal-overlay {')));
  check('goal overlay fills the transcript stage', styles.includes('.chat-goal-overlay')
    && goalOverlay.includes('position: absolute')
    && goalOverlay.includes('inset: 0')
    && !goalOverlay.includes('bottom: calc(56px')
    && goalOverlay.includes('var(--bg-panel)')
    && goalOverlay.includes('var(--border-strong)'));
  const goalClose = styles.slice(styles.indexOf('.chat-goal-overlay__close {'), styles.indexOf('}', styles.indexOf('.chat-goal-overlay__close {')));
  check('goal overlay close is 44px on the far right', goalClose.includes('width: 44px') && goalClose.includes('height: 44px') && goalClose.includes('margin-left: auto'));
  const gearBlock = styles.slice(styles.indexOf('#chat-project-gear-btn {'), styles.indexOf('}', styles.indexOf('#chat-project-gear-btn {')));
  check('project gear is a 44px hit target', gearBlock.includes('min-width: 44px') && gearBlock.includes('min-height: 44px'));
  const tmuxControls = styles.slice(styles.indexOf('.chat-tmux__controls {'), styles.indexOf('}', styles.indexOf('.chat-tmux__controls {')));
  check('tmux send controls stay on one row', tmuxControls.includes('flex-wrap: nowrap'));
  check('gear menu width is 720px not 340px', styles.includes('width: min(720px, calc(100% - 36px))')
    && !styles.includes('width: min(340px, calc(100% - 36px))'));
  check('tmux snapshot wraps instead of dual-axis scroll', /\.chat-tmux__pre \{[^}]*white-space:\s*pre-wrap/s.test(styles)
    && /\.chat-tmux__pre \{[^}]*overflow:\s*auto/s.test(styles));
  check('Channel omits Sending-to-N-runners status',
    !broadcast.includes('Sending to')
    && !broadcast.includes('recipientLine')
    && !broadcast.includes('chat-broadcast__recipients')
    && !styles.includes('.chat-broadcast__recipients')
    && broadcast.includes('Send one message to the agent team. @ a runner to assign work.')
    && broadcast.includes('No runner named @')
    && broadcast.includes('matches more than one runner'));
  check('invite submit is Add runner', invite.includes("? 'Connecting…' : 'Add'}")
    && invite.includes("? 'Save token and reconnect' : 'Add runner'}"));
  check('push fingerprint includes collectRunnerTokens', app.includes('tokens: collectRunnerTokens(storage)'));
  {
    const helperStart = runners.indexOf('function isListenLocalTmuxCommand');
    const helperEnd = runners.indexOf('export class RunnerRegistry');
    const helper = helperStart >= 0 && helperEnd > helperStart ? runners.slice(helperStart, helperEnd) : '';
    const bypassStart = runners.indexOf('if (isListenLocalTmuxCommand(command))');
    const bypassEnd = runners.indexOf('const sourceSessionId', bypassStart);
    const bypass = bypassStart >= 0 && bypassEnd > bypassStart ? runners.slice(bypassStart, bypassEnd) : '';
    check('all listen-local tmux RPCs skip chat subscription',
      helper.includes("case 'tmux_list_sessions'")
      && helper.includes("case 'tmux_pane_snapshot'")
      && helper.includes("case 'tmux_send_keys'")
      && helper.includes("case 'tmux_session_create'")
      && helper.includes("case 'tmux_session_delete'")
      && helper.includes("case 'tmux_terminal_attach'")
      && helper.includes("case 'tmux_terminal_input'")
      && helper.includes("case 'tmux_terminal_resize'")
      && helper.includes("case 'tmux_terminal_detach'")
      && bypass.includes('if (isListenLocalTmuxCommand(command))')
      && bypass.includes('return this.sendCommand(runner, runner.generation, command, options.commandId);')
      && !bypass.includes('ensureSubscription')
      && !bypass.includes('sourceSessionId'));
  }
}

{
  function toolCardCopyRight(opts: {
    viewport: number;
    titleMinWidth: 'auto' | 0;
    titleNatural: number;
    copy: number;
    transcriptPad: number;
    cardPadX: number;
  }): number {
    const inner = opts.viewport - opts.transcriptPad * 2 - opts.cardPadX * 2;
    const titleUsed = opts.titleMinWidth === 'auto'
      ? opts.titleNatural
      : Math.min(opts.titleNatural, Math.max(0, inner - opts.copy));
    return opts.transcriptPad + opts.cardPadX + titleUsed + opts.copy;
  }
  const measured = {
    viewport: 390,
    titleNatural: 565.5,
    copy: 44,
    transcriptPad: 18,
    cardPadX: 10,
  };
  check(
    '390px tool-card copy overflows when the title min-width is auto',
    toolCardCopyRight({ ...measured, titleMinWidth: 'auto' }) > 390,
    String(toolCardCopyRight({ ...measured, titleMinWidth: 'auto' })),
  );
  check(
    '390px tool-card copy stays inside the viewport when the title min-width is 0',
    toolCardCopyRight({ ...measured, titleMinWidth: 0 }) <= 390,
    String(toolCardCopyRight({ ...measured, titleMinWidth: 0 })),
  );
  const titleRule = styles.slice(styles.indexOf('.chat-app .tool-card__name,'), styles.indexOf('.chat-session__tool {'));
  check(
    'chat tool-card titles shrink and wrap instead of overflowing the stage',
    titleRule.includes('min-width: 0') && titleRule.includes('overflow-wrap: anywhere'),
    titleRule,
  );
  check(
    'chat ToolCard sits in a max-width host',
    sessionView.includes('className="chat-session__tool"')
      && /\.chat-session__tool \{[^}]*max-width:\s*100%/.test(styles)
      && /\.chat-app \.tool-card \{[^}]*max-width:\s*100%/.test(styles),
  );
}

{
  function renderChip(connState: Parameters<typeof hostConnectionChip>[0], stableError: string): string {
    return renderToStaticMarkup(createElement(HostConnectionChip, { connState, stableError }));
  }
  const pulseModel = hostConnectionChip('reconnecting', 'stale socket error');
  check(
    'pulse model is a mark, not header prose',
    pulseModel !== null && pulseModel.kind === 'pulse' && !('text' in pulseModel),
    JSON.stringify(pulseModel),
  );
  const reconnectingHtml = renderChip('reconnecting', 'stale socket error');
  check(
    'reconnecting chip is a compact pulse mark, not header prose',
    reconnectingHtml.includes('id="host-connection-chip"')
      && reconnectingHtml.includes('host-connection-chip--pulse')
      && reconnectingHtml.includes('chat-slow-pulse')
      && reconnectingHtml.includes('data-kind="pulse"')
      && reconnectingHtml.includes(`aria-label="${HOST_RECONNECTING_STATUS}"`)
      && !reconnectingHtml.includes(`>${HOST_RECONNECTING_STATUS}<`)
      && !reconnectingHtml.includes('stale socket error'),
    reconnectingHtml,
  );
  const connectingHtml = renderChip('connecting', 'stale socket error');
  check(
    'connecting chip uses the same pulse as reconnecting',
    connectingHtml.includes('host-connection-chip--pulse')
      && connectingHtml.includes('chat-slow-pulse')
      && connectingHtml.includes(`aria-label="${HOST_RECONNECTING_STATUS}"`)
      && !connectingHtml.includes(`>${HOST_RECONNECTING_STATUS}<`),
    connectingHtml,
  );
  const onHtml = renderChip('on', 'stale socket error');
  check('connected chip is absent', onHtml === '', onHtml);
  const stallOnHtml = renderToStaticMarkup(createElement(HostConnectionChip, {
    connState: 'on',
    stableError: '',
    streamStall: true,
  }));
  check(
    'stream stall pulses the header chip while connState stays on',
    stallOnHtml.includes('host-connection-chip--pulse')
      && stallOnHtml.includes('chat-slow-pulse')
      && stallOnHtml.includes(`aria-label="${HOST_RECONNECTING_STATUS}"`)
      && hostConnectionChip('on', '', true)?.kind === 'pulse'
      && hostConnectionChip('on', '', false) === null,
    stallOnHtml,
  );
  const tokenHtml = renderChip('token_required', HOST_TOKEN_REQUIRED_ERROR);
  check(
    'token_required chip shows the raw error without pulse',
    tokenHtml.includes('host-connection-chip--error')
      && tokenHtml.includes(HOST_TOKEN_REQUIRED_ERROR)
      && !tokenHtml.includes('chat-slow-pulse')
      && !tokenHtml.includes('Reconnecting…'),
    tokenHtml,
  );
  const offlineError = 'cannot reach this machine';
  const offlineHtml = renderChip('offline', offlineError);
  check(
    'offline chip shows the raw error without pulse',
    offlineHtml.includes('host-connection-chip--error')
      && offlineHtml.includes(offlineError)
      && !offlineHtml.includes('chat-slow-pulse')
      && !offlineHtml.includes('Reconnecting…'),
    offlineHtml,
  );
  check(
    '/web header mounts the connection chip from connState',
    webApp.includes('<HostConnectionChip')
      && webApp.includes('connState={connState}')
      && webApp.includes('streamStall={activeStreamStall}'),
  );
  check(
    '/web styles pulse the header chip with chat-slow-pulse',
    webStyles.includes('@keyframes chat-slow-pulse')
      && /\.chat-slow-pulse \{[^}]*animation:\s*chat-slow-pulse/.test(webStyles)
      && /\.host-connection-chip \{[^}]*color:\s*var\(--muted\)/.test(webStyles)
      && /\.host-connection-chip--pulse \{[^}]*width:\s*10px/.test(webStyles)
      && /\.host-connection-chip--pulse \{[^}]*height:\s*10px/.test(webStyles)
      && /\.host-connection-chip--pulse \{[^}]*background:\s*var\(--warn\)/.test(webStyles)
      && /\.host-connection-chip--error \{[^}]*color:\s*var\(--error\)/.test(webStyles)
      && /\.host-connection-chip--error \{[^}]*animation:\s*none/.test(webStyles),
  );
  const phone480Start = webStyles.indexOf('@media (max-width: 480px) {');
  const phone480End = webStyles.indexOf('\n@media (prefers-reduced-motion: reduce)', phone480Start);
  const phone480 = phone480Start >= 0 && phone480End > phone480Start
    ? webStyles.slice(phone480Start, phone480End)
    : '';
  check(
    'phone header keeps thinking readable while reconnecting',
    /#thinking-select \{[^}]*flex:\s*0 0 auto/.test(phone480)
      && /#thinking-select \{[^}]*min-width:\s*96px/.test(phone480)
      && !/#thinking-select \{[^}]*min-width:\s*0/.test(phone480)
      && /\.host-connection-chip--pulse \{[^}]*max-width:\s*none/.test(phone480),
    phone480.slice(0, 800),
  );
}

{
  const shown = renderToStaticMarkup(createElement(ReconnectOverlay, { visible: true, text: RECONNECT_OVERLAY_TEXT }));
  const firstConnect = renderToStaticMarkup(createElement(ReconnectOverlay, { visible: true, text: '正在连接…' }));
  const hidden = renderToStaticMarkup(createElement(ReconnectOverlay, { visible: false }));
  check(
    'fullscreen overlay renders spinner and reconnect copy',
    shown.includes('id="reconnect-overlay"')
      && shown.includes('reconnect-overlay__spinner')
      && shown.includes(RECONNECT_OVERLAY_TEXT)
      && shown.includes('role="alert"'),
    shown,
  );
  check('first-contact overlay never claims a disconnect', firstConnect.includes('正在连接…') && !firstConnect.includes('连接断开'));
  check('hidden overlay renders nothing', hidden === '');
  check('reconnecting is overlay-visible', reconnectOverlayVisible('reconnecting') === true);
  check('authoritative on dismisses overlay', reconnectOverlayVisible('on', false) === false);
  check(
    '/web mounts the fullscreen overlay from connState',
    webApp.includes('<ReconnectOverlay')
      && webApp.includes('visible={reconnectOverlayVisible(connState)}'),
    webApp.includes('ReconnectOverlay') ? 'mounted' : 'missing',
  );
  check(
    '/web does not mark connState on before bootstrap refill',
    !webApp.slice(
      webApp.indexOf('const catchup = planReconnectDisplayedCatchup'),
      webApp.indexOf("sendCommand({ type: 'get_messages', sessionId: refillSessionId }"),
    ).includes("setConnState('on')"),
  );
  check(
    '/web dismisses overlay only after refill, restore, goal, and get_commands',
    webApp.includes('return afterSwitch.then(() => refillWork())')
      && !webApp.includes('Promise.all([refill, restore])')
      && webApp.indexOf('refreshGoal(sessionIdRef.current)') > webApp.indexOf('return afterSwitch.then(() => refillWork())')
      && webApp.indexOf("type: 'get_commands'") > webApp.indexOf('return afterSwitch.then(() => refillWork())')
      && webApp.indexOf("setConnState('on')") > webApp.indexOf("type: 'get_commands'"),
  );
  check(
    '/web send failure tears the transport so overlay appears',
    webApp.includes("tearTransportRef.current('send failed')"),
  );
  check(
    '/chat mounts the fullscreen overlay over the workspace shell',
    app.includes('<ReconnectOverlay')
      && app.includes('id="chat-projects"')
      && app.includes('reconnectOverlayVisible('),
  );
  const overlayRule = (sheet: string) => {
    const start = sheet.indexOf('.reconnect-overlay {');
    const end = sheet.indexOf('}', start);
    return start >= 0 && end > start ? sheet.slice(start, end + 1) : '';
  };
  const webOverlay = overlayRule(webStyles);
  const chatOverlay = overlayRule(styles);
  check(
    '/web overlay is fixed fullscreen above header/footer',
    webOverlay.includes('position: fixed')
      && webOverlay.includes('inset: 0')
      && /z-index:\s*80/.test(webOverlay),
    webOverlay,
  );
  check(
    '/chat overlay is fixed fullscreen above header/footer',
    chatOverlay.includes('position: fixed')
      && chatOverlay.includes('inset: 0')
      && /z-index:\s*80/.test(chatOverlay),
    chatOverlay,
  );
}

{
  const activitySrc = readFileSync(join(here, '../src/panels/ActivityPanel.tsx'), 'utf8');
  const headStart = activitySrc.indexOf('<div className="todo-panel__head">');
  const closeAt = activitySrc.indexOf('id="activity-close-btn"');
  const afterClose = closeAt >= 0 ? activitySrc.slice(closeAt) : '';
  const headEndRel = afterClose.indexOf('</div>');
  const headBlock = headStart >= 0 && closeAt > headStart
    ? activitySrc.slice(headStart, closeAt + Math.max(headEndRel, 0))
    : '';
  const afterHead = headEndRel >= 0 ? afterClose.slice(headEndRel) : '';
  check('Activity panel head is a div, not a document header', activitySrc.includes('<div className="todo-panel__head">')
    && !activitySrc.includes('<header')
    && webApp.includes("activePanel === 'activity'")
    && webApp.includes('<ActivityPanel'));
  check('Activity filter and phases are siblings of the head, not inside it',
    !headBlock.includes('subagents-panel__filter')
    && !headBlock.includes('todo-panel__phases')
    && afterHead.includes('subagents-panel__filter')
    && afterHead.includes('todo-panel__phases'));
  check('Activity body keeps a dedicated scroller after the sticky head',
    /#activity-panel \.todo-panel__phases\s*\{[^}]*flex:\s*1 1 auto/.test(webStyles)
    && /#activity-panel \.todo-panel__phases\s*\{[^}]*overflow-y:\s*auto/.test(webStyles)
    && /#activity-panel \.todo-panel__phases\s*\{[^}]*min-height:\s*4rem/.test(webStyles));
  const emptySnapshot: ActivitySessionSnapshot = { sessionId: 'sess-a', entries: [], sequence: 0, actors: {} };
  const running = applyActivityEvent(emptySnapshot, {
    type: 'job_updated',
    sessionId: 'sess-a',
    sequence: 4,
    job: {
      id: 'job-1',
      status: 'running',
      description: 'reviewing files',
      agent: 'glm',
      createdAt: 100,
    },
  });
  check('job_updated lands a recent Activity entry', running.entries.length === 1
    && running.entries[0]?.summary === 'reviewing files'
    && running.entries[0]?.actor === 'glm'
    && activityEntryMatchesPage(running.entries[0]!, 'recent')
    && !activityEntryMatchesPage(running.entries[0]!, 'settled'));
  const populated = renderToStaticMarkup(createElement(ActivityPanel, {
    activity: running.entries,
    onClose: () => undefined,
  }));
  check('Activity panel paints the live job instead of the empty copy', populated.includes('id="activity-panel"')
    && populated.includes('reviewing files')
    && populated.includes('glm')
    && populated.includes('running')
    && !populated.includes('No ordinary activity yet.')
    && !populated.includes('No settled ordinary activity yet.'));
  const blank = renderToStaticMarkup(createElement(ActivityPanel, {
    activity: [],
    onClose: () => undefined,
  }));
  check('empty Activity still shows the empty copy, title, and filter', blank.includes('Activity')
    && blank.includes('No ordinary activity yet.')
    && blank.includes('Recent')
    && blank.includes('Settled'));
}

{
  const phoneMqStart = webStyles.indexOf('@media (max-width: 768px) {');
  const phoneMq = phoneMqStart >= 0 ? webStyles.slice(phoneMqStart, webStyles.indexOf('\n}\n\n@media (max-width: 480px)', phoneMqStart)) : '';
  const rowStart = webStyles.indexOf('.steering-queue__row {');
  const row = rowStart >= 0 ? webStyles.slice(rowStart, webStyles.indexOf('}', rowStart)) : '';
  const textStart = webStyles.indexOf('.steering-queue__text {');
  const text = textStart >= 0 ? webStyles.slice(textStart, webStyles.indexOf('}', textStart)) : '';
  const bodyStart = webStyles.indexOf('.subagent-details__body {\n  flex: 1 1 auto');
  const body = bodyStart >= 0 ? webStyles.slice(bodyStart, webStyles.indexOf('}', bodyStart)) : '';
  const headStart = webStyles.indexOf('.subagent-details__head {');
  const head = headStart >= 0 ? webStyles.slice(headStart, webStyles.indexOf('}', headStart)) : '';
  const sheetStart = webStyles.indexOf('.subagent-details {\n  display: flex');
  const sheet = sheetStart >= 0 ? webStyles.slice(sheetStart, webStyles.indexOf('}', sheetStart)) : '';
  // The subagent sheet's phone overrides live in their OWN
  // `@media (max-width: 768px)` block placed after the base rules (a later
  // same-specificity declaration must win), so scan every phone block for
  // them instead of reading only the first one.
  const phoneBlocks = [];
  {
    const marker = '@media (max-width: 768px) {';
    let from = webStyles.indexOf(marker);
    while (from >= 0) {
      const end = webStyles.indexOf('\n}\n', from);
      phoneBlocks.push(webStyles.slice(from, end < 0 ? undefined : end));
      from = webStyles.indexOf(marker, end < 0 ? webStyles.length : end);
    }
  }
  const phoneRuleFor = (selector) => {
    for (let i = phoneBlocks.length - 1; i >= 0; i -= 1) {
      const at = phoneBlocks[i].indexOf(selector);
      if (at >= 0) return phoneBlocks[i].slice(at, phoneBlocks[i].indexOf('}', at));
    }
    return '';
  };
  const phoneBody = phoneRuleFor('.subagent-details__body {');
  const phoneSheet = phoneRuleFor('.subagent-details {');
  check('390 steering tray rows are 36-44px with ellipsis', row.includes('min-height: 36px') && row.includes('max-height: 44px') && text.includes('text-overflow: ellipsis') && text.includes('white-space: nowrap') && !webStyles.includes('.steering-queue__head'));
  // Anchored on the RULE (two-space indent), not the bare `footer {`
  // substring: a nested rule such as `.app-layout--drawer-open footer {`
  // would otherwise be read first. The slice start is the first
  // `@media (max-width: 768px)` block, which other work may add earlier.
  const footerPhoneStart = phoneMq.indexOf('\n  footer {');
  const footerPhone = footerPhoneStart >= 0 ? phoneMq.slice(footerPhoneStart, phoneMq.indexOf('}', footerPhoneStart)) : '';
  check('390 footer overflow stays visible so the steering overlay is not clipped', footerPhone.includes('overflow: visible') && !footerPhone.includes('overflow-y: auto'));
  check(
    'mobile drawers fill the header-to-footer inset with an explicit calc height',
    /@media \(max-width: 768px\) \{[\s\S]*?\.todo-panel,[\s\S]*?height:\s*calc\(100dvh - var\(--app-header-height/.test(webStyles),
  );
  check('/web status row keeps Working left and Steering right', (() => {
    const rowStart = webStyles.indexOf('.composer-status-row {');
    const row = rowStart >= 0 ? webStyles.slice(rowStart, webStyles.indexOf('}', rowStart)) : '';
    const queueStart = webStyles.indexOf('#steering-queue {');
    const queue = queueStart >= 0 ? webStyles.slice(queueStart, webStyles.indexOf('}', queueStart)) : '';
    const wrap = webApp.indexOf('id="composer-status-row"');
    return row.includes('display: flex')
      && queue.includes('flex: 1 1 auto')
      && !queue.includes('position: absolute')
      && wrap >= 0
      && webApp.indexOf('id="activity-strip"') > wrap
      && webApp.indexOf('<SteeringQueue') > webApp.indexOf('id="activity-strip"');
  })());
  check('composer input is 16px so iOS Safari does not auto-zoom and steal hits', (() => {
    const start = webStyles.indexOf('#prompt-input {');
    const block = start >= 0 ? webStyles.slice(start, webStyles.indexOf('}', start)) : '';
    return block.includes('font-size: 16px') && !block.includes('font-size: 13px');
  })());
  check('/chat composer textarea is also 16px', (() => {
    const start = styles.indexOf('.chat-session__composer textarea {');
    const block = start >= 0 ? styles.slice(start, styles.indexOf('}', start)) : '';
    return block.includes('font-size: 16px') && !block.includes('font-size: 13px');
  })());
  check('App get_state watchdog is per-session and arms on streaming start',
    webApp.includes('createStreamWatchdog')
    && webApp.includes('armStreamWatchdogRef.current(identity.sessionId)')
    && webApp.includes('markStreamingFor(identity.sessionId, false)')
    && webApp.includes('streamWatchdogRef.current?.clear(sid)')
    && webApp.includes('armStreamWatchdogRef.current(target)')
    && webApp.includes('STREAM_WATCHDOG_MS')
    && webApp.includes("type: 'get_state'")
    && webApp.includes('planSettledStreamRefillRequest')
    && !webApp.includes('streamWatchdogSessionRef')
    && !webApp.includes('lastStreamEventAtBySessionRef')
    && !webApp.includes('streamWatchdogTimerBySessionRef'));
  check('stream stall paints pulsing Reconnecting only on a failed probe', (() => {
    const probe = webApp.indexOf('onSilence: (sid) => {');
    const block = probe >= 0 ? webApp.slice(probe, probe + 900) : '';
    const strip = webApp.indexOf('id="activity-strip"');
    const stripBlock = strip >= 0 ? webApp.slice(strip, webApp.indexOf('</div>', strip)) : '';
    return probe >= 0
      && webApp.includes("io.noteStreamStall(sid, { type: 'watchdog_probe_failed' })")
      && block.includes("io.noteStreamStall(sid, { type: 'watchdog_probe_alive' })")
      && !webApp.includes("type: 'watchdog_silence'")
      && block.includes("type: 'get_state'")
      && !block.includes('closeForHeartbeat')
      && !block.includes('ws.close')
      && webApp.includes('noteStreamingSocketDrop')
      && webApp.includes("type: 'socket_drop'")
      && webApp.includes("type: 'refill_finished'")
      && webApp.includes("type: 'live_event'")
      && stripBlock.includes("className={activeActivityStripView.state === 'reconnecting' ? 'chat-slow-pulse' : undefined}")
      && webApp.includes('activityStripView(activeActivityStrip, connState, activeStreamStall)')
      && webApp.includes('streamWatchdogRef.current?.probe(sid)');
  })());
  check('390 subagent sheet header stays visible', head.includes('flex: none') && sheet.includes('overflow: hidden') && sheet.includes('min-height: 0'));
  check('390 subagent sheet body is the only scroller', body.includes('overflow-y: auto') && body.includes('min-height: 0') && body.includes('flex: 1 1 auto') && phoneSheet.includes('max-height: 94dvh') && phoneBody.includes('-webkit-overflow-scrolling: touch'));
  const sectionStart = webStyles.indexOf('.subagent-details__section {\n  display: flex');
  const section = sectionStart >= 0 ? webStyles.slice(sectionStart, webStyles.indexOf('}', sectionStart)) : '';
  const descriptionStart = webStyles.indexOf('.subagent-details__description {');
  const description = descriptionStart >= 0 ? webStyles.slice(descriptionStart, webStyles.indexOf('}', descriptionStart)) : '';
  const shrinking = subagentSheetSectionRects({
    viewportWidth: 390,
    padX: 16,
    gap: 10,
    statusHeight: 20,
    taskText: CJK_SHEET_TASK,
    fontSize: 13,
    lineHeight: 1.45,
    cjkAdvance: 1,
    activityHeight: 36,
    transcriptLabelHeight: 16,
    taskShrinks: true,
  });
  const shipped = phoneSheetRects(webStyles);
  check('CJK TASK paints over LATEST ACTIVITY when the TASK block shrinks',
    rectsIntersect(shrinking.task, shrinking.activity)
    && rectsIntersect(shrinking.task, shrinking.transcript));
  check('shipped sheet CSS does not shrink or absolutely position TASK',
    !subagentSheetTaskShrinks(webStyles)
    && section.includes('flex: none')
    && !section.includes('min-height: 0')
    && !description.includes('position: absolute')
    && !description.includes('position: fixed')
    && !/(?:^|[^\w-])(?:max-)?height:\s*\d/.test(description)
    && !webStyles.includes('.subagent-details__section--transcript { flex: 1; }')
    && webStyles.includes('.subagent-details__body > * {'));
  check('390 CJK TASK and LATEST ACTIVITY bounding boxes do not intersect',
    !rectsIntersect(shipped.task, shipped.activity)
    && !rectsIntersect(shipped.activity, shipped.transcript)
    && shipped.task.bottom <= shipped.activity.top);
  const kidsStart = webStyles.indexOf('.transcript-content > * {');
  const kids = kidsStart >= 0 ? webStyles.slice(kidsStart, webStyles.indexOf('}', kidsStart)) : '';
  const cardStart = webStyles.indexOf('.tool-card {\n  align-self: flex-start');
  const card = cardStart >= 0 ? webStyles.slice(cardStart, webStyles.indexOf('}', cardStart)) : '';
  const rawStart = webStyles.indexOf('.tool-card__raw {');
  const raw = rawStart >= 0 ? webStyles.slice(rawStart, webStyles.indexOf('}', rawStart)) : '';
  const argsStart = webStyles.indexOf('.tool-card__args {');
  const args = argsStart >= 0 ? webStyles.slice(argsStart, webStyles.indexOf('}', argsStart)) : '';
  const outputStart = webStyles.indexOf('.tool-card__output {');
  const output = outputStart >= 0 ? webStyles.slice(outputStart, webStyles.indexOf('}', outputStart)) : '';
  const thinkingStart = webStyles.indexOf('.thinking {\n  margin: 0 0 6px');
  const thinking = thinkingStart >= 0 ? webStyles.slice(thinkingStart, webStyles.indexOf('}', thinkingStart)) : '';
  const asstStart = webStyles.indexOf('.msg--assistant {\n  align-self: flex-start');
  const asst = asstStart >= 0 ? webStyles.slice(asstStart, webStyles.indexOf('}', asstStart)) : '';
  const shrinkingBlocks = adjacentTranscriptRects({
    viewportWidth: 390,
    padX: 16,
    gap: 8,
    toolContentHeight: 220,
    thinkingHeight: 72,
    toolShrinks: true,
  });
  const shippedBlocks = phoneTranscriptAdjacentRects(webStyles);
  check('CJK tool-card paints over Thinking when the card shrinks',
    rectsIntersect(shrinkingBlocks.tool, shrinkingBlocks.thinking));
  check('shipped transcript CSS does not shrink or absolutely position tool-card raw args',
    !transcriptBlocksCanOverlap(webStyles)
    && kids.includes('flex: none')
    && card.includes('flex: none')
    && !raw.includes('position: absolute')
    && !args.includes('position: absolute')
    && !/(?:^|[^\w-])(?:max-)?height:\s*\d/.test(raw)
    && !/(?:^|[^\w-])(?:max-)?height:\s*\d/.test(args)
    && output.includes('flex: none')
    && output.includes('overflow-y: auto')
    && thinking.includes('flex: none')
    && row.includes('overflow: hidden')
    && webStyles.includes('.transcript-content details:not([open]) > *:not(summary)'));
  check('390 CJK tool-card and following Thinking bounding boxes do not intersect',
    !rectsIntersect(shippedBlocks.tool, shippedBlocks.thinking)
    && shippedBlocks.tool.bottom <= shippedBlocks.thinking.top
    && CJK_TRANSCRIPT_TOOL.length > 40);
  const stolenRaw = rawArgsOutsideVisualCard({
    cardBottom: 550.23, outputBottom: 378.92, rawTop: 382.92, outputHasOwnBorder: true,
  });
  const enclosedRaw = rawArgsOutsideVisualCard({
    cardBottom: 550.23, outputBottom: 378.92, rawTop: 382.92, outputHasOwnBorder: false,
  });
  const cardBox = { left: 12, right: 363, top: 143.25, bottom: 550.23 };
  const rawBox = { left: 23, right: 352, top: 382.92, bottom: 543.23 };
  const thinkingInset = adjacentTranscriptRects({
    viewportWidth: 390, padX: 12, gap: 8, toolContentHeight: 200, thinkingHeight: 72, toolShrinks: false, thinkingInset: 10,
  });
  check('inner output border leaves raw args outside the visual card',
    stolenRaw && !enclosedRaw && rawArgsInsideCardBorder(cardBox, rawBox));
  check('shipped tool-card chrome encloses raw args and shares Thinking left edge',
    !outputFrameStealsRawArgs(webStyles)
    && !assistantIndentsThinking(webStyles)
    && card.includes('overflow: hidden')
    && asst.includes('padding-inline: 0')
    && webStyles.includes('.assistant-text { white-space: pre-wrap; overflow-wrap: anywhere; padding-inline: 11px; }')
    && siblingLeftsAligned(shippedBlocks.tool, shippedBlocks.thinking)
    && thinkingInset.thinking.left - thinkingInset.tool.left === 10);
  check('390 feature nav odd last item spans both columns',
    /\.session-sidebar__nav\s*>\s*:last-child:nth-child\(odd\)\s*\{[^}]*grid-column:\s*1\s*\/\s*-1/.test(webStyles)
    && /@media \(max-width: 720px\)[\s\S]*\.session-sidebar__nav \.panel-toggle/.test(webStyles));
}

{
  function phoneComposerColumn(opts: {
    viewport: number;
    transcriptGutter: number;
    footerGutter: number;
    transcriptPadX: number;
    footerPadX: number;
    cardMaxWidthFraction: number;
  }): { cardLeft: number; cardRight: number; composerLeft: number; composerRight: number } {
    const transcriptClient = opts.viewport - opts.transcriptGutter;
    const footerClient = opts.viewport - opts.footerGutter;
    const cardWidth = (transcriptClient - 2 * opts.transcriptPadX) * opts.cardMaxWidthFraction;
    const cardLeft = opts.transcriptPadX;
    const cardRight = cardLeft + cardWidth;
    return {
      cardLeft,
      cardRight,
      composerLeft: opts.footerPadX,
      composerRight: footerClient - opts.footerPadX,
    };
  }
  const mismatchPad = phoneComposerColumn({
    viewport: 390, transcriptGutter: 15, footerGutter: 15, transcriptPadX: 10, footerPadX: 12, cardMaxWidthFraction: 1,
  });
  check('390 pad 10 vs 12 splits card and composer edges',
    mismatchPad.cardLeft !== mismatchPad.composerLeft
      && mismatchPad.cardRight !== mismatchPad.composerRight);
  const mismatchPercent = phoneComposerColumn({
    viewport: 390, transcriptGutter: 15, footerGutter: 15, transcriptPadX: 12, footerPadX: 12, cardMaxWidthFraction: 0.96,
  });
  check('390 tool-card max-width 96% splits card and composer edges',
    mismatchPercent.cardLeft === 12
      && mismatchPercent.cardRight !== mismatchPercent.composerRight);
  const mismatchGutter = phoneComposerColumn({
    viewport: 390, transcriptGutter: 15, footerGutter: 0, transcriptPadX: 12, footerPadX: 12, cardMaxWidthFraction: 1,
  });
  check('390 reserved scrollbar gutter splits card and composer right edges',
    mismatchGutter.cardLeft === mismatchGutter.composerLeft
      && mismatchGutter.cardRight !== mismatchGutter.composerRight);
  const shipped = phoneComposerColumn({
    viewport: 390, transcriptGutter: 15, footerGutter: 15, transcriptPadX: 12, footerPadX: 12, cardMaxWidthFraction: 1,
  });
  check('390 shipped column aligns tool-card, textarea, composer-main, and action-row edges',
    shipped.cardLeft === 12
      && shipped.cardLeft === shipped.composerLeft
      && shipped.cardRight === shipped.composerRight);
  check('Stop tap uses pointerdown, optimistic stopped footer, and does not gate abort on the streaming ref',
    webApp.includes('applyComposerAbortTap')
      && webApp.includes('id="send-btn"')
      && webApp.includes('onPointerDown')
      && webApp.includes('markStreamingFor(sid, false)')
      && webApp.includes('shouldHoldOptimisticStop')
      && !webApp.slice(webApp.indexOf('const abortActiveRun = useCallback'), webApp.indexOf('const abortActiveRun = useCallback') + 2500).includes('streamingBySessionIdRef.current[sid]'));
  check('Stop button is touch-manipulated and aborting uses --error fill',
    webStyles.includes('touch-action: manipulation')
      && webStyles.includes('#send-btn.composer-action--stop[data-aborting="true"]')
      && webStyles.includes('background: var(--error)'));
  const pickerHeading = webStyles.indexOf('/* Command picker (slash menu)');
  const baseCommandBtnStart = webStyles.indexOf('#command-btn {', pickerHeading);
  const baseCommandBtn = webStyles.slice(baseCommandBtnStart, webStyles.indexOf('}', baseCommandBtnStart) + 1);
  const baseLabelStart = webStyles.indexOf('.command-btn__label {', pickerHeading);
  const baseLabel = webStyles.slice(baseLabelStart, webStyles.indexOf('}', baseLabelStart) + 1);
  const mediaAfterPicker = webStyles.indexOf('@media', pickerHeading);
  const phone480 = webStyles.slice(
    webStyles.indexOf('@media (max-width: 480px) {'),
    webStyles.indexOf('/* Rows get slightly larger hit areas on small screens */'),
  );
  check('idle web composer is 44px and icon controls never stretch',
    /#composer-row \{[^}]*align-items:\s*flex-end/.test(webStyles)
    && /#composer-buttons \{[^}]*align-items:\s*center/.test(webStyles)
    && /\.command-picker \{[^}]*position:\s*relative/.test(webStyles)
    && !/#command-btn \{\s*height:\s*100%/.test(webStyles)
    && baseCommandBtnStart < mediaAfterPicker
    && baseLabelStart < mediaAfterPicker
    && /\n\s*width:\s*44px;/.test(baseCommandBtn)
    && /\n\s*min-width:\s*44px;/.test(baseCommandBtn)
    && /\n\s*max-width:\s*44px;/.test(baseCommandBtn)
    && /\n\s*height:\s*44px;/.test(baseCommandBtn)
    && /\n\s*min-height:\s*44px;/.test(baseCommandBtn)
    && /\n\s*max-height:\s*44px;/.test(baseCommandBtn)
    && /\n\s*padding:\s*0;/.test(baseCommandBtn)
    && /justify-content:\s*center/.test(baseCommandBtn)
    && /\.command-btn__label \{\s*display:\s*none/.test(baseLabel)
    && !phone480.includes('.command-btn__label')
    && !/#command-btn \{[^}]*width:\s*44px/.test(phone480)
    && /#send-btn \{[^}]*height:\s*44px/.test(webStyles)
    && /#attach-btn,[^}]*\{[^}]*width:\s*44px/.test(webStyles));
  check('toasts sit on the live footer height so they cannot cover Stop',
    webStyles.includes('bottom: calc(var(--app-footer-height) + 8px)')
      && !webStyles.includes('bottom: calc(128px + env(safe-area-inset-bottom))'));
  check('/chat Stop tap flips stopped immediately and sends abort then get_state',
    sessionView.includes('onPointerDown')
      && sessionView.includes("type: 'abort'")
      && sessionView.includes("type: 'get_state'")
      && sessionView.includes('applyComposerAbortTap')
      && sessionView.includes('setOptimisticStopped(true)')
      && sessionView.includes('composerActionClickKind'));
  const toolCardRule = webStyles.slice(webStyles.indexOf('.tool-card {'), webStyles.indexOf('}', webStyles.indexOf('.tool-card {')));
  check('web tool-card is full column width, not 96%',
    toolCardRule.includes('width: 100%') && !/max-width:\s*96%/.test(toolCardRule) && !/max-width:\s*96%/.test(webStyles));
  const phoneMqStart = webStyles.indexOf('@media (max-width: 768px) {');
  const phoneMq = phoneMqStart >= 0 ? webStyles.slice(phoneMqStart, webStyles.indexOf('\n}\n\n@media (max-width: 480px)', phoneMqStart)) : '';
  check('390 transcript padding matches footer 12px and both reserve a stable scrollbar gutter',
    webStyles.includes('.transcript-content { padding: 10px 12px; }')
      && /footer \{[^}]*padding:\s*8px 12px/.test(phoneMq)
      && /footer \{[^}]*scrollbar-gutter:\s*stable/.test(phoneMq)
      && /#transcript \{[^}]*scrollbar-gutter:\s*stable/.test(webStyles)
      && !/#transcript \{[^}]*scrollbar-gutter:\s*auto/.test(phoneMq)
      && /#prompt-input \{[^}]*width:\s*100%/.test(webStyles));
  const webTableRule = mdTableCellRule(webStyles);
  const chatTableRule = mdTableCellRule(styles);
  const webTablePolicy = tableCellWrapPolicy(webTableRule);
  const chatTablePolicy = tableCellWrapPolicy(chatTableRule);
  const breakAll = { wordBreak: 'break-all' as const, overflowWrap: 'anywhere' as const };
  const phoneCol = {
    viewportWidth: PHONE_TABLE_VIEWPORT,
    transcriptPadX: 12,
    columns: 6,
    cellPadX: 10,
    borderPx: 1,
    charPx: 8,
    priorityToken: 'P0',
  };
  const squeezed = phonePriorityColumnContentPx({ ...phoneCol, wordBreak: 'break-all', overflowWrap: 'anywhere' });
  const anywhereCol = phonePriorityColumnContentPx({
    ...phoneCol,
    wordBreak: 'normal',
    overflowWrap: 'anywhere',
    priorityToken: 'P0',
  });
  const shippedCol = phonePriorityColumnContentPx({
    ...phoneCol,
    wordBreak: webTablePolicy.wordBreak,
    overflowWrap: webTablePolicy.overflowWrap,
  });
  const anywherePolicy = { wordBreak: 'normal' as const, overflowWrap: 'anywhere' as const };
  check('390 table cells ship word-break:normal and overflow-wrap:break-word',
    shippedTableCellsKeepAsciiTokens(webTablePolicy)
      && shippedTableCellsKeepAsciiTokens(chatTablePolicy)
      && webTablePolicy.overflowWrap === 'break-word'
      && chatTablePolicy.overflowWrap === 'break-word'
      && !webTableRule.includes('break-all')
      && !chatTableRule.includes('break-all')
      && !webTableRule.includes('anywhere')
      && !chatTableRule.includes('anywhere'));
  check('390 first column keeps CJK labels on one line',
    firstColumnKeepsCjkLabel(mdTableFirstColumnRule(webStyles))
      && firstColumnKeepsCjkLabel(mdTableFirstColumnRule(styles))
      && mdTableIsScrollWrap(webStyles)
      && mdTableIsScrollWrap(styles));
  check('390 break-all squeezes P0 onto two lines',
    ASCII_PRIORITY_TOKENS.every((token) => asciiTokenLineCount(token, squeezed, phoneCol.charPx, breakAll) === 2));
  check('390 overflow-wrap:anywhere min-content also splits P0/P1/P2',
    ASCII_PRIORITY_TOKENS.every((token) => asciiTokenLineCount(token, anywhereCol, phoneCol.charPx, anywherePolicy) === 2));
  const tableLayout = measurePhoneTableLineRects({ webRule: webTableRule, chatRule: chatTableRule });
  if (tableLayout.skipped) {
    check('390 P0/P1 browser layout skipped only for a named environmental reason',
      tableLayout.reason === 'missing-agent-browser' || tableLayout.reason === 'agent-browser-wedged',
      tableLayout.reason);
    console.warn(`390 browser measurement SKIPPED (${tableLayout.reason}) — the wrap policy is still enforced by the static CSS-rule checks above`);
  } else {
    check('390 P0/P1 stay one line on /web and /chat table cells',
      tableLayout.web.p0 === 1
      && tableLayout.web.p1 === 1
      && tableLayout.chat.p0 === 1
      && tableLayout.chat.p1 === 1,
      JSON.stringify(tableLayout));
    check('390 break-all control wraps P0/P1 onto more than one line box',
      tableLayout.breakAll.p0 >= 2
      && tableLayout.breakAll.p1 >= 2
      && tableLayout.breakAll.p0Height > tableLayout.web.p0Height,
      JSON.stringify(tableLayout));
  }
  check('390 CJK table text still wraps between characters',
    cjkTokenLineCount('优先级唤醒冻结', shippedCol, 13) > 1);
}

// ---- answer prose reads with ONE face AND one measure: the transcript answer
//      text is the reference, and the hunk-review comment body, the side-chat
//      body and the /chat message body resolve to the same prose family/size/
//      leading and the same readable text-block measure, instead of inheriting
//      the mono grid the base document sets. Stylesheet contract: the
//      effective declarations for one class are read out of the two sheets,
//      not measured against a rendered element.
{
  /** Effective value of ONE property for ONE selector: the last declaration
   *  of that property among every rule whose selector list names the selector
   *  exactly (all such rules share specificity, so source order decides).
   *  Comments are stripped first so a brace inside prose cannot shift a
   *  rule's text. */
  const effective = (css: string, selector: string, property: string): string => {
    let value = '';
    const sheet = css.replace(/\/\*[\s\S]*?\*\//g, '');
    for (const rule of sheet.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
      if (!rule[1].split(',').some((part) => part.trim() === selector)) continue;
      for (const declaration of rule[2].split(';')) {
        const colon = declaration.indexOf(':');
        if (colon < 0) continue;
        if (declaration.slice(0, colon).trim() !== property) continue;
        value = declaration.slice(colon + 1).trim();
      }
    }
    return value;
  };
  const PROSE_FAMILY = 'var(--font-prose)';
  const readsAsTranscriptProse = (css: string, selector: string): boolean =>
    effective(css, selector, 'font-family') === PROSE_FAMILY
    && effective(css, selector, 'font-size') === '15px'
    && effective(css, selector, 'line-height') === '1.65';
  /** Direct text blocks only: the transcript caps running text and leaves
   *  fences, tables, images and diagrams at the full column width. */
  const MEASURE_BLOCKS = ['.md-p', '.md-list', '.md-quote', '.md-h1', '.md-h2', '.md-h3', '.md-h4', '.md-h5', '.md-h6'];
  const capsAtTranscriptMeasure = (css: string, root: string): boolean =>
    MEASURE_BLOCKS.every((block) => effective(css, `${root} > ${block}`, 'max-width') === '68ch');
  check('the transcript answer text is the prose reference (family/size/leading)',
    readsAsTranscriptProse(webStyles, '.assistant-text')
      && readsAsTranscriptProse(webStyles, '.msg--user__text'));
  check('hunk-review comment prose reads at the transcript prose face',
    readsAsTranscriptProse(webStyles, '.code-review__comment-text'));
  check('side-chat prose reads at the transcript prose face',
    readsAsTranscriptProse(webStyles, '.side-chat__text'));
  check('/chat message prose reads at the transcript prose face',
    readsAsTranscriptProse(styles, '.chat-session-markdown'));
  check('thinking chrome (hunk review + side chat) takes the prose family',
    effective(webStyles, '.thinking__body', 'font-family') === PROSE_FAMILY);
  check('/chat thinking fold takes the prose family',
    effective(styles, '.chat-session-thinking', 'font-family') === PROSE_FAMILY);
  check('the transcript text blocks are the measure reference',
    capsAtTranscriptMeasure(webStyles, '.assistant-text'));
  check('hunk-review and side-chat text blocks keep the transcript measure',
    capsAtTranscriptMeasure(webStyles, '.code-review__comment-text')
      && capsAtTranscriptMeasure(webStyles, '.side-chat__text'));
  check('/chat text blocks keep the transcript measure',
    capsAtTranscriptMeasure(styles, '.chat-session-markdown'));
}

console.log(`\nchatLayout.test: ${assertions} assertions, ${failures.length} failure(s)`);
if (failures.length > 0) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
