import {expect, type Page} from '@playwright/test';

/**
 * The sidebar libraries, as labelled on the strip beneath the header.
 *
 * The sidebar shows one library at a time. Routes is the default, so a spec
 * that wants a supernode, plugin config or store has to open its library
 * first -- the lists are not merely hidden, they are unmounted, so a row
 * locator finds nothing until the strip button is clicked.
 */
export type Library = 'Supernodes' | 'Plugin configs' | 'Stores';

/**
 * Opens a sidebar library and waits for its list to be on screen.
 *
 * Idempotent in intent but not in effect: the strip buttons toggle, so
 * calling this twice for the same library would close it again. Call it once,
 * before the first interaction with that library's rows.
 */
export async function openLibrary(page: Page, library: Library): Promise<void> {
  const button = page.getByRole('button', {name: library, exact: true});
  await expect(button).toBeVisible();
  await button.click();
  await expect(button).toHaveAttribute('aria-pressed', 'true');
}
