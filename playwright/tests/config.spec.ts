import { test, expect } from '@playwright/test';
import { attachConsoleCollector, assertCleanConsole } from './helpers/console-check';

test.describe('Config Page', () => {
  test('navigates via sidebar', async ({ page }) => {
    const cons = attachConsoleCollector(page);
    await page.goto('/');

    await page.locator('nav.sidebar a', { hasText: 'Config' }).click();
    await expect(page).toHaveURL(/\/config/);
    await expect(page.locator('.header h2')).toHaveText('Configuration');

    assertCleanConsole(cons);
  });

  test('displays config as formatted JSON', async ({ page }) => {
    const cons = attachConsoleCollector(page);
    await page.goto('/config');

    const pre = page.locator('.card pre');
    await expect(pre).toBeVisible();
    const text = await pre.textContent();
    expect(text).toContain('"mode"');
    expect(text).toContain('"server"');

    assertCleanConsole(cons);
  });
});
