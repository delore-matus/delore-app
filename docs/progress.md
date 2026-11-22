# DeLore Project Progress

| Phase | Status | Notes |
|---|---|---|
| 1. Zero-percent slider regression | PASS | Real Tauri window/CDP and expanded ignored integration flow verified against owned coordinators; details below. |
| 2. Connection resilience and single instance | PASS | Reconnect backoff, in-flight result discard, explicit connection state, and focus/restore on second launch verified. |
| 3. First-run onboarding and data management | PASS | Adaptive startup readiness, first-run welcome, and local data-folder access verified. |
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

Phase 3 evidence (all timestamps UTC):
- Engine readiness now retries adaptively (200 ms up to 2 s) for a maximum of 180 seconds. A fake local engine that became ready after 31 seconds passed the regression test (`waits_for_a_slow_starting_fake_engine_past_the_previous_timeout`; 31.54 s), proving startup no longer fails at the old 30-second limit.
- Recovered Phase 2 measurement: Start clicked at `2026-11-22T21:31:37.540Z`; engine ready at `21:31:49.350Z` (11.810 s); provider online at `21:31:50.636Z` (13.096 s).
- In a real rebuilt Windows desktop window with an isolated `%TEMP%` config, the welcome dialog opened and Continue persisted `onboarding_complete=true` only in that isolated config. Settings → Open data folder returned without a UI error.
- Real Start click at `2026-11-22T22:50:13.422Z` showed `Preparing` by `22:50:14.996Z`, with accessible text “Starting DeLore. This can take up to 3 minutes.” At `22:50:18.044Z`, the UI showed Online and Coordinator connected (4.622 s from click). The machine requests reduced motion, so the indeterminate indicator used a static 32% bar rather than falsely showing 100%; the Preparing label remained explicit. The provider was stopped through the UI at `22:50:32.257Z`; no managed `ollama.exe` remained.
- Pre-build disk space: C: 26.69 GB free; Q: 4.27 GB free. Cargo used isolated `Q:\gpu-swarm-phase3-88e391cb1c1c460d98e7`; application configs used isolated `%TEMP%` folders. The real `%APPDATA%\GpuSwarm\config.json` was not accessed.
- `cargo test --workspace`: 90 passed, 0 failed, 4 ignored. `cargo clippy --workspace --all-targets`: passed with the existing coordinator `while_let_loop` warning. `cargo fmt --all` and frontend tests (5 passed) and build passed.
- All four ignored tests were explicitly run and passed: `close_shutdown_helper_stops_the_managed_runtime`, `managed_provider_runs_a_job_and_stops`, `disconnect_during_job_discards_result_and_reconnects`, and `performance_slider_controls_registration_and_job_flow`. A stale assertion in the provider-core flow test expected the CLI path to write desktop history; that test-only mismatch was corrected in separate commit `2dd15c1`.

Next: Phase 4 — coordinator hardening (SQLite).
