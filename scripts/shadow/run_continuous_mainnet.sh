#!/usr/bin/env bash
# WHI-715 — long-running signerless shadow monitor against Mantle mainnet.
# WHI-740 — registers the merged multi-protocol `bot` [[bin]] as a launchable
#           service key alongside the three legacy example services.
#
# Starts one or more shadow service processes under SHADOW_MODE=1, writing
# per-service ledgers under evidence/shadow/continuous/<svc>/ledger.jsonl.
# Never requires a signing key; refuses to start if any forbidden signer env var
# is present.
#
# Usage (from repo root, with .env providing mainnet RPC URLs):
#
#   ./scripts/shadow/run_continuous_mainnet.sh              # all services (incl. bot)
#   SERVICES=v2,v3-1559 ./scripts/shadow/run_continuous_mainnet.sh
#   SERVICES=bot ./scripts/shadow/run_continuous_mainnet.sh
#   FOREGROUND=1 SERVICES=v2 ./scripts/shadow/run_continuous_mainnet.sh
#   RESOLVE_ONLY=1 SERVICES=bot ./scripts/shadow/run_continuous_mainnet.sh  # no RPC
#
# Environment (all optional except RPC URLs, unless RESOLVE_ONLY=1):
#   BOT_CHAIN_ID=5000                   expected chain (WHI-776; default 5000)
#   MANTLE_MAINNET_RPC_URL / _WS_URL    canonical mainnet pair (WHI-776)
#   MANTLE_RPC_URL / MANTLE_RPC_WS_URL  legacy mainnet aliases
#   MANTLE_HTTP_URL / MANTLE_WS_URL     legacy generic aliases
#   RPC_HTTP_URL / RPC_WS_URL           explicit overrides
#   ARBITRAGE_EXECUTOR_ADDRESS          default 0x...0002 (shadow placeholder)
#   SHADOW_ROOT                         default evidence/shadow/continuous
#   SERVICES                            comma list: v2,v3-1559,moe,bot (default all)
#   RESTART_DELAY_SEC                   supervisor backoff (default 5)
#   FOREGROUND                          if 1, run supervisor in foreground
#   CARGO_BIN_DIR                       if set, use prebuilt binaries from here
#                                       (e.g. target/release/examples for example
#                                       targets; bin targets resolve one level up
#                                       when CARGO_BIN_DIR ends in /examples)
#   RESOLVE_ONLY                        if 1, print name|kind|target|subdir rows
#                                       for SERVICES and exit (no RPC, no launch)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

# --- service table ----------------------------------------------------------------
# Row format: name|kind|target|ledger_subdir
#   kind=example → cargo run --example <target>  (or CARGO_BIN_DIR/<target>)
#   kind=bin     → cargo run --bin <target>      (or sibling of examples/ when
#                  CARGO_BIN_DIR ends in /examples)
VALID_SERVICE_KEYS="v2|v3-1559|moe|bot"

service_row() {
  case "$1" in
    v2)       echo "v2|example|v2_monitor_executor_service|v2" ;;
    v3-1559)  echo "v3-1559|example|v3_monitor_executor_service_1559|v3-1559" ;;
    moe)      echo "moe|example|moe_monitor_executor_service|moe" ;;
    bot)      echo "bot|bin|bot|bot" ;;
    *)        return 1 ;;
  esac
}

resolve_prebuilt_path() {
  # $1=kind $2=target  → absolute-ish path under CARGO_BIN_DIR
  local kind="$1" target="$2"
  case "$kind" in
    example)
      echo "${CARGO_BIN_DIR}/${target}"
      ;;
    bin)
      if [[ "$(basename "${CARGO_BIN_DIR}")" == "examples" ]]; then
        echo "$(dirname "${CARGO_BIN_DIR}")/${target}"
      else
        echo "${CARGO_BIN_DIR}/${target}"
      fi
      ;;
    *)
      echo "error: unknown target kind '$kind' (want example|bin)" >&2
      return 1
      ;;
  esac
}

# Dry-run path for acceptance checks: no RPC, no signer env, no process start.
if [[ "${RESOLVE_ONLY:-0}" == "1" ]]; then
  SERVICES_CSV="${SERVICES:-v2,v3-1559,moe,bot}"
  IFS=',' read -r -a SERVICE_LIST <<<"$SERVICES_CSV"
  for key in "${SERVICE_LIST[@]}"; do
    key="$(echo "$key" | tr -d '[:space:]')"
    [[ -z "$key" ]] && continue
    row="$(service_row "$key")" || {
      echo "error: unknown service key '$key' (want ${VALID_SERVICE_KEYS})" >&2
      exit 1
    }
    IFS='|' read -r _ kind target subdir <<<"$row"
    echo "$row"
    case "$kind" in
      example)
        echo "  cargo: cargo run --locked --example ${target} -- --shadow --ledger <path>"
        ;;
      bin)
        # bot accepts --watch (continuous multi-protocol loop, WHI-741) + --ledger
        # (shadow evidence, WHI-739). SHADOW_MODE=1 is exported by the full launcher.
        echo "  cargo: cargo run --locked --bin ${target} -- --watch --ledger <path>"
        ;;
      *)
        echo "error: unknown target kind '$kind'" >&2
        exit 1
        ;;
    esac
  done
  exit 0
fi

# Load local .env if present (never committed). Do not export values into the
# shell history; just source for this process tree.
if [[ -f "$ROOT/.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  source "$ROOT/.env"
  set +a
fi

SHADOW_ROOT="${SHADOW_ROOT:-$ROOT/evidence/shadow/continuous}"
SERVICES_CSV="${SERVICES:-v2,v3-1559,moe,bot}"
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

# --- resolve RPC endpoints (WHI-776 precedence for mainnet) -----------------------
# RPC_* → MANTLE_MAINNET_* → MANTLE_RPC_* → MANTLE_HTTP_*/MANTLE_WS_*
HTTP_URL="${RPC_HTTP_URL:-${MANTLE_MAINNET_RPC_URL:-${MANTLE_RPC_URL:-${MANTLE_HTTP_URL:-}}}}"
WS_URL="${RPC_WS_URL:-${MANTLE_MAINNET_RPC_WS_URL:-${MANTLE_RPC_WS_URL:-${MANTLE_WS_URL:-}}}}"

if [[ -z "$HTTP_URL" || -z "$WS_URL" ]]; then
  echo "error: need mainnet RPC endpoints. Set MANTLE_MAINNET_RPC_URL + MANTLE_MAINNET_RPC_WS_URL" >&2
  echo "       (or MANTLE_RPC_URL/MANTLE_RPC_WS_URL, MANTLE_HTTP_URL/MANTLE_WS_URL," >&2
  echo "        or RPC_HTTP_URL/RPC_WS_URL)." >&2
  exit 1
fi

# Pin aliases so legacy example services and the bot resolvers all see the pair.
# Never export Sepolia vars from this launcher.
export BOT_CHAIN_ID="${BOT_CHAIN_ID:-5000}"
export RPC_HTTP_URL="$HTTP_URL"
export RPC_WS_URL="$WS_URL"
export MANTLE_MAINNET_RPC_URL="$HTTP_URL"
export MANTLE_MAINNET_RPC_WS_URL="$WS_URL"
export MANTLE_RPC_URL="$HTTP_URL"
export MANTLE_RPC_WS_URL="$WS_URL"
export MANTLE_HTTP_URL="$HTTP_URL"
export MANTLE_WS_URL="$WS_URL"

# Shadow-safe placeholder executor when operator has not set one.
export ARBITRAGE_EXECUTOR_ADDRESS="${ARBITRAGE_EXECUTOR_ADDRESS:-0x0000000000000000000000000000000000000002}"
# Required by build_shadow_execution_context (no default inside the service).
export MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH="${MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH:-$ROOT/config/gas_profiles/shadow_thresholds.mantle_mainnet.json}"
if [[ ! -f "$MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH" ]]; then
  echo "error: MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH not found: $MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH" >&2
  exit 1
fi
export SHADOW_MODE=1
export RUST_LOG="${RUST_LOG:-info,amms=info}"

mkdir -p "$SHADOW_ROOT" "$RUN_DIR" "$LOG_DIR" "$PID_DIR"

IFS=',' read -r -a SERVICE_LIST <<<"$SERVICES_CSV"

launch_one() {
  local key="$1"
  local row kind target subdir ledger_path service_log pid_file
  row="$(service_row "$key")" || {
    echo "error: unknown service key '$key' (want ${VALID_SERVICE_KEYS})" >&2
    return 1
  }
  IFS='|' read -r _ kind target subdir <<<"$row"
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
    local bin
    bin="$(resolve_prebuilt_path "$kind" "$target")" || return 1
    if [[ ! -x "$bin" ]]; then
      echo "error: CARGO_BIN_DIR set but missing executable $bin" >&2
      return 1
    fi
    case "$kind" in
      example) cmd=("$bin" --shadow --ledger "$ledger_path") ;;
      bin)     cmd=("$bin" --watch --ledger "$ledger_path") ;;
      *)
        echo "error: unknown target kind '$kind'" >&2
        return 1
        ;;
    esac
  else
    case "$kind" in
      example)
        cmd=(cargo run --locked --example "$target" -- --shadow --ledger "$ledger_path")
        ;;
      bin)
        cmd=(cargo run --locked --bin "$target" -- --watch --ledger "$ledger_path")
        ;;
      *)
        echo "error: unknown target kind '$kind'" >&2
        return 1
        ;;
    esac
  fi

  echo "starting $key → ledger=$ledger_path log=$service_log"
  (
    # Re-assert no_send inside the child environment.
    for var in "${FORBIDDEN_SIGNER_VARS[@]}"; do unset "$var" 2>/dev/null || true; done
    export SHADOW_MODE=1
    export BOT_CHAIN_ID
    export RPC_HTTP_URL RPC_WS_URL
    export MANTLE_MAINNET_RPC_URL MANTLE_MAINNET_RPC_WS_URL
    export MANTLE_RPC_URL MANTLE_RPC_WS_URL
    export MANTLE_HTTP_URL MANTLE_WS_URL
    export ARBITRAGE_EXECUTOR_ADDRESS
    export MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH
    # Supervisor loop: restart until stop file appears.
    # NOTE: this is a bare subshell, not a function — do not use `local`.
    while [[ ! -f "$RUN_DIR/STOP" ]]; do
      echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] launching $key" >>"$service_log"
      "${cmd[@]}" >>"$service_log" 2>&1 &
      child=$!
      echo "$child" >"$pid_file"
      rc=0
      wait "$child" || rc=$?
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
echo "  thresholds: $MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH"
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
