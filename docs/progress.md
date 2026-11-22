# DeLore Project Progress

| Phase | Status | Notes |
|---|---|---|
| 1. Zero-percent slider regression | PASS | Real Tauri window/CDP and expanded ignored integration flow verified against owned coordinators; details below. |
| 2. Connection resilience and single instance | PASS | Reconnect backoff, in-flight result discard, explicit connection state, and focus/restore on second launch verified. |
| 3. First-run onboarding and data management | TODO | |
| 4. Coordinator hardening (SQLite) | TODO | |
| 5. Coordinator URL and TLS | TODO | |
| 6. Network-selected model | TODO | |
| 7. Branding and installers | TODO | |
| 8. Installed-app behavior (Windows) | TODO | |
| 9. Whole-app end-to-end test | TODO | |

Implementation commits are recorded in git history.

Phase 1 evidence (all timestamps UTC):
- Real window with isolated config: provider registered at 0% as paused/`accepting_jobs=false` at `2026-11-22T20:03:11.629Z`.
- First job stayed queued from `20:03:28.220Z` through `20:03:58.325Z`; slider changed through the UI at `20:04:10.421Z`; job completed `20:04:19.951Z` with payout `9000`, fee `1000`, and `pause_ms=9855`.
- At 0% again, the second job stayed queued from `20:04:31.451Z` through `20:05:01.534Z`.
- At 100%, two back-to-back jobs completed `20:05:18.377Z` and `20:05:19.447Z`; neither had a cooldown log.
- Confirmation-needed state and dialog were observed in the real UI. The UI test used `GPUSWARM_CONFIG_DIR`; the real config was not read or changed.
- `cargo test --workspace`: 87 passed, 0 failed, 3 ignored. `cargo test --workspace -- --ignored --nocapture`: all 3 ignored tests passed against an owned coordinator. `cargo clippy --workspace --all-targets`: passed with the existing coordinator `while_let_loop` warning. Frontend tests: 2 passed; build passed.

Phase 2 evidence (all timestamps UTC):
- With a running desktop provider, job `phase2-disconnect-c77a6fc803c94c909c7aef9892d30bfb` entered `running` at `21:19:58.050Z`. Dropping its WebSocket at `21:19:58.132Z` made the coordinator report `failed` at `21:19:58.455Z` with `Provider disconnected while job was running`; the provider log at `21:19:58.120Z` says it discarded the result.
- Stopping the coordinator at `21:20:33.565Z` made the real UI show `Coordinator unreachable` at `21:20:37.151Z`. After restart, the provider reconnected at `21:21:43.891Z`; `GET /providers` returned the provider once, idle and accepting jobs, at `21:22:28.889Z`.
- With the rebuilt desktop app, a second launch while the existing window was minimized exited as a separate process and restored/focused the original window at `21:34:47.023Z`. The app and managed engine each remained a single process, and the coordinator still listed one provider.
- `cargo test --workspace`: 88 passed, 0 failed, 4 ignored. `cargo clippy --workspace --all-targets`: passed with the existing coordinator `while_let_loop` warning. `cargo fmt --all -- --check` and `npm --prefix desktop run build` passed. The focused provider disconnect integration test passed earlier against the cached local runtime/model.

Next: Phase 3 — first-run onboarding and data management.
