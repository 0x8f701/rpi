// Web /skill candidate regression lane (playwright half of
// E2E.d/web/skill_completion.sh).
//
// Environment:
//   RPI_URL             http://127.0.0.1:<port>/web
//   RPI_TOKEN           token file content (served via rpi-auth.<token> subprotocol)
//   RPI_PHASE           "with-skills" | "no-skill"
//   RPI_SKILL_NAME      first fixture skill name ("greet")
//   RPI_SKILL_DESC      first fixture skill description
//   RPI_SECOND_SKILL_NAME  second fixture skill name ("docs")
//   RPI_SECOND_SKILL_DESC  second fixture skill description
//   RPI_CHROME          executable path of the system Chrome (optional)
//   RPI_EVIDENCE        evidence dir for screenshots
//
// Assertions:
//   S1  the command picker lists /compact /skill /code-review (first open)
//   S2  REAL page.click on /skill drills the picker into skills mode (title
//       "Skills", back button present) and Skills STAYS OPEN — the click is
//       never misread as selecting a candidate; skills mode immediately shows
//       the persistent instruction "Select a skill, then press Enter to run
//       it". chooseCommand uses playwright's real click (mousedown → mouseup →
//       click), because a synthetic DOM row.click() skips the mousedown leg
//       and cannot catch a regression where choosing on mousedown swaps the
//       list mid-gesture and the trailing click selects a candidate.
//   S2b Escape in skills mode DRILLS BACK to the command list (title gone,
//       popover STAYS open, /skill parent visible again)
//   S3  with-skills: BOTH on-disk fixture skills render as
//       .command-picker__option[data-skill-name] rows with bare name +
//       non-empty frontmatter description — candidates come from the REAL
//       loaded catalog (get_commands), never a hardcoded list
//   S3b 主搜索: the main Command search box (commands mode) finds a concrete
//       skill by NAME directly (one candidate row, no Skills header)
//   S3b2 the same search also matches by a DESCRIPTION fragment (global-skill
//       search surfaces candidates by name OR description)
//   S3c Escape in commands mode CLOSES the popover (search input unmounts,
//       focus returns to the Command button); clicking Command again REOPENS
//       (再次打开) — and Escape closes the reopened popover too
//   S4  no-skill: skills mode shows "No skills loaded" + reload guidance
//       (project .pi/skills / user skills directory; restart the listener or
//       open a new session to reload) and zero candidate rows
//   S5  typing a concrete `/skill <query>` draft before opening the picker
//       goes straight to Skills prefiltered by that query (composer entry #2);
//       after the prefilter assertions, Escape drills back, Escape closes, and
//       Command REOPENS the picker
//   S5b REAL click on the greet candidate inserts `/skill greet` WITHOUT
//       auto-submitting: no user bubble, the composer carries the draft
//       SELECTED (visible ready state — Enter runs, typing replaces), and a
//       toast states "/skill greet ready — press Enter to run"
//   S6  Enter dispatches the `skill` RPC and the frontmatter summary bubble
//       (div.msg.msg--summary, label "skill") renders `name: greet` + the
//       description text

import { chromium } from 'playwright';

const url = process.env.RPI_URL;
const token = process.env.RPI_TOKEN || '';
const phase = process.env.RPI_PHASE || 'with-skills';
const skillName = process.env.RPI_SKILL_NAME || 'greet';
const skillDesc = process.env.RPI_SKILL_DESC || 'Greet skill for E2E';
const secondSkillName = process.env.RPI_SECOND_SKILL_NAME || 'docs';
const secondSkillDesc = process.env.RPI_SECOND_SKILL_DESC || 'Docs skill for E2E';
const chromePath = process.env.RPI_CHROME || '';
const evidence = process.env.RPI_EVIDENCE || '.';

function fail(message) {
  console.error(`web-skill-completion (${phase}): FAIL: ${message}`);
  process.exit(2);
}

async function waitFor(page, fn, label, timeoutMs = 25000, arg) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await page.evaluate(fn, arg)) return;
    await page.waitForTimeout(120);
  }
  fail(`${label} (timeout ${timeoutMs}ms)`);
}

async function openPicker(page) {
  // The Command button is a TOGGLE: clicking it while the popover is open
  // (e.g. after an Escape drill-back from skills mode) CLOSES it — the
  // waitFor below would then fail with "command picker popover did not open".
  // Only click when the popover is absent so this helper means "ensure open"
  // no matter the prior state (first open, reopen after Escape-close, or a
  // still-open drill-back). State check, not a timeout: deterministic.
  const alreadyOpen = await page.evaluate(
    () => document.querySelector('.command-picker__popover') !== null
  );
  if (alreadyOpen) return;
  await page.click('#command-btn');
  await waitFor(
    page,
    () => document.querySelector('.command-picker__popover') !== null,
    'command picker popover did not open'
  );
}

async function chooseCommand(page, name) {
  // The option rows are recreated per render, so resolve the CURRENT row with
  // a locator, then use playwright's REAL click (mousedown → mouseup → click).
  // A synthetic DOM row.click() dispatches only the click leg and cannot catch
  // the /skill drill regression: choosing on mousedown swaps the list to
  // skills mid-gesture, so the trailing click of the same press lands on a
  // candidate at that position and closes the popover. CommandPicker's
  // contract is mousedown = preventDefault only, click = the sole chooser —
  // this helper must exercise the full real-press sequence to guard it.
  const row = page.locator('.command-picker__option').filter({
    has: page.locator('.command-picker__name', { hasText: new RegExp(`^/${name}$`) }),
  });
  await row.first().waitFor({ state: 'visible', timeout: 25000 });
  await row.first().click();
}

/** REAL-click a skill candidate row (data-skill-name === name). Same
 *  mousedown → mouseup → click sequence a user produces. */
async function chooseSkill(page, name) {
  const row = page.locator('.command-picker__option[data-skill-name]').filter({
    has: page.locator('.command-picker__name', { hasText: new RegExp(`^${name}$`) }),
  });
  await row.first().waitFor({ state: 'visible', timeout: 25000 });
  await row.first().click();
}

/** Render the candidate rows as {name, desc} pairs. */
const candidateRows = () => {
  const rows = Array.from(document.querySelectorAll('.command-picker__option[data-skill-name]'));
  return rows.map((row) => ({
    name: row.getAttribute('data-skill-name'),
    desc: row.querySelector('.command-picker__desc')?.textContent?.trim() || '',
    label: row.querySelector('.command-picker__name')?.textContent?.trim() || '',
  }));
};

async function main() {
  if (!url) fail('RPI_URL is required');
  const launchOptions = chromePath ? { executablePath: chromePath } : {};
  const browser = await chromium.launch(launchOptions);
  try {
    const page = await browser.newPage();
    if (token) {
      await page.addInitScript((t) => { window.localStorage.setItem('rpi-web-token', t); }, token);
    }
    page.on('pageerror', (err) => {
      console.error(`web-skill-completion: page error: ${err.message}`);
    });

    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 20000 });
    await waitFor(page, () => document.title === 'rpi web', 'S1: page title missing');
    await waitFor(page, () => document.getElementById('conn-state') !== null, 'S1: conn-state missing');
    await waitFor(
      page,
      () => document.getElementById('conn-state').dataset.state === 'on',
      'S1: WS did not reach "connected"'
    );

    // S1: the picker lists the builtin commands.
    await openPicker(page);
    await waitFor(
      page,
      () => {
        const names = Array.from(document.querySelectorAll('.command-picker__option .command-picker__name')).map((el) => el.textContent.trim());
        return names.includes('/compact') && names.includes('/skill') && names.includes('/code-review');
      },
      'S1: picker did not list /compact /skill /code-review'
    );

    // S2: REAL click on /skill drills into skills mode — and Skills STAYS
    // OPEN (the waitFor below fails if the press was misread as selecting a
    // candidate, which would close the popover). chooseCommand clicks at
    // coordinates (mousedown → mouseup → click), matching real users.
    await chooseCommand(page, 'skill');
    await waitFor(
      page,
      () => document.querySelector('.command-picker__title')?.textContent?.trim() === 'Skills',
      'S2: real click on /skill did not drill the picker into skills mode (or the popover closed — a candidate was selected mid-press)'
    );
    const backBtn = await page.evaluate(
      () => document.querySelector('.command-picker__back') !== null
    );
    if (!backBtn) fail('S2: skills mode has no back button');
    // The persistent Enter-to-run instruction renders immediately with the
    // Skills title — the picker never auto-submits, so the contract is stated
    // inline (visible with candidates AND with an empty catalog).
    const tip = await page.evaluate(
      () => document.querySelector('.command-picker__tip')?.textContent?.trim() || ''
    );
    if (tip !== 'Select a skill, then press Enter to run it') {
      fail(`S2: skills mode instruction missing (got "${tip}")`);
    }

    if (phase === 'no-skill') {
      // S4: zero candidates + the guided empty state — "No skills loaded"
      // (title) plus where skills load from and how to reload (detail).
      await waitFor(
        page,
        () => document.querySelector('.command-picker__hint-title')?.textContent?.trim() === 'No skills loaded',
        'S4: no-skill workspace did not show the "No skills loaded" hint'
      );
      const guidance = await page.evaluate(
        () => document.querySelector('.command-picker__hint-detail')?.textContent || ''
      );
      for (const fragment of ['.pi/skills', 'user skills directory', 'Restart the listener', 'new session']) {
        if (!guidance.includes(fragment)) {
          fail(`S4: reload guidance missing "${fragment}" (got "${guidance}")`);
        }
      }
      const count = await page.evaluate(() => document.querySelectorAll('.command-picker__option[data-skill-name]').length);
      if (count !== 0) fail(`S4: no-skill workspace rendered ${count} candidate rows (expected 0)`);
      await page.screenshot({ path: `${evidence}/skill-no-skill.png`, fullPage: true });
      console.log(`web-skill-completion (${phase}): PASSED (builtins, real-click /skill drill-in with instruction, guided "No skills loaded" empty state with zero candidates)`);
      return;
    }

    // S3: BOTH on-disk skills are candidates (real loaded catalog, not a
    // hardcoded single entry).
    await waitFor(
      page,
      () => {
        const rows = Array.from(document.querySelectorAll('.command-picker__option[data-skill-name]'));
        return rows.length >= 2;
      },
      'S3: skills mode never rendered two candidate rows',
      25000
    );
    const candidates = await page.evaluate(candidateRows);
    const greet = candidates.find((c) => c.name === skillName);
    const docs = candidates.find((c) => c.name === secondSkillName);
    if (!greet) fail(`S3: ${skillName} candidate missing (got ${JSON.stringify(candidates)})`);
    if (!docs) fail(`S3: ${secondSkillName} candidate missing (got ${JSON.stringify(candidates)})`);
    if (greet.label !== skillName) fail(`S3: ${skillName} candidate label wrong (${greet.label})`);
    if (!greet.desc.includes(skillDesc)) fail(`S3: ${skillName} candidate desc does not carry its frontmatter (${greet.desc})`);
    if (!docs.desc.includes(secondSkillDesc)) fail(`S3: ${secondSkillName} candidate desc does not carry its frontmatter (${docs.desc})`);
    await page.screenshot({ path: `${evidence}/skill-candidates-multi.png`, fullPage: true });

    // S2b: Escape in skills mode DRILLS BACK to the command list — the
    // popover STAYS open and the `/skill` parent is visible again. Escape
    // never closes from skills mode (drill-back, not dismiss).
    await page.press('.command-picker__search', 'Escape');
    await waitFor(
      page,
      () => {
        const popover = document.querySelector('.command-picker__popover');
        if (!popover) return false;
        const names = Array.from(document.querySelectorAll('.command-picker__option .command-picker__name')).map((el) => el.textContent.trim());
        return document.querySelector('.command-picker__title') === null && names.includes('/skill');
      },
      'S2b: Escape did not drill back from skills mode to the command list (popover stays open)'
    );

    // S3b 主搜索: the main Command search box (commands mode — the popover is
    // still open after the drill-back, so no reopen is needed) finds a
    // concrete skill directly: one candidate row, no Skills header.
    await page.fill('.command-picker__search', secondSkillName);
    await waitFor(
      page,
      (want) => {
        const rows = Array.from(document.querySelectorAll('.command-picker__option[data-skill-name]'));
        return document.querySelector('.command-picker__title') === null
          && rows.length === 1
          && rows[0].getAttribute('data-skill-name') === want
          && (rows[0].querySelector('.command-picker__name')?.textContent || '').includes(want);
      },
      `S3b: primary command search did not find skill ${secondSkillName}`,
      10000,
      secondSkillName,
    );
    await page.fill('.command-picker__search', '');

    // S3b2: the SAME search box also matches by DESCRIPTION fragment (not
    // just name) — global-skill search surfaces candidates by either field.
    // Use the first two words of the fixture description ("Docs skill").
    const descFragment = secondSkillDesc.split(/\s+/).slice(0, 2).join(' ');
    await page.fill('.command-picker__search', descFragment);
    await waitFor(
      page,
      (want) => {
        const rows = Array.from(document.querySelectorAll('.command-picker__option[data-skill-name]'));
        return document.querySelector('.command-picker__title') === null
          && rows.length === 1
          && rows[0].getAttribute('data-skill-name') === want;
      },
      `S3b2: description search "${descFragment}" did not surface ${secondSkillName}`,
      10000,
      secondSkillName,
    );
    await page.fill('.command-picker__search', '');

    // S3c: Escape in commands mode CLOSES the popover — the search input
    // unmounts and focus returns to the trigger button.
    await page.press('.command-picker__search', 'Escape');
    await waitFor(
      page,
      () => {
        const popover = document.querySelector('.command-picker__popover');
        const search = document.querySelector('.command-picker__search');
        return popover === null && search === null && document.activeElement?.id === 'command-btn';
      },
      'S3c: Escape did not close the popover and return focus to the Command button'
    );

    // S3c 再次打开: clicking Command after an Escape-close reopens the picker.
    await openPicker(page);
    await waitFor(
      page,
      () => document.querySelector('.command-picker__popover') !== null,
      'S3c: Command click after Escape-close did not reopen the popover'
    );
    // ...and the reopened popover closes the same way.
    await page.press('.command-picker__search', 'Escape');
    await waitFor(
      page,
      () => document.querySelector('.command-picker__popover') === null,
      'S3c: Escape did not close the reopened popover'
    );

    // S5: typing a concrete `/skill <query>` draft before opening the picker
    // goes straight to Skills with that query prefilled and only matches the
    // requested skill. This is the primary discoverability path for large
    // catalogs; users need not click `/skill` then search from scratch.
    await page.fill('#prompt-input', `/skill ${secondSkillName}`);
    await openPicker(page);
    await waitFor(
      page,
      (want) => {
        const search = document.querySelector('.command-picker__search');
        const rows = Array.from(document.querySelectorAll('.command-picker__option[data-skill-name]'));
        return document.querySelector('.command-picker__title')?.textContent?.trim() === 'Skills'
          && search?.value === want
          && rows.length === 1
          && rows[0].getAttribute('data-skill-name') === want;
      },
      `S5: /skill ${secondSkillName} did not open a prefiltered skill search`,
      10000,
      secondSkillName,
    );
    await page.screenshot({ path: `${evidence}/skill-direct-search.png`, fullPage: true });
    // Escape #1 (skills mode, prefiltered): drill back to the command list —
    // the popover stays open, the `/skill` parent is visible again.
    await page.press('.command-picker__search', 'Escape');
    await waitFor(
      page,
      () => {
        const popover = document.querySelector('.command-picker__popover');
        if (!popover) return false;
        const names = Array.from(document.querySelectorAll('.command-picker__option .command-picker__name')).map((el) => el.textContent.trim());
        return document.querySelector('.command-picker__title') === null && names.includes('/skill');
      },
      'S5: Escape did not drill the prefiltered skills mode back to commands'
    );
    // Escape #2 (commands mode): CLOSES the popover.
    await page.press('.command-picker__search', 'Escape');
    await waitFor(
      page,
      () => document.querySelector('.command-picker__popover') === null,
      'S5: Escape did not close the picker from commands mode'
    );
    await page.fill('#prompt-input', '');
    // 再次打开: Command reopens after the Escape-close (fresh commands mode —
    // the emptied composer carries no /skill intent).
    await openPicker(page);
    await waitFor(
      page,
      () => document.querySelector('.command-picker__popover') !== null,
      'S5: Command click did not reopen the picker after Escape-close'
    );
    await chooseCommand(page, 'skill');

    // S5b: REAL click on the greet candidate -> `/skill greet` in the
    // composer, NO auto-submit (no user bubble until Enter).
    await chooseSkill(page, skillName);
    await waitFor(
      page,
      (want) => (document.getElementById('prompt-input')?.value.trim() || '') === `/skill ${want}`,
      `S5b: selecting ${skillName} did not insert /skill ${skillName} into the composer`,
      10000,
      skillName,
    );
    // Feedback: the draft is SELECTED in the composer — the visible ready
    // state (Enter runs, typing replaces the selection).
    const selection = await page.evaluate((want) => {
      const input = document.getElementById('prompt-input');
      if (!input) return null;
      return { value: input.value, start: input.selectionStart, end: input.selectionEnd };
    }, skillName);
    if (!selection) fail('S5b: composer input missing after skill selection');
    const draft = `/skill ${skillName}`;
    if (selection.value.trim() !== draft) fail(`S5b: composer draft wrong (${JSON.stringify(selection.value)})`);
    if (selection.start !== 0 || selection.end !== draft.length) {
      fail(`S5b: /skill draft not fully selected (start=${selection.start}, end=${selection.end}, len=${draft.length})`);
    }
    // Feedback: a toast states the exact draft + the Enter-to-run action.
    await waitFor(
      page,
      (want) => {
        const toasts = Array.from(document.querySelectorAll('#toasts .toast'));
        return toasts.some((t) => (t.textContent || '').includes(`${want} ready — press Enter to run`));
      },
      `S5b: selecting ${skillName} did not toast "${draft} ready — press Enter to run"`,
      10000,
      draft,
    );
    const autoSubmitted = await page.evaluate(() =>
      Array.from(document.querySelectorAll('.msg--user')).some((el) =>
        (el.textContent || '').includes('/skill')
      )
    );
    if (autoSubmitted) fail('S5b: /skill auto-submitted — a user bubble appeared without Enter');

    // S6: Enter dispatches the skill RPC and the summary bubble renders the
    // frontmatter (label "skill", text carries `name: greet` + description).
    await page.press('#prompt-input', 'Enter');
    await waitFor(
      page,
      (args) => {
        const bubbles = Array.from(document.querySelectorAll('.msg--summary'));
        return bubbles.some((b) => {
          const label = b.querySelector('.msg--summary__label')?.textContent?.trim() || '';
          const text = b.querySelector('.msg--summary__text')?.textContent || '';
          return label === 'skill' && text.includes(`name: ${args.skillName}`) && text.includes(args.desc);
        });
      },
      `S6: /skill ${skillName} did not render the frontmatter summary bubble`,
      20000,
      { skillName, desc: skillDesc }
    );
    await page.screenshot({ path: `${evidence}/skill-summary.png`, fullPage: true });

    console.log(`web-skill-completion (${phase}): PASSED (builtins first open; real-click /skill drill-in with Enter-to-run instruction; Escape drill-back keeps popover open; name search finds skill directly; description-fragment search; Escape close + focus restore; Command reopen; /skill <query> prefilter; Escape drill-back + close + reopen; real-click greet select -> /skill greet selected + ready toast, no auto-submit; Enter -> frontmatter summary bubble)`);
  } finally {
    await browser.close().catch(() => {});
  }
}

main().catch((err) => {
  console.error(`web-skill-completion (${phase}): FAIL: ${err && err.message ? err.message : err}`);
  process.exit(2);
});
