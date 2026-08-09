/**
 * The command palette (Ctrl+K) and the global bare-letter shortcuts it
 * documents. See E2E_TESTBOOK.md ("Command palette").
 */
import { test, expect, type Page } from '@playwright/test';
import { adminApi } from '../helpers/admin';

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

    // Nothing is selected here, so no graph is open and the two canvas-owned
    // actions must not be listed. This is not the same case as E2E-UI-21:
    // there no canvas is mounted at all, whereas App mounts GraphCanvas with
    // `policy={null}` on a fresh load, and GraphCanvas registers its actions
    // above its own empty-state early return (hook order can't vary). So
    // "something registered `save-graph`" is not "a graph is open" —
    // CommandContext.editorOpen is what decides, and without it both rows
    // would show up right here and run as no-ops.
    await expect(palette.getByText('Add plugin to canvas')).toHaveCount(0);
    await expect(palette.getByText('Save policy')).toHaveCount(0);
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

  /**
   * E2E-UI-19: a bare shortcut runs its action; typing does not — not in a
   * text input, not behind an open dialog, and not in a native `<select>`.
   */
  test('E2E-UI-19: bare shortcuts are inert in inputs, behind a dialog, and in a select', async ({
    page,
  }) => {
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

    // Dialog has no focus trap and no Escape handling (see its own docblock),
    // so clicking its non-interactive header blurs the autofocused field and
    // focus falls back to <body> — past the input exemption above. A bare
    // letter must still not fire: without the modal guard, "s" would stack a
    // second dialog on top of this one at the same z-index.
    await dialog.getByText('New route', { exact: true }).click();
    expect(await page.evaluate(() => document.activeElement?.tagName)).toBe('BODY');
    await page.keyboard.press('s');
    await expect(page.getByRole('dialog', { name: 'New supernode' })).toHaveCount(0);
    await expect(page.getByRole('dialog')).toHaveCount(1);

    await page.getByRole('button', { name: 'Cancel' }).click();
    await expect(dialog).toHaveCount(0);

    // A native <select> uses bare letters for type-ahead, so it needs the same
    // exemption as INPUT/TEXTAREA. Seed a shared config whose name starts with
    // "s" — the same letter bound to New supernode — so the two behaviours are
    // distinguishable: with SELECT missing from the guard, preventDefault()
    // kills the type-ahead (value stays "") and the supernode dialog opens.
    const api = await adminApi();
    await api.delete('/api/plugin-configs/s-cp-select-probe');
    expect(
      (
        await api.put('/api/plugin-configs/s-cp-select-probe', {
          data: {
            name: 's-cp-select-probe',
            type: 'cors',
            config: { allowed_origins: ['https://probe.example.com'] },
          },
        })
      ).ok()
    ).toBeTruthy();

    // Reload so the new config is in the catalog the inspector's picker reads.
    await openRoute(page, 'echo-api');
    await page.locator('.react-flow__node', { hasText: 'cors' }).first().click();
    // NodeInspector's "Shared config" picker — identified by the option it now
    // offers, so it can't be confused with a SchemaForm enum field.
    const picker = page
      .locator('select')
      .filter({ has: page.locator('option[value="s-cp-select-probe"]') });
    await expect(picker).toHaveValue('');

    // focus(), not click(): clicking opens the native dropdown popup, which
    // lives outside the page and swallows the keystroke.
    await picker.focus();
    await page.keyboard.press('s');
    await expect(picker).toHaveValue('s-cp-select-probe');
    await expect(page.getByRole('dialog', { name: 'New supernode' })).toHaveCount(0);

    await api.delete('/api/plugin-configs/s-cp-select-probe');
    await api.dispose();
  });

  /** E2E-UI-20: canvas-owned actions appear only with the editor open. */
  test('E2E-UI-20: "A" opens the plugin drawer from the canvas', async ({ page }) => {
    // With a policy open in the editor:
    await openRoute(page, 'echo-api');

    await page.keyboard.press('a');
    await expect(page.getByPlaceholder('Search plugins')).toBeVisible();
    // The drawer autofocuses its search box, so Escape there closes it.
    await page.keyboard.press('Escape');
    await expect(page.getByPlaceholder('Search plugins')).toHaveCount(0);

    await page.keyboard.press('Control+k');
    const palette = page.getByRole('dialog', { name: 'Command palette' });
    await expect(palette.getByText('Add plugin to canvas')).toBeVisible();
    await expect(palette.getByText('Save policy')).toBeVisible();
  });

  /** E2E-UI-21: canvas-owned actions stay hidden with no canvas mounted. */
  test('E2E-UI-21: a selected plugin config (no canvas) hides the canvas-owned actions', async ({ page }) => {
    const api = await adminApi();
    await api.delete('/api/plugin-configs/e2e-cp-no-canvas');
    expect(
      (
        await api.put('/api/plugin-configs/e2e-cp-no-canvas', {
          data: { name: 'e2e-cp-no-canvas', type: 'mocking', config: { response_status: 200 } },
        })
      ).ok()
    ).toBeTruthy();

    await page.goto('/');
    // Selecting a shared plugin config renders PluginConfigPanel instead of
    // GraphCanvas — no canvas is mounted, so no editor action is registered.
    await page.getByText('e2e-cp-no-canvas', { exact: true }).click();
    await expect(page.getByText('e2e-cp-no-canvas').first()).toBeVisible();

    await page.keyboard.press('a');
    await expect(page.getByPlaceholder('Search plugins')).toHaveCount(0);

    await page.keyboard.press('Control+k');
    const palette = page.getByRole('dialog', { name: 'Command palette' });
    await expect(palette).toBeVisible();
    await expect(palette.getByText('Add plugin to canvas')).toHaveCount(0);
    await expect(palette.getByText('Save policy')).toHaveCount(0);
    await page.keyboard.press('Escape');

    await api.delete('/api/plugin-configs/e2e-cp-no-canvas');
  });
  /**
   * E2E-UI-22: Ctrl+S reaches the save action from inside a text field, and is
   * swallowed even when there is nothing to save.
   */
  test('E2E-UI-22: Ctrl+S saves from a focused text field and is always suppressed', async ({
    page,
  }) => {
    const api = await adminApi();
    // rt-policy is the designated throwaway; restore it verbatim afterwards so
    // the save this test performs leaves nothing behind (node positions).
    const seed = await (await api.get('/api/policies/rt-policy')).json();

    await openRoute(page, 'rt-api');
    await page.locator('.react-flow__node', { hasText: 'auth' }).first().click();
    // The inspector's read-only Node ID field: an <input>, hence covered by the
    // single-letter exemption — which is exactly the point. Ctrl+S is a
    // modifier binding and must run anyway. Before the fix the exemption
    // returned before the shortcut loop was ever reached, so nothing happened.
    const nodeId = page.locator('input[readonly][value="auth"]');
    await expect(nodeId).toBeVisible();
    await nodeId.click();
    await page.keyboard.press('Control+s');
    await expect(page.getByText('Policy saved')).toBeVisible();

    await api.put('/api/policies/rt-policy', { data: seed });

    // With nothing selected, `save-graph` is unavailable — but Ctrl+S must
    // still call preventDefault() or the browser opens its Save Page dialog.
    // That dialog is browser chrome and invisible to Playwright, so observe
    // the only in-page consequence there is: defaultPrevented on the event.
    // This listener is added after App's (both on window, both bubbling), so
    // it runs second and sees the flag App set — which also pins the memoized
    // CommandContext: if the keydown effect resubscribed on every render,
    // App's listener would be re-added *after* this one and see `false`.
    await page.goto('/');
    await expect(page.getByText('rt-api', { exact: true })).toBeVisible();
    await page.evaluate(() => {
      (window as unknown as { __ctrlS: boolean[] }).__ctrlS = [];
      window.addEventListener('keydown', (e) => {
        if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 's') {
          (window as unknown as { __ctrlS: boolean[] }).__ctrlS.push(e.defaultPrevented);
        }
      });
    });
    await page.keyboard.press('Control+s');
    expect(await page.evaluate(() => (window as unknown as { __ctrlS: boolean[] }).__ctrlS)).toEqual(
      [true]
    );
    // ...and being suppressed is all it does: no save toast, since no graph.
    await expect(page.getByText('Policy saved')).toHaveCount(0);

    await api.dispose();
  });

  /** E2E-UI-23: Escape closes the palette even after focus leaves its input. */
  test('E2E-UI-23: Escape closes the palette with focus outside the search input', async ({
    page,
  }) => {
    await page.goto('/');
    await page.keyboard.press('Control+k');
    const palette = page.getByRole('dialog', { name: 'Command palette' });
    await expect(palette).toBeVisible();

    // Filter to nothing, so the list is one non-interactive row to click.
    await page.keyboard.type('zzz-no-such-command');
    const empty = palette.getByText('No matching command');
    await expect(empty).toBeVisible();

    // The row is a plain div, so clicking it blurs the search input; the panel
    // stops the click from reaching the closing backdrop, so the palette stays.
    await empty.click();
    await expect(palette).toBeVisible();
    expect(await page.evaluate(() => document.activeElement?.tagName)).toBe('BODY');

    // The palette's own Escape binding lives on the input it just lost, so
    // this only closes because the global handler answers Escape too.
    await page.keyboard.press('Escape');
    await expect(palette).toHaveCount(0);
  });

});
