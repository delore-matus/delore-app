#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

ISOLATED_TEST_DIR="$(mktemp -d "${TMPDIR:-/tmp}/gpu-swarm-test.XXXXXX")"
export GPUSWARM_CONFIG_DIR="$ISOLATED_TEST_DIR/config"
mkdir -p "$GPUSWARM_CONFIG_DIR"
trap 'rm -rf "$ISOLATED_TEST_DIR"' EXIT
echo "Using isolated provider config: $GPUSWARM_CONFIG_DIR"

cat >"$GPUSWARM_CONFIG_DIR/config.json" <<'JSON'
{
  "payout_address": null,
  "provider_id": "test-provider",
  "coordinator_ws": "ws://127.0.0.1:8081/ws",
  "ollama_model": "llama3.2:1b",
  "resource_settings": {
    "mode": "only_when_idle",
    "idle_threshold_minutes": 5,
    "schedule": null,
    "preset": "balanced",
    "do_not_run_on_battery": true,
    "not_recommended_confirmed": true,
    "performance_percent": 50
  },
  "desktop_preferences": {
    "theme": "dark",
    "language": "english",
    "start_on_login": false,
    "start_minimized_to_tray": false,
    "keep_running_in_tray": false,
    "notifications_enabled": false
  }
}
JSON

export GPUSWARM_FEE_ADDRESS="${GPUSWARM_FEE_ADDRESS:-0x20759eE366409B7dFE73a5325bfe0Dd57B6d7850}"
export OLLAMA_MODEL="${OLLAMA_MODEL:-llama3.2:1b}"

COORDINATOR_LOG="$ROOT/coordinator-test.log"
PROVIDER_LOG="$ROOT/provider-test.log"
: >"$COORDINATOR_LOG"
: >"$PROVIDER_LOG"

cargo run -p coordinator >"$COORDINATOR_LOG" 2>&1 &
COORDINATOR_PID=$!
trap 'kill "${PROVIDER_PID:-}" "${COORDINATOR_PID:-}" 2>/dev/null || true; rm -rf "$ISOLATED_TEST_DIR"' EXIT

for _ in $(seq 1 50); do
  if curl -fsS http://127.0.0.1:8080/fees >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
done
curl -fsS http://127.0.0.1:8080/fees
echo

cargo run -p provider-daemon -- start >"$PROVIDER_LOG" 2>&1 &
PROVIDER_PID=$!

echo "Waiting for provider registration..."
REGISTERED=false
for _ in $(seq 1 100); do
  PROVIDERS="$(curl -fsS http://127.0.0.1:8080/providers 2>/dev/null || true)"
  if [ -n "$PROVIDERS" ] && [ "$PROVIDERS" != "[]" ]; then
    echo "$PROVIDERS"
    REGISTERED=true
    break
  fi
  sleep 0.1
done
if [ "$REGISTERED" != true ]; then
  echo "FAIL: no provider registered"
  cat "$PROVIDER_LOG"
  exit 1
fi

echo
echo "Testing missing X-Payment header:"
curl -sS -i -X POST http://127.0.0.1:8080/request-job \
  -H 'Content-Type: application/json' \
  -d '{"id":"job-missing-payment","prompt":"Respond with exactly: ok"}'

JOB_ID="job-$(date +%s%N)"
echo
echo "Testing accepted job: $JOB_ID"
curl -sS -i -X POST http://127.0.0.1:8080/request-job \
  -H 'Content-Type: application/json' \
  -H 'X-Payment: mock-payment-ok' \
  -d "{\"id\":\"$JOB_ID\",\"prompt\":\"Respond with exactly: ok\"}"

echo
echo "Polling $JOB_ID..."
for _ in $(seq 1 600); do
  JOB="$(curl -fsS "http://127.0.0.1:8080/jobs/$JOB_ID" 2>/dev/null || true)"
  if [ -n "$JOB" ] && printf '%s' "$JOB" | grep -q '"status":"completed"'; then
    echo "$JOB"
    exit 0
  fi
  if [ -n "$JOB" ] && printf '%s' "$JOB" | grep -q '"status":"failed"'; then
    echo "$JOB"
    exit 1
  fi
  sleep 0.1
done

echo "FAIL: job did not finish"
exit 1
