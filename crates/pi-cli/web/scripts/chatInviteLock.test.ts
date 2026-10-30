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
const chat = readFileSync(join(here, '..', 'src', 'chat', 'ChatApp.tsx'), 'utf8');

check('declares isAddingRef', chat.includes('const isAddingRef = useRef(false);'));
check('declares inviteBusyRef', chat.includes('const inviteBusyRef = useRef(false);'));

const addRunner = sliceFn(chat, 'const addRunner = async (input: { label: string; address: string; token: string }): Promise<boolean> => {', 'const saveIndependentToken');
const addAndInvite = sliceFn(chat, 'const addAndInvite = async (input: { label: string; address: string; token: string }): Promise<boolean> => {', 'const saveTokenAndInvite');
const inviteExisting = sliceFn(chat, 'const inviteExisting = async (runnerId: string): Promise<boolean> => {', 'const addRunner = async');
const removeRunner = sliceFn(chat, 'const removeRunner = async (runnerId: string) => {', 'const projectFormController');
const saveIndependentToken = sliceFn(chat, 'const saveIndependentToken = async (runnerId: string, nextToken: string): Promise<boolean> => {', 'const addAndInvite = async');
const saveTokenAndInvite = sliceFn(chat, 'const saveTokenAndInvite = async (runnerId: string, nextToken: string): Promise<boolean> => {', 'const removeRunner = async');

check(
  'addRunner claims isAddingRef before registry.add',
  addRunner.includes('if (isAddingRef.current) return false')
    && addRunner.includes('isAddingRef.current = true')
    && addRunner.indexOf('isAddingRef.current = true') < addRunner.indexOf('registry.add('),
);
check(
  'addAndInvite selectedProject returns before claim',
  addAndInvite.includes('if (!selectedProject) return false')
    && addAndInvite.indexOf('if (!selectedProject) return false') < addAndInvite.indexOf('isAddingRef.current = true'),
);
check(
  'addAndInvite claims isAddingRef before registry.add',
  addAndInvite.includes('if (isAddingRef.current) return false')
    && addAndInvite.includes('isAddingRef.current = true')
    && addAndInvite.indexOf('isAddingRef.current = true') < addAndInvite.indexOf('registry.add('),
);
check(
  'inviteExisting selectedProject returns before claim',
  inviteExisting.includes('if (!selectedProject) return false')
    && inviteExisting.indexOf('if (!selectedProject) return false') < inviteExisting.indexOf('inviteBusyRef.current = true'),
);
check(
  'inviteExisting claims inviteBusyRef before mapProject',
  inviteExisting.includes('if (inviteBusyRef.current) return false')
    && inviteExisting.includes('inviteBusyRef.current = true')
    && inviteExisting.indexOf('inviteBusyRef.current = true') < inviteExisting.indexOf('mappingStore.mapProject'),
);
check(
  'removeRunner claims inviteBusyRef before mappingStore.removeRunner',
  removeRunner.includes('if (inviteBusyRef.current) return')
    && removeRunner.includes('inviteBusyRef.current = true')
    && removeRunner.indexOf('inviteBusyRef.current = true') < removeRunner.indexOf('mappingStore.removeRunner'),
);
check(
  'saveIndependentToken claims inviteBusyRef before updateToken',
  saveIndependentToken.includes('if (inviteBusyRef.current) return false')
    && saveIndependentToken.includes('inviteBusyRef.current = true')
    && saveIndependentToken.indexOf('inviteBusyRef.current = true') < saveIndependentToken.indexOf('registry.updateToken'),
);
check(
  'saveTokenAndInvite selectedProject returns before claim',
  saveTokenAndInvite.includes('if (!selectedProject) return false')
    && saveTokenAndInvite.indexOf('if (!selectedProject) return false') < saveTokenAndInvite.indexOf('inviteBusyRef.current = true'),
);
check(
  'saveTokenAndInvite claims inviteBusyRef before updateToken',
  saveTokenAndInvite.includes('inviteBusyRef.current = true')
    && saveTokenAndInvite.indexOf('inviteBusyRef.current = true') < saveTokenAndInvite.indexOf('registry.updateToken'),
);
check(
  'createChatProjectFormController keeps closure busy',
  sliceFn(chat, 'export function createChatProjectFormController', 'export interface ChatProjectFormProps').includes('if (busy) return'),
);

console.log(`\nchatInviteLock.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);
