// Shared feature-menu journey: the #feature-menu portal starts closed.
const FEATURE_MENU_BTN = '#feature-menu-btn';
const FEATURE_MENU = '#feature-menu';

export async function openFeatureMenu(page) {
  const trigger = page.locator(FEATURE_MENU_BTN);
  if ((await trigger.getAttribute('aria-expanded')) !== 'true') await trigger.click();
  await page.locator(FEATURE_MENU).waitFor({ state: 'visible' });
}

// Open, click the explicit original control id, then wait for the close.
export async function clickFeature(page, selector) {
  await openFeatureMenu(page);
  await page.locator(selector).click();
  await page.locator(FEATURE_MENU).waitFor({ state: 'hidden' });
}

const SESSIONS_ITEM = '#sidebar-menu-item';

// Feature-menu journey: ends with the drawer OPEN.
// Desktop keeps the idempotent early return (an already-open rail must not be
// anti-toggled by a second journey). At ≤720px there is NO early return: the
// trigger click itself force-collapses the drawer (App closes the sidebar on
// menu open), so the real Menu→Sessions journey is race-free against a
// viewport crossing and always ends OPEN.
export async function openSessionsDrawer(page) {
  const mobile = await page.evaluate(() => window.matchMedia('(max-width: 720px)').matches);
  if (!mobile && await page.evaluate(() => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true)) return;
  await openFeatureMenu(page);
  await page.locator(SESSIONS_ITEM).click();
  await page.waitForFunction(
    () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') === true,
  );
}

// Menu journey close: ≤720px trigger click collapses the drawer (Escape kills
// the menu); desktop toggles via the entry. Idempotent: an already-closed
// drawer returns without the menu trip.
export async function closeSessionsDrawer(page) {
  if (await page.evaluate(() => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') !== true)) return;
  await openFeatureMenu(page);
  if (await page.evaluate(() => window.matchMedia('(max-width: 720px)').matches)) {
    await page.keyboard.press('Escape');
  } else {
    await page.locator(SESSIONS_ITEM).click();
  }
  await page.waitForFunction(
    () => document.querySelector('.app-layout')?.classList.contains('app-layout--drawer-open') !== true,
  );
}
