import fs from 'node:fs';
import path from 'node:path';
import http from 'node:http';
import crypto from 'node:crypto';
import assert from 'node:assert/strict';
import { createFreshSessionViaManage } from './lib/session-lifecycle.mjs';
import { clickFeature, openSessionsDrawer } from './lib/feature-menu.mjs';
const env = process.env;
const files = { dirty: env.RPI_DIRTY_FILE || 'app.rs', critical: env.RPI_CRIT_FILE || 'crit.rs', overdesign: env.RPI_WARN_FILE || 'warn.rs', naming: env.RPI_INFO_FILE || 'info.rs', preview: env.RPI_PREVIEW_FILE || 'notes.md' };
const IDENTITY_KEYS = ['snapshotId', 'path', 'oldStart', 'oldCount', 'newStart', 'newCount', 'contentHash'];
const FINDINGS_SCHEMA_KEYS = ['category', 'side', 'startLine', 'endLine', 'summary', 'evidence'];
const triageSchema = { type: 'object', additionalProperties: false, required: ['core', 'skipped'], properties: { core: { type: 'array', items: { type: 'string' } }, skipped: { type: 'integer', minimum: 0 } } };
const purposeSchema = { type: 'object', additionalProperties: false, required: ['repositories', 'changes'], properties: { repositories: { type: 'array', items: { type: 'object', additionalProperties: false, required: ['repo', 'purpose'], properties: { repo: { type: 'string', minLength: 1 }, purpose: { type: 'string', minLength: 1 } } } }, changes: { type: 'array', items: { type: 'object', additionalProperties: false, required: ['repo', 'kind', 'id', 'purpose'], properties: { repo: { type: 'string', minLength: 1 }, kind: { type: 'string', enum: ['commit', 'committed_file', 'staged', 'unstaged', 'untracked'] }, id: { type: 'string', minLength: 1 }, purpose: { type: 'string', minLength: 1 } } } } } };
const providerMode = process.argv[2] === '--provider';
const SUMMARY_MARKER = 'Summarize repository changes using the required purpose JSON.\n';
const DESIGNATED_KEYS = '\nDesignated purpose keys JSON:\n';
const SUMMARY_FACTS = '\nRepository facts JSON:\n';
const DESIGNATED_CONTRACT = 'This batch designates exactly these keys.';
const OLD_ALL_KEYS = 'Include every repository, commit, committed file, staged file, unstaged file, and untracked file from the facts exactly once.';
const KIND_ORDER = ['commit', 'committed_file', 'staged', 'unstaged', 'untracked'];
const textDigest = text => crypto.createHash('sha256').update(text, 'utf8').digest('hex');
let summaryTruthCache = null;
function summaryTruth() {
  if (!summaryTruthCache) {
    assert.ok(env.RPI_SUMMARY_TRUTH, 'RPI_SUMMARY_TRUTH is required');
    summaryTruthCache = JSON.parse(fs.readFileSync(env.RPI_SUMMARY_TRUTH, 'utf8'));
  }
  return summaryTruthCache;
}
function largeSpecFor(file) {
  const large = summaryTruth().large;
  return large && file && Object.hasOwn(large.files, file.path) ? large.files[file.path] : null;
}
function largePurpose(file) {
  const spec = largeSpecFor(file);
  if (!spec || file.status !== 'modified' || typeof file.patch !== 'string') return '';
  for (const [key, marker] of [['head', 'HEAD'], ['middle', 'MIDDLE'], ['tail', 'TAIL']]) {
    if (file.patch.indexOf(`+${spec.tag}-${marker}-MARKER`) !== spec.markers[key]) return '';
  }
  return spec.token;
}
function receivedLargeRecords(facts) {
  const records = [];
  for (const repository of facts.repositories) {
    for (const [kind, files] of [['staged', repository.staged], ['unstaged', repository.unstaged], ['untracked', repository.untracked]]) {
      for (const file of files || []) if (largeSpecFor(file)) records.push({ repository, kind, file });
    }
    for (const commit of repository.commits || []) {
      for (const file of commit.files || []) if (largeSpecFor(file)) records.push({ repository, kind: 'committed_file', file });
    }
  }
  return records;
}
function assertLargeRecord(repository, kind, file) {
  const large = summaryTruth().large;
  const spec = largeSpecFor(file);
  assert.equal(repository.label, large.repo, `large record repo ${repository.label}`);
  assert.equal(kind, 'unstaged', `large record kind ${kind}`);
  assert.equal(file.status, 'modified', `large record status ${file.status}`);
  assert.equal(file.previous_path, null, `large record previous path ${file.previous_path}`);
  assert.equal(typeof file.patch, 'string', 'large record patch is missing');
  const bytes = Buffer.byteLength(file.patch, 'utf8');
  assert.equal(bytes, spec.patchBytes, `${file.path} patch byte count`);
  assert.equal(textDigest(file.patch), spec.patchSha256, `${file.path} patch sha256`);
  for (const [key, marker] of [['head', 'HEAD'], ['middle', 'MIDDLE'], ['tail', 'TAIL']]) {
    assert.equal(file.patch.indexOf(`+${spec.tag}-${marker}-MARKER`), spec.markers[key], `${file.path} ${key} marker byte offset`);
  }
  assert.ok(file.patch.endsWith(`+${spec.tag}-TAIL-MARKER`), `${file.path} patch tail was truncated`);
}
function summaryParts(userText) {
  assert.ok(userText.startsWith(SUMMARY_MARKER), 'summary prompt marker is not first');
  const keysAt = userText.indexOf(DESIGNATED_KEYS);
  const factsAt = userText.indexOf(SUMMARY_FACTS);
  assert.ok(keysAt > SUMMARY_MARKER.length && factsAt > keysAt, 'designated keys do not precede repository facts');
  const contract = userText.slice(SUMMARY_MARKER.length, keysAt);
  assert.ok(!contract.includes(OLD_ALL_KEYS), 'old all-keys protocol accepted');
  for (const required of ['"repositories"', '"changes"', 'commit|committed_file|staged|unstaged|untracked', 'OID:displaypath', 'first colon', DESIGNATED_CONTRACT, 'JSON only']) {
    assert.ok(contract.includes(required), `summary contract missing ${required}`);
  }
  assert.ok(!contract.trim().startsWith('{'), 'contract paragraph was parsed as facts');
  const languageAt = contract.lastIndexOf('Write every purpose in the language tagged `');
  if (languageAt >= 0) {
    const languageLine = contract.slice(languageAt);
    assert.ok(!languageLine.includes('\n') && languageLine.endsWith('.'), 'language line is not the last contract line before Designated purpose keys');
  }
  const designated = JSON.parse(userText.slice(keysAt + DESIGNATED_KEYS.length, factsAt).trim());
  const facts = JSON.parse(userText.slice(factsAt + SUMMARY_FACTS.length).trim());
  assert.ok(Array.isArray(designated.repositories), 'designated repositories are missing');
  assert.ok(designated.repositories.every(label => typeof label === 'string' && label.length > 0), 'designated repositories are not label strings');
  assert.ok(Array.isArray(designated.changes), 'designated changes are missing');
  for (const change of designated.changes) {
    assert.deepEqual(Object.keys(change).sort(), ['id', 'kind', 'repo'], `designated change carries extra fields: ${JSON.stringify(change)}`);
    assert.ok(!Object.hasOwn(change, 'purpose'), 'designated change carries a purpose');
  }
  return { contract, designated, facts };
}

function purposeFor(file) {
  const patch = typeof file.patch === 'string' ? file.patch : '';
  const stats = file.lines?.kind === 'text' ? `+${file.lines.insertions}/-${file.lines.deletions}` : file.lines?.kind || 'unknown';
  const previous = file.previous_path ? `${file.previous_path} -> ` : '';
  const large = largePurpose(file);
  const marker = large || (patch.includes('todo!()') ? 'todo' : patch.includes('WARN-CHANGED') ? 'warn' : patch.includes('INFO-BETA') ? 'info' : patch.includes('BETA') ? 'beta' : patch.includes('PREVIEW-EXTRA') ? 'preview' : patch.includes('both work') ? 'work' : patch.includes('both index') ? 'index' : patch.includes('ahead only') ? 'ahead' : '');
  return `${file.status} ${previous}${file.path} ${stats}${marker ? ` ${marker}` : ''}`.trim();
}

function originPurpose(repository) {
  return `${repository.label} ${repository.origin?.outcome || 'unknown'} ahead ${repository.origin?.ahead ?? 0}`;
}

function designatedFromFacts(facts) {
  assert.ok(Array.isArray(facts.repositories) && facts.repositories.length > 0, 'facts contain no repositories');
  const changes = [];
  for (const repository of facts.repositories) {
    assert.match(repository.label, /^root\d+(\/|$)/, `repo label is not root-relative: ${repository.label}`);
    for (const commit of repository.commits || []) {
      changes.push({ repo: repository.label, kind: 'commit', id: commit.oid });
      for (const file of commit.files || []) changes.push({ repo: repository.label, kind: 'committed_file', id: `${commit.oid}:${file.path}` });
    }
    for (const [kind, files] of [['staged', repository.staged], ['unstaged', repository.unstaged], ['untracked', repository.untracked]]) {
      for (const file of files || []) changes.push({ repo: repository.label, kind, id: file.path });
    }
  }
  changes.sort((left, right) => left.repo.localeCompare(right.repo) || KIND_ORDER.indexOf(left.kind) - KIND_ORDER.indexOf(right.kind) || left.id.localeCompare(right.id));
  return { repositories: facts.repositories.map(repository => repository.label), changes };
}

function purposeDocument(facts, designated, purposeOverride) {
  const expected = designatedFromFacts(facts);
  assert.equal(facts.repositories.length, 1, 'summary facts are not one repository');
  const labels = new Map(facts.repositories.map(repository => [repository.label, repository]));
  const repositoryKeys = designated.repositories;
  assert.ok(repositoryKeys.length === 0 || JSON.stringify(repositoryKeys) === JSON.stringify(expected.repositories), 'designated repository key is neither absent nor the repository');
  const owned = new Set(expected.changes.map(item => `${item.repo}\0${item.kind}\0${item.id}`));
  const override = typeof purposeOverride === 'string' && purposeOverride.length > 0 ? purposeOverride : '';
  const repositories = repositoryKeys.map(label => {
    const repository = labels.get(label);
    assert.ok(repository, `designated repository missing from facts: ${label}`);
    const purpose = override || originPurpose(repository);
    assert.equal(purpose.trim(), purpose, `blank repository purpose for ${label}`);
    return { repo: label, purpose };
  });
  const seen = new Set(repositoryKeys.map(label => `${label}\0repository`));
  const changes = designated.changes.map(item => {
    const repository = labels.get(item.repo);
    assert.ok(repository, `designated change repo missing: ${item.repo}`);
    const identity = `${item.repo}\0${item.kind}\0${item.id}`;
    assert.ok(owned.has(identity), `designated change is not an owned fact key: ${identity}`);
    let purpose = override;
    if (!purpose && item.kind === 'commit') {
      const commit = (repository.commits || []).find(record => record.oid === item.id);
      assert.ok(commit, `designated commit missing: ${item.id}`);
      purpose = commit.subject;
    } else if (!purpose && item.kind === 'committed_file') {
      const split = item.id.indexOf(':');
      assert.ok(split > 0 && split < item.id.length - 1, `committed file id is not OID:path: ${item.id}`);
      const commit = (repository.commits || []).find(record => record.oid === item.id.slice(0, split));
      const file = (commit?.files || []).find(record => record.path === item.id.slice(split + 1));
      assert.ok(file, `designated committed file missing: ${item.id}`);
      purpose = purposeFor(file);
    } else if (!purpose) {
      const file = (repository[item.kind] || []).find(record => record.path === item.id);
      assert.ok(file, `designated ${item.kind} missing: ${item.id}`);
      purpose = purposeFor(file);
    }
    assert.equal(typeof purpose, 'string', 'purpose is not text');
    assert.ok(purpose.trim() === purpose && purpose.length > 0, `blank purpose for ${item.kind} ${item.id}`);
    assert.ok(!seen.has(identity), `duplicate purpose key ${identity}`);
    seen.add(identity);
    return { repo: item.repo, kind: item.kind, id: item.id, purpose };
  });
  assert.equal(seen.size, designated.repositories.length + designated.changes.length, 'reply omitted a designated key');
  return JSON.stringify({ repositories, changes });
}

function lastUserText(body) {
  const lastUser = [...body.messages].reverse().find(message => message.role === 'user');
  if (!lastUser) return '';
  if (typeof lastUser.content === 'string') return lastUser.content;
  assert.ok(Array.isArray(lastUser.content), 'message content must be text or typed text blocks');
  return lastUser.content.map(part => {
    assert.equal(part?.type, 'text', 'unexpected non-text model input block');
    assert.equal(typeof part.text, 'string', 'typed text block has no text');
    return part.text;
  }).join('\n');
}

function promptIdentities(text) {
  return text.split('\n').filter(line => line.startsWith('Hunk identity: ')).map(line => {
    const fields = {};
    for (const field of line.slice('Hunk identity: '.length).split(/\s+/)) {
      const cut = field.indexOf('=');
      assert.ok(cut > 0, `identity field lacks '=': ${field}`);
      fields[field.slice(0, cut)] = field.slice(cut + 1);
    }
    assert.deepEqual(Object.keys(fields), IDENTITY_KEYS, `identity line keys: ${line}`);
    for (const key of ['oldStart', 'oldCount', 'newStart', 'newCount']) {
      assert.match(fields[key], /^\d+$/, `${key} is not an integer`);
      fields[key] = Number(fields[key]);
    }
    assert.match(fields.contentHash, /^[0-9a-f]{64}$/, 'contentHash is not 64 lowercase hex');
    assert.ok(fields.snapshotId.length > 0 && fields.path.length > 0, 'identity missing snapshot or path');
    return fields;
  });
}

function numberedRows(text, identity) {
  const marker = `Hunk identity: snapshotId=${identity.snapshotId} path=${identity.path} oldStart=${identity.oldStart} oldCount=${identity.oldCount} newStart=${identity.newStart} newCount=${identity.newCount} contentHash=${identity.contentHash}`;
  const start = text.indexOf(marker);
  assert.ok(start >= 0, `identity block missing for ${identity.path}`);
  const body = text.slice(start + marker.length).split('\n').slice(1);
  const rows = [];
  for (const line of body) {
    if (line.startsWith('Hunk identity: ') || line.startsWith('File: ') && rows.length > 0) break;
    const match = line.match(/^\s*(\d+|-)\s+(\d+|-)\s+([ +\-@])(.*)$/);
    if (!match) continue;
    rows.push({ oldNo: match[1] === '-' ? null : Number(match[1]), newNo: match[2] === '-' ? null : Number(match[2]), marker: match[3], text: match[4] });
  }
  return rows;
}

function finding(category, side, line, summary, evidence) {
  return { category, side, startLine: line, endLine: line, summary, evidence };
}

function findingsFor(identity, rows, mode) {
  const added = rows.filter(row => row.marker === '+').map(row => row.newNo);
  const deleted = rows.filter(row => row.marker === '-').map(row => row.oldNo);
  if (identity.path === files.critical) {
    const line = added.find(number => rows.some(row => row.newNo === number && row.text.includes('todo!()')));
    assert.equal(typeof line, 'number', 'crit.rs todo!() row missing');
    const second = added.find(number => rows.some(row => row.newNo === number && row.text.includes('fn data()')));
    assert.equal(typeof second, 'number', 'crit.rs fn data() row missing');
    assert.notEqual(second, line, 'crit.rs findings share one new row');
    return [
      finding('critical', 'new', line, 'Critical: todo!() has no failure path', 'todo!()'),
      finding('naming', 'new', second, 'Naming: data() renames crit_two', 'fn data() {}'),
    ];
  }
  if (identity.path === files.overdesign) {
    const line = added.find(number => rows.some(row => row.newNo === number && row.text.includes('WARN-CHANGED')));
    assert.equal(typeof line, 'number', 'warn.rs WARN-CHANGED row missing');
    return [finding('overdesign', 'new', line, 'Overdesign: WARN-CHANGED wraps the base', 'WARN-CHANGED')];
  }
  if (identity.path === files.naming && mode === 'highlight') {
    const line = deleted[0];
    assert.equal(typeof line, 'number', 'info.rs deletion row missing');
    return [finding('naming', 'old', line, 'Naming: deleted baseline still names the old side', rows.find(row => row.oldNo === line && row.marker === '-').text.trim())];
  }
  return [];
}

function findingsDocument(text, mode) {
  const identities = promptIdentities(text);
  if (mode === 'auto') assert.equal(identities.length, 1, 'Auto findings prompt must scope exactly one hunk');
  if (mode === 'highlight') {
    const expected = [...Object.values(files), 'added.txt', 'both.rs', 'kept.txt', 'loose.txt'].sort();
    assert.ok(identities.length >= expected.length, 'Highlight scope dropped a fixture hunk');
    assert.deepEqual(identities.map(identity => identity.path).sort(), expected, 'Highlight scope paths');
  }
  return {
    hunks: identities.map(identity => ({
      identity,
      findings: findingsFor(identity, numberedRows(text, identity), mode),
    })),
  };
}

function assertFindingsSchema(schema) {
  assert.equal(schema.type, 'object');
  assert.equal(schema.additionalProperties, false);
  assert.deepEqual(schema.required, ['hunks']);
  const item = schema.properties.hunks.items;
  assert.deepEqual(item.required, ['identity', 'findings']);
  assert.deepEqual(item.properties.identity.required, IDENTITY_KEYS);
  assert.deepEqual(item.properties.findings.items.required, FINDINGS_SCHEMA_KEYS);
  assert.deepEqual(item.properties.findings.items.properties.category.enum, ['critical', 'overdesign', 'naming']);
  assert.deepEqual(item.properties.findings.items.properties.side.enum, ['old', 'new']);
}

function assertNoTools(body) {
  const tools = body.tools;
  assert.ok(tools == null || (Array.isArray(tools) && tools.length === 0), 'TLDR request carried tools');
  assert.ok(body.tool_choice == null || body.tool_choice === 'none', 'TLDR request enabled tool choice');
}

async function provider(portFile) {
  const state = { requests: [], errors: [], held: [], released: [], fail: new Set(), tldrLate: false, tldrMalformed: false, tldrGeneration: 0, tldrSeen: 0 };
  const largeSeenByGeneration = new Map();
  const holds = new Set();
  const pending = new Map();
  const arrivals = [];
  let sequence = 0;
  const server = http.createServer(async (req, res) => {
    try {
      const endpoint = new URL(req.url, 'http://localhost');
      const json = value => { res.writeHead(200, { 'content-type': 'application/json' }); res.end(JSON.stringify(value)); };
      if (endpoint.pathname === '/control/state') return json(state);
      if (endpoint.pathname === '/control/await') {
        const kind = endpoint.searchParams.get('kind');
        const count = Number(endpoint.searchParams.get('count'));
        const generation = endpoint.searchParams.has('generation') ? Number(endpoint.searchParams.get('generation')) : null;
        const batch = endpoint.searchParams.has('batch') ? Number(endpoint.searchParams.get('batch')) : null;
        const ready = () => state.requests.filter(entry => entry.kind === kind && (generation === null || entry.generation === generation) && (batch === null || entry.batch === batch)).length >= count;
        if (!ready()) await new Promise(resolve => arrivals.push({ ready, resolve }));
        return json(state);
      }
      if (endpoint.pathname === '/control/hold') { holds.add(endpoint.searchParams.get('kind')); return json({ ok: true }); }
      if (endpoint.pathname === '/control/fail') {
        const kind = endpoint.searchParams.get('kind');
        if (kind) state.fail.add(kind); else state.fail.clear();
        return json({ ok: true });
      }
      if (endpoint.pathname === '/control/tldr-late') { state.tldrLate = state.tldrGeneration; return json({ ok: true, generation: state.tldrGeneration }); }
      if (endpoint.pathname === '/control/tldr-malformed') { state.tldrMalformed = state.tldrGeneration; return json({ ok: true, generation: state.tldrGeneration }); }
      if (endpoint.pathname === '/control/tldr-generation') {
        state.tldrGeneration += 1;
        state.tldrSeen = 0;
        if (state.tldrLate !== state.tldrGeneration) state.tldrLate = false;
        if (state.tldrMalformed !== state.tldrGeneration) state.tldrMalformed = false;
        return json({ ok: true, generation: state.tldrGeneration });
      }
      if (endpoint.pathname === '/control/release') {
        const kind = endpoint.searchParams.get('kind');
        const one = endpoint.searchParams.get('one') === '1';
        if (!one) holds.delete(kind);
        const ready = [...pending].filter(([, entry]) => entry.kind === kind).sort((left, right) => left[0] - right[0]);
        for (const [id, entry] of one ? ready.slice(0, 1) : ready) {
          pending.delete(id);
          entry.finish();
          state.released.push(id);
        }

        return json({ ok: true });
      }
      assert.equal(req.method, 'POST', `unexpected provider endpoint ${req.method} ${req.url}`);
      assert.match(endpoint.pathname, /\/chat\/completions$/, 'unexpected completion path');
      let raw = '';
      for await (const chunk of req) { raw += chunk; assert.ok(raw.length < 4 * 1024 * 1024, 'provider request exceeds bound'); }
      const body = JSON.parse(raw);
      const schemaName = body.response_format?.json_schema?.name;
      const userText = lastUserText(body);
      let kind, answer, failing = false;
      let large = [];
      const identities = userText.includes('Requested hunks:') ? promptIdentities(userText) : [];
      if (schemaName === 'review_triage') {
        kind = 'triage';
        assert.equal(body.response_format?.type, 'json_schema', 'Stage1 plain-text completion rejected');
        assert.equal(body.response_format.json_schema.strict, true);
        assert.deepEqual(body.response_format.json_schema.schema, triageSchema, 'Stage1 schema differs from strict contract');
        assert.ok(!userText.includes('TRIAGE:'), 'obsolete trailer triage protocol');
        const tokens = [...userText.matchAll(/Hunk: ([^\n]+)/g)].map(match => match[1]);
        assert.deepEqual([...tokens].sort(), [...Object.values(files).map(file => `${file}@${file === files.preview ? '4-4' : '1-1'}`), 'added.txt@0-1', 'both.rs@1-1', 'kept.txt@1-0', 'loose.txt@0-1'].sort(), 'triage must scan all nine exact hunks');
        const core = [files.critical, files.overdesign].map(file => `${file}@1-1`);
        answer = JSON.stringify({ core, skipped: tokens.length - core.length });
        state.core = core;
      } else if (identities.length === 1 && userText.includes('Hunk identity:')) {
        kind = 'auto';
        assert.equal(schemaName, undefined, 'auto hunk agent unexpectedly forced review_findings');
        assert.ok(body.tools == null || body.tools.length > 0, 'auto hunk agent dropped its tools');
        assert.match(userText, /Respond in the language tagged `zh(?:-CN)?`\./, 'configured 中文 missing at auto findings boundary');
        assert.ok(!userText.includes('REVIEW-VERDICT:'), 'retired verdict trailer requested');
        failing = state.fail.has('auto');
        const selected = (state.core || []).filter(token => identities[0].path === token.split('@')[0]);
        assert.equal(selected.length, 1, 'auto review must target exactly one selected core hunk');
        answer = failing ? JSON.stringify({ hunks: [{ identity: { ...identities[0], newStart: identities[0].newStart + 50 }, findings: [] }] }) : JSON.stringify(findingsDocument(userText, 'auto'));
      } else if (schemaName === 'review_findings' && identities.length > 1) {
        kind = 'highlight';
        assert.equal(body.response_format?.type, 'json_schema', 'findings plain-text completion rejected');
        assert.equal(body.response_format.json_schema.strict, true);
        assertFindingsSchema(body.response_format.json_schema.schema);
        assert.deepEqual(body.response_format.json_schema.schema.properties.hunks.items.properties.findings.items.properties.category.enum, ['critical', 'overdesign', 'naming'], 'findings category enum');
        assert.match(userText, /Respond in the language tagged `zh(?:-CN)?`\./, 'configured 中文 missing at highlight findings boundary');
        assert.ok(!userText.includes('Respond with a JSON array'), 'bare highlight array prompt must be gone');
        const mode = state.fail.has('highlight-range') ? 'range' : state.fail.has('highlight') ? 'invalid' : 'ok';
        if (mode === 'invalid') answer = 'deliberately invalid highlight reply';
        else if (mode === 'range') {
          const document = findingsDocument(userText, 'highlight');
          const target = document.hunks.find(hunk => hunk.identity.path === files.critical);
          target.findings[0].endLine = target.identity.newStart + target.identity.newCount + 40;
          answer = JSON.stringify(document);
        } else answer = JSON.stringify(findingsDocument(userText, 'highlight'));
        failing = mode !== 'ok';
      } else if (userText.startsWith(SUMMARY_MARKER) && userText.includes(DESIGNATED_KEYS) && userText.includes(SUMMARY_FACTS)) {
        kind = 'tldr';
        assert.equal(body.response_format?.type, 'json_schema', 'TLDR request omitted the strict purpose schema');
        assert.equal(schemaName, 'review_purpose', 'TLDR request schema was not review_purpose');
        assert.equal(body.response_format.json_schema.strict, true, 'TLDR purpose schema was not strict');
        assert.deepEqual(body.response_format.json_schema.schema, purposeSchema, 'TLDR purpose schema differs from the strict contract');
        assertNoTools(body);
        assert.equal(body.messages.filter(message => message.role === 'user').length, 1, 'TLDR prompt included conversation history');
        assert.ok(!userText.includes('Write one Markdown document'), 'old TLDR prompt accepted');
        assert.ok(!userText.includes(OLD_ALL_KEYS), 'old all-keys protocol accepted');
        assert.ok(!userText.includes('comparisonLabel:'), 'TLDR prompt still carries the selected diff');
        if (state.tldrGeneration === 0) { state.tldrGeneration = 1; state.tldrSeen = 0; }
        state.tldrSeen += 1;
        const summary = summaryParts(userText);
        const receivedLarge = receivedLargeRecords(summary.facts);
        for (const { repository, kind: recordKind, file } of receivedLarge) assertLargeRecord(repository, recordKind, file);
        const priorKeys = largeSeenByGeneration.get(state.tldrGeneration) || [];
        for (const { repository, kind: recordKind, file } of receivedLarge) {
          const identity = `${repository.label}\0${recordKind}\0${file.path}`;
          assert.ok(!priorKeys.includes(identity), `duplicate large record ${identity} in generation ${state.tldrGeneration}`);
          priorKeys.push(identity);
        }
        largeSeenByGeneration.set(state.tldrGeneration, priorKeys);
        large = receivedLarge.map(({ repository, kind: recordKind, file }) => ({
          repo: repository.label,
          kind: recordKind,
          path: file.path,
          patchBytes: Buffer.byteLength(file.patch, 'utf8'),
          patchSha256: textDigest(file.patch),
        }));
        const stale = state.tldrLate === state.tldrGeneration;
        const malformed = state.tldrMalformed === state.tldrGeneration && state.tldrSeen === 1;
        answer = malformed ? '{"repositories":[],"changes":[],"extra":true}' : stale ? purposeDocument(summary.facts, summary.designated, 'STALE-LATE-SNAPSHOT') : purposeDocument(summary.facts, summary.designated);
      } else if (userText.includes('User comment:\nOPS accepted comment')) {
        kind = 'comment';
        answer = Array.from({ length: 90 }, (_, index) => `Comment reply ${index + 1}: accepted follow proof.`).join('\n');
      } else throw new Error(`unexpected model prompt: schema=${schemaName ?? 'none'} lastUser=${userText.slice(0, 180)}`);
      const id = ++sequence;
      state.requests.push({ id, kind, model: body.model, generation: kind === 'tldr' ? state.tldrGeneration : undefined, batch: kind === 'tldr' ? state.tldrSeen : undefined, promptBytes: Buffer.byteLength(userText, 'utf8'), promptSha256: textDigest(userText), large, scope: kind === 'auto' || kind === 'highlight' ? promptIdentities(userText).map(identity => identity.path) : undefined, target: kind === 'auto' ? promptIdentities(userText)[0]?.path : undefined });
      const finish = () => {
        const chunk = content => `data: ${JSON.stringify({ id: `ops-${id}`, object: 'chat.completion.chunk', model: body.model, choices: [{ index: 0, delta: { role: 'assistant', content }, finish_reason: null }] })}\n\n`;
        const terminal = `data: ${JSON.stringify({ id: `ops-${id}`, choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] })}\n\ndata: [DONE]\n\n`;
        res.writeHead(200, { 'content-type': 'text/event-stream' });
        if (kind !== 'comment' || failing) {
          res.end(chunk(answer) + terminal);
          return;
        }
        const splitAt = answer.indexOf('Comment reply 41');
        assert.ok(splitAt > 0, 'streamed reply split marker missing');
        res.write(chunk(answer.slice(0, splitAt)));
        setTimeout(() => res.end(chunk(answer.slice(splitAt)) + terminal), 3000);
      };
      if (holds.has(kind)) {
        state.held.push(id);
        pending.set(id, { kind, finish });
        if (kind === 'tldr') holds.delete(kind);
      } else finish();
      for (const arrival of arrivals) if (arrival.ready()) arrival.resolve();
    } catch (error) {
      console.error(`provider request error: ${error.message}`);
      state.errors.push(error.stack || String(error));
      res.writeHead(400, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ error: { message: error.message } }));
    }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  fs.writeFileSync(portFile, String(server.address().port));
}

function sameIdentity(left, right) {
  return IDENTITY_KEYS.every(key => left?.[key] === right?.[key]);
}

function resultFor(snapshot, file) {
  return (snapshot.reviewFindings || []).find(result => result.identity?.path === file && result.identity.snapshotId === snapshot.snapshotId);
}

async function journey() {
  const { chromium } = await import('playwright');
  for (const key of ['RPI_URL', 'RPI_EVIDENCE', 'RPI_WORKSPACE', 'RPI_ATTACH_FILE']) assert.ok(env[key], `${key} is required`);
  const attachFile = env.RPI_ATTACH_FILE;
  const attachPath = path.join(env.RPI_WORKSPACE, attachFile);
  const pngBytes = fs.readFileSync(attachPath);
  assert.ok(pngBytes.length >= 8 && pngBytes[0] === 0x89 && pngBytes[1] === 0x50 && pngBytes[2] === 0x4e && pngBytes[3] === 0x47, 'RPI_ATTACH_FILE is not a PNG');
  const evidence = env.RPI_EVIDENCE;
  const config = JSON.parse(fs.readFileSync(path.join(env.RPI_WORKSPACE, '../home/.rpi/models.json'), 'utf8'));
  const base = config.providers['user-steering'].baseUrl;
  const control = async (action, kind = '') => {
    const response = await fetch(`${base}/control/${action}?kind=${kind}`, { signal: AbortSignal.timeout(30000) });
    assert.equal(response.status, 200, `provider control ${action}/${kind}`);
    return response.json();
  };
  const report = { status: 'FAILED', screenshots: [], errors: [], provider: null, frames: null };
  const browser = await chromium.launch(env.RPI_CHROME ? { executablePath: env.RPI_CHROME } : {});
  const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
  let phase = 'bootstrap';
  const check = (condition, message) => assert.ok(condition, `${phase}: ${message}`);
  const wait = async (fn, label, arg) => {
    try { await page.waitForFunction(fn, arg, { timeout: 30000 }); }
    catch (error) { throw new Error(`${phase}: ${label}; ${error.message}`); }
  };
  const capture = () => page.evaluate(() => window.__ops);
  const snapshots = frames => frames.received.filter(frame => frame.success === true && frame.data?.autoReviewState).map(frame => frame.data);
  const stateWait = async (field, value, from = 0) => {
    try {
      await page.waitForFunction(({ field, value, from }) => window.__ops.received.slice(from).some(frame =>
        frame.success === true && frame.data?.[field] === value),
      { field, value, from }, { timeout: 30000 });
    } catch (error) {
      const latest = snapshots(await capture()).at(-1);
      const detail = latest?.autoReviewError ?? latest?.highlightError ?? latest?.tldrError ?? 'no error';
      throw new Error(`${phase}: ${field} entered ${latest?.[field] ?? 'no state'} before ${value}: ${typeof detail === 'string' ? detail : JSON.stringify(detail)}; ${error.message}`);
    }
  };
  const mark = async () => page.evaluate(() => { window.__ops.received = []; });
  const holdSnapshots = async () => page.evaluate(() => { window.__ops.holdSnapshots = true; window.__ops.heldSnapshots = []; });
  const releaseSnapshots = async () => page.evaluate(() => {
    const held = window.__ops.heldSnapshots || [];
    window.__ops.holdSnapshots = false; window.__ops.heldSnapshots = [];
    for (const raw of held) window.__opsSocket.dispatchEvent(new MessageEvent('message', { data: raw }));
  });
  const gapEscapeStop = async (operation, closesBefore) => {
    const stopsBefore = (await capture()).sent.filter(frame => frame.type === 'code_review_stop').length;
    await page.evaluate(() => { document.body.tabIndex = -1; document.body.focus(); });
    await page.keyboard.press('Escape');
    await wait(({ stopsBefore, operation }) => {
      const sent = window.__ops.sent.filter(frame => frame.type === 'code_review_stop');
      return sent.length > stopsBefore && sent.at(-1).operation === operation;
    }, 'ACK-gap Escape Stop frame', { stopsBefore, operation });
    const framesNow = await capture();
    assert.equal(framesNow.sent.filter(frame => frame.type === 'code_review_close').length, closesBefore, `${operation} gap Escape closed the panel`);
    check(await page.locator('#code-review-panel').isVisible(), `${operation} gap Escape closed the panel`);
    return framesNow;
  };
  const barrier = async () => {
    const id = `ops-barrier-${Date.now()}`;
    await page.evaluate(id => { const owner = window.__ops.sent.findLast(frame => frame.type === 'code_review_snapshot'); window.__opsSocket.send(JSON.stringify({ type: 'code_review_snapshot', id, sessionId: owner.sessionId })); }, id);
    await wait(id => window.__ops.received.some(frame => frame.id === id && frame.success), 'authoritative post-release snapshot', id);
    return (await capture()).received.find(frame => frame.id === id).data;
  };
  const terminal = async () => {
    await wait(() => !document.querySelector('.code-review__head .code-review__action--stop, .code-review__head [aria-busy="true"], .code-review__streaming-dot, .code-review__thinking-shimmer'), 'terminal DOM still running');
    check(await page.locator('.code-review__head .code-review__action--stop, .code-review__head [aria-busy="true"], .code-review__streaming-dot, .code-review__thinking-shimmer').count() === 0, 'terminal DOM retained activity');
    check(await page.locator('.code-review__error, .code-review__thread-stale, .code-review__window-error, .code-review__preview-error').count() === 0, 'inline review error or stale-thread warning exists');
    check(!(await page.locator('#code-review-panel').innerText()).includes('Key changes'), 'obsolete Key changes card exists');
    check(await page.locator('.code-review__hunk.has-highlight, .code-review__hunk-highlight-summary').count() === 0, 'whole-hunk highlight chrome remains');
  };
  const operationButtonId = operation => operation === 'autoReview' ? '#code-review-auto-review' : operation === 'highlight' ? '#code-review-highlight' : '#code-review-tldr';
  const waitOperationStop = async operation => {
    const id = operationButtonId(operation);
    await wait(id => {
      const el = document.querySelector(id);
      if (!el) return false;
      return (el.className || '').includes('code-review__action--stop') || (el.getAttribute('aria-label') || '').toLowerCase().includes('stop') || (el.textContent || '').includes('Stop');
    }, 'operation Stop after 151ms gate', id);
  };
  const shot = async name => {
    const surfaceState = await page.evaluate(() => {
      const overflow = [...document.querySelectorAll('body, #code-review-panel, .code-review__head, .code-review__composer, #toasts, #toasts .toast, #toasts .toast__message')]
        .filter(element => element.scrollWidth > element.clientWidth + 1)
        .map(element => element.id || element.className || element.tagName);
      const protectedRegions = [
        document.querySelector('.code-review__head'),
        document.querySelector('.code-review__tab-bar'),
        document.querySelector('.code-review-tldr__head'),
        document.querySelector('.code-review__composer'),
      ].filter(Boolean).map(element => element.getBoundingClientRect());
      const toastOverlap = [...document.querySelectorAll('#toasts .toast')].some(toast => {
        const box = toast.getBoundingClientRect();
        return protectedRegions.some(region => box.left < region.right && box.right > region.left && box.top < region.bottom && box.bottom > region.top);
      });
      return { overflow, toastOverlap };
    });
    check(surfaceState.overflow.length === 0, `horizontal overflow: ${JSON.stringify(surfaceState.overflow)}`);
    check(!surfaceState.toastOverlap, 'toast overlaps Code Review header, tabs, summary controls, or composer');
    const filename = path.join(evidence, `${name}.png`);
    await page.screenshot({ path: filename, fullPage: true }); report.screenshots.push(filename);
  };
  const requireActionToast = async (message = 'operations action failure') => {
    await wait(needle => {
      const toast = [...document.querySelectorAll('#toasts .toast--error')].find(el => (el.textContent || '').includes(needle));
      const host = document.getElementById('toasts');
      if (!toast || !host) return false;
      const box = toast.getBoundingClientRect();
      const hostBox = host.getBoundingClientRect();
      const protectedRegions = [
        document.querySelector('.code-review__head'),
        document.querySelector('.code-review__tab-bar'),
        document.querySelector('.code-review-tldr__head'),
        document.querySelector('.code-review__composer'),
      ].filter(Boolean).map(element => element.getBoundingClientRect());
      const overlapsReviewChrome = protectedRegions.some(region => box.left < region.right && box.right > region.left && box.top < region.bottom && box.bottom > region.top);
      const vw = window.innerWidth;
      const vh = window.innerHeight;
      return box.left >= -1 && box.right <= vw + 1 && box.top >= -1 && box.bottom <= vh + 1 && hostBox.left >= -1 && hostBox.right <= vw + 1 && !overlapsReviewChrome;
    }, 'action toast missing, overflowed, or overlapped Code Review chrome', message);
  };
  const requireSummaryHitTargets = async () => {
    const controls = await page.locator('.code-review-tldr__actions button').evaluateAll(buttons => buttons.map(button => {
      const box = button.getBoundingClientRect();
      return {
        label: button.getAttribute('aria-label'),
        reachable: button.contains(document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2)),
      };
    }));
    check(controls.some(control => control.label === 'Close summary'), 'summary Close control missing');
    check(controls.every(control => control.reachable), `summary controls obscured: ${JSON.stringify(controls)}`);
  };
  const pairedShot = async (name, beforeEach) => {
    const scrollHidden = () => {
      const port = document.querySelector('.transcript-scrollport');
      const btn = port?.querySelector('button.transcript-scroll-bottom');
      const foot = document.querySelector('.app-main > footer');
      const prior = document.activeElement;
      btn?.focus();
      const refused = document.activeElement !== btn;
      if (prior instanceof HTMLElement) prior.focus();
      return !!btn && !!foot && refused && getComputedStyle(port).visibility === 'hidden' && getComputedStyle(btn).visibility === 'hidden' && document.activeElement !== btn && getComputedStyle(foot).display === 'none';
    };
    await header();
    if (beforeEach) await beforeEach();
    check(await page.evaluate(scrollHidden), `${name} desktop: scroll-to-bottom visible or focusable during review`);
    await shot(`${name}-desktop`);
    await page.setViewportSize({ width: 390, height: 844 });
    if (await page.locator('.code-review__tab-bar').count()) await page.getByRole('button', { name: 'Diff', exact: true }).click();
    await header();
    if (beforeEach) await beforeEach();
    check(await page.evaluate(scrollHidden), `${name} mobile: scroll-to-bottom visible or focusable during review`);
    await shot(`${name}-mobile`);
    await page.setViewportSize({ width: 1280, height: 900 });
  };
  const proveHover = async id => {
    await page.locator('.code-review__title').hover();
    const before = await page.locator(id).evaluate(el => {
      const style = getComputedStyle(el);
      return { backgroundColor: style.backgroundColor, color: style.color, borderTopColor: style.borderTopColor, borderRightColor: style.borderRightColor, borderBottomColor: style.borderBottomColor, borderLeftColor: style.borderLeftColor };
    });
    await page.locator(id).hover();
    await wait(({ id, before }) => {
      const el = document.querySelector(id);
      if (!el || !el.matches(':hover')) return false;
      const style = getComputedStyle(el);
      return style.backgroundColor !== before.backgroundColor || style.color !== before.color || style.borderTopColor !== before.borderTopColor || style.borderRightColor !== before.borderRightColor || style.borderBottomColor !== before.borderBottomColor || style.borderLeftColor !== before.borderLeftColor;
    }, 'hover computed style unchanged', { id, before });
    await page.locator('.code-review__title').hover();
  };
  const proveFocusRing = async id => {
    await page.locator(id).focus();
    await page.keyboard.press('Tab');
    await page.keyboard.press('Shift+Tab');
    await wait(id => {
      const el = document.querySelector(id);
      if (!el || el !== document.activeElement || !el.matches(':focus-visible')) return false;
      const style = getComputedStyle(el);
      return style.outlineStyle !== 'none' && Number.parseFloat(style.outlineWidth) > 0 && style.outlineColor !== 'transparent' && style.outlineColor !== 'rgba(0, 0, 0, 0)';
    }, 'keyboard focus ring absent', id);
    await page.evaluate(() => { if (document.activeElement instanceof HTMLElement) document.activeElement.blur(); });
  };
  const header = async () => {
    const controls = await page.locator('.code-review__head .code-review__window-controls button').evaluateAll(nodes => nodes.map(node => ({ label: node.getAttribute('aria-label'), box: node.getBoundingClientRect().toJSON() })));
    const labels = controls.map(control => control.label);
    check(labels.length === 3 && /file tree$/.test(labels[0] || '') && /focus mode/i.test(labels[1] || '') && labels[2] === 'Close code review panel', `${phase}: window controls are not tree, focus, and panel close: ${JSON.stringify(labels)}`);
    for (const control of controls) {
      const box = control.box;
      check(control.label && box.width >= 44 && box.height >= 44 && Math.abs(box.width - box.height) <= 1, 'window control lacks accessible label/square 44px geometry');
    }
    for (let index = 0; index < controls.length - 1; index++) check(Math.abs(controls[index].box.y - controls[index + 1].box.y) < 2 && controls[index + 1].box.x >= controls[index].box.x, 'window controls are not in header order');
    if (await page.locator('.code-review-tldr').count()) {
      const closes = await page.locator('#code-review-panel').getByRole('button', { name: /close/i }).evaluateAll(nodes => nodes.map(node => node.getAttribute('aria-label')));
      check(closes.includes('Close summary') && closes.includes('Close code review panel') && new Set(closes).size === closes.length, `summary close is not distinct from panel close: ${JSON.stringify(closes)}`);
    }
    const mobile = (page.viewportSize() || { width: 1280 }).width <= 768;
    if (mobile) {
      const actions = await page.locator('#code-review-auto-review, #code-review-highlight, #code-review-tldr').evaluateAll(nodes => nodes.map(node => node.getBoundingClientRect().toJSON()));
      check(actions.length === 3, 'Auto Review/Highlight/TLDR missing for mobile target size');
      for (const box of actions) check(box.height >= 44, `mobile action target too small: ${JSON.stringify(box)}`);
      const tabs = page.locator('.code-review__tab-bar .code-review__tab');
      if (await tabs.count()) {
        const tabBoxes = await tabs.evaluateAll(nodes => nodes.map(node => node.getBoundingClientRect().toJSON()));
        for (const box of tabBoxes) check(box.height >= 44, `mobile tab target too small: ${JSON.stringify(box)}`);
      }
    }
    await proveHover('#code-review-auto-review');
    await proveFocusRing('#code-review-tree-toggle');
  };
  const select = async file => {
    const focus = page.getByRole('button', { name: 'Exit focus mode', exact: true });
    if (await focus.count() && await focus.isVisible()) await focus.click();
    const expand = page.getByRole('button', { name: 'Expand file tree', exact: true });
    if (await expand.count() && await expand.isVisible()) await expand.click();
    if (page.viewportSize().width < 721) await page.locator('#code-review-panel').getByRole('button', { name: 'Files', exact: true }).click();
    await page.locator(`button.code-review__file[data-file-path="${file}"]`).click();
    await page.locator('button.code-review__hunk-header:not([disabled])').waitFor();
  };
  const openHunk = async file => {
    await select(file);
    const headerButton = page.locator('button.code-review__hunk-header');
    await headerButton.waitFor({ state: 'attached' });
    if ((await headerButton.getAttribute('aria-pressed')) !== 'true') await headerButton.click();
    await page.locator('.code-review__hunk.is-selected').waitFor({ state: 'attached' });
    const toggle = page.locator('.code-review__hunk.is-selected .code-review__hunk-toggle');
    if ((await toggle.getAttribute('aria-expanded')) === 'false') await toggle.click();
    await page.locator('.code-review__hunk.is-selected .code-review__hunk-lines:not(.is-collapsed)').waitFor({ state: 'attached' });
    if ((page.viewportSize()?.width ?? 1280) < 721 && await page.locator('.code-review__tab-bar').count()) {
      await page.getByRole('button', { name: 'Thread', exact: true }).click();
    }
    await page.locator('.code-review__thread').waitFor();
    check((await headerButton.getAttribute('aria-pressed')) === 'true', `${file} hunk was not selected`);
  };
  const assertLangCentered = async () => {
    const lang = page.getByLabel('Auto review reply language');
    const metrics = await lang.evaluate(el => {
      const style = getComputedStyle(el);
      const box = el.getBoundingClientRect();
      return { textAlign: style.textAlign, left: box.left, right: box.right, top: box.top, bottom: box.bottom, width: box.width, height: box.height };
    });
    const viewport = page.viewportSize() || { width: 1280, height: 900 };
    check(metrics.textAlign === 'center', `Chinese selector text not centered: ${JSON.stringify(metrics)}`);
    check(metrics.left >= 0 && metrics.right <= viewport.width + 1 && metrics.top >= 0 && metrics.bottom <= viewport.height + 1, `Chinese selector geometry escaped viewport: ${JSON.stringify(metrics)}`);
  };
  const issueButtons = () => page.getByRole('group', { name: 'Issue navigation' }).getByRole('button');
  const issuePosition = () => page.getByRole('group', { name: 'Issue navigation' }).locator('.code-review__hunk-nav-position');
  const assertRowMarks = async (file, finding) => {
    const hunk = page.locator('.code-review__hunk');
    const marked = await hunk.locator('.code-review__line.has-finding--danger, .code-review__line.has-finding--warn, .code-review__line.has-finding--info').evaluateAll(nodes => nodes.map(node => ({
      oldNo: node.querySelector('.code-review__line-no--old')?.textContent.trim() || '',
      newNo: node.querySelector('.code-review__line-no--new')?.textContent.trim() || '',
      selected: node.classList.contains('is-finding-selected'),
      text: node.querySelector('.code-review__line-text')?.textContent || '',
    })));
    const expectedNo = String(finding.startLine);
    const matching = marked.filter(row => (finding.side === 'old' ? row.oldNo : row.newNo) === expectedNo);
    check(matching.length === 1 && matching[0].selected, `${file} ${finding.summary} row mark missing or unselected: ${JSON.stringify(marked)}`);
    const foreign = marked.filter(row => (finding.side === 'old' ? row.oldNo : row.newNo) !== expectedNo && row.selected);
    check(foreign.length === 0, `${file} selected a row outside the finding: ${JSON.stringify(foreign)}`);
    const lineCount = await hunk.locator('.code-review__line--addition, .code-review__line--deletion, .code-review__line--context').count();
    check(marked.length < lineCount, `${file} tinted every hunk row`);
  };
  const assertFindingDetail = async finding => {
    const selected = page.locator('.code-review__finding.is-selected');
    await selected.waitFor();
    const text = await selected.innerText();
    check(text.includes(`${finding.category} · ${finding.side} L${finding.startLine}–${finding.endLine}`), `detail meta mismatch: ${text}`);
    check(text.includes(finding.summary) && text.includes(finding.evidence), `detail body mismatch: ${text}`);
    check(await page.locator('.code-review__finding.is-selected').count() === 1, 'more than one finding detail selected');
  };
  const assertNoWarningInfo = async snapshot => {
    const serialized = JSON.stringify(snapshot.reviewFindings || []);
    check(!/"(?:warning|info|clean)"/.test(serialized), `warning/info/clean leaked into findings: ${serialized}`);

  };
  try {
    page.on('pageerror', error => report.errors.push(`page: ${error.message}`));
    page.on('console', message => { if (message.type() === 'error') report.errors.push(`console: ${message.text()}`); });
    await page.addInitScript(token => {
      localStorage.setItem('rpi-web-token', token); localStorage.setItem(`rpi-web-token:${location.host}`, token);
      const Native = WebSocket, frames = { sent: [], received: [], allReceived: [], parseErrors: [], toasts: [], failNext: false, failSnapshots: 0, holdSnapshots: false, heldSnapshots: [], snapshotSendOwners: [] };
      class Traced extends Native {
        constructor(...args) {
          super(...args);
          window.__opsSocket = this;
          this.addEventListener('message', event => {
            try {
              const frame = JSON.parse(event.data);
              if (frames.holdSnapshots && frame.command === 'code_review_snapshot') {
                frames.heldSnapshots.push(event.data);
                event.stopImmediatePropagation();
                return;
              }
              frames.received.push(frame); frames.allReceived.push(frame);
            } catch (error) { frames.parseErrors.push(String(error)); }
          });
        }
        send(raw) {
          const frame = JSON.parse(raw); frames.sent.push(frame);
          if (frame.type === 'code_review_snapshot') frames.snapshotSendOwners.push({ id: frame.id, callback: window.__currentSendCallback ?? null });
          if (frames.failNext && frame.type === 'code_review_highlight') {
            frames.failNext = false;
            queueMicrotask(() => this.dispatchEvent(new MessageEvent('message', { data: JSON.stringify({ type: 'response', id: frame.id, command: frame.type, success: false, error: 'operations action failure', sessionId: frame.sessionId }) })));
            return;
          }
          // INJECTED client fault: a non-not-open failure for this REAL poll id.
          if (frames.failSnapshots > 0 && frame.type === 'code_review_snapshot') {
            frames.failSnapshots -= 1;
            queueMicrotask(() => this.dispatchEvent(new MessageEvent('message', { data: JSON.stringify({ type: 'response', id: frame.id, command: frame.type, success: false, error: 'injected client snapshot failure', sessionId: frame.sessionId }) })));
            return;
          }
          return super.send(raw);
        }
      }
      window.WebSocket = Traced; window.__ops = frames;
      new MutationObserver(records => { for (const record of records) for (const node of record.addedNodes) if (node instanceof Element) for (const toast of [node, ...node.querySelectorAll('.toast')]) if (toast.matches('.toast')) frames.toasts.push(toast.textContent); }).observe(document, { childList: true, subtree: true });
    }, env.RPI_TOKEN || '');
    await page.goto(env.RPI_URL, { waitUntil: 'domcontentloaded' });
    await page.locator('#send-btn:not([disabled])').waitFor();
    await page.fill('#prompt-input', '/review HEAD'); await page.press('#prompt-input', 'Enter');
    await page.locator('#code-review-panel').waitFor(); await select(files.critical);
    check(await page.evaluate(() => document.getElementById('prompt-input')?.value === ''), 'typed /review open did not consume the command draft — the command-origin contract broke');
    await page.locator('button.code-review__hunk-header').click();
    const language = page.getByLabel('Auto review reply language');
    check(await language.locator('option:checked').innerText() === '中文', '中文 not preselected');
    await language.click(); await language.selectOption({ label: '中文' });
    const model = page.getByLabel('Review reply model');
    await wait(() => {
      const el = document.querySelector('.code-review__model-select');
      const snap = window.__ops.received.findLast(frame => frame.success && frame.data?.reviewModelProvider !== undefined);
      return el && el.value === 'user-steering/mock' && snap && snap.data.reviewModelProvider === 'user-steering' && snap.data.reviewModelId === 'mock';
    }, 'default review model identity not selected');
    const defaultModel = await model.evaluate(el => ({ value: el.value, label: el.selectedOptions[0]?.textContent ?? '' }));
    check(defaultModel.value === 'user-steering/mock', `default model identity wrong: ${JSON.stringify(defaultModel)}`);
    check(defaultModel.label === 'Steering Mock', `default model display label wrong: ${JSON.stringify(defaultModel)}`);
    await assertLangCentered();
    await model.click();
    const modelValue = await model.locator('option').evaluateAll(options => options.find(option => option.value.includes('mock-2'))?.value);
    check(modelValue, 'second fixture model absent'); await model.selectOption(modelValue);
    await wait(() => window.__ops.received.some(frame => frame.command === 'code_review_set_model' && frame.success), 'model click ack');
    const switched = await model.evaluate(el => ({ value: el.value, label: el.selectedOptions[0]?.textContent ?? '' }));
    check(switched.value === 'user-steering/mock-2', `model switch identity wrong: ${JSON.stringify(switched)}`);
    check(switched.label === 'Steering Mock Two', `model switch display label wrong: ${JSON.stringify(switched)}`);
    await header();

    phase = 'two-stage'; await control('hold', 'triage'); await control('hold', 'auto'); await mark();
    await page.locator('#code-review-auto-review').click(); await stateWait('autoReviewState', 'triaging');
    await wait(() => document.querySelector('.code-review__head .code-review__auto-progress')?.textContent?.trim() === 'Screening…', 'Screening… progress absent');
    await control('await', 'triage&count=1'); await control('release', 'triage'); await stateWait('autoReviewState', 'running');
    let frames = await capture(), running = snapshots(frames).find(snap => snap.autoReviewState === 'running');
    assert.deepEqual(running.autoReviewCounts, { submitted: 2, completed: 0, failed: 0 }, 'Stage2 exact initial counts');
    await wait(() => document.querySelector('.code-review__head .code-review__auto-progress')?.textContent?.trim() === 'Review 0/2', 'Review 0/2 progress absent');
    const assertThreadFollowsTail = async surface => {
      await wait(() => {
        const thread = document.querySelector('.code-review__thread');
        if (!thread || thread.scrollHeight <= thread.clientHeight) return false;
        return Math.abs(thread.scrollHeight - thread.clientHeight - thread.scrollTop) <= 1;
      }, `${surface} thread did not follow overflowing tail`);
      const metrics = await page.locator('.code-review__thread').evaluate(node => ({ bottom: node.scrollHeight - node.clientHeight, scrollTop: node.scrollTop }));
      check(Math.abs(metrics.bottom - metrics.scrollTop) <= 1, `${surface} thread did not follow tail: ${JSON.stringify(metrics)}`);
    };
    await control('await', 'auto&count=2'); await control('release', 'auto');
    await stateWait('autoReviewState', 'done'); await terminal();
    frames = await capture(); const done = snapshots(frames).findLast(snap => snap.autoReviewState === 'done');
    assert.deepEqual(done.autoReviewCounts, { submitted: 2, completed: 2, failed: 0 }, 'Stage2 terminal counts');
    const autoCritical = resultFor(done, files.critical);
    const autoOverdesign = resultFor(done, files.overdesign);
    check(autoCritical && autoCritical.findings.length === 2, 'auto crit.rs did not publish two findings');
    check(autoOverdesign && autoOverdesign.findings.length === 1, 'auto warn.rs did not publish its finding');
    check(autoCritical.findings.every(item => item.side === 'new') && autoOverdesign.findings[0].category === 'overdesign', 'auto findings lost category or side');
    await assertNoWarningInfo(done);
    assert.deepEqual(done.threads.map(thread => thread.identity.path).sort(), [files.critical, files.overdesign].sort(), 'Stage2 reviewed non-selected hunks');
    await openHunk(files.critical);
    await page.locator('.code-review__thread').evaluate(node => { node.scrollTop = 0; node.dispatchEvent(new Event('scroll')); });
    const frozenThread = await page.locator('.code-review__thread').evaluate(node => ({ bottom: node.scrollHeight - node.clientHeight, scrollTop: node.scrollTop }));
    check(frozenThread.scrollTop <= 1, `manual thread scroll was stolen before follow proof: ${JSON.stringify(frozenThread)}`);
    phase = 'thread-follow';
    await page.locator('.code-review__thread').evaluate(node => { node.scrollTop = node.scrollHeight; node.dispatchEvent(new Event('scroll')); });
    await page.setViewportSize({ width: 1280, height: 860 });
    if (frozenThread.bottom > 1) await assertThreadFollowsTail('desktop resumed');
    await page.locator('.code-review__thread').evaluate(node => { node.scrollTop = 0; node.dispatchEvent(new Event('scroll')); });
    await openHunk(files.overdesign);
    await shot('01-thread-follow-desktop');
    await page.setViewportSize({ width: 390, height: 844 });
    await header();
    await page.getByRole('button', { name: 'Thread', exact: true }).click();
    await page.locator('.code-review__thread').evaluate(node => { node.scrollTop = 0; node.dispatchEvent(new Event('scroll')); });
    await openHunk(files.critical);
    await page.getByRole('button', { name: 'Thread', exact: true }).click();
    await shot('01-thread-follow-mobile');
    await page.setViewportSize({ width: 1280, height: 900 });
    await header();
    await pairedShot('01-triage');

    phase = 'batch-failure';
    const lastGood = done.reviewFindings;
    await control('hold', 'auto'); await control('fail', 'auto'); await control('hold', 'triage'); await mark();
    await page.locator('#code-review-auto-review').click();
    await stateWait('autoReviewState', 'triaging');
    await control('await', 'triage&count=2'); await control('release', 'triage'); await stateWait('autoReviewState', 'running');
    await page.getByRole('button', { name: 'Close code review panel', exact: true }).click();
    await page.locator('.code-review__confirm').waitFor();
    check((await page.locator('.code-review__confirm').innerText()).includes('in-flight review work'), 'close warning missing during triage');
    await page.getByRole('button', { name: 'Keep editing' }).click();
    await page.locator('.code-review__confirm').waitFor({ state: 'detached' });
    await page.locator('#code-review-panel button[title="Reload the diff snapshot"]').click();
    await page.locator('.code-review__confirm').waitFor();
    check((await page.locator('.code-review__confirm').innerText()).includes('Review work is still in flight'), 'refresh warning missing during triage');
    await page.getByRole('button', { name: 'Keep editing' }).click();
    await page.locator('.code-review__confirm').waitFor({ state: 'detached' });
    await control('release', 'auto'); await stateWait('autoReviewState', 'failed');
    await page.locator('#toasts .toast--error').filter({ hasText: 'Auto review' }).waitFor();
    const batchFailureToasts = (await capture()).toasts.length;
    const preserved = await barrier();
    await terminal();
    check((await capture()).toasts.length === batchFailureToasts, 'batch failure emitted duplicate toasts');
    check(await page.locator('.code-review__error, .code-review__thread-stale').count() === 0, 'batch failure leaked an inline error node');
    assert.equal(JSON.stringify(preserved.reviewFindings), JSON.stringify(lastGood), 'invalid auto range replaced last-good findings');
    await pairedShot('07-batch-failure');
    await control('fail', '');

    phase = 'highlight'; await mark(); await page.locator('#code-review-highlight').click(); await stateWait('highlightState', 'done');
    const painted = snapshots(await capture()).findLast(snap => snap.highlightState === 'done');
    check(painted.highlightHunks == null && painted.autoReview == null, 'aggregate highlight metadata remains');
    const highlighted = painted.reviewFindings.filter(result => result.findings.length > 0);
    assert.equal(highlighted.length, 3, 'exactly three non-empty highlight identities');
    const empty = painted.reviewFindings.filter(result => result.findings.length === 0);
    check(['added.txt', 'both.rs', 'kept.txt', 'loose.txt', files.dirty, files.preview].every(path => empty.some(result => result.identity.path === path)), 'Highlight omitted a clean scoped hunk');
    for (const result of painted.reviewFindings) {
      assert.deepEqual(Object.keys(result.identity).sort(), [...IDENTITY_KEYS].sort(), `${result.identity.path} identity fields`);
      assert.equal(result.identity.snapshotId, painted.snapshotId, `${result.identity.path} snapshot id`);
      assert.match(result.identity.contentHash, /^[0-9a-f]{64}$/, `${result.identity.path} content hash`);
    }
    const crit = resultFor(painted, files.critical);
    const warn = resultFor(painted, files.overdesign);
    const info = resultFor(painted, files.naming);
    assert.equal(crit.findings.length, 2, 'same-hunk pair missing');
    assert.equal(warn.findings.length, 1, 'other-file finding missing');
    assert.equal(info.findings[0].side, 'old', 'old-side deletion was not published');
    await assertNoWarningInfo(painted);
    const issueOrder = [];
    await openHunk(files.critical);
    check(await issueButtons().nth(0).isEnabled(), 'previous issue disabled before a selection');
    check(await issueButtons().nth(1).isEnabled(), 'next issue disabled before a selection');
    await issueButtons().nth(0).click();
    check((await issuePosition().innerText()).trim() === '4 / 4', 'unselected previous did not land on the last finding');
    const lastDetail = await page.locator('.code-review__finding.is-selected').innerText();
    check(lastDetail.includes(warn.findings[0].summary), `unselected previous selected ${lastDetail}`);
    for (let step = 0; step < 3; step++) await issueButtons().nth(0).click();
    check((await issuePosition().innerText()).trim() === '1 / 4', 'previous did not walk back to the first finding');
    check(await issueButtons().nth(0).isDisabled(), 'previous stayed enabled on the first finding');
    for (let index = 0; index < 4; index++) {
      const position = (await issuePosition().innerText()).trim();
      check(position === `${index + 1} / 4`, `issue position ${position}`);
      const detail = await page.locator('.code-review__finding.is-selected').innerText();
      issueOrder.push(detail.split('\n')[1]);
      const currentFile = await page.locator('.code-review__diff-path').innerText();
      const finding = painted.reviewFindings.flatMap(result => result.findings.map(item => ({ ...item, path: result.identity.path }))).find(item => detail.includes(item.summary));
      check(finding && currentFile.includes(finding.path), `selection did not open ${finding?.path}`);
      await assertRowMarks(finding.path, finding);
      await assertFindingDetail(finding);
      if (index < 3) {
        check(await issueButtons().nth(1).isEnabled(), 'next disabled before the last finding');
        await issueButtons().nth(1).click();
      }
    }
    check(await issueButtons().nth(1).isDisabled(), 'next issue wrapped past the last finding');
    await issueButtons().nth(0).click();
    check((await issuePosition().innerText()).trim() === '3 / 4', 'previous did not step back one');
    await issueButtons().nth(0).click();
    await issueButtons().nth(0).click();
    check((await issuePosition().innerText()).trim() === '1 / 4', 'previous did not return to the first finding');
    check(await issueButtons().nth(0).isDisabled(), 'previous issue wrapped past the first finding');
    assert.deepEqual(issueOrder, [
      crit.findings[0].summary,
      crit.findings[1].summary,
      info.findings[0].summary,
      warn.findings[0].summary,
    ], `navigation left snapshot file order: ${JSON.stringify(issueOrder)}`);
    await pairedShot('02-findings');
    await terminal();

    phase = 'highlight-reject';
    const highlightGood = painted.reviewFindings;
    await control('fail', 'highlight-range'); await mark();
    await page.locator('#code-review-highlight').click();
    await stateWait('highlightState', 'failed');
    await page.locator('#toasts .toast--error').filter({ hasText: 'Highlight:' }).waitFor();
    const rejected = await barrier();
    assert.equal(rejected.highlightState, 'failed', 'invalid range did not fail the highlight');
    assert.equal(JSON.stringify(rejected.reviewFindings), JSON.stringify(highlightGood), 'out-of-range highlight replaced last-good findings');
    check((await capture()).toasts.filter(text => text.includes('Highlight:')).length === 1, 'invalid highlight emitted more than one error');
    await control('fail', '');
    await terminal();

    phase = 'tldr';
    let summaryGeneration = 0;
    const expectedSummaryBatches = new Map();
    const beginSummary = async () => {
      const advanced = await control('tldr-generation');
      summaryGeneration = advanced.generation;
      await control('hold', 'tldr');
      return summaryGeneration;
    };
    const awaitSummary = async (count, generation = summaryGeneration) => control('await', `tldr&count=${count}&generation=${generation}`);
    const releaseOpenSummary = async generation => {
      const open = (await control('state')).requests.filter(entry => entry.kind === 'tldr' && entry.generation === generation);
      assert.ok(open.length >= 1, `summary generation ${generation} had no open request`);
      for (const entry of open) {
        await control('release', 'tldr&one=1');
      }
      expectedSummaryBatches.set(generation, open.length);
    };
    const releaseSummary = async () => {
      const generation = summaryGeneration;
      const expected = 5;
      for (let batch = 1; batch <= expected; batch += 1) {
        await control('await', `tldr&count=1&generation=${generation}&batch=${batch}`);
        const seen = (await control('state')).requests.filter(entry => entry.kind === 'tldr' && entry.generation === generation).map(entry => entry.batch);
        assert.deepEqual(seen, Array.from({ length: batch }, (_, index) => index + 1), `summary generation ${generation} batch ${batch} was not next`);
        if (batch < expected) await control('hold', 'tldr');
        await control('release', 'tldr&one=1');
      }
      expectedSummaryBatches.set(generation, expected);
    };
    await mark();
    const stoppedGeneration = await beginSummary();
    await page.locator('#code-review-tldr').click();
    await wait(command => window.__ops.received.some(frame => frame.success && frame.command === command && frame.data?.state === 'running' && typeof frame.data?.tldrGeneration === 'number'), 'TLDR start ack', 'code_review_tldr');
    await stateWait('tldrState', 'running');
    await page.locator('.code-review-tldr').waitFor();
    check(await page.locator('.code-review-tldr').getAttribute('data-tldr-state') === 'running', 'TLDR pane did not open running');
    await awaitSummary(1, stoppedGeneration);
    await pairedShot('09-tldr-running', async () => {
      await requireActionToast('Highlight:');
      await requireSummaryHitTargets();
      check(await page.getByRole('button', { name: 'Stop generating summary', exact: true }).count() === 1, 'summary Stop control missing');
    });
    const tldrFrom = (await capture()).received.length;
    await page.locator('.code-review-tldr').getByRole('button', { name: 'Stop generating summary', exact: true }).click();
    await requireActionToast('Highlight:');
    await stateWait('tldrState', 'idle', tldrFrom);
    await control('tldr-late');
    await releaseOpenSummary(stoppedGeneration);
    const stoppedTldr = await barrier();
    assert.equal(stoppedTldr.tldrState, 'idle', 'late TLDR revived a stopped empty summary');
    assert.equal(stoppedTldr.tldrMarkdown, '', 'late TLDR snapshot painted markdown');
    const stoppedBody = (await page.locator('.code-review-tldr__body').innerText()).trim();
    check(stoppedBody === '', `stopped TLDR painted markdown: ${stoppedBody}`);
    check(!stoppedBody.includes('STALE-LATE-SNAPSHOT'), 'late TLDR marker painted over the empty same-snapshot summary');
    await mark();
    await beginSummary();
    await page.locator('#code-review-tldr').click();
    await releaseSummary();
    await stateWait('tldrState', 'done');
    const summary = page.locator('.code-review-tldr');
    await summary.waitFor();
    check(await summary.getAttribute('data-tldr-state') === 'done', 'TLDR did not settle done');
    const viewport = summary.locator('.code-review-tldr__viewport');
    check(await viewport.getAttribute('data-expanded') === 'true', 'TLDR did not open expanded');
    check(await summary.getByRole('button', { name: 'Collapse details', exact: true }).getAttribute('aria-expanded') === 'true', 'open TLDR control is not Collapse details');
    const openedText = await summary.locator('.code-review-tldr__body').innerText();
    const gitTruth = JSON.parse(fs.readFileSync(path.join(env.RPI_WORKSPACE, '../summary-git-truth.json'), 'utf8'));
    const repoHeadings = await summary.locator('.code-review-tldr__body h2').evaluateAll(nodes => nodes.map(node => (node.textContent || '').trim()));
    assert.deepEqual(repoHeadings, gitTruth.labels, `summary repo headings: ${JSON.stringify(repoHeadings)}`);
    check(openedText.includes(`origin/review-base ahead ${gitTruth.primary.ahead}`), 'summary ahead count is not git truth');
    check(openedText.includes(gitTruth.primary.head) && openedText.includes(gitTruth.primary.origin), 'summary omitted pinned OIDs');
    check(openedText.includes('no origin'), 'bare repo did not render no origin');
    const stagedNeedle = `M both.rs ${gitTruth.both.staged}`;
    const unstagedNeedle = `M both.rs ${gitTruth.both.unstaged}`;
    if (stagedNeedle === unstagedNeedle) check(openedText.split(stagedNeedle).length - 1 >= 2, 'both.rs staged and unstaged counts collapsed');
    else check(openedText.includes(stagedNeedle) && openedText.includes(unstagedNeedle), 'both.rs layer counts are not git numstat');
    check(openedText.includes('work') && openedText.includes('index'), 'both.rs layer purposes missing');
    check(openedText.includes(`A added.txt ${gitTruth.added}`) && openedText.includes(`D kept.txt ${gitTruth.deleted}`) && openedText.includes('R old-name.txt -> new-name.txt'), 'added deleted rename counts missing');
    for (const name of ['left.txt', 'right.txt']) {
      const spec = gitTruth.large.files[name];
      check(openedText.includes(`M ${name} ${spec.numstat}`), `large ${name} numstat missing from the summary`);
      check(openedText.includes(spec.token), `large ${name} patch-derived purpose missing from the summary`);
    }
    const layers = await summary.locator('.code-review-tldr__body').evaluate(node => {
      const sections = { Commits: '', Staged: '', Unstaged: '' };
      let repo = '';
      let bucket = '';
      for (const element of node.querySelectorAll('h2, h3, li, p')) {
        const text = (element.textContent || '').trim();
        if (element.tagName === 'H2') { repo = text; bucket = ''; continue; }
        if (repo !== 'root0') continue;
        if (element.tagName === 'H3') {
          if (text === 'Commits') bucket = 'Commits';
          else if (text === 'Staged' || text === 'Unstaged') bucket = text;
          else if (text === 'Untracked') bucket = '';
          continue;
        }
        if (bucket) sections[bucket] += `\n${element.textContent || ''}`;
      }
      return sections;
    });
    check(layers.Staged.includes(stagedNeedle), 'Staged section lacks the staged both.rs count');
    check(layers.Unstaged.includes(unstagedNeedle), 'Unstaged section lacks the unstaged both.rs count');
    check(layers.Staged.includes('pixel.png') && layers.Staged.includes('binary'), 'staged binary pixel.png missing');
    check(layers.Commits.includes('pixel.png') && layers.Commits.includes('binary'), 'committed binary pixel.png missing');
    check(openedText.includes('loose.txt'), 'untracked loose.txt missing');
    check((await viewport.evaluate(node => getComputedStyle(node).overflowY)) === 'auto', 'expanded TLDR viewport does not scroll');
    await summary.getByRole('button', { name: 'Collapse details', exact: true }).click();
    check(await viewport.getAttribute('data-expanded') === 'false', 'Collapse details did not clip');
    check((await viewport.evaluate(node => getComputedStyle(node).overflow)) === 'hidden', 'compact TLDR viewport is not clipped');
    const collapsedText = await summary.locator('.code-review-tldr__body').innerText();
    check(collapsedText.includes('root0') && collapsedText === openedText, 'collapse changed the markdown body');
    await summary.getByRole('button', { name: 'Expand details', exact: true }).click();
    check(await viewport.getAttribute('data-expanded') === 'true', 'Expand details did not unclip');
    check((await viewport.evaluate(node => getComputedStyle(node).overflowY)) === 'auto', 'reexpand did not restore scroll');
    const reexpandedText = await summary.locator('.code-review-tldr__body').innerText();
    check(reexpandedText.includes(gitTruth.primary.head) && reexpandedText === openedText, 'reexpand changed the markdown body');
    const showLayer = async (title, needle) => {
      if (await page.locator('.code-review-tldr__viewport[data-expanded="false"]').count()) {
        await page.locator('.code-review-tldr').getByRole('button', { name: 'Expand details', exact: true }).click();
      }
      const visible = await page.locator('.code-review-tldr__viewport').evaluate((node, target) => {
        const heading = [...node.querySelectorAll('h3')].find(element => (element.textContent || '').trim() === target.title);
        let cursor = heading ? heading.nextElementSibling : null;
        let row = null;
        while (cursor && cursor.tagName !== 'H2' && cursor.tagName !== 'H3') {
          row = [...cursor.querySelectorAll('li, p')].find(element => (element.textContent || '').includes(target.needle)) || ((cursor.textContent || '').includes(target.needle) ? cursor : null);
          if (row) break;
          cursor = cursor.nextElementSibling;
        }
        if (!row) return false;
        row.scrollIntoView({ block: 'center' });
        const box = row.getBoundingClientRect();
        const frame = node.getBoundingClientRect();
        return box.height > 0 && box.top >= 0 && box.bottom <= window.innerHeight && box.top < frame.bottom && box.bottom > frame.top;
      }, { title, needle });
      check(visible, `${title} ${needle} is outside the visible frame`);
    };
    await pairedShot('09-tldr-staged', () => showLayer('Staged', stagedNeedle));
    await pairedShot('09-tldr-unstaged', () => showLayer('Unstaged', unstagedNeedle));
    const goodMarkdown = (await barrier()).tldrMarkdown;
    check(goodMarkdown.includes('## root0') && goodMarkdown.includes(gitTruth.primary.head), 'done TLDR lost the host inventory');
    const finalState = await control('state');
    const doneRequests = finalState.requests.filter(entry => entry.kind === 'tldr' && entry.generation === summaryGeneration);
    assert.equal(doneRequests.length, 5, `done generation batches: ${doneRequests.map(entry => entry.batch)}`);
    assert.deepEqual(doneRequests.map(entry => entry.batch), [1, 2, 3, 4, 5], 'done generation batch order');
    for (const entry of doneRequests) check(entry.promptBytes < 2 * 1024 * 1024, `batch ${entry.batch} prompt ${entry.promptBytes} exceeds 2 MiB`);
    const largeSeen = doneRequests.flatMap(entry => entry.large);
    assert.deepEqual(largeSeen.map(record => `${record.repo}\0${record.kind}\0${record.path}`).sort(), ['root0/big\0unstaged\0left.txt', 'root0/big\0unstaged\0right.txt'], 'done generation large file/status keys');
    for (const record of largeSeen) {
      const spec = gitTruth.large.files[record.path];
      assert.equal(record.patchBytes, spec.patchBytes, `${record.path} exact patch bytes across batches`);
      assert.equal(record.patchSha256, spec.patchSha256, `${record.path} exact patch sha256 across batches`);
    }
    assert.ok(largeSeen.reduce((sum, record) => sum + record.patchBytes, 0) > 2 * 1024 * 1024, 'cumulative large patch bytes did not exceed 2 MiB');
    const largePrompts = doneRequests.filter(entry => entry.large.length > 0);
    assert.equal(largePrompts.length, 2, 'large records were not split into two batches');
    assert.ok(largePrompts[0].promptBytes > 1100000 && largePrompts[1].promptBytes > 1100000, 'large batch prompts are not full');
    assert.notEqual(largePrompts[0].promptSha256, largePrompts[1].promptSha256, 'large batch prompts are identical');
    assert.ok(largePrompts.reduce((sum, entry) => sum + entry.promptBytes, 0) > 2 * 1024 * 1024, 'large batch prompt bytes did not total over 2 MiB');
    const stoppedRequests = finalState.requests.filter(entry => entry.kind === 'tldr' && entry.generation === stoppedGeneration);
    check(stoppedRequests.length >= 1 && stoppedRequests.every(entry => entry.large.length === 0), 'stopped generation counted large records');
    const startedBatches = [...fs.readFileSync(path.join(evidence, 'rpi.stderr'), 'utf8').matchAll(/summary_batch category=start batch=(\d+)\/(\d+) designated_keys=\d+ estimated_input_bytes=(\d+)/g)].map(match => ({ batch: Number(match[1]), total: Number(match[2]), bytes: Number(match[3]) }));
    const finalStarts = startedBatches.slice(-5);
    assert.equal(finalStarts.length, 5, `summary start lines: ${JSON.stringify(startedBatches)}`);
    finalStarts.forEach((line, index) => {
      assert.equal(line.batch, index + 1, `stderr batch ${index + 1}`);
      assert.equal(line.total, 5, `stderr batch total ${line.total}`);
      assert.equal(line.bytes, doneRequests[index].promptBytes, `stderr batch ${index + 1} estimated_input_bytes`);
    });
    await mark();
    const lateGeneration = await beginSummary();
    await control('tldr-late');
    await page.locator('#code-review-tldr').click();
    await stateWait('tldrState', 'running');
    await awaitSummary(1, lateGeneration);
    const lateFrom = (await capture()).received.length;
    await page.locator('#code-review-tldr').click();
    await stateWait('tldrState', 'done', lateFrom);
    await releaseOpenSummary(lateGeneration);
    const afterLate = await barrier();
    assert.equal(afterLate.tldrMarkdown, goodMarkdown, 'late TLDR replaced the last-good snapshot');
    assert.equal(afterLate.tldrState, 'done', 'late TLDR changed the settled state');
    check(!(await page.locator('.code-review-tldr__body').innerText()).includes('STALE-LATE-SNAPSHOT'), 'stale TLDR body painted');
    await mark();
    const malformedGeneration = await beginSummary();
    await control('tldr-malformed');
    await page.locator('#code-review-tldr').click();
    await awaitSummary(1, malformedGeneration);
    await releaseOpenSummary(malformedGeneration);
    await stateWait('tldrState', 'failed');
    const afterMalformed = await barrier();
    assert.equal(afterMalformed.tldrMarkdown, goodMarkdown, 'malformed purpose replaced last-good');
    check((await page.locator('.code-review-tldr__body').innerText()).includes(gitTruth.primary.head), 'malformed purpose changed the visible last-good');
    check(!(await page.locator('.code-review-tldr__body').innerText()).includes('"extra"'), 'malformed JSON painted');
    const purposeToast = page.locator('#toasts .toast--error').filter({ hasText: 'purpose JSON rejected' }).filter({ hasText: 'unknown field' }).filter({ hasText: 'repositories' }).filter({ hasText: 'changes' });
    await purposeToast.waitFor();
    check(await purposeToast.count() === 1, 'malformed purpose did not emit exactly one purpose JSON rejection toast');
    await purposeToast.click();
    await purposeToast.waitFor({ state: 'detached' });
    check(await page.locator('#toasts .toast--error').filter({ hasText: 'purpose JSON rejected' }).count() === 0, 'malformed purpose toast remained after dismiss');
    const waitFor = async (target, fn, label, timeoutMs = 30000, arg) => {
      try { await target.waitForFunction(fn, arg, { timeout: timeoutMs }); }
      catch (error) { throw new Error(`${phase}: ${label}; ${error.message}`); }
    };
    const highlightToast = page.locator('#toasts .toast--error').filter({ hasText: 'Highlight:' });
    while (await highlightToast.count()) {
      await highlightToast.first().click();
      await highlightToast.first().waitFor({ state: 'detached' });
    }
    check(await page.locator('#toasts .toast--error').filter({ hasText: 'Highlight:' }).count() === 0, 'asserted Highlight toast remained before the clean summary');

    const selectMockTwo = async () => {
      const model = page.getByLabel('Review reply model');
      await model.waitFor();
      const selected = await model.evaluate(el => el.selectedOptions[0]?.textContent ?? '');
      if (selected === 'Steering Mock Two') return;
      const acksBefore = (await capture()).received.filter(frame => frame.command === 'code_review_set_model' && frame.success).length;
      await model.selectOption({ label: 'Steering Mock Two' });
      await wait(acksBefore => window.__ops.received.filter(frame => frame.command === 'code_review_set_model' && frame.success).length > acksBefore, 'reopened review model ack', acksBefore);
      const switched = await model.evaluate(el => ({ value: el.value, label: el.selectedOptions[0]?.textContent ?? '' }));
      check(switched.label === 'Steering Mock Two', `reopened review model wrong: ${JSON.stringify(switched)}`);
    };
    const seedSummary = async () => {
      if (await page.locator('#code-review-panel').count() === 0) {
        await page.fill('#prompt-input', '/review HEAD');
        await page.press('#prompt-input', 'Enter');
        await page.locator('#code-review-panel').waitFor();
      }
      await selectMockTwo();
      await mark();
      await beginSummary();
      await page.locator('#code-review-tldr').click();
      await awaitSummary(1);
      await releaseSummary();
      await stateWait('tldrState', 'done');
      const body = page.locator('.code-review-tldr__body');
      await body.filter({ hasText: gitTruth.primary.head }).waitFor();
      const seedText = await body.innerText();
      check(seedText.includes(gitTruth.primary.head), 'fresh summary OID was not visible before the session switch');
      for (const name of ['left.txt', 'right.txt']) {
        check(seedText.includes(gitTruth.large.files[name].token), `${page.viewportSize().width}: large ${name} purpose missing from the fresh summary`);
      }
      check(await page.locator('.code-review-tldr').getAttribute('data-tldr-state') === 'done', 'fresh summary was not ready before the session switch');
    };
    const assertChromeWhileSummaryOpen = async () => {
      const review = await page.locator('#code-review-panel').evaluate(node => {
        const box = node.getBoundingClientRect();
        const style = getComputedStyle(node);
        const root = getComputedStyle(document.documentElement);
        const px = name => {
          const raw = root.getPropertyValue(name).trim();
          const value = Number.parseFloat(raw);
          return raw.endsWith('px') && Number.isFinite(value) ? value : null;
        };
        return {
          top: box.top,
          left: box.left,
          right: box.right,
          bottom: box.bottom,
          height: box.height,
          cssTop: style.top,
          cssBottom: style.bottom,
          cssHeight: style.height,
          headerHeight: px('--app-header-height'),
          footerHeight: px('--app-footer-height'),
          viewportWidth: window.innerWidth,
          viewportHeight: window.innerHeight,
        };
      });
      const footerTop = review.footerHeight === null ? null : review.viewportHeight - review.footerHeight;
      check(!(review.top <= 1 && review.bottom >= review.viewportHeight - 1 && review.cssTop === '0px' && (review.cssBottom === '0px' || review.cssHeight === '100dvh' || review.height >= review.viewportHeight - 1)), `review still uses the old full-viewport inset (${JSON.stringify(review)})`);
      check(review.headerHeight !== null && Math.abs(review.top - review.headerHeight) <= 1, `review top ${review.top} is not the header bottom ${review.headerHeight}`);
      check(footerTop !== null && Math.abs(review.bottom - footerTop) <= 1, `review bottom ${review.bottom} is not the footer top ${footerTop}`);
      check(review.height < review.viewportHeight - 1 && review.cssHeight !== '100dvh', `review height still covers the viewport (${review.cssHeight}, box ${review.height})`);
      const targets = [
        ['header', 'header'],
        ['feature menu trigger', '#feature-menu-btn'],
      ];
      for (const [label, selector] of targets) {
        const hit = await page.locator(selector).evaluate(node => {
          const box = node.getBoundingClientRect();
          const style = getComputedStyle(node);
          const x = Math.min(Math.max(box.left + Math.min(box.width / 2, 8), 0), window.innerWidth - 1);
          const y = Math.min(Math.max(box.top + box.height / 2, 0), window.innerHeight - 1);
          const top = document.elementFromPoint(x, y);
          return {
            width: box.width,
            height: box.height,
            visible: style.display !== 'none' && style.visibility !== 'hidden' && Number.parseFloat(style.opacity || '1') > 0 && box.width > 0 && box.height > 0 && box.bottom > 0 && box.top < window.innerHeight,
            interactable: top === node || node.contains(top),
          };
        });
        check(hit.visible && hit.interactable, `${label} is not visible and interactable while summary is open: ${JSON.stringify(hit)}`);
      }
      const footerState = await page.evaluate(() => {
        const f = document.querySelector('.app-main > footer');
        if (!f) return null;
        const cs = getComputedStyle(f);
        return {
          display: cs.display,
          heightVar: Number.parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--app-footer-height')) || 0,
          rectH: f.getBoundingClientRect().height,
        };
      });
      check(footerState !== null && footerState.display === 'none' && footerState.heightVar === 0, `composer footer must be WHOLE-hidden at 0px while review is open: ${JSON.stringify(footerState)}`);
      await page.evaluate(() => { document.body.tabIndex = -1; document.body.focus(); });
      for (let i = 0; i < 40; i++) {
        await page.keyboard.press('Tab');
        const landed = await page.evaluate(() => {
          const footer = document.querySelector('.app-main > footer');
          return { inside: footer !== null && footer.contains(document.activeElement), id: document.activeElement?.id || document.activeElement?.tagName || '' };
        });
        check(!landed.inside, `Tab ${i + 1} focus entered the hidden composer footer (${landed.id}) — display:none must keep it out of the tab order at EVERY step, not just the last`);
      }
    };
    const ensureSidebarOpen = async () => {
      const open = await page.evaluate(() => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true);
      if (!open) await openSessionsDrawer(page);
      await wait(() => {
        const button = document.getElementById('sidebar-manage-btn');
        const box = button?.getBoundingClientRect();
        return document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true && !!box && box.width > 0 && box.height > 0;
      }, 'session sidebar did not open');
    };
    const clickSessionRow = async (sessionId, label, { summaryOpen = false } = {}) => {
      const row = page.locator(`.session-sidebar__switch[data-session-id="${sessionId}"]`);
      await row.waitFor({ state: 'visible' });
      const hitCheck = () => row.evaluate(node => {
        const box = node.getBoundingClientRect();
        const x = box.left + Math.min(Math.max(box.width / 2, 1), box.width - 1);
        const y = box.top + box.height / 2;
        const top = document.elementFromPoint(x, y);
        return {
          width: box.width,
          height: box.height,
          interactable: top === node || node.contains(top),
          top: top ? `${top.id || top.tagName}.${top.className || ''}` : null,
        };
      });
      let hit = await hitCheck();
      if (!hit.interactable) {
        // Fullshell desktop legitimately covers the rail while the review is
        // open — take the REAL ≤720 sessions drawer (z 50 above the panel);
        // the review is never closed, so the ownership test stays alive.
        const before = page.viewportSize();
        await page.setViewportSize({ width: 390, height: 844 });
        await openSessionsDrawer(page);
        await wait(() => document.querySelector('.app-layout--drawer-open') !== null, `${label} sessions drawer did not open over the review`);
        hit = await hitCheck();
        await row.click();
        await wait(() => document.querySelector('.app-layout--drawer-open') === null, `${label} drawer did not auto-close after the switch`);
        await page.setViewportSize({ width: before.width, height: before.height });
        return;
      }
      check(hit.width > 0 && hit.height > 0 && hit.interactable, `${label} session row is covered: ${JSON.stringify(hit)}`);
      if (summaryOpen) {
        check(await page.locator('#code-review-panel').count() === 1, `${label} review closed before the row click`);
        check(await page.locator('.code-review-tldr').count() === 1, `${label} summary closed before the row click`);
      }
      await row.click();
    };
    await page.setViewportSize({ width: 1280, height: 900 });
    if (await page.locator('#code-review-panel').count()) {
      await page.getByRole('button', { name: 'Close code review panel', exact: true }).click();
      await page.locator('#code-review-panel').waitFor({ state: 'detached' });
    }
    await ensureSidebarOpen();
    const otherSession = await createFreshSessionViaManage(page, waitFor, 'summary switch target', { closePanel: false });
    await page.getByRole('button', { name: 'Close session panel', exact: true }).click();
    await page.locator('#session-panel').waitFor({ state: 'detached' });
    await page.locator(`.session-sidebar__switch[data-session-id="${otherSession.beforeSessionId}"]`).click();
    await wait(id => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId === id, 'did not return to the summary session before the proof', otherSession.beforeSessionId);
    for (const [width, height] of [[1280, 900], [390, 844]]) {
      await page.setViewportSize({ width, height });
      await seedSummary();
      await assertChromeWhileSummaryOpen();
      await showLayer('Staged', stagedNeedle);
      check(await page.locator('#toasts .toast--error').count() === 0, `${width}: error toast painted before the clean staged shot`);
      await shot(`summary-clean-staged-${width}`);
      await showLayer('Unstaged', unstagedNeedle);
      check(await page.locator('#toasts .toast--error').count() === 0, `${width}: error toast painted before the clean unstaged shot`);
      await shot(`summary-clean-unstaged-${width}`);
      const summaryOid = gitTruth.primary.head;
      check(await page.locator('#code-review-panel').count() === 1, 'review closed before the session switch');
      check(await page.locator('.code-review-tldr').count() === 1, 'summary closed before the session switch');
      check(await page.locator('#session-panel').count() === 0, 'Manage Session opened before the direct row switch');
      await ensureSidebarOpen();
      await clickSessionRow(otherSession.sessionId, `${width} switch`, { summaryOpen: true });
      await wait(id => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId === id, 'direct session row did not switch', otherSession.sessionId);
      check(await page.locator('.code-review-tldr').count() === 0, 'old summary stayed after session switch');
      check(!(await page.locator('body').innerText()).includes(summaryOid), 'old summary OID painted after session switch');
      await ensureSidebarOpen();
      await page.locator(`.session-sidebar__switch[data-session-id="${otherSession.beforeSessionId}"]`).click();
      await wait(id => document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId === id, 'session switch did not return', otherSession.beforeSessionId);
      check(await page.locator('.code-review-tldr').count() === 0, 'old summary repainted after returning');
    }
    await page.setViewportSize({ width: 1280, height: 900 });
    if (await page.locator('#code-review-panel').count() === 0) {
      await page.fill('#prompt-input', '/review HEAD');
      await page.press('#prompt-input', 'Enter');
      await page.locator('#code-review-panel').waitFor();
      await select(files.critical);
    }
    await selectMockTwo();
    if (await page.locator('.code-review-tldr').count()) {
      await page.locator('.code-review-tldr').getByRole('button', { name: 'Close summary', exact: true }).click();
      await page.locator('.code-review-tldr').waitFor({ state: 'detached' });
    }
    await terminal();

    phase = 'targeted-stop';
    for (const [operation, kind, field, terminalValue, escape] of [['autoReview', 'triage', 'autoReviewState', 'failed', false], ['highlight', 'highlight', 'highlightState', 'idle', true]]) {
      await control('hold', kind); await mark();
      await page.locator(operationButtonId(operation)).click();
      await stateWait(field, operation === 'autoReview' ? 'triaging' : 'running');
      await control('await', `${kind}&count=${operation === 'autoReview' ? 3 : 2}`);
      await waitOperationStop(operation);
      if (operation === 'highlight') {
        await page.getByRole('button', { name: 'Close code review panel', exact: true }).click();
        await page.locator('.code-review__confirm').waitFor();
        check((await page.locator('.code-review__confirm').innerText()).includes('in-flight review work'), 'close warning missing during highlight');
        await page.getByRole('button', { name: 'Keep editing' }).click();
        await page.locator('.code-review__confirm').waitFor({ state: 'detached' });
      }
      await pairedShot(`03-running-${operation}`);
      const afterStopFrom = (await capture()).received.length;
      if (escape) {
        await page.evaluate(() => { document.body.tabIndex = -1; document.body.focus(); });
        await page.keyboard.press('Escape');
      } else {
        await page.locator(operationButtonId(operation)).click();
      }
      await stateWait(field, terminalValue, afterStopFrom);
      check(await page.locator('#code-review-panel').isVisible(), 'Esc closed panel before stopping');
      const stopped = snapshots(await capture()).at(-1);
      await control('release', kind);
      const after = await barrier();
      assert.equal(after[field], terminalValue, 'late response revived/reset stopped operation');
      const other = operation === 'autoReview' ? 'highlightState' : 'autoReviewState';
      assert.equal(after[other], stopped[other], 'targeted Stop changed the other operation');
      await terminal(); await pairedShot(`03-stop-${operation}`);
    }

    phase = 'comment-follow';
    await openHunk(files.dirty);
    await page.getByRole('textbox', { name: 'Hunk comment', exact: true }).fill('OPS accepted comment one');
    await page.getByRole('textbox', { name: 'Hunk comment', exact: true }).press('Enter');
    await wait(() => document.querySelector('.code-review__thread')?.textContent.includes('Comment reply 1'), 'accepted comment reply absent');
    await assertThreadFollowsTail('accepted comment follows');
    await page.locator('.code-review__thread').evaluate(node => { node.scrollTop = 0; node.dispatchEvent(new Event('scroll')); });
    await wait(() => {
      const thread = document.querySelector('.code-review__thread');
      return thread && thread.scrollHeight > thread.clientHeight && thread.scrollTop <= 1;
    }, 'suspended thread not at top');
    const frozenTop = await page.locator('.code-review__thread').evaluate(node => node.scrollTop);
    await page.getByRole('textbox', { name: 'Hunk comment', exact: true }).fill('OPS accepted comment two');
    await page.getByRole('textbox', { name: 'Hunk comment', exact: true }).press('Enter');
    await wait(() => document.querySelector('.code-review__thread')?.textContent.includes('OPS accepted comment two'), 'second same-hunk comment absent');
    await wait(() => {
      const thread = document.querySelector('.code-review__thread');
      return thread && thread.scrollTop <= 1;
    }, 'accepted same-hunk comment jumped an unpinned reading position');
    await page.locator('.code-review__thread').evaluate(node => { node.scrollTop = 0; node.dispatchEvent(new Event('scroll')); });
    await wait(top => (document.querySelector('.code-review__thread')?.scrollTop ?? 1) <= top + 1, 'thread did not suspend after scroll up', frozenTop);
    await page.locator('.code-review__thread').evaluate(node => { node.scrollTop = node.scrollHeight; node.dispatchEvent(new Event('scroll')); });
    await assertThreadFollowsTail('returned to bottom resumes');

    phase = 'ack-gap';
    for (const [operation, ackCommand, ackField] of [['autoReview', 'code_review_auto_review', 'batchId'], ['highlight', 'code_review_highlight', 'highlightGeneration'], ['tldr', 'code_review_tldr', 'tldrGeneration']]) {
      const frozen = await barrier();
      const gapField = operation === 'autoReview' ? 'autoReviewState' : operation === 'highlight' ? 'highlightState' : 'tldrState';
      const gapValue = operation === 'tldr' ? frozen.tldrState : operation === 'autoReview' ? 'failed' : 'idle';
      await holdSnapshots(); await mark();
      // Arm an UNSETTLED flight of this panel before the ACK path exercises it.
      await wait(() => (window.__ops.heldSnapshots || []).length > 0, 'an owned snapshot response is held before the ACK trigger');
      let gapGeneration = null;
      if (operation === 'tldr') {
        gapGeneration = await beginSummary();
        expectedSummaryBatches.set(gapGeneration, 1);
        await control('hold', 'tldr'); // hold BEFORE the start: the Stop must cancel a real arrived request, not a pre-dispatch cancel
      } else await control('hold', operation === 'autoReview' ? 'triage' : operation);
      const beforeGap = await capture();
      const startsBefore = beforeGap.sent.filter(frame => frame.type === ackCommand).length;
      const closesBefore = beforeGap.sent.filter(frame => frame.type === 'code_review_close').length;
      const heldSnapshotIds = beforeGap.heldSnapshots.map(raw => JSON.parse(raw).id); // frozen pre-ACK ids, sampled before the click
      await page.locator(operationButtonId(operation)).click();
      await wait(ackCommand => window.__ops.received.some(frame => frame.success && frame.command === ackCommand && frame.data?.state !== undefined), 'start ACK during the gap', ackCommand);
      // ACK contract: releaseSnapshotFlight drops the pre-ACK join and issues exactly ONE fresh owned snapshot request.
      await page.evaluate(() => new Promise(resolve => window.setTimeout(resolve, 0)));
      const postAckSnapshotSends = (await capture()).sent.slice(beforeGap.sent.length).filter(frame => frame.type === 'code_review_snapshot');
      assert.equal(postAckSnapshotSends.length, 1, `${operation} ACK issued ${postAckSnapshotSends.length} code_review_snapshot sends instead of exactly one fresh owned request`);
      assert.ok(!heldSnapshotIds.includes(postAckSnapshotSends[0].id), `${operation} ACK reused a held pre-ACK snapshot request id instead of opening a fresh owned flight`);
      if (operation === 'tldr') await control('await', `tldr&count=1&generation=${gapGeneration}&batch=1`); // event barrier: the real provider request has ENTERED before the Stop
      check((await page.locator(operationButtonId(operation)).getAttribute('class') || '').includes('code-review__action--stop'), `${operation} gap button is not Stop with snapshots held`);
      const ackFrames = (await capture()).received.filter(frame => frame.success && frame.command === ackCommand);
      assert.ok(ackFrames.some(frame => typeof frame.data[ackField] === 'number'), `${ackCommand} ACK carries no ${ackField} identity`);
      if (operation === 'highlight') {
        const stopsBefore = (await capture()).sent.filter(frame => frame.type === 'code_review_stop').length;
        await page.locator(operationButtonId(operation)).click();
        await wait(({ stopsBefore, operation }) => window.__ops.sent.filter(frame => frame.type === 'code_review_stop').length > stopsBefore && window.__ops.sent.filter(frame => frame.type === 'code_review_stop').at(-1).operation === operation, 'second-click Stop frame', { stopsBefore, operation });
      } else {
        await gapEscapeStop(operation, closesBefore);
      }
      const after = await capture();
      assert.equal(after.sent.filter(frame => frame.type === 'code_review_close').length, closesBefore, `${operation} gap closed the review`);
      assert.equal(after.sent.filter(frame => frame.type === ackCommand).length, startsBefore + 1, `${operation} gap issued a second start`);
      check(await page.locator(operationButtonId(operation)).evaluate(el => el.classList.contains('code-review__action--stop')), `${operation} gap lost the Stop chrome before release`);
      await releaseSnapshots();
      await wait(({ field, value }) => window.__ops.received.some(frame => frame.success === true && frame.data?.[field] === value), `${gapField}=${gapValue}`, { field: gapField, value: gapValue });
      const settled = await barrier();
      assert.equal(settled[gapField], gapValue, `${operation} gap did not restore the frozen state`);
      if (operation === 'tldr') assert.equal(settled.tldrMarkdown, frozen.tldrMarkdown, 'tldr gap replaced the frozen summary');
      await control('release', operation === 'autoReview' ? 'triage' : operation);
      await terminal();
    }

    phase = 'action-toast'; await page.evaluate(() => { window.__ops.failNext = true; });
    const toastBefore = (await capture()).toasts.length;
    await page.locator('#code-review-highlight').click();
    await page.locator('#toasts .toast--error').filter({ hasText: 'operations action failure' }).waitFor();
    check((await capture()).toasts.length === toastBefore + 1, 'action failure emitted duplicate App toast');
    await terminal();
    await pairedShot('06-action-toast', async () => {
      await requireActionToast();
      if (await page.locator('.code-review-tldr').count()) await requireSummaryHitTargets();
    });
    if (await page.locator('.code-review-tldr').count()) {
      await page.locator('.code-review-tldr').getByRole('button', { name: 'Close summary', exact: true }).click();
      await page.locator('.code-review-tldr').waitFor({ state: 'detached' });
    }
    await requireActionToast();

    phase = 'reopen-cycle';
    for (let cycle = 1; cycle <= 2; cycle++) {
      const toastsBefore = (await capture()).toasts.length;
      const receivedBefore = (await capture()).received.length;
      await page.getByRole('button', { name: 'Close code review panel', exact: true }).click();
      await page.locator('#code-review-panel').waitFor({ state: 'detached' });
      await wait(() => document.activeElement?.id === 'prompt-input'
        && getComputedStyle(document.querySelector('.app-main > footer')).display !== 'none'
        && (Number.parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--app-footer-height')) || 0) > 0
        && getComputedStyle(document.querySelector('.transcript-scrollport')).visibility === 'visible'
        && (document.querySelector('button.transcript-scroll-bottom')?.getBoundingClientRect().width || 0) >= 44,
      `review close ${cycle} did not restore the footer, prompt focus, and scroll-to-bottom layout`);
      await page.fill('#prompt-input', '/review HEAD'); await page.press('#prompt-input', 'Enter');
      await page.locator('#code-review-panel').waitFor();
      await openHunk(files.critical);
      await wait(() => {
        const el = document.querySelector('.code-review__model-select');
        return el && el.value === 'user-steering/mock';
      }, `reopen ${cycle} default model identity not selected`);
      const reopened = await page.getByLabel('Review reply model').evaluate(el => ({ value: el.value, label: el.selectedOptions[0]?.textContent ?? '' }));
      check(reopened.value === 'user-steering/mock' && reopened.label === 'Steering Mock', `reopen ${cycle} model identity wrong: ${JSON.stringify(reopened)}`);
      check(await page.locator('.code-review__hunk-lines:not(.is-collapsed)').count() > 0, `reopen ${cycle} hunk body not usable`);
      check((await capture()).toasts.length === toastsBefore, `reopen ${cycle} emitted an error toast`);
      const postReopen = (await capture()).received.slice(receivedBefore);
      check(!postReopen.some(frame => frame.command === 'code_review_snapshot' && frame.success === false), `reopen ${cycle} snapshot failed after panel live`);
      await header();
      await assertLangCentered();
      await shot(`08-reopen-${cycle}-desktop`);
      await page.setViewportSize({ width: 390, height: 844 });
      await header();
      await assertLangCentered();
      await shot(`08-reopen-${cycle}-mobile`);
      await page.setViewportSize({ width: 1280, height: 900 });
      await header();
    }
    await selectMockTwo();
    for (const [name, viewport] of [['desktop', { width: 1280, height: 900 }], ['mobile', { width: 390, height: 844 }]]) {
      phase = `${name}-surface`; await page.setViewportSize(viewport); await header(); await select(files.preview);
      await page.getByRole('button', { name: 'Preview', exact: true }).click();
      await page.locator('.code-review__preview-markdown').filter({ hasText: 'PREVIEW-EXTRA line' }).waitFor();
      check(await page.getByRole('button', { name: 'Preview', exact: true }).getAttribute('aria-pressed') === 'true', 'Preview segment not active');
      await shot(`04-${name}-preview`); await page.getByRole('button', { name: 'Source', exact: true }).click();
      await openHunk(files.dirty);
      if (name === 'mobile') await page.getByRole('button', { name: 'Thread', exact: true }).click();
      const input = page.getByRole('textbox', { name: 'Hunk comment', exact: true }); const before = await input.boundingBox();
      await input.hover(); await input.click(); await input.fill(Array.from({ length: 12 }, (_, index) => `safe unsent line ${index}`).join('\n'));
      await wait(() => document.querySelector('.code-review__comment-input').clientHeight > 100, 'composer auto-grow');
      const after = await input.boundingBox(); check(after.height > before.height && after.x >= 0 && after.x + after.width <= viewport.width + 1 && after.y + after.height <= viewport.height, 'composer geometry escaped viewport/did not grow');
      check(await input.evaluate(node => node === document.activeElement && node.matches(':hover')), 'composer focus/hover missing');
      check((await page.locator('.code-review__action--send').innerText()).includes('➤'), 'composer send icon wrong');
      const panel = page.locator('#code-review-panel');
      const attach = panel.getByRole('button', { name: 'Attach files', exact: true });
      const mic = panel.getByRole('button', { name: 'Microphone', exact: true });
      check(await attach.isEnabled(), 'hunk Attach files is disabled');
      check(await mic.isEnabled() && await mic.getAttribute('aria-label') === 'Microphone' && await mic.getAttribute('title') === 'Hold to talk (voice to text)' && await mic.getAttribute('aria-pressed') === 'false', 'mic is not an enabled idle control');
      if (viewport.width <= 768) {
        const send = panel.locator('.code-review__action--send');
        const targets = await Promise.all([attach, mic, send].map(el => el.boundingBox()));
        check(targets[0] && targets[0].width >= 44 && targets[0].height >= 44 && Math.abs(targets[0].width - targets[0].height) <= 1, `mobile attach target too small: ${JSON.stringify(targets[0])}`);
        check(targets[1] && targets[1].width >= 44 && targets[1].height >= 44 && Math.abs(targets[1].width - targets[1].height) <= 1, `mobile mic target too small: ${JSON.stringify(targets[1])}`);
        check(targets[2] && targets[2].height >= 44, `mobile send target too small: ${JSON.stringify(targets[2])}`);
      }
      const placeholder = `${await attach.getAttribute('title') || ''} ${await mic.getAttribute('title') || ''} ${await page.locator('#code-review-panel .code-review__composer').innerText()}`;
      check(!/not supported|unsupported/i.test(placeholder), 'unsupported placeholder remains on attach/mic');
      const picker = panel.locator('input[type=file]');
      check(await picker.count() === 1 && (await picker.getAttribute('accept') || '').includes('image/png'), 'hunk hidden image input missing image/png accept');
      const [chooser] = await Promise.all([page.waitForEvent('filechooser'), attach.click()]);
      check(await chooser.element().evaluate(el => el.closest('#code-review-panel') !== null && el.getAttribute('type') === 'file'), 'filechooser was not the hunk hidden image input');
      await chooser.setFiles(attachPath);
      await wait(({ name }) => {
        const chip = document.querySelector('#code-review-panel .code-review__composer-attachments .composer-attachment');
        const thumb = chip && chip.querySelector('img.composer-attachment__thumb');
        const label = chip && (chip.querySelector('.composer-attachment__name')?.textContent || '').trim();
        return !!(chip && label === name && thumb && (thumb.getAttribute('src') || '').length > 0);
      }, 'hunk image chip name/thumbnail', { name: attachFile });
      await shot(`05-${name}-composer`);
      await panel.getByRole('button', { name: `Remove ${attachFile}`, exact: true }).click();
      await wait(() => !document.querySelector('#code-review-panel .code-review__composer-attachments .composer-attachment'), 'hunk image chip remained after remove');
      await input.fill(''); await terminal();
    }
    report.provider = await control('state'); frames = await capture();
    assert.deepEqual(report.provider.errors, [], 'strict provider rejected a runtime request');
    const kinds = report.provider.requests.map(entry => entry.kind);
    assert.equal(kinds.filter(kind => kind === 'triage').length, 4, 'final triage request count');
    assert.equal(kinds.filter(kind => kind === 'auto').length, 4, 'auto findings request count');
    assert.equal(kinds.filter(kind => kind === 'highlight').length, 4, 'highlight request count');
    const summaryRequests = report.provider.requests.filter(entry => entry.kind === 'tldr');
    const generations = [...new Set(summaryRequests.map(entry => entry.generation))].sort((left, right) => left - right);
    assert.deepEqual(generations, [...expectedSummaryBatches.keys()].sort((left, right) => left - right), 'summary generations differ from the journey record');
    for (const generation of generations) {
      const batches = summaryRequests.filter(entry => entry.generation === generation).map(entry => entry.batch);
      const expected = expectedSummaryBatches.get(generation);
      assert.deepEqual(batches, Array.from({ length: expected }, (_, index) => index + 1), `summary generation ${generation} batches were not ${expected}`);
    }
    assert.equal(kinds.filter(kind => kind === 'comment').length, 2, 'accepted comment reply count');
    assert.equal(report.provider.released.length, report.provider.held.length, 'unreleased held model requests');
    const autoTargets = report.provider.requests.filter(entry => entry.kind === 'auto').map(entry => entry.target);
    check(autoTargets.every(target => target === files.critical || target === files.overdesign), `auto scoped an unselected hunk: ${JSON.stringify(autoTargets)}`);
    check(report.provider.requests.filter(entry => entry.kind === 'highlight').every(entry => entry.scope.length === 9), 'highlight scope was not the full fixture');
    check(report.provider.requests.every(entry => entry.model === 'mock-2'), 'model picker did not reach provider boundary');
    assert.equal(frames.sent.filter(frame => frame.type === 'code_review_comment').length, 2, 'accepted same-hunk comment count');
    assert.deepEqual(frames.sent.filter(frame => frame.type === 'code_review_stop').map(frame => frame.operation), ['tldr', 'tldr', 'autoReview', 'highlight', 'autoReview', 'highlight', 'tldr'], 'exact targeted and gap stop frames');
    assert.equal(frames.sent.filter(frame => frame.type === 'code_review_auto_review').length, 4, 'auto click count');
    assert.equal(frames.sent.filter(frame => frame.type === 'code_review_highlight').length, 5, 'highlight click count');
    assert.equal(frames.sent.filter(frame => frame.type === 'code_review_tldr').length, 7, 'tldr click count');
    for (const [operation, field, expected, startCommand] of [['autoReview', 'autoReviewState', 'failed', 'code_review_auto_review'], ['highlight', 'highlightState', 'idle', 'code_review_highlight']]) {
      const ackIndex = frames.allReceived.findIndex(frame => frame.command === 'code_review_stop' && frame.success && frame.data?.operation === operation);
      check(ackIndex >= 0, `${operation} stop acknowledgement absent`);
      const nextStart = frames.allReceived.findIndex((frame, index) => index > ackIndex && frame.command === startCommand && frame.success);
      const bound = nextStart === -1 ? frames.allReceived.length : nextStart;
      const later = frames.allReceived.slice(ackIndex + 1, bound).filter(frame => frame.success && frame.data?.[field] !== undefined);
      check(later.length > 0 && later.every(frame => frame.data[field] === expected), `${operation} revived in captured post-stop frames`);
    }
    const lateTldrStop = frames.allReceived.findLastIndex(frame => frame.command === 'code_review_stop' && frame.success && frame.data?.operation === 'tldr');
    const afterTldrStop = frames.allReceived.slice(lateTldrStop + 1).filter(frame => frame.success && frame.data?.tldrMarkdown);
    check(afterTldrStop.every(frame => !String(frame.data.tldrMarkdown).includes('STALE-LATE-SNAPSHOT')), 'late TLDR markdown was accepted');
    assert.deepEqual(frames.parseErrors, [], 'unparseable socket frames'); assert.deepEqual(report.errors, [], 'browser errors');
    const lateToasts = frames.toasts.slice(toastBefore);
    check(lateToasts.length === 1 && lateToasts.every(text => text.includes('operations action failure')), `final capture added ${JSON.stringify(lateToasts)} instead of exactly one operations action failure toast`);
    await terminal();
    // ---- PRESERVE LEG (desktop first; mobile after the product GO): a
    // non-empty main draft + a REAL attachment chip must survive the REAL
    // Files right-click review route (a surface open is not a command
    // submit). Actual close order: review × first (footer must stay hidden
    // under the open Files drawer), then Files close → main visible; the
    // exact draft, chip identity, autofocus, and a keyboard suffix are
    // asserted BEFORE any fill/focus. Surface origin preserves the draft
    // (the typed /review command origin consumes it — asserted at bootstrap).
    for (const [preserveName, preserveViewport] of [['desktop', { width: 1280, height: 900 }], ['mobile', { width: 390, height: 844 }]]) {
      phase = `${preserveName}-preserve`;
      await page.setViewportSize(preserveViewport);
      await waitFor(page, (size) => window.innerWidth === size.width && window.innerHeight === size.height, 'preserve viewport did not settle', 10000, preserveViewport);
    if (await page.locator('#code-review-panel').count()) {
      await page.getByRole('button', { name: 'Close code review panel', exact: true }).click();
      await page.locator('#code-review-panel').waitFor({ state: 'detached' });
    }
    const preserveDraft = 'unrelated preserve draft line';
    await page.fill('#prompt-input', preserveDraft);
    const [preserveChooser] = await Promise.all([page.waitForEvent('filechooser'), page.locator('#attach-btn').click()]);
    await preserveChooser.setFiles(attachPath);
    await wait((want) => (document.querySelector('#composer-attachments .composer-attachment__name')?.textContent || '').trim() === want, 'preserve: main composer attachment chip missing', attachFile);
    const preserveToggle = () => clickFeature(page, '#file-explorer-toggle-btn');
    await preserveToggle();
    await page.locator('section.file-explorer').waitFor();
    const clickRow = page.locator('button.file-explorer__row--dir').first();
    const clickBox = await clickRow.boundingBox();
    const clickHit = await clickRow.evaluate(node => {
      const r = node.getBoundingClientRect();
      const top = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2);
      return { isSelf: top === node || node.contains(top), topCls: typeof top?.className === 'string' ? top.className : '' };
    });
    if (!clickHit.isSelf) fail(`preserve: the first dir row is not its own hit target before the right-click: ${JSON.stringify({ clickBox, clickHit })}`);
    await clickRow.click({ button: 'right' });
    await page.locator('[data-testid="file-explorer-review-dialog"]').waitFor();
    await page.getByRole('button', { name: 'Review', exact: true }).click();
    await page.locator('#code-review-panel').waitFor();
    await page.locator('#code-review-panel .code-review__head').waitFor();
    await shot(`preserve-${preserveName}-review-open`);
    await wait(() => document.querySelector('section.file-explorer') === null
      && getComputedStyle(document.querySelector('.app-main > footer')).display === 'none',
    'preserve: review open did not close Files (fullshell design) and hide the composer footer');
    await page.getByRole('button', { name: 'Close code review panel', exact: true }).click();
    await page.locator('#code-review-panel').waitFor({ state: 'detached' });
    await wait(({ draft, chip }) => document.activeElement?.id === 'prompt-input'
      && getComputedStyle(document.querySelector('.app-main > footer')).display !== 'none'
      && document.getElementById('prompt-input')?.value === draft
      && (document.querySelector('#composer-attachments .composer-attachment__name')?.textContent || '').trim() === chip,
    'preserve: close did not restore footer/focus/exact draft/chip identity', { draft: preserveDraft, chip: attachFile }).catch(async () => {
      const s = await page.evaluate(() => ({ len: document.getElementById('prompt-input')?.value.length ?? -1, focus: document.activeElement?.id === 'prompt-input', footer: getComputedStyle(document.querySelector('.app-main > footer')).display }));
      throw new Error(`preserve: close-restore failed — draftLength=${s.len}/${preserveDraft.length} focus=${s.focus} footer=${s.footer}`);
    });
    await page.keyboard.type(' ok');
    await wait((base) => document.getElementById('prompt-input')?.value === `${base} ok`, 'preserve: keyboard suffix did not land in the autofocused composer', preserveDraft);
    await shot(`preserve-${preserveName}-close-restored`);
    }
    phase = 'snapshot-pressure';
    // code_review_snapshot senders: CodeReviewPanel bootstrap/poll/probe; hold = CLIENT pending proof only.
    const pressure = { firstHeldId: null, causalOwnerFound: false, baseline: null, tickCount: 0, txids: 0, responseIds: 0, maxPending: 0, filesList: '', failureSource: '', snapshotFailures: 0, errorToasts: 0, staleFailureToasts: 0 };
    report.snapshotPressure = pressure;
    const errorsAtStart = report.errors.length;
    const toastsAtStart = (await capture()).toasts.length;
    const respondId = async (action, type, label) => {
      const at = (await capture()).sent.length;
      await action();
      await wait(({ at: savedAt, type }) => window.__ops.sent.slice(savedAt).some(frame => frame.type === type), `${label}: no new ${type} send`, { at, type });
      const id = await page.evaluate(({ at, type }) => window.__ops.sent.slice(at).find(frame => frame.type === type)?.id || '', { at, type });
      check(id !== '', `${label}: no new ${type} send observed`);
      await wait(pid => window.__ops.received.some(frame => frame.id === pid && frame.success), label, id);
      return id;
    };
    // Preserve-phase assertions already passed; clear their leftover attachment
    // chips via the real remove buttons so the slash is not read as a prompt.
    while (await page.locator('#composer-attachments .composer-attachment__remove').count()) {
      await page.locator('#composer-attachments .composer-attachment__remove').first().click();
    }
    await wait(() => document.querySelector('#composer-attachments .composer-attachment') === null, 'composer attachment chips removed before the review open');
    await page.evaluate(() => {
      const Native = window.setTimeout;
      window.__tickLog = [];
      window.__currentSendCallback = null;
      window.setTimeout = (fn, ms, ...rest) => (ms === 1500 || ms === 200)
        ? Native(() => {
            const previous = window.__currentSendCallback;
            window.__currentSendCallback = fn;
            try { window.__tickLog.push(fn); fn(); } finally { window.__currentSendCallback = previous; }
          }, ms, ...rest)
        : Native(fn, ms, ...rest);
    });
    await respondId(async () => {
      await page.fill('#prompt-input', '/review HEAD'); await page.press('#prompt-input', 'Enter');
    }, 'code_review_open', 'review open settled before the pressure hold');
    const sentAtHold = (await capture()).sent.length;
    await holdSnapshots();
    await wait(() => window.__ops.heldSnapshots.some(raw => {
      const id = JSON.parse(raw).id;
      const owner = window.__ops.snapshotSendOwners.find(entry => entry.id === id);
      return !!(owner && owner.callback);
    }), 'first held owned snapshot response with a causal poll-tick owner');
    const bound = await page.evaluate(() => {
      for (const raw of window.__ops.heldSnapshots) {
        const id = JSON.parse(raw).id;
        const owner = window.__ops.snapshotSendOwners.find(entry => entry.id === id);
        if (owner && owner.callback) {
          window.__pressurePollFn = owner.callback;
          window.__pressurePollFires = window.__tickLog.filter(fn => fn === owner.callback).length;
          return { id, found: true, fires: window.__pressurePollFires, sent: window.__ops.sent.length };
        }
      }
      return { id: null, found: false, fires: 0, sent: window.__ops.sent.length };
    });
    pressure.firstHeldId = bound.id;
    pressure.causalOwnerFound = bound.found;
    pressure.baseline = bound.sent;
    pressure.tickCount = bound.fires;
    report.snapshotPressure = pressure;
    check(bound.found && bound.id, 'no held response with a causal poll-tick owner — scenario not armed');
    const afterFirstHeldSent = pressure.baseline;
    await wait(() => window.__tickLog.filter(fn => fn === window.__pressurePollFn).length >= window.__pressurePollFires + 2, 'two more real poll-loop tick firings (causally owned callback)');
    pressure.tickCount = await page.evaluate(() => window.__tickLog.filter(fn => fn === window.__pressurePollFn).length);
    report.snapshotPressure = pressure;
    const held = await page.evaluate(({ at, from }) => ({
      heldIds: window.__ops.heldSnapshots.map(raw => JSON.parse(raw).id),
      ownedSends: window.__ops.sent.slice(at).filter(frame => frame.type === 'code_review_snapshot').length,
      tailSends: window.__ops.sent.slice(from).filter(frame => frame.type === 'code_review_snapshot').length,
    }), { at: sentAtHold, from: afterFirstHeldSent });
    pressure.txids = held.ownedSends; pressure.responseIds = held.heldIds.length; pressure.maxPending = held.heldIds.length;
    report.snapshotPressure = pressure;
    check(held.heldIds.length >= 1, 'hold never captured an owned snapshot response — scenario not armed');
    check(held.tailSends === 0, `singleflight broke: ${held.tailSends} duplicate owned code_review_snapshot sends after the first held response across >=2 causally-owned poll ticks (same owner/revision — any callback counts, must be 0)`);
    check(held.heldIds.length < 16, `client pending reached ${held.heldIds.length} unresolved owned snapshots (client-side bound; backend 16-slot saturation not claimed)`);
    await page.getByRole('button', { name: 'Close code review panel', exact: true }).click();
    await page.locator('#code-review-panel').waitFor({ state: 'detached' });
    await clickFeature(page, '#file-explorer-toggle-btn');
    await page.locator('section.file-explorer').waitFor();
    await respondId(() => page.locator('button.file-explorer__row--dir').first().click(), 'file_explorer_list', 'Files list RPC succeeded while snapshot responses stayed held');
    pressure.filesList = 'file_explorer_list';
    report.snapshotPressure = pressure;
    await clickFeature(page, '#file-explorer-toggle-btn');
    await wait(() => document.querySelector('section.file-explorer') === null, 'Files closed before the review remount');
    const sentBeforeRemount = (await capture()).sent.length;
    await respondId(async () => {
      await page.fill('#prompt-input', '/review HEAD'); await page.press('#prompt-input', 'Enter');
    }, 'code_review_open', 'remount open settled while the old flight is still held');
    await page.locator('#code-review-panel .code-review__head').waitFor();
    // The unmounted owner's request stays UNSETTLED; the new mount owns its flight.
    await wait(({ at, old }) => window.__ops.sent.slice(at).some(frame => frame.type === 'code_review_snapshot' && !old.includes(frame.id)
      && window.__ops.heldSnapshots.some(raw => JSON.parse(raw).id === frame.id)), 'new-epoch snapshot request and its held response while the old request stays pending', { at: sentBeforeRemount, old: held.heldIds });
    const remount = await capture();
    const newEpochId = (remount.heldSnapshots || []).map(raw => JSON.parse(raw).id).find(id => !held.heldIds.includes(id)) ?? '';
    check(newEpochId !== '' && !held.heldIds.includes(newEpochId), 'new epoch reused the unmounted owner request id');
    const remountSnapshotFails = remount.received.filter(frame => frame.command === 'code_review_snapshot' && frame.success === false).length;
    check(remountSnapshotFails === 0, `remount produced ${remountSnapshotFails} failed owned snapshots`);
    // Release ONLY the new epoch's response; the old frame stays undelivered.
    const releasedNew = await page.evaluate(old => {
      window.__ops.holdSnapshots = false;
      const heldFrames = window.__ops.heldSnapshots || [];
      window.__ops.heldSnapshots = [];
      let dispatched = 0;
      for (const raw of heldFrames) if (!old.includes(JSON.parse(raw).id)) { dispatched += 1; window.__opsSocket.dispatchEvent(new MessageEvent('message', { data: raw })); }
      return dispatched;
    }, held.heldIds);
    check(releasedNew >= 1, 'no independent new-epoch response was held to release');
    await wait(id => window.__ops.received.some(frame => frame.id === id && frame.success === true), 'new-epoch flight settled on its own response while the old request stays pending', newEpochId);
    const remounted = await capture();
    check(!remounted.received.some(frame => held.heldIds.includes(frame.id)), 'a response for the unmounted owner was delivered before its single failure release');
    check(remounted.toasts.length === toastsAtStart, `settling the new epoch while the unmounted owner stayed pending leaked ${remounted.toasts.length - toastsAtStart} user-visible errors`);
    const projection = () => page.evaluate(() => { const panel = document.querySelector('#code-review-panel'); return panel === null ? null : { label: panel.querySelector('.code-review__label')?.textContent?.trim() ?? '', totals: panel.querySelector('.code-review__totals')?.textContent?.trim() ?? '', files: panel.querySelectorAll('button.code-review__file').length }; });
    const lastGoodProjection = await projection();
    // Exactly ONE release for the old id, and it is a FAILURE (still pending).
    const staleSend = remounted.sent.find(frame => frame.id === held.heldIds[0]);
    check(staleSend !== undefined, 'the unmounted request id is absent from the sent log');
    await page.evaluate(({ id, sessionId }) => {
      window.__opsSocket.dispatchEvent(new MessageEvent('message', { data: JSON.stringify({ type: 'response', id, command: 'code_review_snapshot', success: false, error: 'late unmounted-owner snapshot failure', sessionId }) }));
    }, { id: held.heldIds[0], sessionId: staleSend.sessionId });
    await wait(id => window.__ops.received.some(frame => frame.id === id && frame.success === false), 'the still-pending unmounted request failed exactly once', held.heldIds[0]);
    const stale = await capture();
    check(stale.received.filter(frame => frame.id === held.heldIds[0] && frame.success === false).length === 1, 'the unmounted request received more than one failure release');
    check(!stale.received.some(frame => frame.id === held.heldIds[0] && frame.success === true), 'the unmounted request was settled with a success before its failure release');
    pressure.staleFailureToasts = stale.toasts.length - toastsAtStart;
    check(pressure.staleFailureToasts === 0, `a failure addressed to the unmounted owner's exact request id leaked ${pressure.staleFailureToasts} user-visible errors onto the new mount`);
    assert.deepEqual(await projection(), lastGoodProjection, 'a failure addressed to the unmounted owner changed the new mount\'s last-good projection');
    await mark();
    const lastGoodInjected = await projection();
    await page.evaluate(() => { window.__ops.failSnapshots = 3; });
    await wait(count => window.__ops.received.filter(frame => frame.command === 'code_review_snapshot' && frame.success === false).length >= count, 'three injected failures answered for real live poll request ids', 3);
    const failed = await capture();
    const injected = failed.received.filter(frame => frame.command === 'code_review_snapshot' && frame.success === false);
    check(injected.length >= 3 && injected.every(frame => failed.sent.some(sent => sent.type === 'code_review_snapshot' && sent.id === frame.id)), 'injected failures did not answer real sent poll request ids');
    check(injected.every(frame => frame.data?.code !== 'code_review_not_open'), 'injected failure was the typed not-open recovery');
    pressure.failureSource = 'client_fault_injection'; pressure.snapshotFailures = injected.length; pressure.errorToasts = failed.toasts.length - toastsAtStart;
    report.snapshotPressure = pressure;
    assert.deepEqual(await projection(), lastGoodInjected, 'injected snapshot failures changed the last-good workspace');
    check(pressure.errorToasts === 1, `error ownership broke: ${pressure.errorToasts} user-visible error toasts across ${pressure.snapshotFailures} injected snapshot failures (exactly 1 per stretch)`);
    check(await page.locator('#toasts .toast--error').count() === 1, 'visible error toast DOM count is not exactly 1');
    check(report.errors.length === errorsAtStart, `unexpected console/pageerror during pressure: ${report.errors.slice(errorsAtStart).join(' | ')}`);
    report.status = 'PASSED';
  } catch (error) {
    report.status = 'FAILED';
    report.errors.push(`${phase}: ${error.stack || error}`);
    try {
      const filename = path.join(evidence, `failure-${phase}.png`);
      await page.screenshot({ path: filename, fullPage: true });
      report.screenshots.push(filename);
    } catch { /* page gone */ }
    try { report.frames = await capture(); } catch { /* page gone */ }
    process.exitCode = 2;
  }
  finally {
    fs.writeFileSync(path.join(evidence, 'code-review-operations-report.json'), JSON.stringify(report, null, 2));
    console.log(JSON.stringify(report)); await browser.close();
  }
}

(providerMode ? provider(process.argv[3]) : journey()).catch(error => { console.error(error.stack || error); process.exitCode = 2; });
