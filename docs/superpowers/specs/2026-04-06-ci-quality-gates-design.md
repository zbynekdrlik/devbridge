# CI Quality Gates: Mutation Testing + Playwright Browser E2E

**Date:** 2026-04-06
**Status:** Draft
**Approach:** Sequential — mutation testing first, then Playwright

## Problem

The CI pipeline enforces format, lint, test, build, audit, and API-based E2E tests. Two quality gates are missing:

1. **Mutation testing** — proves tests verify behavior, not just execute code. A test suite can have 100% line coverage but catch almost no real bugs if assertions are weak.
2. **Playwright browser E2E** — the dashboard (Leptos WASM, 5 pages) has zero browser-based tests. The existing E2E binary tests only API endpoints via HTTP, never opens a browser.

## Scope

This design covers adding two new CI jobs to Tier 1. No changes to the existing E2E binary, self-hosted runners, or deployment pipeline.

---

## Part 1: Mutation Testing (`cargo-mutants`)

### CI Job

New job `mutation` in Tier 1, parallel with `test` and `build`, after `lint`:

```yaml
mutation:
  name: Mutation Testing
  runs-on: ubuntu-latest
  needs: lint
  timeout-minutes: 45
  steps:
    - uses: actions/checkout@v4
    - uses: dtolnay/rust-toolchain@stable
    - uses: Swatinem/rust-cache@v2
      with:
        shared-key: mutation
    - name: Install protoc
      uses: arduino/setup-protoc@v3
      with:
        version: "28.3"
        repo-token: ${{ secrets.GITHUB_TOKEN }}
    - name: Install cargo-mutants
      run: |
        curl -L --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | bash
        cargo binstall cargo-mutants --no-confirm
    - name: Run mutation testing
      run: cargo mutants --workspace --timeout 120
```

### Policy

- **Full workspace** — all 6 workspace crates are mutated
- **Zero tolerance** — any surviving mutant fails CI
- **Per-mutant timeout:** 120 seconds (kills slow/hanging mutants)
- **Job timeout:** 45 minutes (generous for full workspace)
- Added to `tier1-pass` gate so it blocks the entire pipeline

### Expected Impact

Running `cargo mutants --workspace` will likely find surviving mutants in existing code where tests execute lines but don't assert on behavior. These must be killed with stronger assertions before CI goes green.

### Gate Integration

Add `mutation` to the `tier1-pass` job's `needs` array and verify its result.

---

## Part 2: Playwright Browser E2E Tests

### Directory Structure

```
playwright/
  package.json
  playwright.config.ts
  test-config.toml          # DevBridge server config for test mode
  tests/
    dashboard.spec.ts       # Dashboard page tests
    jobs.spec.ts            # Jobs page tests
    printers.spec.ts        # Printers page tests
    config.spec.ts          # Config page tests
    logs.spec.ts            # Logs page tests
    helpers/
      console-check.ts      # Shared console.error/warn collector
```

### Test Server

Playwright tests start a real `devbridge-service` binary on ubuntu-latest:

- **Mode:** server (no client connection needed for UI tests)
- **Config:** `playwright/test-config.toml` with ephemeral settings (random port, temp spool dir, no TLS)
- **Data:** Empty database — tests verify empty states and UI structure
- Binary comes from the `build` job via artifact upload/download

### Test Coverage

| Page | Tests |
|------|-------|
| Dashboard | Page loads, mode indicator renders, status section shows, WebSocket connects (no errors) |
| Jobs | Job list shows empty state, timeline toggle works, filter controls render |
| Printers | Printer list renders, virtual printer creation form works, delete confirmation |
| Config | Config form renders with values, fields are editable |
| Logs | Log viewer renders, auto-scroll behavior |

### Console Enforcement

Every test file includes console error/warning collection per airuleset `browser-console-zero-errors`:

```typescript
test.beforeEach(async ({ page }) => {
  const messages: string[] = [];
  page.on('console', (msg) => {
    if (msg.type() === 'error' || msg.type() === 'warning') {
      messages.push(`[${msg.type()}] ${msg.text()}`);
    }
  });
  // Store for assertion in afterEach
});
```

Final assertion in every test: `expect(consoleMessages).toEqual([])`.

### CI Job

```yaml
playwright:
  name: Playwright E2E
  runs-on: ubuntu-latest
  needs: build
  timeout-minutes: 15
  steps:
    - uses: actions/checkout@v4

    - uses: actions/download-artifact@v4
      with:
        name: devbridge-linux-binary
        path: artifacts/

    - uses: actions/setup-node@v4
      with:
        node-version: 22

    - name: Install Playwright
      working-directory: playwright
      run: npm ci && npx playwright install chromium

    - name: Start test dashboard
      run: |
        chmod +x artifacts/devbridge-service
        artifacts/devbridge-service --config playwright/test-config.toml &
        timeout 30 bash -c 'until curl -s http://localhost:9120 > /dev/null; do sleep 1; done'

    - name: Run Playwright tests
      working-directory: playwright
      run: npx playwright test

    - uses: actions/upload-artifact@v4
      if: failure()
      with:
        name: playwright-report
        path: playwright/test-results/
        retention-days: 3
```

### Build Job Changes

The existing `build` job (ubuntu-latest) needs to upload the compiled binary:

```yaml
# Add to end of existing build job
- name: Upload Linux binary for Playwright
  uses: actions/upload-artifact@v4
  with:
    name: devbridge-linux-binary
    path: target/release/devbridge-service
    retention-days: 1
```

### Gate Integration

Add `playwright` to the `tier1-pass` job's `needs` array and verify its result.

---

## Implementation Order

1. Add `cargo-mutants` CI job
2. Run it, identify all surviving mutants
3. Write test assertions to kill every surviving mutant
4. Verify mutation CI job passes
5. Set up `playwright/` directory with Node.js project
6. Modify `build` job to upload Linux binary artifact
7. Write Playwright tests for all 5 dashboard pages
8. Add `playwright` CI job
9. Add both jobs to `tier1-pass` gate
10. Verify full pipeline is green

## Non-Goals

- No changes to existing Rust E2E binary
- No Playwright on self-hosted Windows runners
- No code coverage threshold enforcement (separate concern)
- No changes to deployment pipeline
