/**
 * The command palette (Ctrl+K) and the global bare-letter shortcuts it
 * documents. See E2E_TESTBOOK.md ("Command palette").
 */
import { test, expect, type Page } from '@playwright/test';

/** Opens a route's policy on the canvas and waits for the graph to render. */
async function openRoute(page: Page, route: string) {
  await page.goto('/');
  await page.getByText(route, { exact: true }).click();
  await page.waitForSelector('.react-flow__node');
}

test.describe('Command palette', () => {
  /** E2E-UI-17: Ctrl+K opens the palette, which lists actions with shortcuts. */
  test('E2E-UI-17: command palette opens and lists actions with shortcuts', async ({ page }) => {
    await page.goto('/');

    await page.keyboard.press('Control+k');
    const palette = page.getByRole('dialog', { name: 'Command palette' });
    await expect(palette).toBeVisible();
    await expect(palette.getByText('Toggle port names')).toBeVisible();
    await expect(palette.getByText('New route')).toBeVisible();
    // Shortcut chips are rendered beside the titles.
    await expect(palette.locator('kbd', { hasText: 'P' }).first()).toBeVisible();
    // Filtering narrows the list.
    await page.keyboard.type('route');
    await expect(palette.getByText('New route')).toBeVisible();
    await expect(palette.getByText('Toggle port names')).toHaveCount(0);
    await page.keyboard.press('Escape');
    await expect(palette).toHaveCount(0);
  });

  /** E2E-UI-18: toggling port names hides the rows and survives a reload. */
  test('E2E-UI-18: port-name toggle persists across reload', async ({ page }) => {
    await openRoute(page, 'echo-api');
    const corsNode = page.locator('.react-flow__node', { hasText: 'cors' }).first();
    await expect(corsNode.getByText('preflight', { exact: true })).toBeVisible();

    await page.keyboard.press('Control+k');
    await page.getByRole('dialog', { name: 'Command palette' }).getByText('Toggle port names').click();
    await expect(corsNode.getByText('preflight', { exact: true })).toHaveCount(0);
    // The handle itself survives — only the label is hidden.
    await expect(corsNode.locator('[data-handleid="preflight"]')).toHaveCount(1);

    await page.reload();
    // Selection state lives in React, not localStorage: re-select the route.
    await page.getByText('echo-api', { exact: true }).click();
    await page.waitForSelector('.react-flow__node');
    const afterReload = page.locator('.react-flow__node', { hasText: 'cors' }).first();
    await expect(afterReload).toBeVisible();
    await expect(afterReload.getByText('preflight', { exact: true })).toHaveCount(0);
  });

  /** E2E-UI-19: a bare shortcut runs its action; typing in a field does not. */
  test('E2E-UI-19: bare shortcut opens the new-route dialog, inputs are exempt', async ({ page }) => {
    await page.goto('/');

    await page.keyboard.press('r');
    const dialog = page.getByRole('dialog', { name: 'New route' });
    await expect(dialog).toBeVisible();

    // Typing "r" inside the dialog's field must not re-trigger the shortcut.
    // DialogField's <label> has no for/id — the codebase's own idiom (see
    // editor.spec.ts E2E-UI-06) is to target the field by its placeholder.
    const nameField = page.getByPlaceholder('echo-api');
    await nameField.click();
    await nameField.press('r');
    // Discriminating assertion: `dialog.toHaveCount(1)` alone would pass even
    // if the input-exemption guard were removed, because App's
    // handleCreateRoute is idempotent (re-opening an already-open dialog just
    // resets its fields) — a broken guard would still leave exactly one
    // dialog on screen. Asserting the keystroke actually landed in the field
    // is what a broken guard changes: with the guard removed, the global
    // handler's preventDefault() would suppress the character and
    // handleCreateRoute's setNewName('') would clear the field, so the value
    // would be '' instead of 'r'.
    await expect(nameField).toHaveValue('r');
    await expect(dialog).toHaveCount(1);

    await page.keyboard.press('Escape');
  });

  /** E2E-UI-20: canvas-owned actions appear only with the editor open. */
  test('E2E-UI-20: "A" opens the plugin drawer from the canvas', async ({ page }) => {
    // With a policy open in the editor:
    await openRoute(page, 'echo-api');

    await page.keyboard.press('a');
    await expect(page.getByPlaceholder('Search plugins')).toBeVisible();
    await page.keyboard.press('Escape');

    await page.keyboard.press('Control+k');
    const palette = page.getByRole('dialog', { name: 'Command palette' });
    await expect(palette.getByText('Add plugin to canvas')).toBeVisible();
    await expect(palette.getByText('Save policy')).toBeVisible();
  });
});
