#!/bin/bash
# Run Criterion benchmarks (optional) and push metrics to Pushgateway.
#
# Prerequisites:
#   - docker compose up -d   (in monad-scripts/monitoring/)
#   - jq installed
#
# Examples:
#   ./push-bench-metrics.sh                         # run core benches + push
#   SKIP_BENCH=1 ./push-bench-metrics.sh            # push existing target/criterion only
#   BENCH_MODE=single BENCH_PKG=monad-crypto BENCH_FILTER=hasher_bench ./push-bench-metrics.sh
#   BENCH_MODE=full ./push-bench-metrics.sh         # cargo bench (needs libclang; may fail)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
BENCH_LIST="${BENCH_LIST:-$SCRIPT_DIR/benches.txt}"
CRITERION_DIR="${CRITERION_DIR:-$REPO_ROOT/target/criterion}"
PUSHGATEWAY_URL="${PUSHGATEWAY_URL:-http://localhost:9091}"
JOB_NAME="${JOB_NAME:-monad_criterion_bench}"
INSTANCE="${INSTANCE:-$(hostname -s 2>/dev/null || echo local)}"
BUILD_ID="${BUILD_ID:-$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)}"
BENCH_MODE="${BENCH_MODE:-core}"

if ! command -v jq >/dev/null 2>&1; then
  echo "Error: jq is required. Install with: sudo apt install jq" >&2
  exit 1
fi

run_core_benches() {
  local pkg bench failed=0 passed=0
  echo "Running core benches from $BENCH_LIST ..."
  while read -r pkg bench; do
    [[ -z "$pkg" || "$pkg" =~ ^# ]] && continue
    echo
    echo "==> cargo bench -p $pkg --bench $bench"
    if cargo bench -p "$pkg" --bench "$bench"; then
      passed=$((passed + 1))
    else
      echo "Warning: $pkg/$bench failed (continuing)" >&2
      failed=$((failed + 1))
    fi
  done < "$BENCH_LIST"
  echo
  echo "Core benches done: $passed passed, $failed failed"
  if [[ "$passed" -eq 0 ]]; then
    echo "Error: no benchmarks succeeded." >&2
    exit 1
  fi
}

run_benches() {
  cd "$REPO_ROOT"
  case "$BENCH_MODE" in
    core)
      run_core_benches
      ;;
    single)
      if [[ -z "${BENCH_FILTER:-}" ]]; then
        echo "Error: set BENCH_FILTER=<bench_name> for BENCH_MODE=single" >&2
        exit 1
      fi
      if [[ -n "${BENCH_PKG:-}" ]]; then
        cargo bench -p "$BENCH_PKG" --bench "$BENCH_FILTER"
      else
        cargo bench --bench "$BENCH_FILTER"
      fi
      ;;
    full)
      echo "Warning: BENCH_MODE=full runs all workspace benches." >&2
      echo "         Requires libclang-dev and a fully compiling tree." >&2
      echo "         Install: sudo apt install libclang-dev" >&2
      cargo bench
      ;;
    *)
      echo "Error: unknown BENCH_MODE=$BENCH_MODE (use core|single|full)" >&2
      exit 1
      ;;
  esac
}

if [[ "${SKIP_BENCH:-0}" != "1" ]]; then
  echo "Running benchmarks in $REPO_ROOT (BENCH_MODE=$BENCH_MODE) ..."
  run_benches
fi

if [[ ! -d "$CRITERION_DIR" ]]; then
  echo "Error: criterion output not found at $CRITERION_DIR" >&2
  echo "Run benchmarks first or set CRITERION_DIR." >&2
  exit 1
fi

METRICS_FILE="$(mktemp)"
trap 'rm -f "$METRICS_FILE"' EXIT

echo "Generating Prometheus metrics from $CRITERION_DIR ..."
"$REPO_ROOT/monad-scripts/jenkins/prometheus_metrics.sh" "$CRITERION_DIR" \
  > "$METRICS_FILE"

if [[ ! -s "$METRICS_FILE" ]]; then
  echo "Error: no metrics generated. Did any benchmark produce results?" >&2
  exit 1
fi

METRIC_COUNT="$(wc -l < "$METRICS_FILE" | tr -d ' ')"
PUSH_URL="${PUSHGATEWAY_URL}/metrics/job/${JOB_NAME}/instance/${INSTANCE}/build/${BUILD_ID}"
echo "Pushing $METRIC_COUNT metrics to $PUSH_URL ..."
curl -fsS --data-binary @"$METRICS_FILE" "$PUSH_URL"

echo
echo "Done. Metrics pushed ($METRIC_COUNT series)."
echo "  Prometheus:  http://localhost:9090"
echo "  Pushgateway: http://localhost:9091"
echo "  Grafana:     http://localhost:3000  (admin / admin)"
echo
echo "Example PromQL:"
echo "  {job=\"${JOB_NAME}\"}"
echo "  hasher_sha256_10KB_batch_ns_per_iter"
