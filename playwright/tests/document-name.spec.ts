import { test, expect } from '@playwright/test';
import { attachConsoleCollector, assertCleanConsole } from './helpers/console-check';
import { submitIppJob } from './helpers/ipp-client';

test.describe('Document name capture (#30)', () => {
  test('Dashboard JobCard shows real name when document-name is sent', async ({ page }) => {
    const console_ = attachConsoleCollector(page);

    const docName = 'Playwright-Invoice-2026.pdf';
    await submitIppJob({
      documentName: docName,
      jobName: docName,
      requestingUser: 'playwright-e2e',
    });

    await page.goto('/');

    // Find the JobCard that contains our user name, then confirm the muted
    // name line under it shows the real document name.
    const jobCard = page
      .locator('.main-content .card')
      .filter({ hasText: 'playwright-e2e' })
      .first();
    await expect(jobCard).toBeVisible({ timeout: 10_000 });
    await expect(jobCard).toContainText(docName);

    assertCleanConsole(console_);
  });

  test('Dashboard JobCard hides name line when neither document-name nor job-name is sent', async ({
    page,
  }) => {
    const console_ = attachConsoleCollector(page);

    // Submit without any name attributes — backend should store empty string
    // and UI helper should hide the line.
    await submitIppJob({
      requestingUser: 'playwright-anon',
    });

    await page.goto('/');
    const jobCard = page
      .locator('.main-content .card')
      .filter({ hasText: 'playwright-anon' })
      .first();
    await expect(jobCard).toBeVisible({ timeout: 10_000 });

    // The card must NOT contain the muted monospace line. Concretely: no
    // element inside this card matches the name-line style signature.
    const mutedNameLine = jobCard.locator(
      'div[style*="font-family: monospace"][style*="color: var(--text-muted)"]'
    );
    await expect(mutedNameLine).toHaveCount(0);

    assertCleanConsole(console_);
  });

  test('Jobs page table shows name under user cell when present', async ({ page }) => {
    const console_ = attachConsoleCollector(page);

    const docName = 'Playwright-Jobs-Page-Test.pdf';
    await submitIppJob({
      documentName: docName,
      jobName: docName,
      requestingUser: 'jobs-page-user',
    });

    await page.goto('/jobs');
    const row = page.locator('table tbody tr').filter({ hasText: 'jobs-page-user' }).first();
    await expect(row).toBeVisible({ timeout: 10_000 });
    await expect(row).toContainText(docName);

    assertCleanConsole(console_);
  });

  test('Jobs page table hides name line when absent', async ({ page }) => {
    const console_ = attachConsoleCollector(page);

    await submitIppJob({
      requestingUser: 'jobs-anon-user',
    });

    await page.goto('/jobs');
    const row = page.locator('table tbody tr').filter({ hasText: 'jobs-anon-user' }).first();
    await expect(row).toBeVisible({ timeout: 10_000 });

    // User is the 2nd column (after Time). The muted name div must not exist.
    const userCell = row.locator('td').nth(1);
    const mutedNameLine = userCell.locator(
      'div[style*="font-family: monospace"][style*="color: var(--text-muted)"]'
    );
    await expect(mutedNameLine).toHaveCount(0);

    assertCleanConsole(console_);
  });
});
