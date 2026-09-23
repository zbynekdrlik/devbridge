---
paths:
  - ".github/workflows/**"
  - "crates/devbridge-ui-util/**"
---

# CI pipeline — gotchas (auto-loads on workflows and devbridge-ui-util)

- **CI runs on `push` to dev/main only** — a PR has no own run; its checks are the dev push run of the same head SHA. Merging to main re-runs the whole pipeline (incl. E2E on pz-server/pz-snv and Stable Release, which publishes `v<version>` that the fleet auto-update installs).
- **The Mutation Testing job can pass without testing anything.** It runs `cargo mutants --in-diff` under the job-wide `RUSTFLAGS=-D warnings`; a whole-function replacement mutant (`replace f -> Option<String> with None`) leaves the parameter unused, the `unused variable` warning becomes an error, and cargo-mutants counts the mutant as **unviable** (not caught, not missed) → exit 0. Seen on 0.8.38: `3 mutants tested in 18s: 3 unviable` for `devbridge_ui_util::version_label`. Always read the job's `N mutants tested: … unviable` line (or `mutants-out/unviable.txt` in the `mutation-results` artifact) — "passed" with only unviable mutants proves nothing about your tests.
- **Pull a single job's log** with `gh api repos/zbynekdrlik/devbridge/actions/jobs/<job-id>/logs` (job ids from `gh run view <run> --json jobs`); `gh run view --log` mislabels job names.
