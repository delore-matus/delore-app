# GPU Swarm MVP

This local Rust prototype coordinates provider daemons and a local Ollama server.
It is for testing on a local machine or testnet only; it does not use real money
or private keys.

## Configuration

| Variable | Default | Used by |
|---|---|---|
| `COORDINATOR_WS` | `ws://127.0.0.1:8081/ws` | Provider WebSocket URL |
| `PROVIDER_ID` | `provider-node-01` | Provider identity |
| `OLLAMA_MODEL` | `llama3.2:3b` | Ollama model for inference |

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

## Fees

The coordinator sets a platform fee for the mock job ledger. Providers do not
set this fee, and the provider app never stores a private key. The default is
1000 basis points, or 10%, so the provider receives 90% of each mock job price.
Configure it on the coordinator with `PLATFORM_FEE_BPS` from 0 to 5000 and
`JOB_PRICE_UNITS` (default 10000 units, where 1 USDC would be 1,000,000 units).
This is bookkeeping only in test mode: no blockchain calls and no real money are
used or paid.

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
