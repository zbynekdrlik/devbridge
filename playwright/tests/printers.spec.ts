import { test, expect } from '@playwright/test';
import { attachConsoleCollector, assertCleanConsole } from './helpers/console-check';

test.describe('Printers Page', () => {
  test('navigates via sidebar', async ({ page }) => {
    const cons = attachConsoleCollector(page);
    await page.goto('/');

    await page.locator('nav.sidebar a', { hasText: 'Printers' }).click();
    await expect(page).toHaveURL(/\/printers/);
    await expect(page.locator('.header h2')).toHaveText('Printers');

    assertCleanConsole(cons);
  });

  test('shows Virtual Printers section with table headers', async ({ page }) => {
    const cons = attachConsoleCollector(page);
    await page.goto('/printers');

    await expect(page.locator('h3', { hasText: 'Virtual Printers' })).toBeVisible();
    const headers = page.locator('.card').first().locator('table thead th');
    await expect(headers).toHaveCount(3);
    await expect(headers.nth(0)).toHaveText('Name');
    await expect(headers.nth(1)).toHaveText('Paired Client');
    await expect(headers.nth(2)).toHaveText('Actions');

    assertCleanConsole(cons);
  });

  test('shows empty states for printers and clients', async ({ page }) => {
    const cons = attachConsoleCollector(page);
    await page.goto('/printers');

    // Wait for async resources to load — virtual printers list is a LocalResource
    await expect(page.locator('text=No virtual printers configured.')).toBeVisible({ timeout: 15000 });
    await expect(page.locator('h3', { hasText: 'Registered Clients' })).toBeVisible();
    await expect(page.locator('text=No clients registered yet')).toBeVisible({ timeout: 15000 });

    assertCleanConsole(cons);
  });

  test('has Add Printer button', async ({ page }) => {
    const cons = attachConsoleCollector(page);
    await page.goto('/printers');

    await expect(page.locator('button', { hasText: 'Add Printer' })).toBeVisible();

    assertCleanConsole(cons);
  });
});
