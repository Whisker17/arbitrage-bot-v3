#!/usr/bin/env bash
# WHI-715 — long-running signerless shadow monitor against Mantle mainnet.
#
# Starts one or more *_monitor_executor_service processes under SHADOW_MODE=1,
# writing per-service ledgers under evidence/shadow/continuous/<svc>/ledger.jsonl.
# Never requires a signing key; refuses to start if any forbidden signer env var
# is present.
#
# Usage (from repo root, with .env providing MANTLE_RPC_URL / MANTLE_RPC_WS_URL):
#
#   ./scripts/shadow/run_continuous_mainnet.sh              # all four services
#   SERVICES=v2,v3 ./scripts/shadow/run_continuous_mainnet.sh
#   FOREGROUND=1 SERVICES=v2 ./scripts/shadow/run_continuous_mainnet.sh
#
# Environment (all optional except RPC URLs):
#   MANTLE_RPC_URL / MANTLE_RPC_WS_URL   preferred mainnet RPC pair
#   MANTLE_HTTP_URL / MANTLE_WS_URL      aliases also accepted
#   RPC_HTTP_URL / RPC_WS_URL            lowest-level aliases
#   ARBITRAGE_EXECUTOR_ADDRESS          default 0x...0002 (shadow placeholder)
#   SHADOW_ROOT                         default evidence/shadow/continuous
#   SERVICES                            comma list: v2,v3,v3-1559,moe (default all)
#   RESTART_DELAY_SEC                   supervisor backoff (default 5)
#   FOREGROUND                          if 1, run supervisor in foreground
#   CARGO_BIN_DIR                       if set, use prebuilt binaries from here
#                                       (e.g. target/release/examples) instead of
#                                       `cargo run`

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

# Load local .env if present (never committed). Do not export values into the
# shell history; just source for this process tree.
if [[ -f "$ROOT/.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  source "$ROOT/.env"
  set +a
fi

SHADOW_ROOT="${SHADOW_ROOT:-$ROOT/evidence/shadow/continuous}"
SERVICES_CSV="${SERVICES:-v2,v3,v3-1559,moe}"
RESTART_DELAY_SEC="${RESTART_DELAY_SEC:-5}"
FOREGROUND="${FOREGROUND:-0}"
RUN_DIR="$SHADOW_ROOT/run"
LOG_DIR="$SHADOW_ROOT/logs"
PID_DIR="$RUN_DIR/pids"

# --- no_send invariant: refuse any signer material --------------------------------
FORBIDDEN_SIGNER_VARS=(
  MANTLE_SEPOLIA_PRIVATE_KEY
  MANTLE_MAINNET_PRIVATE_KEY
  MANTLE_PRIVATE_KEY
  PRIVATE_KEY
  EXECUTION_PRIVATE_KEY
)

for var in "${FORBIDDEN_SIGNER_VARS[@]}"; do
  if [[ -n "${!var:-}" ]]; then
    echo "error: $var is set; continuous shadow mode must run without any signing key" >&2
    echo "       unset $var (and peers) before starting." >&2
    exit 1
  fi
  # Presence with empty value also counts as present in the Rust guard; drop it.
  unset "$var" 2>/dev/null || true
done

# --- resolve RPC endpoints --------------------------------------------------------
HTTP_URL="${RPC_HTTP_URL:-${MANTLE_HTTP_URL:-${MANTLE_RPC_URL:-}}}"
WS_URL="${RPC_WS_URL:-${MANTLE_WS_URL:-${MANTLE_RPC_WS_URL:-}}}"

if [[ -z "$HTTP_URL" || -z "$WS_URL" ]]; then
  echo "error: need mainnet RPC endpoints. Set MANTLE_RPC_URL + MANTLE_RPC_WS_URL" >&2
  echo "       (or MANTLE_HTTP_URL/MANTLE_WS_URL, or RPC_HTTP_URL/RPC_WS_URL)." >&2
  exit 1
fi

# Services read these names; pin all aliases so no service falls back to sepolia.
export RPC_HTTP_URL="$HTTP_URL"
export RPC_WS_URL="$WS_URL"
export MANTLE_HTTP_URL="$HTTP_URL"
export MANTLE_WS_URL="$WS_URL"

# Shadow-safe placeholder executor when operator has not set one.
export ARBITRAGE_EXECUTOR_ADDRESS="${ARBITRAGE_EXECUTOR_ADDRESS:-0x0000000000000000000000000000000000000002}"
export SHADOW_MODE=1
export RUST_LOG="${RUST_LOG:-info,amms=info}"

mkdir -p "$SHADOW_ROOT" "$RUN_DIR" "$LOG_DIR" "$PID_DIR"

# --- service table ----------------------------------------------------------------
# name|example_target|ledger_subdir
service_row() {
  case "$1" in
    v2)       echo "v2|v2_monitor_executor_service|v2" ;;
    v3)       echo "v3|v3_monitor_executor_service|v3" ;;
    v3-1559)  echo "v3-1559|v3_monitor_executor_service_1559|v3-1559" ;;
    moe)      echo "moe|moe_monitor_executor_service|moe" ;;
    *)        return 1 ;;
  esac
}

IFS=',' read -r -a SERVICE_LIST <<<"$SERVICES_CSV"

launch_one() {
  local key="$1"
  local row example subdir ledger_path service_log pid_file
  row="$(service_row "$key")" || {
    echo "error: unknown service key '$key' (want v2|v3|v3-1559|moe)" >&2
    return 1
  }
  IFS='|' read -r _ example subdir <<<"$row"
  mkdir -p "$SHADOW_ROOT/$subdir"
  ledger_path="$SHADOW_ROOT/$subdir/ledger.jsonl"
  service_log="$LOG_DIR/${key}.log"
  pid_file="$PID_DIR/${key}.pid"

  if [[ -f "$pid_file" ]] && kill -0 "$(cat "$pid_file")" 2>/dev/null; then
    echo "already running: $key (pid $(cat "$pid_file"))"
    return 0
  fi

  local -a cmd
  if [[ -n "${CARGO_BIN_DIR:-}" ]]; then
    local bin="$CARGO_BIN_DIR/$example"
    if [[ ! -x "$bin" ]]; then
      echo "error: CARGO_BIN_DIR set but missing executable $bin" >&2
      return 1
    fi
    cmd=("$bin" --shadow --ledger "$ledger_path")
  else
    cmd=(cargo run --locked --example "$example" -- --shadow --ledger "$ledger_path")
  fi

  echo "starting $key → ledger=$ledger_path log=$service_log"
  (
    # Re-assert no_send inside the child environment.
    for var in "${FORBIDDEN_SIGNER_VARS[@]}"; do unset "$var" 2>/dev/null || true; done
    export SHADOW_MODE=1
    export RPC_HTTP_URL MANTLE_HTTP_URL RPC_WS_URL MANTLE_WS_URL
    export ARBITRAGE_EXECUTOR_ADDRESS
    # Supervisor loop: restart until stop file appears.
    while [[ ! -f "$RUN_DIR/STOP" ]]; do
      echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] launching $key" >>"$service_log"
      "${cmd[@]}" >>"$service_log" 2>&1 &
      local child=$!
      echo "$child" >"$pid_file"
      wait "$child" || true
      local rc=$?
      echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] $key exited rc=$rc; restart in ${RESTART_DELAY_SEC}s" >>"$service_log"
      if [[ -f "$RUN_DIR/STOP" ]]; then
        break
      fi
      sleep "$RESTART_DELAY_SEC"
    done
    rm -f "$pid_file"
  ) &
  local supervisor=$!
  echo "$supervisor" >"$PID_DIR/${key}.supervisor.pid"
  echo "  supervisor pid=$supervisor"
}

rm -f "$RUN_DIR/STOP"

echo "WHI-715 continuous shadow"
echo "  root:     $SHADOW_ROOT"
echo "  services: ${SERVICE_LIST[*]}"
echo "  http:     ${HTTP_URL%%\?*}… (redacted query)"
echo "  executor: $ARBITRAGE_EXECUTOR_ADDRESS"
echo "  SHADOW_MODE=1; signer env vars forced unset"

for key in "${SERVICE_LIST[@]}"; do
  key="$(echo "$key" | tr -d '[:space:]')"
  [[ -z "$key" ]] && continue
  launch_one "$key"
done

cat >"$SHADOW_ROOT/STATUS.md" <<EOF
# Continuous shadow run (WHI-715)

- Started (UTC): $(date -u +%Y-%m-%dT%H:%M:%SZ)
- Services: ${SERVICE_LIST[*]}
- Ledger root: \`$SHADOW_ROOT\`
- SHADOW_MODE: 1
- send capability: no_send (enforced by runtime + this launcher)
- Executor: \`$ARBITRAGE_EXECUTOR_ADDRESS\`

Stop with: \`./scripts/shadow/stop_continuous_mainnet.sh\`
Status: \`./scripts/shadow/status_continuous_mainnet.sh\`
EOF

echo "wrote $SHADOW_ROOT/STATUS.md"

if [[ "$FOREGROUND" == "1" ]]; then
  echo "foreground mode: waiting on supervisors (Ctrl-C then run stop script)"
  wait
fi
