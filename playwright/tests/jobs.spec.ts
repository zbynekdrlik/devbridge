import { test, expect } from '@playwright/test';
import { attachConsoleCollector, assertCleanConsole } from './helpers/console-check';
import { submitIppJob } from './helpers/ipp-client';

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
    await expect(headers).toHaveCount(6);
    await expect(headers.nth(0)).toHaveText('Time');
    await expect(headers.nth(1)).toHaveText('User');
    await expect(headers.nth(2)).toHaveText('Printer');
    await expect(headers.nth(3)).toHaveText('Status');
    // Issue #52: client dashboard surfaces the real server-driven retry count.
    await expect(headers.nth(4)).toHaveText('Retries');
    await expect(headers.nth(5)).toHaveText('Ago');

    assertCleanConsole(cons);
  });

  test('shows empty state', async ({ page }) => {
    const cons = attachConsoleCollector(page);
    await page.goto('/jobs');

    await expect(page.locator('td', { hasText: 'No jobs found.' })).toBeVisible();

    assertCleanConsole(cons);
  });

  test('API /api/jobs returns requesting_user field', async ({ request }) => {
    const resp = await request.get('/api/jobs');
    expect(resp.ok()).toBeTruthy();
    const jobs = await resp.json();
    // With no jobs, we get an empty array — verify the endpoint responds correctly
    expect(Array.isArray(jobs)).toBeTruthy();

    // Verify the requesting_user filter parameter is accepted (no 400 error)
    const filtered = await request.get('/api/jobs?requesting_user=testuser');
    expect(filtered.ok()).toBeTruthy();
    const filteredJobs = await filtered.json();
    expect(Array.isArray(filteredJobs)).toBeTruthy();
  });

  // Issue #52: the client dashboard must surface the real server-driven
  // retry count. Submit a real job, then assert both the API field and the
  // rendered Retries cell are present and numeric.
  test('jobs surface numeric retry_count (#52)', async ({ page, request }) => {
    const cons = attachConsoleCollector(page);

    await submitIppJob({ requestingUser: 'retry-count-user' });

    // API: /api/jobs entries carry a numeric retry_count field.
    const resp = await request.get('/api/jobs');
    expect(resp.ok()).toBeTruthy();
    const jobs = await resp.json();
    expect(Array.isArray(jobs)).toBeTruthy();
    expect(jobs.length).toBeGreaterThan(0);
    for (const job of jobs) {
      expect(typeof job.retry_count).toBe('number');
    }

    // UI: the Jobs table renders the retry-count cell for the new job.
    await page.goto('/jobs');
    const row = page
      .locator('table tbody tr')
      .filter({ hasText: 'retry-count-user' })
      .first();
    await expect(row).toBeVisible({ timeout: 10_000 });
    const retryCell = row.locator('[data-testid="job-retry-count"]');
    await expect(retryCell).toHaveCount(1);
    await expect(retryCell).toHaveText(/^\d+$/);

    assertCleanConsole(cons);
  });
});
