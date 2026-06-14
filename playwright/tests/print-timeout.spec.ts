import { test, expect } from '@playwright/test';
import { attachConsoleCollector, assertCleanConsole } from './helpers/console-check';

// Issue #53: the effective [jobs].print_timeout_secs must be surfaced on the
// dashboard UI so non-engineer operators can verify the running value without
// grepping config.toml. The Playwright harness runs a server-mode dashboard
// (playwright/test-config.toml sets print_timeout_secs = 1800), which renders
// the value in the stats bar; the client dashboard renders the same value
// (verified end-to-end against real hardware by devbridge-e2e test 29).
test.describe('Print timeout tuning surface', () => {
  test('dashboard renders the effective print_timeout_secs value', async ({ page }) => {
    const cons = attachConsoleCollector(page);
    await page.goto('/');

    // Wait for the stats bar to hydrate from /api/status.
    const statsBar = page.locator('.main-content .card').first();
    await expect(statsBar).toBeVisible();

    const timeout = page.locator('[data-testid="print-timeout"]');
    await expect(timeout).toBeVisible();
    // test-config.toml configures print_timeout_secs = 1800.
    await expect(timeout).toContainText('1800s');

    assertCleanConsole(cons);
  });

  test('/api/status JSON exposes the configured print_timeout_secs', async ({ request }) => {
    const resp = await request.get('/api/status');
    expect(resp.ok()).toBeTruthy();
    const json = await resp.json();
    // Must be present and match the value the running service loaded.
    expect(typeof json.print_timeout_secs).toBe('number');
    expect(json.print_timeout_secs).toBe(1800);
  });
});
