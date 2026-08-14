#!/usr/bin/env node
// Focused regression for src/commands.ts — the pure catalog helpers behind
// the composer Command picker: wire normalization (filter/dedupe/coerce of
// name/description/source/argumentHint/requiresArguments), the Web-executable
// surface filter, search, the requiresArguments-driven trailing-space rule,
// and the submit-dispatch parse helper. Run through `npm run build`.
//
// Exit codes: 0 = every assertion held; 1 = a regression.
//
// Assertions exercise BEHAVIOR (what the helpers return), not source strings.
// The backend get_commands catalog stays authoritative; the only hardcoded
// surface is the 6-name Web-execution allowlist (compact/skill/code-review/
// loop/goal/ps).
import {
  normalizeCommands,
  filterSupportedCommands,
  filterCommands,
  composeCommandText,
  composeSkillCommandText,
  appendDraft,
  parseSupportedCommand,
  isSkillCandidate,
  primaryCommands,
  skillCandidates,
  pickerIntentFromDraft,
  WEB_SUPPORTED_COMMANDS,
} from '../src/commands.ts';
const failures = [];
let ran = 0;
function check(name, cond, detail) {
  ran += 1;
  if (!cond) failures.push(`${name}${detail ? `: ${detail}` : ''}`);
}

// ---- wire normalization: bad shapes yield [] ----
{
  check('null -> []', normalizeCommands(null).length === 0);
  check('non-object -> []', normalizeCommands('commands').length === 0);
  check('object without commands -> []', normalizeCommands({}).length === 0);
  check('commands not an array -> []', normalizeCommands({ commands: 'x' }).length === 0);
  check('commands null -> []', normalizeCommands({ commands: null }).length === 0);
}

// ---- wire normalization: filters malformed entries, coerces scalar fields ----
{
  const data = {
    commands: [
      { name: 'compact', description: 'Manually compact', source: 'builtin', argumentHint: '[--snap] [instructions]', requiresArguments: false },
      { name: '', description: 'empty name dropped', source: 'builtin' },
      { name: 42, description: 'non-string name dropped', source: 'builtin' },
      { name: 'skill', description: 'Show a skill', source: 'builtin', argumentHint: '<name>', requiresArguments: true },
      null,
      'not-an-object',
      { description: 'missing name dropped' },
      { name: 'code-review', description: 'Browse a diff', source: 'builtin' }, // missing hints -> coerced
    ],
  };
  const out = normalizeCommands(data);
  check('valid entries kept in order', out.map((c) => c.name).join(',') === 'compact,skill,code-review', JSON.stringify(out.map((c) => c.name)));
  check('empty name filtered', !out.some((c) => c.name === ''));
  check('non-string name filtered', !out.some((c) => typeof c.name !== 'string'));
  check('null/non-object entries filtered', out.length === 3);
  // argumentHint + requiresArguments coercion
  check('compact argumentHint preserved', out[0].argumentHint === '[--snap] [instructions]');
  check('compact requiresArguments false', out[0].requiresArguments === false);
  check('skill requiresArguments true', out[1].requiresArguments === true);
  check('skill argumentHint preserved', out[1].argumentHint === '<name>');
  check('missing argumentHint coerced to empty string', out[2].argumentHint === '');
  check('missing requiresArguments coerced to false', out[2].requiresArguments === false);
  // description/source still coerced
  check('valid description preserved', out[0].description === 'Manually compact');
  check('valid source preserved', out[0].source === 'builtin');
}

// ---- wire normalization: requiresArguments only literal true ----
{
  const out = normalizeCommands({ commands: [
    { name: 'a', requiresArguments: true },
    { name: 'b', requiresArguments: false },
    { name: 'c', requiresArguments: 'true' }, // non-bool string -> false
    { name: 'd', requiresArguments: 1 },      // non-bool number -> false
    { name: 'e' },                            // absent -> false
  ] });
  const map = Object.fromEntries(out.map((c) => [c.name, c.requiresArguments]));
  check('literal true -> true', map.a === true);
  check('literal false -> false', map.b === false);
  check('string "true" -> false (strict)', map.c === false);
  check('number 1 -> false (strict)', map.d === false);
  check('absent -> false', map.e === false);
}

// ---- wire normalization: dedupe by name, first wins, backend order kept ----
{
  const data = {
    commands: [
      { name: 'compact', description: 'first', source: 'builtin', requiresArguments: false },
      { name: 'goal', description: 'goal-1', source: 'builtin' },
      { name: 'compact', description: 'duplicate dropped', source: 'builtin' },
      { name: 'skill', description: 'skill-1', source: 'builtin', requiresArguments: true },
    ],
  };
  const out = normalizeCommands(data);
  check('dedupe keeps first occurrence', out.length === 3, `len=${out.length}`);
  check('order preserved after dedupe', out.map((c) => c.name).join(',') === 'compact,goal,skill');
  check('first compact wins (description)', out[0].description === 'first');
}

// ---- non-string description/source/argumentHint coerced, entry still kept ----
{
  const out = normalizeCommands({ commands: [
    { name: 'compact', description: 123, source: { weird: true }, argumentHint: 99 },
  ] });
  check('non-string description coerced to empty', out[0].description === '');
  check('non-string source coerced to empty', out[0].source === '');
  check('non-string argumentHint coerced to empty', out[0].argumentHint === '');
  check('entry still kept', out.length === 1);
}

// ---- Web-executable surface filter: compact/skill/code-review/loop/goal/ps ----
{
  check('allowlist has exactly compact + skill + code-review + loop + goal + ps',
    Object.keys(WEB_SUPPORTED_COMMANDS).sort().join(',') === 'code-review,compact,goal,loop,ps,skill');
  const cmds = normalizeCommands({ commands: [
    { name: 'compact', source: 'builtin', requiresArguments: false },
    { name: 'goal', source: 'builtin' },
    { name: 'skill', source: 'builtin', requiresArguments: true },
    { name: 'code-review', source: 'builtin', requiresArguments: false },
    { name: 'loop', source: 'builtin', requiresArguments: true },
    { name: 'ps', source: 'builtin', requiresArguments: false },
    { name: 'workflow', source: 'builtin' },
    { name: 'btw', source: 'builtin' },
  ] });
  const supported = filterSupportedCommands(cmds);
  check('supported filter keeps the 6', supported.map((c) => c.name).join(',') === 'compact,goal,skill,code-review,loop,ps', JSON.stringify(supported.map((c) => c.name)));
  check('supported filter preserves backend order', supported[0].name === 'compact' && supported[1].name === 'goal' && supported[2].name === 'skill' && supported[3].name === 'code-review' && supported[4].name === 'loop' && supported[5].name === 'ps');
  check('goal kept (dispatch wired)', supported.some((c) => c.name === 'goal'));
  check('loop kept (dispatch wired)', supported.some((c) => c.name === 'loop'));
  check('ps kept (dispatch wired)', supported.some((c) => c.name === 'ps'));
  check('workflow filtered out', !supported.some((c) => c.name === 'workflow'));
  check('supported filter on empty -> []', filterSupportedCommands([]).length === 0);
  // requiresArguments survives the filter (needed for compose below).
  check('skill still requiresArguments after filter', supported.find((c) => c.name === 'skill').requiresArguments === true);
  check('loop still requiresArguments after filter', supported.find((c) => c.name === 'loop').requiresArguments === true);
  check('ps still requiresArguments false after filter', supported.find((c) => c.name === 'ps').requiresArguments === false);
}

// ---- search: empty query returns everything (post-filter) ----
{
  const cmds = filterSupportedCommands(normalizeCommands({ commands: [
    { name: 'compact', description: 'Manually compact session context', source: 'builtin', requiresArguments: false },
    { name: 'skill', description: 'Show a loaded skill', source: 'builtin', requiresArguments: true },
    { name: 'code-review', description: 'Browse a Git diff', source: 'builtin', requiresArguments: false },
  ] }));
  check('empty query -> all supported', filterCommands(cmds, '').length === 3);
  check('whitespace query -> all supported', filterCommands(cmds, '   ').length === 3);
  check('name substring match (compact)', filterCommands(cmds, 'compact').map((c) => c.name).join(',') === 'compact');
  check('name match is case-insensitive', filterCommands(cmds, 'CODE-REVIEW').map((c) => c.name).join(',') === 'code-review');
  check('description substring match (skill via "loaded")', filterCommands(cmds, 'loaded').map((c) => c.name).join(',') === 'skill');
  check('description match is case-insensitive', filterCommands(cmds, 'GIT').map((c) => c.name).join(',') === 'code-review');
  check('no match -> []', filterCommands(cmds, 'nope-xyz').length === 0);
  check('partial name match (code)', filterCommands(cmds, 'code').map((c) => c.name).join(',') === 'code-review');
}

// ---- picker open intent: direct `/skill <query>` search from composer ----
{
  check('plain draft opens commands', JSON.stringify(pickerIntentFromDraft('hello')) === JSON.stringify({ mode: 'commands', query: '' }));
  check('bare /skill opens all skills', JSON.stringify(pickerIntentFromDraft('/skill')) === JSON.stringify({ mode: 'skills', query: '' }));
  check('/skill trailing space opens all skills', JSON.stringify(pickerIntentFromDraft('/skill   ')) === JSON.stringify({ mode: 'skills', query: '' }));
  check('/skill query prefills search', JSON.stringify(pickerIntentFromDraft('/skill research')) === JSON.stringify({ mode: 'skills', query: 'research' }));
  check('/skill query trims whitespace', JSON.stringify(pickerIntentFromDraft('  /skill   code review  ')) === JSON.stringify({ mode: 'skills', query: 'code review' }));
  check('/SKILL is case-insensitive', JSON.stringify(pickerIntentFromDraft('/SKILL Docs')) === JSON.stringify({ mode: 'skills', query: 'Docs' }));
  // The /skill-prefix boundary: ONLY the exact `/skill` command (case-insensitive,
  // optional single-space query tail) opens the skill catalog. Any other
  // `/skill*` spelling (x-suffix, dash) is NOT the skill command and falls
  // back to the primary command list — a typo must never silently prefilter.
  check('/skillx stays commands', JSON.stringify(pickerIntentFromDraft('/skillx research')) === JSON.stringify({ mode: 'commands', query: '' }));
  check('bare /skillx stays commands', JSON.stringify(pickerIntentFromDraft('/skillx')) === JSON.stringify({ mode: 'commands', query: '' }));
  check('/skill-extra stays commands', JSON.stringify(pickerIntentFromDraft('/skill-extra')) === JSON.stringify({ mode: 'commands', query: '' }));
  // Query tails: tab counts as whitespace, internal spaces are preserved
  // (search is substring-based, so `code review` still matches "code review").
  check('/skill tab query opens skills', JSON.stringify(pickerIntentFromDraft('/skill\tresearch')) === JSON.stringify({ mode: 'skills', query: 'research' }));
  check('/skill query keeps internal spaces', JSON.stringify(pickerIntentFromDraft('/skill code review')) === JSON.stringify({ mode: 'skills', query: 'code review' }));
}

{
  const skills = [
    { name: 'skill:greet', description: 'Greeting helper', source: 'skill', argumentHint: '', requiresArguments: false, skillName: 'greet' },
    { name: 'skill:docs', description: 'Documentation helper', source: 'skill', argumentHint: '', requiresArguments: false, skillName: 'docs' },
  ];
  const primary = [
    { name: 'compact', description: 'Compact context', source: 'builtin', argumentHint: '', requiresArguments: false },
  ];
  check('primary typed search finds concrete skill', filterCommands([...primary, ...skills], 'docs').map((entry) => entry.name).join(',') === 'skill:docs');
  check('selected primary skill composes executable command', composeSkillCommandText(filterCommands([...primary, ...skills], 'docs')[0].skillName) === '/skill docs');
}

// ---- trailing-space rule: driven by requiresArguments (catalog authority) ----
{
  // compact: no args -> bare.
  check('/compact (requiresArguments=false) -> bare', composeCommandText('compact', false) === '/compact');
  // skill: requiresArguments=true -> trailing space.
  check('/skill (requiresArguments=true) -> trailing space', composeCommandText('skill', true) === '/skill ');
  // code-review: OPTIONAL args -> bare (no forced trailing space).
  check('/code-review (requiresArguments=false) -> bare', composeCommandText('code-review', false) === '/code-review');
  // Goal/workflow (not supported, but the rule is generic) -> bare.
  check('/goal (requiresArguments=false) -> bare', composeCommandText('goal', false) === '/goal');
  check('/loop (requiresArguments=true) -> trailing space', composeCommandText('loop', true) === '/loop ');
  // ps: no args -> bare (the picker drafts exactly `/ps`).
  check('/ps (requiresArguments=false) -> bare', composeCommandText('ps', false) === '/ps');
  // Already-slashed name is not double-slashed; requiresArguments still applies.
  check('already-slashed compact not double-slashed', composeCommandText('/compact', false) === '/compact');
  check('already-slashed skill keeps trailing space', composeCommandText('/skill', true) === '/skill ');
}

// ---- parseSupportedCommand: submit-dispatch parse helper (no RPC mapping) ----
{
  check('bare /compact', JSON.stringify(parseSupportedCommand('/compact')) === JSON.stringify({ name: 'compact', args: '' }));
  check('/compact --snap -> args', JSON.stringify(parseSupportedCommand('/compact --snap')) === JSON.stringify({ name: 'compact', args: '--snap' }));
  check('/skill foo -> args', JSON.stringify(parseSupportedCommand('/skill foo')) === JSON.stringify({ name: 'skill', args: 'foo' }));
  check('/skill foo bar -> args tail preserved', JSON.stringify(parseSupportedCommand('/skill foo bar')) === JSON.stringify({ name: 'skill', args: 'foo bar' }));
  check('/code-review bare -> empty args', JSON.stringify(parseSupportedCommand('/code-review')) === JSON.stringify({ name: 'code-review', args: '' }));
  check('/code-review HEAD~1 HEAD -> args', JSON.stringify(parseSupportedCommand('/code-review HEAD~1 HEAD')) === JSON.stringify({ name: 'code-review', args: 'HEAD~1 HEAD' }));
  check('leading/trailing whitespace trimmed', JSON.stringify(parseSupportedCommand('  /compact  ')) === JSON.stringify({ name: 'compact', args: '' }));
  check('extra spaces between name and args collapsed', parseSupportedCommand('/skill    foo').args === 'foo');
  // The 5-name Web surface: loop/goal parse like any other supported command;
  // their argument VALIDATION lives in resolveSlashAction (parseSupportedCommand
  // performs no RPC mapping), so a bare /goal still parses.
  check('/loop bare -> parse (dispatch errors on usage)', JSON.stringify(parseSupportedCommand('/loop')) === JSON.stringify({ name: 'loop', args: '' }));
  check('/loop args -> tail preserved', JSON.stringify(parseSupportedCommand('/loop 5m check deploy')) === JSON.stringify({ name: 'loop', args: '5m check deploy' }));
  check('/goal bare -> parse', JSON.stringify(parseSupportedCommand('/goal')) === JSON.stringify({ name: 'goal', args: '' }));
  check('/goal show -> args', JSON.stringify(parseSupportedCommand('/goal show')) === JSON.stringify({ name: 'goal', args: 'show' }));
  check('/goal ship -> args', JSON.stringify(parseSupportedCommand('/goal ship')) === JSON.stringify({ name: 'goal', args: 'ship' }));
  // /ps parses like any other supported command; argument VALIDATION (bare
  // only) lives in resolveSlashAction — parseSupportedCommand performs no
  // RPC mapping, so `/ps extra` still parses with the tail preserved.
  check('/ps bare -> parse', JSON.stringify(parseSupportedCommand('/ps')) === JSON.stringify({ name: 'ps', args: '' }));
  check('/ps extra -> tail preserved (rejected later by resolveSlashAction)', JSON.stringify(parseSupportedCommand('/ps extra')) === JSON.stringify({ name: 'ps', args: 'extra' }));
  // Non-supported commands -> null (Main wires dispatch only for the 6).
  check('/workflow -> null', parseSupportedCommand('/workflow') === null);
  check('plain text -> null', parseSupportedCommand('hello there') === null);
  check('empty -> null', parseSupportedCommand('') === null);
  check('bare slash -> null', parseSupportedCommand('/') === null);
}

// ---- appendDraft: picker draft insertion preserves editable text ----
{
  // The E2E lane's step-3 scenario: an empty composer takes the draft as-is.
  check('empty composer takes draft as-is', appendDraft('', '/code-review') === '/code-review');
  check('whitespace-only composer takes draft as-is', appendDraft('   ', '/code-review') === '   /code-review');
  // Existing draft text is preserved: the command goes on its own line.
  check('non-empty composer gets newline separator', appendDraft('hello', '/code-review') === 'hello\n/code-review');
  check('draft appended after trailing text', appendDraft('line1\nline2', '/compact') === 'line1\nline2\n/compact');
  // A value already ending in a newline never gains a blank line.
  check('trailing newline not doubled', appendDraft('hello\n', '/code-review') === 'hello\n/code-review');
  check('CRLF value keeps single separator', appendDraft('hello\r\n', '/code-review') === 'hello\r\n/code-review');
  check('empty draft appended verbatim', appendDraft('hello', '') === 'hello');
  // Same boundary on the other separator branch: a trailing-newline value is
  // also returned untouched — an empty draft must never inject anything.
  check('empty draft leaves trailing-newline value untouched', appendDraft('hello\n', '') === 'hello\n');
}

// ---- end-to-end: a realistic get_commands payload round-trips through the picker ----
{
  const payload = {
    commands: [
      { name: 'compact', description: 'Manually compact session context', source: 'builtin', argumentHint: '[--snap] [instructions]', requiresArguments: false },
      { name: 'skill', description: "Show a loaded skill's frontmatter summary", source: 'builtin', argumentHint: '<name>', requiresArguments: true },
      { name: 'code-review', description: 'Browse a working-tree or two-revision Git diff in a fullscreen panel', source: 'builtin', argumentHint: '[<from> <to>]', requiresArguments: false },
      { name: 'loop', description: 'Run a prompt on a recurring interval', source: 'builtin', argumentHint: '[list|...|<interval> <prompt>]', requiresArguments: true },
      { name: 'goal', description: 'Create, inspect, pin, or drop the session goal', source: 'builtin', argumentHint: '[show|create ...]', requiresArguments: false },
      { name: 'ps', description: 'List supervised processes', source: 'builtin', argumentHint: null, requiresArguments: false },
    ],
  };
  const cmds = filterSupportedCommands(normalizeCommands(payload));
  // Only the 6 Web-executable commands reach the menu, in backend order.
  check('round-trip: only supported reach menu', cmds.map((c) => c.name).join(',') === 'compact,skill,code-review,loop,goal,ps');
  // Search for "review" lands on code-review alone.
  check('round-trip: search review -> code-review', filterCommands(cmds, 'review').map((c) => c.name).join(',') === 'code-review');
  // Selecting each composes the contract-correct draft text.
  const byName = Object.fromEntries(cmds.map((c) => [c.name, c]));
  check('round-trip: draft compact', composeCommandText(byName.compact.name, byName.compact.requiresArguments) === '/compact');
  check('round-trip: draft skill (trailing space)', composeCommandText(byName.skill.name, byName.skill.requiresArguments) === '/skill ');
  check('round-trip: draft code-review (bare, optional args)', composeCommandText(byName['code-review'].name, byName['code-review'].requiresArguments) === '/code-review');
  // requiresArguments drives the trailing space from the backend field: loop
  // (cannot run bare) drafts with a trailing space, goal (bare = show) drafts bare.
  check('round-trip: draft loop (trailing space)', composeCommandText(byName.loop.name, byName.loop.requiresArguments) === '/loop ');
  check('round-trip: draft goal (bare)', composeCommandText(byName.goal.name, byName.goal.requiresArguments) === '/goal');
  // ps (no args) drafts bare — the picker inserts exactly `/ps`.
  check('round-trip: draft ps (bare)', composeCommandText(byName.ps.name, byName.ps.requiresArguments) === '/ps');
  // Submit parse round-trips the drafted text back to a dispatchable command.
  check('round-trip: parse /compact', JSON.stringify(parseSupportedCommand('/compact')) === JSON.stringify({ name: 'compact', args: '' }));
  check('round-trip: parse /skill foo', JSON.stringify(parseSupportedCommand('/skill foo')) === JSON.stringify({ name: 'skill', args: 'foo' }));
  check('round-trip: parse /code-review', JSON.stringify(parseSupportedCommand('/code-review')) === JSON.stringify({ name: 'code-review', args: '' }));
  check('round-trip: parse /loop 5m probe', JSON.stringify(parseSupportedCommand('/loop 5m probe')) === JSON.stringify({ name: 'loop', args: '5m probe' }));
  check('round-trip: parse /goal', JSON.stringify(parseSupportedCommand('/goal')) === JSON.stringify({ name: 'goal', args: '' }));
  check('round-trip: parse /ps', JSON.stringify(parseSupportedCommand('/ps')) === JSON.stringify({ name: 'ps', args: '' }));
  // Acceptance flow: the selected draft lands in an EMPTY composer as the
  // bare '/code-review' the E2E lane asserts, and still parses back to the
  // dispatchable command (selection never auto-submits).
  const e2eDraft = composeCommandText(byName['code-review'].name, byName['code-review'].requiresArguments);
  check('round-trip: code-review draft into empty composer', appendDraft('', e2eDraft) === '/code-review');
  check('round-trip: inserted draft still parses', JSON.stringify(parseSupportedCommand(appendDraft('', e2eDraft))) === JSON.stringify({ name: 'code-review', args: '' }));
  // Same contract for the loop/goal drafts: selection only drafts, the typed
  // draft still parses back to the dispatchable command.
  check('round-trip: loop draft into empty composer', appendDraft('', composeCommandText(byName.loop.name, byName.loop.requiresArguments)) === '/loop ');
  check('round-trip: goal draft into empty composer', appendDraft('', composeCommandText(byName.goal.name, byName.goal.requiresArguments)) === '/goal');
  check('round-trip: loop draft parses back', JSON.stringify(parseSupportedCommand('/loop ')) === JSON.stringify({ name: 'loop', args: '' }));
  check('round-trip: goal draft parses back', JSON.stringify(parseSupportedCommand('/goal')) === JSON.stringify({ name: 'goal', args: '' }));
  // Same contract for the ps draft: selection only drafts, the bare `/ps`
  // still parses back to the dispatchable command.
  check('round-trip: ps draft into empty composer', appendDraft('', composeCommandText(byName.ps.name, byName.ps.requiresArguments)) === '/ps');
  check('round-trip: ps draft parses back', JSON.stringify(parseSupportedCommand('/ps')) === JSON.stringify({ name: 'ps', args: '' }));
}

// ---- skill candidates: get_commands projects loaded skills with skillName ----
{
  // A realistic executable-catalog payload: builtins + a loaded skill entry
  // (name "skill:greet", source "skill", bare skillName "greet") + a prompt +
  // an extension dynamic command. The picker must keep the supported builtins
  // AND the skill candidate, but drop prompt/extension from the Web surface.
  const payload = {
    commands: [
      { name: 'compact', description: 'Manually compact', source: 'builtin', argumentHint: '[--snap] [instructions]', requiresArguments: false, skillName: null },
      { name: 'skill', description: "Show a loaded skill's frontmatter summary", source: 'builtin', argumentHint: '<name>', requiresArguments: true, skillName: null },
      { name: 'code-review', description: 'Browse a Git diff', source: 'builtin', argumentHint: '[<from> <to>]', requiresArguments: false, skillName: null },
      { name: 'skill:greet', description: 'Greet skill for E2E', source: 'skill', skillName: 'greet' },
      { name: 'skill:research', description: 'Deep-dive codebase researcher', source: 'skill', skillName: 'research' },
      { name: 'prompt-only', description: 'A prompt template', source: 'prompt' },
      { name: 'extension-only', description: 'An extension command', source: 'extension' },
    ],
  };
  const cmds = normalizeCommands(payload);
  // skillName is preserved only on skill entries; null/absent on others.
  const greet = cmds.find((c) => c.name === 'skill:greet');
  check('skill entry kept', !!greet, 'skill:greet missing');
  check('skill entry carries bare skillName', greet && greet.skillName === 'greet', JSON.stringify(greet && greet.skillName));
  check('builtin skill has no skillName', cmds.find((c) => c.name === 'skill').skillName === undefined);
  // A skill entry whose skillName is missing/empty is dropped (unusable candidate).
  const dropped = normalizeCommands({ commands: [
    { name: 'skill:bare', source: 'skill' },               // no skillName
    { name: 'skill:empty', source: 'skill', skillName: '' }, // empty skillName
  ] });
  check('skill without skillName dropped', !dropped.some((c) => c.name === 'skill:bare'));
  check('skill with empty skillName dropped', !dropped.some((c) => c.name === 'skill:empty'));

  // filterSupportedCommands: builtins + skill candidates, NO prompt/extension.
  const supported = filterSupportedCommands(cmds);
  const supportedNames = supported.map((c) => c.name);
  check('supported keeps compact/skill/code-review builtins',
    ['compact', 'skill', 'code-review'].every((n) => supportedNames.includes(n)),
    JSON.stringify(supportedNames));
  check('supported keeps skill candidates',
    ['skill:greet', 'skill:research'].every((n) => supportedNames.includes(n)),
    JSON.stringify(supportedNames));
  check('supported drops prompt-only', !supportedNames.includes('prompt-only'));
  check('supported drops extension-only', !supportedNames.includes('extension-only'));

  // isSkillCandidate / primaryCommands / skillCandidates split the surface.
  check('isSkillCandidate true for skill:greet', isSkillCandidate(greet) === true);
  check('isSkillCandidate false for builtin skill', isSkillCandidate(cmds.find((c) => c.name === 'skill')) === false);
  check('isSkillCandidate false for prompt', isSkillCandidate(cmds.find((c) => c.name === 'prompt-only')) === false);
  const candidates = skillCandidates(supported);
  check('skillCandidates lists only skill entries',
    candidates.length === 2 && candidates.every((c) => c.source === 'skill'),
    JSON.stringify(candidates.map((c) => c.name)));
  const primary = primaryCommands(supported);
  check('primaryCommands excludes skill candidates',
    primary.length === 3 && primary.every((c) => c.source === 'builtin'),
    JSON.stringify(primary.map((c) => c.name)));

  // composeSkillCommandText: `/skill <name>` (no auto-submit).
  check('composeSkillCommandText greet -> /skill greet', composeSkillCommandText('greet') === '/skill greet');
  check('composeSkillCommandText research -> /skill research', composeSkillCommandText('research') === '/skill research');
  // The composed draft round-trips through the submit parse helper.
  check('round-trip: parse /skill greet', JSON.stringify(parseSupportedCommand(composeSkillCommandText('greet'))) === JSON.stringify({ name: 'skill', args: 'greet' }));
}

// ---- skill picker flow: bare /skill -> candidates, filter, select, no-skill ----
{
  // Fixture catalog the picker holds after get_commands resolves.
  const cmds = filterSupportedCommands(normalizeCommands({ commands: [
    { name: 'compact', description: 'Manually compact', source: 'builtin', requiresArguments: false, skillName: null },
    { name: 'skill', description: "Show a loaded skill's frontmatter summary", source: 'builtin', argumentHint: '<name>', requiresArguments: true, skillName: null },
    { name: 'code-review', description: 'Browse a Git diff', source: 'builtin', requiresArguments: false, skillName: null },
    { name: 'skill:greet', description: 'Greet skill for E2E', source: 'skill', skillName: 'greet' },
    { name: 'skill:research', description: 'Deep-dive codebase researcher', source: 'skill', skillName: 'research' },
  ] }));

  // bare /skill: the picker's command-mode list includes the `/skill` parent.
  const primary = primaryCommands(cmds);
  check('bare /skill: parent present in command mode', primary.some((c) => c.name === 'skill'));
  // selecting /skill drills into skill candidates (the picker's skills list).
  const candidates = skillCandidates(cmds);
  check('drill-in: skill candidates exposed', candidates.map((c) => c.skillName).join(',') === 'greet,research',
    JSON.stringify(candidates.map((c) => c.skillName)));

  // filter: empty query returns every candidate; query narrows by name OR description.
  check('skill filter: empty -> all candidates', filterCommands(candidates, '').length === 2);
  check('skill filter: "greet" -> greet only', filterCommands(candidates, 'greet').map((c) => c.skillName).join(',') === 'greet');
  check('skill filter: "research" -> research only', filterCommands(candidates, 'research').map((c) => c.skillName).join(',') === 'research');
  check('skill filter: description match ("Deep-dive") -> research', filterCommands(candidates, 'Deep-dive').map((c) => c.skillName).join(',') === 'research');
  check('skill filter: case-insensitive ("GREET") -> greet', filterCommands(candidates, 'GREET').map((c) => c.skillName).join(',') === 'greet');
  check('skill filter: no match -> []', filterCommands(candidates, 'nope-xyz').length === 0);

  // keyboard/mouse select: selecting a candidate composes `/skill <name>` and
  // the draft round-trips through the submit parse helper (no auto-submit).
  const selected = candidates.find((c) => c.skillName === 'greet');
  const draft = composeSkillCommandText(selected.skillName);
  check('select greet candidate -> /skill greet', draft === '/skill greet');
  check('selected draft parses to skill RPC args', JSON.stringify(parseSupportedCommand(draft)) === JSON.stringify({ name: 'skill', args: 'greet' }));
  // Inserting into an empty composer lands the bare draft (no auto-submit).
  check('selected draft into empty composer', appendDraft('', draft) === '/skill greet');

  // no-skill: a catalog with no loaded skills surfaces an empty candidate list
  // (the picker renders its "No skills loaded" hint).
  const noSkillCmds = filterSupportedCommands(normalizeCommands({ commands: [
    { name: 'compact', description: 'Manually compact', source: 'builtin', requiresArguments: false, skillName: null },
    { name: 'skill', description: 'Show a skill', source: 'builtin', requiresArguments: true, skillName: null },
    { name: 'code-review', description: 'Browse a diff', source: 'builtin', requiresArguments: false, skillName: null },
  ] }));
  check('no-skill: candidate list empty', skillCandidates(noSkillCmds).length === 0);
  check('no-skill: parent /skill still present', primaryCommands(noSkillCmds).some((c) => c.name === 'skill'));
  check('no-skill: filter on empty candidates -> []', filterCommands(skillCandidates(noSkillCmds), '').length === 0);
}

console.log(`\ncommands.test: ${ran} assertions, ${failures.length} failure(s)`);
if (failures.length) {
  console.error(failures.join('\n'));
  process.exit(1);
}
process.exit(0);