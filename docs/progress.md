# DeLore Project Progress

| Phase | Status | Notes |
|---|---|---|
| 1. Zero-percent slider regression | PASS | Real Tauri window/CDP and expanded ignored integration flow verified against owned coordinators; details below. |
| 2. Connection resilience and single instance | PASS | Reconnect backoff, in-flight result discard, explicit connection state, and focus/restore on second launch verified. |
| 3. First-run onboarding and data management | PASS | Adaptive startup readiness, first-run welcome, and local data-folder access verified. |
| 4. Coordinator hardening (SQLite) | PASS | Durable job lifecycle and restart recovery verified with a real coordinator restart. |
| 5. Coordinator URL and TLS | PASS | Supports secure remote WebSocket endpoints and loopback-only plaintext; remote certificate handshake not live-verified. |
| 6. Network-selected model | PASS | Coordinator `/fees` publishes the validated network model; providers prefer it over saved config, with backward-compatible fallback. |
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

Phase 4 evidence (all timestamps UTC):
- Added SQLite job persistence at `coordinator.sqlite3` by default, configurable with `GPUSWARM_DB_PATH`. Queued jobs are restored after restart; terminal results remain queryable; jobs that were running during restart are marked failed. SQLite uses WAL, `synchronous=FULL`, a busy timeout, schema versioning, and an integrity check.
- Full workspace tests: 92 passed, 0 failed, 4 ignored. Coordinator tests passed after the final storage change: 17 passed. Workspace clippy passed; the follow-up coordinator clippy check after the final storage change also passed, with only the existing `while_let_loop` warning in `coordinator/src/main.rs`.
- Explicitly ran all three ignored integration tests: `managed_provider_runs_a_job_and_stops`, `disconnect_during_job_discards_result_and_reconnects`, and `performance_slider_controls_registration_and_job_flow`; all passed. Their coordinator used a temporary SQLite database, and the provider config/log path was isolated with `GPUSWARM_CONFIG_DIR`.
- Real restart check: submitted job `phase4-restart-73b1132341c8456ca2d3e24fa7f57b57`; it was queued at `2026-11-22T23:40:30.831Z`. Stopped coordinator PID `14916` at `23:40:38.565Z`; restarted as PID `2844` at `23:40:47.843Z`. Startup reported `1 jobs loaded; 1 queued`, and `GET /jobs/{id}` returned `status=queued`. Both runs used a temporary database and the configured test fee address.
- Disk space before the first Phase 4 rebuild was C: 25.06 GB free and Q: 3.99 GB free. The first default-debug ignored-test build exhausted Q:. Only the dedicated build target `Q:\\gpu-swarm-phase4-88e391cb1c1c460d98e7` was removed; builds were then retried with reduced debug output and two jobs. No files under the repository other than project build output were deleted.

Phase 5 evidence (all timestamps UTC):
- Added strict coordinator URL validation: `/ws` only, no credentials/query/fragment, and `ws://` restricted to `localhost`, `127.0.0.1`, and `::1`. Network hosts require `wss://`. Invalid values are rejected before managed setup and are never saved over the existing config.
- Enabled native TLS for both provider WebSocket clients. TLS uses the operating system trust store; no certificate-validation bypass was added. The CLI fee endpoint maps `ws://.../ws` to `http://.../fees` and `wss://.../ws` to `https://.../fees`, with an explicit unit test.
- `COORDINATOR_WS` now overrides the saved endpoint for both desktop and CLI processes. README documents URL rules and the override.
- Full workspace tests: 95 passed, 0 failed, 4 ignored. Workspace clippy passed with the existing `while_let_loop` warning in `coordinator/src/main.rs`; `cargo fmt --all -- --check` passed. Explicitly ran all three ignored integration tests (`managed_provider_runs_a_job_and_stops`, `disconnect_during_job_discards_result_and_reconnects`, `performance_slider_controls_registration_and_job_flow`); all passed against a temporary coordinator and isolated config directory.
- Pre-build free space was C: 21.39 GB and Q: 4.26 GB. Cargo used isolated `Q:\\gpu-swarm-phase5-88e391cb1c1c460d98e7` with reduced debug output and two build jobs. No live remote TLS endpoint/certificate was available, so a trusted remote TLS handshake remains NOT VERIFIED; local URL policy, TLS-enabled compilation, and HTTP/HTTPS endpoint selection are verified.
- Final recheck after tightening raw URL parsing: `cargo test --workspace` again passed (95 passed, 0 failed, 4 ignored), formatting passed, and workspace clippy passed with only the pre-existing coordinator loop warning. Re-ran all three ignored integration tests explicitly against a fresh owned coordinator; all passed. Their log timestamps were `2026-11-23T00:15:08.621Z` (fee config), `00:15:16.412Z` (first completed job), `00:15:47.479Z` (provider listed paused at 0%), and `00:16:13.407Z` (100% job completed). Each job settlement showed payout 9000 and platform fee 1000. Temporary `GPUSWARM_CONFIG_DIR` and `GPUSWARM_DB_PATH` values were used; runtime/model cache remained in the existing LocalAppData install. Final pre-build free space: C: 21.35 GB, Q: 1.21 GB.

Phase 6 evidence (all timestamps UTC):
- Added coordinator `GPUSWARM_NETWORK_MODEL` (validated; default `llama3.2:3b`) to `GET /fees`. Providers fetch the network selection before setup, prefer it over the saved value, and retain `OLLAMA_MODEL` as an explicit developer override. Older coordinators without `network_model` fall back to saved config. HTTP discovery uses the default 8081→8080 port mapping, supports an optional `COORDINATOR_HTTP` origin, requires HTTPS for non-loopback HTTP endpoints, and has URL/compatibility tests.
- On the final integration run, the coordinator published `llama3.2:1b` while the policy test's saved config deliberately specified `llama3.2:3b`; the test passed after verifying registration used the network-selected value. A job then ran through the selected model. The same run verified pause/queue/resume behavior: paused at `2026-11-23T00:50:52.336Z`, resumed job completed at `00:50:58.105Z`, and the two 100% jobs completed by `00:51:17.067Z`.
- `cargo test --workspace`: 100 passed, 0 failed, 4 ignored. `cargo clippy --workspace --all-targets` passed with only the existing coordinator `while_let_loop` warning; `cargo fmt --all -- --check` passed. Explicitly ran all three ignored integration tests (`managed_provider_runs_a_job_and_stops`, `disconnect_during_job_discards_result_and_reconnects`, `performance_slider_controls_registration_and_job_flow`); all passed against an owned coordinator with isolated `GPUSWARM_CONFIG_DIR` and `GPUSWARM_DB_PATH`.
- Before the Phase 6 full build, free space was C: 21.34 GB and Q: 4.26 GB. Cargo used isolated `Q:\\gpu-swarm-phase6-88e391cb1c1c460d98e7`; no real `%APPDATA%\\GpuSwarm\\config.json` was accessed.

Next: Phase 7 — branding and installers.
