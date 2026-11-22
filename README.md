# GPU Swarm MVP

This Rust workspace coordinates test providers and a managed local inference
process. It is for local/testnet use only: it does not use real money or private
keys.

## Desktop app

The Tauri v2 app lives in `desktop/`. It provides Status, Earnings, and Settings
screens, a system tray, optional start-on-login, and a local TESTNET earnings
history.

Install Node.js and the Tauri prerequisites for the target OS, then run:

```powershell
cd desktop
npm install
cargo tauri dev
```

The desktop UI does not shell out to `gpuswarm`. Both the app and CLI use
`provider-core` for provider control, configuration, the managed process, and
job execution.

### Performance slider

The Performance slider now ranges from 0% to 100%, with a default of 50%.
At 0%, the provider finishes any current job, then stops accepting work. From
1% to 100%, the slider is an approximate workload limit, not a hard GPU cap:
after each job it pauses for `job_duration × (100 / percentage - 1)`, capped at
30 minutes. At 100% there is no duty-cycle pause. The Pause button always takes
precedence over the slider; increasing the slider does not override a manual
pause.

## Configuration

| Variable | Default | Used by |
|---|---|---|
| `COORDINATOR_WS` | `ws://127.0.0.1:8081/ws` | Provider WebSocket URL |
| `PROVIDER_ID` | `provider-node-01` | Provider identity |
| `OLLAMA_MODEL` | `llama3.2:3b` | Model fetched by the managed inference runtime |

Set variables in PowerShell before starting a process, for example:

```powershell
$env:OLLAMA_MODEL = "llama3.2:3b"
$env:PROVIDER_ID = "provider-node-01"
$env:COORDINATOR_WS = "ws://127.0.0.1:8081/ws"
```

For local testing only, set `PROVIDER_ID` before starting a provider to override
the saved config for that process without changing `config.json`. This is useful
for running multiple test providers on one Windows machine. Real users normally
need only one provider instance per machine.

## Managed inference runtime

The provider downloads and manages Ollama v0.40.17 on first start. It uses plain
official release archives, never the Windows installer or macOS disk image.
The SHA-256 values for the supported archives are pinned in provider-core and
cross-checked against the official `sha256sum.txt` fetched over HTTPS at runtime;
downloaded archives are rejected if either checksum does not match.

| Platform | Archive |
|---|---|
| Windows x64 | `ollama-windows-amd64.zip` (~1.51 GiB) |
| Windows ARM64 | `ollama-windows-arm64.zip` |
| Linux x64 | `ollama-linux-amd64.tar.zst` (~1.44 GiB) |
| Linux ARM64 | `ollama-linux-arm64.tar.zst` |
| macOS | `ollama-darwin.tgz` (~179 MiB) |

ROCm and MLX add-on archives are not downloaded. Downloads resume from a partial
archive where the server supports HTTP range requests. Before downloading, the
provider queries the selected model manifest, displays the combined runtime
archive plus model download size, and checks free space for the archive,
conservative extraction reserve, and model. It checks the actual extracted
archive size again before extraction. `provider-core` reports progress through
`RuntimeState` callbacks, including the expected combined download bytes before
the transfer starts.

Runtime files and models are kept under the OS-local application data directory:

- Windows: `%LOCALAPPDATA%\GpuSwarm\runtime` and `%LOCALAPPDATA%\GpuSwarm\models`
- macOS: `~/Library/Application Support/GpuSwarm/runtime` and
  `~/Library/Application Support/GpuSwarm/models`
- Linux: `$XDG_DATA_HOME/GpuSwarm/runtime` and
  `$XDG_DATA_HOME/GpuSwarm/models` (normally `~/.local/share/GpuSwarm`)

The app starts the bundled executable directly, sets `OLLAMA_MODELS` to its
private model directory, and binds the API to a dynamically selected
`127.0.0.1` port. The process is stopped with the provider. The app does not
install Ollama system-wide, change `PATH`, create services, or modify the
registry. Any system Ollama installation is ignored.
For a single-job integration test, run `gpuswarm start --once`; the provider
shuts the managed runtime down after returning that job result.

The app checks for readiness with an adaptive probe interval and allows up to
three minutes for startup. While it waits, the Status screen shows an animated
Preparing state; only after the limit is reached does it show a friendly
startup error.

On first launch, a welcome screen explains the testnet and local resource use.
Settings and earnings history stay in the app's local configuration folder.
Use Settings → Local Data → Open data folder to review or back up those files;
the app does not delete them automatically.

Intel integrated graphics, unsupported/unknown GPUs, and CPU-only systems are
marked not recommended. The CLI asks for explicit confirmation before runtime
download on those machines; inference can then run on CPU. The macOS archive
binary is checked against the machine architecture at runtime. macOS behavior
has not been built or run on this Windows development machine.

The `zstd` crate decodes Linux `.tar.zst` archives in-process, without requiring
an installed `zstd` executable. The `tar` crate reads and extracts the tar
payload in-process, without requiring system tar tooling.

`OLLAMA_MODEL` remains configurable. Use `llama3.2:1b` for a smaller development
model, for example `$env:OLLAMA_MODEL = "llama3.2:1b"` in PowerShell. The
default `llama3.2:3b` model is subject to Meta's Llama 3.2 Community License,
including attribution requirements. TODO: review model licensing before any
commercial release.

## GPU support status

The provider reports GPU detection separately from GPU job support:

- `nvidia-supported`: NVIDIA GPU detected by `nvidia-smi`.
- `integrated-intel-unsupported`: Intel HD/UHD/Iris integrated graphics detected.
  These GPUs are shown as not suitable for GPU jobs.
- `integrated-amd-best-effort`: AMD/Radeon graphics detected with best-effort
  detection only. ROCm-specific execution is not implemented.
- `apple-silicon-best-effort`: Apple Silicon chip/GPU information detected.
  Ollama may use Metal, but the app does not perform Metal-specific detection.
- `none` or `other-best-effort`: no clearly supported GPU was identified.

## Fees

The coordinator sets a platform fee for the mock job ledger. Providers do not
set this fee, and the provider app never stores a private key. The default is
1000 basis points, or 10%, so the provider receives 90% of each mock job price.
Configure it on the coordinator with `PLATFORM_FEE_BPS` from 0 to 5000 and
`JOB_PRICE_UNITS` (default 10000 units, where 1 USDC would be 1,000,000 units).
This is bookkeeping only in test mode: no blockchain calls and no real money are
used or paid.

Set the coordinator's TESTNET fee recipient address before starting the
coordinator:

```powershell
$env:GPUSWARM_FEE_ADDRESS="0x1111111111111111111111111111111111111111"
cargo run -p coordinator
```

```bash
export GPUSWARM_FEE_ADDRESS="0x1111111111111111111111111111111111111111"
cargo run -p coordinator
```

The address must be `0x` followed by exactly 40 hexadecimal characters. For
local testnet use only, `GPUSWARM_ALLOW_NO_FEE_ADDRESS=1` permits a missing
address and emits a loud warning. The provider receives the configured fee
address from the coordinator as read-only fee configuration.

## Coordinator job storage

The coordinator stores job state in SQLite at `coordinator.sqlite3` in its
working directory. Set `GPUSWARM_DB_PATH` to choose another file. Queued and
terminal jobs survive coordinator restarts; any job that was running at restart
is marked failed because its provider connection no longer exists. Provider
connections themselves are temporary and must register again after a restart.

## Job reliability and disconnects

A new job is queued and assigned once an idle provider is available. If a
provider disconnects while a job is running, the coordinator marks that job
failed immediately and writes the reason to `failed_jobs.log`. Pending-job
assignment is serialized, so the same queued job cannot be sent concurrently
to multiple idle providers. The coordinator does not requeue or retry that
running job automatically after a disconnect, so there is no retry count and
only successful completion can create an `earnings.log` entry.

Jobs that are still queued because no provider is idle remain queued until a
provider becomes available. If no provider connects, a queued job stays queued
and does not earn anything. A running job that times out (default 120 seconds)
is also marked failed and is not retried.

## Local smoke tests

The coordinator must use ports 8080 for HTTP and 8081 for WebSocket traffic.
Both smoke-test scripts set a fake fee recipient when it is not already
configured and use the small `llama3.2:1b` development model.

Windows:

```powershell
.\test.ps1
```

The script starts the coordinator and provider, waits for provider registration,
tests a missing payment header, submits a job, and polls `GET /jobs/{id}` until
completion or failure.

Bash:

```bash
./test.sh
```
