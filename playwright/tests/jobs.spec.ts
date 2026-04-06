import { test, expect } from '@playwright/test';
import { attachConsoleCollector, assertCleanConsole } from './helpers/console-check';

test.describe('Jobs Page', () => {
  test('navigates via sidebar', async ({ page }) => {
    const cons = attachConsoleCollector(page);
    await page.goto('/');

    await page.locator('nav.sidebar a', { hasText: 'Jobs' }).click();
    await expect(page).toHaveURL(/\/jobs/);
    await expect(page.locator('.header h2')).toHaveText('Jobs');

    assertCleanConsole(cons);
  });

  test('shows table with correct headers', async ({ page }) => {
    const cons = attachConsoleCollector(page);
    await page.goto('/jobs');

    const headers = page.locator('table thead th');
    await expect(headers).toHaveCount(5);
    await expect(headers.nth(0)).toHaveText('ID');
    await expect(headers.nth(1)).toHaveText('Name');
    await expect(headers.nth(2)).toHaveText('Printer');
    await expect(headers.nth(3)).toHaveText('Status');
    await expect(headers.nth(4)).toHaveText('Created');

    assertCleanConsole(cons);
  });

  test('shows empty state', async ({ page }) => {
    const cons = attachConsoleCollector(page);
    await page.goto('/jobs');

    await expect(page.locator('td', { hasText: 'No jobs found.' })).toBeVisible();

    assertCleanConsole(cons);
  });
});
