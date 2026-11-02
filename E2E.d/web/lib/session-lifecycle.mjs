// Shared Web E2E journey for the only user-facing New-session route:
// sidebar Manage Session -> Session panel -> New session.

function panelSessionId() {
  const dts = [...document.querySelectorAll('#session-panel dl dt')];
  const idx = dts.findIndex((dt) => dt.textContent === 'Session id');
  return idx >= 0 ? (dts[idx]?.nextElementSibling?.textContent || '').trim() : '';
}

export async function assertSidebarNewActionAbsent(page, label = 'obsolete sidebar New action still rendered') {
  const present = await page.evaluate(() => document.getElementById('sidebar-new-session-btn') !== null);
  if (present) throw new Error(label);
}

async function clickControl(locator, dispatchClick) {
  if (dispatchClick) {
    await locator.evaluate((element) => {
      element.click();
    });
    return;
  }
  await locator.click();
}

export async function openManageSession(page, waitFor, label, timeoutMs = 30000, options = {}) {
  await assertSidebarNewActionAbsent(page, `${label}: obsolete sidebar New action still rendered`);
  const button = page.locator('#sidebar-manage-btn');
  if ((await button.count()) === 0) throw new Error(`${label}: Manage Session button missing`);
  await clickControl(button, options.dispatchClick === true);
  await waitFor(
    page,
    () => document.getElementById('session-panel') !== null && document.getElementById('session-new-btn') !== null,
    `${label}: session panel did not open from Manage Session`,
    timeoutMs,
  );
}

export async function closeManageSession(page, waitFor, label, timeoutMs = 30000) {
  const panelOpen = await page.evaluate(() => document.getElementById('session-panel') !== null);
  if (!panelOpen) return;
  await page.locator('#session-close-btn').click();
  await waitFor(
    page,
    () => document.getElementById('session-panel') === null,
    `${label}: session panel did not close`,
    timeoutMs,
  );
}

export async function createFreshSessionViaManage(
  page,
  waitFor,
  label,
  { timeoutMs = 30000, closePanel = true, explicitCwd = '' } = {},
) {
  const beforeActive = await page.evaluate(() =>
    document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || ''
  );

  await openManageSession(page, waitFor, label, timeoutMs);
  await waitFor(
    page,
    () => {
      const dts = [...document.querySelectorAll('#session-panel dl dt')];
      const idx = dts.findIndex((dt) => dt.textContent === 'Session id');
      const id = idx >= 0 ? (dts[idx]?.nextElementSibling?.textContent || '').trim() : '';
      return id !== '' && id !== '—';
    },
    `${label}: current session id did not load in Manage Session`,
    timeoutMs,
  );
  const beforePanel = await page.evaluate(panelSessionId);
  const before = beforeActive || beforePanel;
  if (!before) throw new Error(`${label}: current session id missing before New session`);

  // The picker is ALWAYS empty; New only enables after an explicit
  // directory is typed, so every create must fill one before clicking.
  // The current backend cwd is a guaranteed-existing directory; typing it
  // is an explicit selection and the wire frame always carries cwd.
  const displayedCwd = await page.evaluate(() => {
    const el = document.querySelector('[data-testid="session-cwd-value"]');
    return el ? el.getAttribute('title') || '' : '';
  });
  const cwd = explicitCwd || displayedCwd;
  if (!cwd || cwd === 'Unavailable') {
    throw new Error(`${label}: no backend cwd available to type as the explicit New-session directory`);
  }
  await page.fill('#session-new-cwd-input', cwd);
  await page.click('#session-new-btn');
  await waitFor(
    page,
    (previous) => {
      const active = document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '';
      const dts = [...document.querySelectorAll('#session-panel dl dt')];
      const idx = dts.findIndex((dt) => dt.textContent === 'Session id');
      const panel = idx >= 0 ? (dts[idx]?.nextElementSibling?.textContent || '').trim() : '';
      return active !== '' && panel !== '' && panel !== '—' && active !== previous && panel !== previous && active === panel;
    },
    `${label}: Manage Session did not create and activate a fresh session`,
    timeoutMs,
    before,
  );

  const after = await page.evaluate(() => {
    const dts = [...document.querySelectorAll('#session-panel dl dt')];
    const idx = dts.findIndex((dt) => dt.textContent === 'Session id');
    return {
      active: document.querySelector('.session-sidebar__row--active .session-sidebar__switch')?.dataset.sessionId || '',
      panel: idx >= 0 ? (dts[idx]?.nextElementSibling?.textContent || '').trim() : '',
    };
  });
  if (!after.active || after.active !== after.panel || after.active === before) {
    throw new Error(`${label}: fresh session identity mismatch (before=${before}, active=${after.active}, panel=${after.panel})`);
  }

  if (closePanel) await closeManageSession(page, waitFor, label, timeoutMs);
  return { beforeSessionId: before, sessionId: after.active, explicitCwd: cwd };
}

export async function clickNewSessionExpectingFailureViaManage(
  page,
  waitFor,
  label,
  { timeoutMs = 30000, failureText = '', closePanel = false } = {},
) {
  await openManageSession(page, waitFor, label, timeoutMs, { dispatchClick: true });

  // The picker is ALWAYS empty; the failure click must still type an
  // explicit directory first (the panel may not have backend info during a
  // disconnect, so fall back to a guaranteed-existing absolute directory).
  const cwd = await page.evaluate(() => {
    const el = document.querySelector('[data-testid="session-cwd-value"]');
    return el ? el.getAttribute('title') || '' : '';
  });
  const explicitCwd = cwd && cwd !== 'Unavailable' ? cwd : '/tmp';
  await page.fill('#session-new-cwd-input', explicitCwd);

  const toastsBefore = await page.evaluate(() => ({
    total: document.querySelectorAll('#toasts .toast').length,
    transportKind: document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length,
  }));
  await clickControl(page.locator('#session-new-btn'), true);
  // The failure surface is the panel-owned bottom-right toast and ONLY
  // that: exactly ONE new toast naming new_session (error-styled), NO
  // App-owned transport-kind outage toast (never duplicates), and ZERO
  // in-panel status (the panel no longer renders .panel__status at all).
  await waitFor(
    page,
    ({ expected, before }) => {
      const toasts = [...document.querySelectorAll('#toasts .toast')].map((t) => t.textContent || '');
      const fresh = toasts.slice(before.total);
      return fresh.length === 1
        && fresh[0].includes('new session failed:')
        && (expected === '' || fresh[0].includes(expected));
    },
    `${label}: New-session failure did not surface as exactly one bottom-right toast`,
    timeoutMs,
    { expected: failureText, before: toastsBefore },
  );
  const surface = await page.evaluate((before) => ({
    fresh: [...document.querySelectorAll('#toasts .toast')].slice(before.total).map((t) => ({
      text: t.textContent || '',
      error: t.classList.contains('toast--error'),
    })),
    transportKind: document.querySelectorAll('#toasts .toast[data-toast-kind="transport"]').length,
    panelStatus: document.querySelector('#session-panel .panel__status'),
  }), toastsBefore);
  const fresh = surface.fresh;
  if (fresh.length !== 1) {
    throw new Error(`${label}: expected exactly one failure toast (got ${fresh.length})`);
  }
  const error = fresh[0].text;
  if (!fresh[0].error) {
    throw new Error(`${label}: the New-session failure toast is not error-styled (${error})`);
  }
  if (surface.transportKind !== toastsBefore.transportKind) {
    throw new Error(`${label}: an App-owned transport-kind outage toast appeared alongside the panel failure toast`);
  }
  if (surface.panelStatus !== null) {
    throw new Error(`${label}: panel rendered an in-panel status surface (${surface.panelStatus?.textContent || ''})`);
  }
  if (closePanel) await closeManageSession(page, waitFor, label, timeoutMs);
  return error;
}
