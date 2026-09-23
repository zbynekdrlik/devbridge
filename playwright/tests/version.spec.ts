import { test, expect } from '@playwright/test';
import { attachConsoleCollector, assertCleanConsole } from './helpers/console-check';

// Issue #82: every dashboard (server + client) must visibly show the deployed
// version, so a post-deploy check can read it from the DOM instead of only
// from the /api/status JSON.
test.describe('Version label', () => {
  test('sidebar shows v<semver> equal to /api/status version', async ({ page, request }) => {
    const cons = attachConsoleCollector(page);

    const statusResp = await request.get('/api/status');
    expect(statusResp.ok()).toBeTruthy();
    const status = await statusResp.json();
    expect(status.version).toMatch(/^\d+\.\d+\.\d+/);

    await page.goto('/');

    const label = page.locator('nav.sidebar [data-testid=app-version]');
    await expect(label).toBeVisible();
    await expect(label).toHaveText(/^v\d+\.\d+\.\d+/);
    await expect(label).toHaveText(`v${status.version}`);

    // The brand heading stays exactly "DevBridge" (the version is its own element).
    await expect(page.locator('nav.sidebar h1')).toHaveText('DevBridge');

    assertCleanConsole(cons);
  });

  test('version label is shown on every page, not only the dashboard', async ({ page, request }) => {
    const cons = attachConsoleCollector(page);
    const status = await (await request.get('/api/status')).json();

    for (const path of ['/jobs', '/printers', '/config', '/logs']) {
      await page.goto(path);
      await expect(page.locator('nav.sidebar [data-testid=app-version]')).toHaveText(
        `v${status.version}`
      );
    }

    assertCleanConsole(cons);
  });
});
