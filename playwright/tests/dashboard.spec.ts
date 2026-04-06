import { test, expect } from '@playwright/test';
import { attachConsoleCollector, assertCleanConsole } from './helpers/console-check';

test.describe('Dashboard Page', () => {
  test('loads with sidebar and main content', async ({ page }) => {
    const cons = attachConsoleCollector(page);
    await page.goto('/');

    await expect(page.locator('nav.sidebar')).toBeVisible();
    await expect(page.locator('nav.sidebar h1')).toHaveText('DevBridge');
    await expect(page.locator('main.main-content')).toBeVisible();

    assertCleanConsole(cons);
  });

  test('sidebar shows all server-mode navigation links', async ({ page }) => {
    const cons = attachConsoleCollector(page);
    await page.goto('/');

    const links = page.locator('nav.sidebar a');
    await expect(links).toHaveCount(5);
    await expect(links.nth(0)).toHaveText('Dashboard');
    await expect(links.nth(1)).toHaveText('Jobs');
    await expect(links.nth(2)).toHaveText('Printers');
    await expect(links.nth(3)).toHaveText('Config');
    await expect(links.nth(4)).toHaveText('Logs');

    assertCleanConsole(cons);
  });

  test('shows page header and stats bar', async ({ page }) => {
    const cons = attachConsoleCollector(page);
    await page.goto('/');

    await expect(page.locator('.header h2')).toHaveText('Dashboard');

    // Stats bar is the first .card — contains mode and status badge
    const statsBar = page.locator('.main-content .card').first();
    await expect(statsBar).toBeVisible();
    await expect(statsBar).toContainText('server');
    await expect(statsBar.locator('.badge-success')).toContainText('online');

    assertCleanConsole(cons);
  });

  test('shows empty state when no jobs exist', async ({ page }) => {
    const cons = attachConsoleCollector(page);
    await page.goto('/');

    await expect(page.locator('h3')).toContainText('Recent Jobs');
    await expect(page.locator('text=No jobs yet')).toBeVisible();

    assertCleanConsole(cons);
  });
});
