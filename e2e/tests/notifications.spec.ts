/**
 * The notification log: every toast also lands in a persistent,
 * inspectable list (bell in the sidebar footer, Ctrl+K "Show notifications").
 * See E2E_TESTBOOK.md ("Notifications").
 */
import {test, expect, type Page} from '@playwright/test';

/** Opens a route's policy on the canvas and waits for the graph to render. */
async function openRoute(page: Page, route: string) {
  await page.goto('/');
  await page.getByText(route, {exact: true}).click();
  await page.waitForSelector('.react-flow__node');
}

/** Deletes cors's `preflight` edge (cors -> client) so the next save is rejected. */
async function unwirePreflight(page: Page) {
  const edge = page.locator('[aria-label="Edge from cors to client"]');
  const box = await edge.boundingBox();
  if (!box) throw new Error('preflight edge (cors -> client) not found on canvas');
  await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
  await page.getByRole('button', {name: 'Delete Edge'}).click();
}

test.describe('Notifications', () => {
  test.beforeEach(async ({page}) => {
    // Each scenario starts from an empty log: the store is per browser
    // (localStorage), and Playwright gives every test a fresh context, but be
    // explicit so a future shared-context change can't leak entries across.
    await page.goto('/');
    await page.evaluate(() => localStorage.removeItem('featherbit.notifications'));
  });

  /**
   * E2E-NOTIF-01: a rejected save is not just a 5-second toast. The bell
   * shows an unread badge, the toast's "Details" opens the log on that
   * entry, and the entry carries the server's full rejection — still there
   * after a reload.
   */
  test('E2E-NOTIF-01: a rejected save is inspectable afterwards, and survives a reload', async ({page}) => {
    await openRoute(page, 'echo-api');
    const bell = page.getByRole('button', {name: 'Notifications'});
    await expect(bell).toBeVisible();
    await expect(page.getByTestId('notifications-badge')).toHaveCount(0);

    await unwirePreflight(page);
    await page.getByRole('button', {name: 'Save Policy'}).click();
    await expect(page.getByText('Failed to save policy')).toBeVisible();

    // The unread badge counts the error, and the toast links into the log.
    await expect(page.getByTestId('notifications-badge')).toHaveText('1');
    await page.getByRole('button', {name: 'Details'}).click();

    const panel = page.getByRole('dialog', {name: 'Notifications'});
    await expect(panel).toBeVisible();
    const row = panel.getByRole('button', {name: /Failed to save policy/});
    await expect(row).toBeVisible();
    // Opened on that entry: its details are already expanded, showing the
    // server's authoritative reason verbatim.
    const details = panel.getByTestId('notification-details');
    await expect(details).toContainText('must be wired — add an edge from');
    await expect(details).toContainText("'cors'");
    // Opening the panel marks everything as seen.
    await expect(page.getByTestId('notifications-badge')).toHaveCount(0);

    // Persistence: still listed after a full reload, and still expandable.
    await page.reload();
    await page.getByRole('button', {name: 'Notifications'}).click();
    const reopened = page.getByRole('dialog', {name: 'Notifications'});
    await expect(reopened.getByRole('button', {name: /Failed to save policy/})).toBeVisible();
    await reopened.getByRole('button', {name: /Failed to save policy/}).click();
    await expect(reopened.getByTestId('notification-details')).toContainText('must be wired');
  });

  /**
   * E2E-NOTIF-02: successes are logged too (a timeline of what was saved
   * when), the palette opens the panel, the Errors filter narrows the list,
   * and Clear empties it — including the persisted copy.
   */
  test('E2E-NOTIF-02: successes are logged, palette opens the panel, filter and clear work', async ({page}) => {
    await openRoute(page, 'echo-api');
    await page.getByRole('button', {name: 'Save Policy'}).click();
    await expect(page.getByText('Policy saved')).toBeVisible();
    // A success never raises the unread badge.
    await expect(page.getByTestId('notifications-badge')).toHaveCount(0);

    await unwirePreflight(page);
    await page.getByRole('button', {name: 'Save Policy'}).click();
    await expect(page.getByText('Failed to save policy')).toBeVisible();

    await page.keyboard.press('Control+k');
    await page.getByRole('dialog', {name: 'Command palette'}).getByText('Show notifications').click();
    const panel = page.getByRole('dialog', {name: 'Notifications'});
    await expect(panel).toBeVisible();
    await expect(panel.getByRole('button', {name: /Policy saved/})).toBeVisible();
    await expect(panel.getByRole('button', {name: /Failed to save policy/})).toBeVisible();

    await panel.getByRole('button', {name: 'Errors', exact: true}).click();
    await expect(panel.getByRole('button', {name: /Policy saved/})).toHaveCount(0);
    await expect(panel.getByRole('button', {name: /Failed to save policy/})).toBeVisible();

    await panel.getByRole('button', {name: 'Clear log'}).click();
    await expect(panel.getByText('No notifications yet')).toBeVisible();
    expect(
      await page.evaluate(() => JSON.parse(localStorage.getItem('featherbit.notifications') ?? '[]')),
    ).toEqual([]);
  });
});
