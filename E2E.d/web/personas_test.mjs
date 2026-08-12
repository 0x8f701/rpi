// Focused personas-panel verification against the real fixture: steering
// mock + orchestration enabled + a seeded durable persona (persona.md +
// memory + sessions) under the fixture agent dir. Standalone evidence for the
// persistent-persona acceptance:
//   - list with memory/session counts (no absolute paths in the panel DOM)
//   - view definition (dialog a11y, literal content, persistence semantics)
//   - select as preferred
//   - run -> task_spawn pointing at the persona AGENT name (job card in the
//     Subagents panel bound to "(mentor)")
//   - create -> catalog discoverable after the config save
//   - edit name-agreement gate (mismatched frontmatter name rejected)
//   - remove vs purge confirmation dialog, with the containment semantics
//     verified on the REAL fixture filesystem (remove keeps memory/sessions,
//     purge deletes the root)
//   - DOM hygiene: no credentials, no absolute fixture paths
import { chromium } from 'playwright';
import fs from 'node:fs';
import path from 'node:path';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const evidence = process.env.RPI_EVIDENCE || '.';
const chromePath = process.env.RPI_CHROME || '';
const personaRoot = process.env.RPI_PERSONA_ROOT || '';

function fail(message) {
  console.error(`personas: FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 30000, arg) {
  try {
    await page.waitForFunction(fn, arg, { timeout: timeoutMs });
  } catch (err) {
    fail(`${label} (timeout ${timeoutMs}ms)`);
  }
}

async function main() {
  if (!url) fail('RPI_URL is required');
  if (!personaRoot) fail('RPI_PERSONA_ROOT is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage();
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    page.on('pageerror', (err) => {
      console.error(`personas: page error: ${err.message}`);
    });

    // Capture every WS frame (sent + received) so RPC privacy is evidenced on
    // the REAL wire, not just the DOM: no fixture credential, no absolute
    // persona path, and no full absolute persona.md path may appear in any
    // frame, while the persona definition BODY stays legitimately visible.
    const sentFrames = [];
    const receivedFrames = [];
    page.on('websocket', (ws) => {
      ws.on('framesent', (frame) => {
        const payload = typeof frame.payload === 'string' ? frame.payload : '';
        if (payload) sentFrames.push(payload);
      });
      ws.on('framereceived', (frame) => {
        const payload = typeof frame.payload === 'string' ? frame.payload : '';
        if (payload) receivedFrames.push(payload);
      });
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.getElementById('conn-state') !== null, 'conn-state missing');
    await waitFor(page, () => document.getElementById('conn-state').dataset.state === 'on', 'WS never connected');

    // --- Personas panel ---
    await page.click('#personas-toggle-btn');
    await waitFor(page, () => document.getElementById('personas-panel') !== null, 'personas panel did not open');
    // The panel fetches persona_list on mount, so WAIT for the seeded row.
    await waitFor(
      page,
      () => {
        const row = document.querySelector('[data-persona-name="mentor"]');
        return row !== null && row.querySelector('[data-persona-persistence]') !== null;
      },
      'seeded mentor persona never listed',
      15000
    );

    // List shows the durable memory/session counts.
    const persistence = await page.evaluate(() => {
      const row = document.querySelector('[data-persona-name="mentor"]');
      return row ? row.querySelector('[data-persona-persistence]').textContent || '' : '';
    });
    if (!persistence.includes('memory: 1 entry') || !persistence.includes('sessions: 1 archive')) {
      fail(`mentor persistence counts wrong: "${persistence}"`);
    }

    // --- View definition (dialog a11y + literal content + persistence) ---
    await page.click('[data-persona-name="mentor"] [data-action="view"]');
    await waitFor(page, () => document.querySelector('[data-persona-detail]') !== null, 'detail modal never opened');
    // The definition body arrives via persona_get; wait for it before reading.
    await waitFor(
      page,
      () => {
        const content = document.querySelector('[data-persona-content]');
        return content !== null && (content.textContent || '').includes('durable mentor persona');
      },
      'detail definition never loaded',
      15000
    );
    const detailA11y = await page.evaluate(() => {
      const dlg = document.querySelector('[data-persona-detail]');
      const close = document.querySelector('[data-persona-detail-close]');
      return {
        role: dlg?.getAttribute('role') || '',
        ariaModal: dlg?.getAttribute('aria-modal') || '',
        name: dlg?.getAttribute('data-persona-name') || '',
        focusOnClose: !!close && document.activeElement === close,
      };
    });
    if (detailA11y.role !== 'dialog') fail(`detail modal must be role=dialog, got "${detailA11y.role}"`);
    if (detailA11y.ariaModal !== 'true') fail('detail modal must set aria-modal=true');
    if (detailA11y.name !== 'mentor') fail(`detail modal bound to wrong persona: "${detailA11y.name}"`);
    if (!detailA11y.focusOnClose) fail('detail modal initial focus did not land on Close');
    const detailContent = await page.evaluate(() => {
      const content = document.querySelector('[data-persona-content]');
      const persistence = document.querySelector('[data-persona-persistence-detail]');
      return {
        content: content ? content.textContent || '' : '',
        persistence: persistence ? persistence.textContent || '' : '',
      };
    });
    if (!detailContent.content.includes('durable mentor persona')) {
      fail(`detail content missing the definition: "${detailContent.content}"`);
    }
    if (!detailContent.persistence.includes('remove keeps it, purge deletes it')) {
      fail(`detail persistence semantics missing: "${detailContent.persistence}"`);
    }
    // Close via the Close button: the click bubbles through the dialog body
    // (stopPropagation on the dialog) into closeView — a real click inside
    // the dialog, unlike Escape.
    await page.click('[data-persona-detail-close]');
    await waitFor(page, () => document.querySelector('[data-persona-detail]') === null, 'Close did not close the detail modal');

    // Boundary persistence states on the real fixture: a persona with NO
    // durable state yet shows zero counts, and a persona whose memory file is
    // a symlink fails closed to the fixed "unreadable" literal (never a path).
    const leanPersistence = await page.evaluate(() => {
      const row = document.querySelector('[data-persona-name="lean"]');
      return row ? row.querySelector('[data-persona-persistence]').textContent || '' : '';
    });
    if (!leanPersistence.includes('memory: 0 entries') || !leanPersistence.includes('sessions: 0 archives')) {
      fail(`lean persistence counts wrong: "${leanPersistence}"`);
    }
    const ghostPersistence = await page.evaluate(() => {
      const row = document.querySelector('[data-persona-name="ghost"]');
      return row ? row.querySelector('[data-persona-persistence]').textContent || '' : '';
    });
    if (!ghostPersistence.includes('memory/session state unreadable')) {
      fail(`ghost persistence must show the unreadable literal: "${ghostPersistence}"`);
    }

    // --- Select as preferred ---
    await page.click('[data-persona-name="mentor"] [data-action="select"]');
    await waitFor(
      page,
      () => document.querySelector('[data-persona-name="mentor"]')?.getAttribute('data-preferred') === 'true',
      'select did not mark the persona as preferred'
    );

    // --- Clear the preference via the toolbar (persona_clear), then
    // re-select so the run below keeps the same preferred state. ---
    await page.click('#personas-clear-pref-btn');
    await waitFor(
      page,
      () => document.querySelector('[data-persona-name="mentor"]')?.getAttribute('data-preferred') === 'false',
      'clear did not reset the preferred persona'
    );
    await page.click('[data-persona-name="mentor"] [data-action="select"]');
    await waitFor(
      page,
      () => document.querySelector('[data-persona-name="mentor"]')?.getAttribute('data-preferred') === 'true',
      're-select did not mark the persona as preferred again'
    );

    // --- Run -> task_spawn pointing at the persona AGENT name ---
    await page.click('[data-persona-name="mentor"] [data-action="run"]');
    await waitFor(
      page,
      () => document.querySelector('#persona-run-input-mentor') !== null,
      'run input never opened'
    );
    await page.fill('#persona-run-input-mentor', 'persona-e2e: web-e2e-subagent audit the persona contract and report findings');
    await page.click('#persona-run-start-mentor');
    // The spawned job must point at the persona agent: open the Subagents
    // panel and assert a job card carrying "(mentor)" and the task text.
    await page.click('#subagents-toggle-btn');
    await waitFor(page, () => document.getElementById('subagents-panel') !== null, 'subagents panel did not open');
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        return cards.some(
          (c) =>
            (c.textContent || '').includes('(mentor)') &&
            (c.textContent || '').includes('audit the persona contract'),
        );
      },
      'spawned persona job never appeared bound to the mentor agent',
      15000
    );
    await page.screenshot({ path: `${evidence}/persona-run.png`, fullPage: true });

    // --- Back to Personas: run again via the input's ENTER key (the
    // onKeyDown path — the mentor run above used the Start click) on the
    // oversized persona; the spawned job must bind to its agent name too. ---
    await page.click('#personas-toggle-btn');
    await waitFor(page, () => document.getElementById('personas-panel') !== null, 'personas panel did not reopen');
    await page.click('[data-persona-name="big"] [data-action="run"]');
    await waitFor(page, () => document.querySelector('#persona-run-input-big') !== null, 'big run input never opened');
    await page.fill('#persona-run-input-big', 'persona-e2e: web-e2e-subagent verify the enter-key run path and report');
    await page.keyboard.press('Enter');
    await page.click('#subagents-toggle-btn');
    await waitFor(page, () => document.getElementById('subagents-panel') !== null, 'subagents panel did not reopen');
    await waitFor(
      page,
      () => {
        const cards = [...document.querySelectorAll('.subagent-job')];
        return cards.some(
          (c) =>
            (c.textContent || '').includes('(big)') &&
            (c.textContent || '').includes('enter-key run path'),
        );
      },
      'enter-key persona job never appeared bound to the big agent',
      15000
    );

    // --- Back to Personas: create -> catalog discoverable after config save ---
    await page.click('#personas-toggle-btn');
    await waitFor(page, () => document.getElementById('personas-panel') !== null, 'personas panel did not reopen');
    await page.click('#personas-new-btn');
    await waitFor(page, () => document.querySelector('[data-persona-editor]') !== null, 'editor never opened');
    // Typing a name outside the charset: the editor surfaces the soft
    // validation (save AND the template seed both disable) instead of sending
    // a bad create. Then the user corrects the name and seeds via the
    // template button.
    await page.fill('#persona-create-name', 'bad name!');
    await waitFor(
      page,
      () => {
        const save = document.querySelector('#persona-editor-save');
        const template = document.querySelector('.personas-editor__template');
        return (
          save !== null &&
          save.disabled &&
          (save.getAttribute('data-save-disabled-reason') || '').includes('nameError') &&
          template !== null &&
          template.disabled
        );
      },
      'invalid persona name did not disable save/template',
      15000
    );
    await page.fill('#persona-create-name', 'scribe');
    await page.click('.personas-editor__template');
    await waitFor(
      page,
      () => (document.querySelector('#persona-create-content')?.value || '').includes('name: scribe'),
      'template never seeded the create editor',
      15000
    );
    await page.click('#persona-editor-save');
    await waitFor(
      page,
      () => document.querySelector('[data-persona-name="scribe"]') !== null,
      'created persona never became discoverable in the catalog',
      15000
    );

    // --- Edit name-agreement gate: mismatched frontmatter name is rejected ---
    await page.click('[data-persona-name="scribe"] [data-action="edit"]');
    await waitFor(
      page,
      () => document.querySelector('#persona-edit-content') !== null,
      'edit editor never opened'
    );
    // openEdit fetches the current definition asynchronously; wait for it so
    // the mismatch fill cannot be clobbered by the fetch.
    await waitFor(
      page,
      () => (document.querySelector('#persona-edit-content')?.value || '').includes('name: scribe'),
      'edit content never loaded into the editor',
      15000
    );
    await page.fill(
      '#persona-edit-content',
      '---\nname: other\ndescription: renamed\n---\nrenamed prompt\n'
    );
    // The frontend shows a soft name-agreement hint but does NOT block the
    // save: the backend is authoritative and rejects the mismatched frontmatter
    // name with an error shown in the editor (draft kept, editor stays open).
    // Wait (bounded) for the enabled state so an async re-render cannot race
    // the assertion; on timeout report the DOM state (save disabled reason,
    // readOnly, draft length, hint) instead of a bare failure.
    try {
      await page.waitForFunction(
        () => {
          const save = document.querySelector('#persona-editor-save');
          const error = document.querySelector('[data-persona-editor-content-error]');
          return (
            save !== null &&
            !save.disabled &&
            error !== null &&
            (error.textContent || '').includes('must match')
          );
        },
        null,
        { timeout: 15000 },
      );
    } catch {
      const diag = await page.evaluate(() => {
        const save = document.querySelector('#persona-editor-save');
        const error = document.querySelector('[data-persona-editor-content-error]');
        const content = document.querySelector('#persona-edit-content');
        return {
          saveDisabled: save ? save.disabled : 'missing',
          disabledReason: save ? save.getAttribute('data-save-disabled-reason') : 'missing',
          readOnly: content ? content.readOnly : 'missing',
          draftLength: content ? (content.value || '').length : -1,
          hint: error ? error.textContent || '' : '',
        };
      });
      fail(`mismatch step: save never became enabled: ${JSON.stringify(diag)}`);
    }
    await page.click('#persona-editor-save');
    await waitFor(
      page,
      () => {
        const err = document.querySelector('[data-persona-editor-error]');
        return err !== null && (err.textContent || '').includes('must match');
      },
      'backend did not reject the mismatched-name save',
      15000
    );
    const mismatchKept = await page.evaluate(() => {
      const editor = document.querySelector('[data-persona-editor]');
      const content = document.querySelector('#persona-edit-content');
      return {
        open: editor !== null,
        draftKept: (content?.value || '').includes('name: other'),
      };
    });
    if (!mismatchKept.open || !mismatchKept.draftKept) {
      fail('mismatched save must keep the editor open with the draft intact');
    }
    // A matching-name edit with CRLF line endings, a comment line, and a
    // QUOTED frontmatter name: all backend-legal, and the frontend hint must
    // resolve the declared name through the same unquote the backend applies.
    await page.fill(
      '#persona-edit-content',
      '---\r\n# refined scribe\r\nname: "scribe"\r\ndescription: refined scribe\r\n---\r\nrefined scribe prompt\r\n'
    );
    await page.click('#persona-editor-save');
    await waitFor(
      page,
      () => {
        const row = document.querySelector('[data-persona-name="scribe"]');
        return row !== null && (row.textContent || '').includes('refined scribe');
      },
      'matching-name edit never applied',
      15000
    );

    // --- Remove vs purge: confirmation dialog keeps the two semantics distinct ---
    // Remove mentor: persona.md deleted, memory/ and sessions/ KEPT on disk.
    const mentorMd = path.join(personaRoot, 'mentor', 'persona.md');
    const mentorMemory = path.join(personaRoot, 'mentor', 'memory', 'entries.jsonl');
    const mentorSessions = path.join(personaRoot, 'mentor', 'sessions', 'Mentor.jsonl');
    if (!fs.existsSync(mentorMd)) fail('fixture mentor persona.md missing before remove');
    await page.click('[data-persona-name="mentor"] [data-action="remove"]');
    await waitFor(page, () => document.querySelector('[data-persona-confirm]') !== null, 'confirm dialog never opened');
    const confirmA11y = await page.evaluate(() => {
      const dlg = document.querySelector('[data-persona-confirm]');
      return {
        role: dlg?.getAttribute('role') || '',
        ariaModal: dlg?.getAttribute('aria-modal') || '',
        name: dlg?.getAttribute('data-persona-name') || '',
        removeNote: document.querySelector('[data-persona-confirm-remove-note]')?.textContent || '',
        purgeNote: document.querySelector('[data-persona-confirm-purge-note]')?.textContent || '',
        focusOnCancel: document.activeElement === document.querySelector('[data-persona-confirm-cancel]'),
      };
    });
    if (confirmA11y.role !== 'dialog' || confirmA11y.ariaModal !== 'true') {
      fail(`confirm dialog a11y wrong: ${JSON.stringify(confirmA11y)}`);
    }
    if (confirmA11y.name !== 'mentor') fail('confirm dialog bound to the wrong persona');
    if (!confirmA11y.removeNote.includes('stay under the persona root')) {
      fail(`remove note must promise state retention: "${confirmA11y.removeNote}"`);
    }
    if (!confirmA11y.purgeNote.includes('whole persona root')) {
      fail(`purge note must promise root deletion: "${confirmA11y.purgeNote}"`);
    }
    if (!confirmA11y.focusOnCancel) fail('confirm dialog initial focus did not land on Cancel');
    await page.click('[data-persona-confirm-remove]');
    await waitFor(
      page,
      () => document.querySelector('[data-persona-name="mentor"]') === null,
      'mentor still listed after remove',
      15000
    );
    if (fs.existsSync(mentorMd)) fail('remove must delete persona.md on disk');
    if (!fs.existsSync(mentorMemory)) fail('remove must KEEP memory/entries.jsonl on disk');
    if (!fs.existsSync(mentorSessions)) fail('remove must KEEP sessions archives on disk');

    // Purge scribe: the whole persona root is deleted on disk.
    const scribeRoot = path.join(personaRoot, 'scribe');
    if (!fs.existsSync(scribeRoot)) fail('fixture scribe root missing before purge');
    await page.click('[data-persona-name="scribe"] [data-action="purge"]');
    await waitFor(page, () => document.querySelector('[data-persona-confirm]') !== null, 'purge confirm dialog never opened');
    await page.click('[data-persona-confirm-purge]');
    await waitFor(
      page,
      () => document.querySelector('[data-persona-name="scribe"]') === null,
      'scribe still listed after purge',
      15000
    );
    if (fs.existsSync(scribeRoot)) fail('purge must delete the whole persona root on disk');
    await page.screenshot({ path: `${evidence}/personas-final.png`, fullPage: true });

    // --- DOM hygiene: no credentials, no absolute fixture paths ---
    const domText = await page.evaluate(() => document.body.textContent || '');
    const fixtureAbsolute = personaRoot;
    if (domText.includes('user-mock-key')) {
      fail('the API key leaked into the DOM');
    }
    if (domText.includes(fixtureAbsolute)) {
      fail('an absolute fixture path leaked into the DOM');
    }

    // --- WIRE hygiene (real WS evidence, like the stt lane): every sent and
    // received frame must stay free of the fixture credential, the absolute
    // persona root, and any full absolute persona.md path, while the persona
    // definition BODY (returned by persona_get by design) remains visible. ---
    const allFrames = [...sentFrames, ...receivedFrames];
    const wireText = allFrames.join('\n');
    // Locate WHICH JSON field carries the leak without echoing the path value
    // into evidence (walk the parsed frame, report only the field path).
    const leakField = (label) => {
      const hit = allFrames.find((f) => f.includes(label));
      if (!hit) return null;
      try {
        const parsed = JSON.parse(hit);
        const walk = (obj, prefix) => {
          if (typeof obj === 'string') return obj.includes(label) ? prefix || '(root string)' : null;
          if (Array.isArray(obj)) {
            for (let i = 0; i < obj.length; i++) {
              const found = walk(obj[i], `${prefix}[${i}]`);
              if (found) return found;
            }
            return null;
          }
          if (obj && typeof obj === 'object') {
            for (const key of Object.keys(obj)) {
              const found = walk(obj[key], prefix ? `${prefix}.${key}` : key);
              if (found) return found;
            }
            return null;
          }
          return null;
        };
        return walk(parsed, '');
      } catch {
        return '<unparseable frame>';
      }
    };
    if (wireText.includes('user-mock-key')) {
      fail(`the API key leaked into a WS frame (field: ${leakField('user-mock-key')})`);
    }
    if (wireText.includes(fixtureAbsolute)) {
      const hit = allFrames.find((f) => f.includes(fixtureAbsolute));
      let redactedError = null;
      if (hit) {
        try {
          const parsed = JSON.parse(hit);
          const err =
            parsed?.data?.job?.result?.error ||
            parsed?.job?.result?.error ||
            parsed?.result?.error ||
            '';
          if (typeof err === 'string') {
            redactedError = err.split(fixtureAbsolute).join('<personaRoot>').slice(0, 300);
          }
        } catch {
          redactedError = null;
        }
      }
      fail(`the absolute persona root leaked into a WS frame (field: ${leakField(fixtureAbsolute)})${redactedError ? ` — redacted: ${redactedError}` : ''}`);
    }
    const mentorMdAbsolute = path.join(personaRoot, 'mentor', 'persona.md');
    if (wireText.includes(mentorMdAbsolute)) {
      fail(`a full absolute persona.md path leaked into a WS frame (field: ${leakField(mentorMdAbsolute)})`);
    }
    if (receivedFrames.length === 0) {
      fail('no WS frames were captured (wire evidence unavailable)');
    }
    if (!wireText.includes('durable mentor persona')) {
      fail('the persona definition body must stay legitimately visible on the wire');
    }
  } finally {
    await browser.close();
  }
}

main().catch((err) => {
  console.error(`personas: crashed: ${err.stack || err}`);
  process.exit(2);
});
