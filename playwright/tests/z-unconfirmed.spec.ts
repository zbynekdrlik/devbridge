import { test, expect, type APIRequestContext } from '@playwright/test';
import { attachConsoleCollector, assertCleanConsole } from './helpers/console-check';
import { submitIppJob } from './helpers/ipp-client';
import { completeJob } from './helpers/grpc-client';

// #114 (owner ruling 20.10.2026: neposielať znova). A direct_ipp printer
// ACCEPTED the job (it has a job-id) but its end was never confirmed: the
// client reports `unconfirmed`, the server never resends it, and the
// dashboards show it as „možno vytlačené" — a warning, not a failure.

const DETAIL =
  'printer accepted job-id=45, completion not confirmed ' +
  '(no final job-state within 60s, last state=6) — not resent';
const WARNING_ICON = '⚠️';

async function jobsOf(request: APIRequestContext, user: string) {
  const resp = await request.get(`/api/jobs?requesting_user=${user}`);
  expect(resp.ok()).toBeTruthy();
  return resp.json();
}

test.describe('Unconfirmed print (#114)', () => {
  test('a job the printer accepted but never confirmed reads „možno vytlačené" and is never resent', async ({
    page,
    request,
  }) => {
    const cons = attachConsoleCollector(page);
    const user = `unconfirmed-114-${Date.now()}`;

    await submitIppJob({
      requestingUser: user,
      documentName: 'Prevodka-114.pdf',
      jobName: 'Prevodka-114.pdf',
    });
    const listed = await jobsOf(request, user);
    expect(listed).toHaveLength(1);
    const id: string = listed[0].id;

    // What a #114 client sends when the printer accepted the job and the
    // confirmation never came.
    await completeJob({
      job_id: id,
      success: false,
      unconfirmed: true,
      error_detail: DETAIL,
      verification_method: 'ipp_job_state',
      verification_evidence: DETAIL,
      client_id: 'playwright-client',
    });

    // Backend: terminal, never retried.
    const after = await jobsOf(request, user);
    expect(after).toHaveLength(1);
    expect(after[0].status).toBe('unconfirmed');
    expect(after[0].retry_count).toBe(0);

    // Jobs page, reached like a user: the warning badge, and the job's
    // timeline (click the row) says why.
    await page.goto('/');
    await page.locator('nav.sidebar a', { hasText: 'Jobs' }).click();
    await expect(page).toHaveURL(/\/jobs/);
    const row = page.locator('table tbody tr').filter({ hasText: user }).first();
    await expect(row).toBeVisible({ timeout: 10_000 });
    const badge = row.locator('.badge');
    await expect(badge).toHaveText('možno vytlačené');
    await expect(badge).toHaveClass(/badge-warning/);
    await row.click();
    const item = page.locator('.timeline-item').filter({ hasText: 'unconfirmed' });
    await expect(item).toHaveCount(1);
    await expect(item.locator('.timeline-detail')).toHaveText(DETAIL);
    await expect(item.locator('.timeline-icon')).toHaveText(WARNING_ICON);

    // Dashboard job card: the same warning badge.
    await page.locator('nav.sidebar a', { hasText: 'Dashboard' }).click();
    const card = page.locator('.main-content .card').filter({ hasText: user }).first();
    await expect(card).toBeVisible({ timeout: 10_000 });
    await expect(card.locator('.badge')).toHaveText('možno vytlačené');
    await expect(card.locator('.badge')).toHaveClass(/badge-warning/);

    // Still the one job, still unconfirmed: nothing was resent meanwhile.
    const later = await jobsOf(request, user);
    expect(later).toHaveLength(1);
    expect(later[0].status).toBe('unconfirmed');
    expect(later[0].retry_count).toBe(0);

    assertCleanConsole(cons);
  });
});
