#!/usr/bin/env bash
set -euo pipefail

# Start the coordinator and provider in separate background processes.
cargo run -p coordinator &
COORDINATOR_PID=$!
sleep 2

cargo run -p provider-daemon &
PROVIDER_PID=$!
sleep 2

# 402 response: X-Payment is missing.
echo "Testing missing X-Payment header:"
curl -i -X POST http://127.0.0.1:8081/request-job \
  -H 'Content-Type: application/json' \
  -d '{"id":"job-001","prompt":"2 + 2"}'

# Accepted response: presence of X-Payment is enough for this mock.
echo
echo "Testing accepted job:"
curl -i -X POST http://127.0.0.1:8081/request-job \
  -H 'Content-Type: application/json' \
  -H 'X-Payment: mock-payment-ok' \
  -d '{"id":"job-001","prompt":"2 + 2"}'

echo
echo "Wait about 2 seconds, then inspect the coordinator terminal output for JobResult."
sleep 3

kill "$PROVIDER_PID" "$COORDINATOR_PID" 2>/dev/null || true
