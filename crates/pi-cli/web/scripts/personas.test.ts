#!/usr/bin/env node
// Focused pure regression for the Personas panel helpers (wire parsing,
// name/content validation, persistence vocabulary, and the Run -> task_spawn
// command builder). Bundled by `npm run build` (esbuild) and executed before
// the production bundle, so a persona wire-shape or spawn-target regression
// fails the build.
import {
  PERSONA_EDIT_MAX_UNITS,
  PURGE_NOTE,
  REMOVE_NOTE,
  buildPersonaCreateContent,
  buildPersonaRunCommand,
  declaredFrontmatterName,
  isEditableContent,
  parsePersonaDetail,
  parsePersonaList,
  parseSpawns,
  persistenceLine,
  sourceLabel,
  validatePersonaName,
} from '../src/personas.ts';

const failures = [];
let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

const row = {
  name: 'mentor',
  description: 'durable mentor',
  source: 'user',
  trusted: true,
  preferred: false,
  contractSummary: 'personality:present',
  memoryEntries: 1,
  sessionCount: 2,
  stateError: null,
};

// ---- parsePersonaList: rows parse, malformed rows fail closed ----
check('valid row parses', parsePersonaList({ personas: [row] }).length === 1);
check(
  'malformed rows are dropped, valid kept',
  parsePersonaList({ personas: [row, { name: 5 }, null, 'x', { name: 'ok' }] }).length === 1,
);
check('non-array personas -> []', parsePersonaList({ personas: 'nope' }).length === 0);
check('null payload -> []', parsePersonaList(null).length === 0);
check('missing personas field -> []', parsePersonaList({}).length === 0);

// ---- parsePersonaDetail: content + truncation flag, malformed -> null ----
const detail = { ...row, content: '---\nname: mentor\ndescription: d\n---\nprompt', contentTruncated: false };
const parsedDetail = parsePersonaDetail(detail);
check('detail parses', parsedDetail !== null && parsedDetail.content.startsWith('---'));
check('detail contentTruncated round-trips', parsePersonaDetail({ ...detail, contentTruncated: true })?.contentTruncated === true);
check('detail without content -> null', parsePersonaDetail({ ...row }) === null);
check('detail with non-string content -> null', parsePersonaDetail({ ...detail, content: 7 }) === null);
check('detail with non-bool flag -> null', parsePersonaDetail({ ...detail, contentTruncated: 'yes' }) === null);

// ---- validatePersonaName (mirrors persona_name_path_safe) ----
check('empty name rejected', validatePersonaName('') !== null);
check('oversized name rejected', validatePersonaName('x'.repeat(65)) !== null);
check('64-char name accepted', validatePersonaName('x'.repeat(64)) === null);
check('spaces rejected', validatePersonaName('my mentor') !== null);
check('dots rejected', validatePersonaName('mentor.v2') !== null);
check('unicode rejected', validatePersonaName('导师') !== null);
check('alnum/underscore/dash accepted', validatePersonaName('mentor_2-x') === null);

// ---- declaredFrontmatterName: mirrors the backend parse_frontmatter+unquote
// (quoted names, comments, CRLF) so a backend-legal file is never wrongly
// rejected by the editor hint ----
const mentorContent = '---\nname: mentor\ndescription: d\n---\nprompt';
check('unquoted name parsed', declaredFrontmatterName(mentorContent) === 'mentor');
check('mismatched name parsed', declaredFrontmatterName('---\nname: other\ndescription: d\n---\nprompt') === 'other');
check('double-quoted name parsed', declaredFrontmatterName('---\nname: "mentor"\ndescription: d\n---\nprompt') === 'mentor');
check('single-quoted name parsed', declaredFrontmatterName("---\nname: 'mentor'\ndescription: d\n---\nprompt") === 'mentor');
check('escaped quote unquoted', declaredFrontmatterName('---\nname: "men\\"tor"\ndescription: d\n---\nprompt') === 'men"tor');
check('inline comment stripped', declaredFrontmatterName('---\nname: mentor # review copy\ndescription: d\n---\nprompt') === 'mentor');
check('comment lines skipped', declaredFrontmatterName('---\n# the name\nname: mentor\ndescription: d\n---\nprompt') === 'mentor');
check('crlf frontmatter parsed', declaredFrontmatterName('---\r\nname: mentor\r\ndescription: d\r\n---\r\nprompt') === 'mentor');
check('indented name is a block child, not the top-level name', declaredFrontmatterName('---\nparent:\n  name: other\n---\nprompt') === null);
check('missing frontmatter -> null', declaredFrontmatterName('just a prompt') === null);
check('empty content -> null', declaredFrontmatterName('') === null);
check('dash/underscore names parsed', declaredFrontmatterName('---\nname: mentor_2-x\ndescription: d\n---\nprompt') === 'mentor_2-x');

// ---- sourceLabel (mirrors persona_source_label) ----
check('user scope label', sourceLabel('user') === 'user scope');
check('project scope label', sourceLabel('project') === 'project scope');
check('bundled label', sourceLabel('bundled') === 'bundled');
check('unknown source passes through', sourceLabel('weird') === 'weird');

// ---- persistenceLine: durable memory/session semantics, never paths ----
check('counts rendered', persistenceLine(row) === 'memory: 1 entry · sessions: 2 archives');
check('plural entries', persistenceLine({ ...row, memoryEntries: 3, sessionCount: 1 }).includes('3 entries'));
check('null counts -> unknown', persistenceLine({ ...row, memoryEntries: null, sessionCount: null }).includes('unknown'));
check('state error collapses to fixed label', persistenceLine({ ...row, stateError: 'unreadable' }) === 'memory/session state unreadable');

// ---- parseSpawns: malformed task_spawn responses fail closed (the Run
// button must error, not fake a success, when no job was produced) ----
check('empty spawns -> []', parseSpawns({ spawns: [] }).length === 0);
check('missing spawns -> []', parseSpawns({}).length === 0);
check('non-array spawns -> []', parseSpawns({ spawns: 'nope' }).length === 0);
check('null payload -> []', parseSpawns(null).length === 0);
check('valid spawn kept with jobId+agentId', JSON.stringify(parseSpawns({ spawns: [{ jobId: 'j1', agentId: 'Mentor' }] })[0]) === JSON.stringify({ jobId: 'j1', agentId: 'Mentor' }));
check('entry without jobId/agentId dropped', parseSpawns({ spawns: [{ bogus: 1 }] }).length === 0);
check('malformed entries dropped, valid kept', parseSpawns({ spawns: [null, 'x', { jobId: 'j2' }] }).length === 1);

// ---- buildPersonaRunCommand: the Web Run button must produce a task spawn
// pointing at the persona AGENT NAME (existing task_spawn path, no heuristic) ----
const runCommand = buildPersonaRunCommand('mentor', 'audit the release notes');
check('run command type is task_spawn', runCommand.type === 'task_spawn');
check(
  'run command carries the persona agent name',
  JSON.stringify(runCommand.args) === JSON.stringify({ task: 'audit the release notes', agent: 'mentor' }),
  JSON.stringify(runCommand),
);

// ---- create seed template declares the name ----
const seed = buildPersonaCreateContent('guide');
check('seed declares the persona name', declaredFrontmatterName(seed) === 'guide');

// ---- remove vs purge vocabulary stays distinct + correct ----
check('remove note mentions keeping memory/sessions', REMOVE_NOTE.includes('stay under the persona root'));
check('purge note mentions deleting the whole root', PURGE_NOTE.includes('whole persona root'));
check('remove and purge notes differ', REMOVE_NOTE !== PURGE_NOTE);

// ---- editability bound: 64 KiB UTF-16 units can never exceed the core 256 KiB byte cap ----
check('small content editable', isEditableContent('x'.repeat(1000)));
check('at-bound content editable', isEditableContent('x'.repeat(PERSONA_EDIT_MAX_UNITS)));
check('over-bound content not editable', !isEditableContent('x'.repeat(PERSONA_EDIT_MAX_UNITS + 1)));

if (failures.length > 0) {
  console.error(`personas: ${failures.length}/${ran} checks FAILED:\n- ${failures.join('\n- ')}`);
  process.exit(1);
}
console.log(`personas: ${ran} checks passed`);
