# DeLore Project Progress

| Phase | Status | Notes |
|---|---|---|
| 1. Zero-percent slider regression | PASS | Real Tauri window/CDP and expanded ignored integration flow verified against owned coordinators; details below. |
| 2. Connection resilience and single instance | TODO | |
| 3. First-run onboarding and data management | TODO | |
| 4. Coordinator hardening (SQLite) | TODO | |
| 5. Coordinator URL and TLS | TODO | |
| 6. Network-selected model | TODO | |
| 7. Branding and installers | TODO | |
| 8. Installed-app behavior (Windows) | TODO | |
| 9. Whole-app end-to-end test | TODO | |

Last implementation commit: `42a22d9`

Phase 1 evidence (all timestamps UTC):
- Real window with isolated config: provider registered at 0% as paused/`accepting_jobs=false` at `2026-11-22T20:03:11.629Z`.
- First job stayed queued from `20:03:28.220Z` through `20:03:58.325Z`; slider changed through the UI at `20:04:10.421Z`; job completed `20:04:19.951Z` with payout `9000`, fee `1000`, and `pause_ms=9855`.
- At 0% again, the second job stayed queued from `20:04:31.451Z` through `20:05:01.534Z`.
- At 100%, two back-to-back jobs completed `20:05:18.377Z` and `20:05:19.447Z`; neither had a cooldown log.
- Confirmation-needed state and dialog were observed in the real UI. The UI test used `GPUSWARM_CONFIG_DIR`; the real config was not read or changed.
- `cargo test --workspace`: 87 passed, 0 failed, 3 ignored. `cargo test --workspace -- --ignored --nocapture`: all 3 ignored tests passed against an owned coordinator. `cargo clippy --workspace --all-targets`: passed with the existing coordinator `while_let_loop` warning. Frontend tests: 2 passed; build passed.

Next: Phase 2 — implement reconnect backoff/status and single-instance focus behavior with deterministic tests.
