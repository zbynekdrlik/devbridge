import { test, expect } from '@playwright/test';
import { attachConsoleCollector, assertCleanConsole } from './helpers/console-check';

test.describe('Logs Page', () => {
  test('navigates via sidebar', async ({ page }) => {
    const cons = attachConsoleCollector(page);
    await page.goto('/');

    await page.locator('nav.sidebar a', { hasText: 'Logs' }).click();
    await expect(page).toHaveURL(/\/logs/);
    await expect(page.locator('.header h2')).toHaveText('Logs');

    assertCleanConsole(cons);
  });

  test('shows log viewer with placeholder content', async ({ page }) => {
    const cons = attachConsoleCollector(page);
    await page.goto('/logs');

    await expect(page.locator('.log-viewer')).toBeVisible();
    await expect(page.locator('text=Live log viewer (placeholder)')).toBeVisible();

    assertCleanConsole(cons);
  });

  test('displays hardcoded log lines', async ({ page }) => {
    const cons = attachConsoleCollector(page);
    await page.goto('/logs');

    const logLines = page.locator('.log-line');
    await expect(logLines).toHaveCount(5);
    await expect(logLines.first()).toContainText('[INFO] DevBridge started');
    await expect(page.locator('.log-info').first()).toBeVisible();
    await expect(page.locator('.log-warn')).toBeVisible();

    assertCleanConsole(cons);
  });

  test('has Connect WebSocket button', async ({ page }) => {
    const cons = attachConsoleCollector(page);
    await page.goto('/logs');

    await expect(page.locator('button.btn-primary', { hasText: 'Connect WebSocket' })).toBeVisible();

    assertCleanConsole(cons);
  });
});
