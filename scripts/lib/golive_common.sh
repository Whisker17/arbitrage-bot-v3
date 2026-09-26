#!/usr/bin/env bash
# Shared helpers for WHI-953 go-live launchers.
# Sourced only — do not execute directly.
#
# Factored preflight (chain id, codehash, universe fingerprint) and the
# FORBIDDEN_ENV_VAR_NAMES boundary so the three launchers cannot drift.

# Must match src/execution/e2e/env_guard.rs::FORBIDDEN_ENV_VAR_NAMES exactly.
GOLIVE_FORBIDDEN_ENV_VAR_NAMES=(
  MANTLE_SEPOLIA_PRIVATE_KEY
  MANTLE_MAINNET_PRIVATE_KEY
  MANTLE_PRIVATE_KEY
  PRIVATE_KEY
  EXECUTION_PRIVATE_KEY
)

# Additional signing material the production arm path may carry. Not part of
# the Rust shadow guard list, but never allowed into the signerless child.
GOLIVE_EXTRA_SIGNER_ENV_NAMES=(
  BOT_HOT_EXECUTOR_PRIVATE_KEY
  BOT_GUARDIAN_PRIVATE_KEY
  BOT_ENABLE_SENDS
)

GOLIVE_WMNT_MAINNET="0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8"
GOLIVE_EXPECTED_CHAIN_ID="5000"
GOLIVE_DEFAULT_UNIVERSE="data/pool_universe.csv"
# WHI-1410: committed pin for the universe the launchers may run.
GOLIVE_UNIVERSE_PIN="config/pool_universe.pin.json"
GOLIVE_DEFAULT_IDENTITY="config/executor_identity.json"
GOLIVE_DEFAULT_ARTIFACT="contracts/executor/artifacts/ArbitrageExecutor.full.json"

golive_repo_root() {
  local here
  here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  cd "$here/../.." && pwd
}

golive_die() {
  echo "ABORT: $*" >&2
  exit 1
}

golive_step() {
  printf '\n=== %s ===\n' "$*"
}

golive_scrub() {
  sed -E 's#(https?|wss?)://[^ "]*#<endpoint>#g'
}

golive_norm_hex() {
  case "$1" in
    0x*|0X*) printf '%s' "$1" ;;
    *) printf '0x%s' "$1" ;;
  esac
}

golive_addr_lc() {
  printf '%s' "$1" | tr '[:upper:]' '[:lower:]'
}

golive_load_dotenv() {
  local root="$1"
  if [[ -f "$root/.env" ]]; then
    # shellcheck disable=SC1091
    set -a
    # shellcheck source=/dev/null
    source "$root/.env"
    set +a
  fi
}

# Print the union of forbidden + extra signer names, one per line.
golive_all_strip_names() {
  local n
  for n in "${GOLIVE_FORBIDDEN_ENV_VAR_NAMES[@]}" "${GOLIVE_EXTRA_SIGNER_ENV_NAMES[@]}"; do
    printf '%s\n' "$n"
  done
}

# Unset every forbidden / extra signer var in the current shell.
golive_unset_signer_env() {
  local n
  while IFS= read -r n; do
    [[ -z "$n" ]] && continue
    unset "$n" 2>/dev/null || true
  done < <(golive_all_strip_names)
}

# Fail if any FORBIDDEN_ENV_VAR_NAMES name is present (even empty).
golive_assert_no_forbidden_env() {
  local n
  for n in "${GOLIVE_FORBIDDEN_ENV_VAR_NAMES[@]}"; do
    if [[ -n "${!n+x}" ]]; then
      golive_die "forbidden env var $n is present (shadow / no_send boundary)"
    fi
  done
}

# Fail if hot/guardian private keys are present (deploy-only / fund boundary).
golive_assert_no_hot_guardian_keys() {
  if [[ -n "${BOT_HOT_EXECUTOR_PRIVATE_KEY+x}" ]]; then
    golive_die "BOT_HOT_EXECUTOR_PRIVATE_KEY must not be present (addresses only)"
  fi
  if [[ -n "${BOT_GUARDIAN_PRIVATE_KEY+x}" ]]; then
    golive_die "BOT_GUARDIAN_PRIVATE_KEY must not be present (addresses only)"
  fi
}

# --- chain / identity / universe preflight ------------------------------------

golive_require_chain_id() {
  local rpc="$1"
  local chain
  chain="$(cast chain-id --rpc-url "$rpc")" || golive_die "cast chain-id failed"
  [[ "$chain" == "$GOLIVE_EXPECTED_CHAIN_ID" ]] \
    || golive_die "chain id is $chain, expected $GOLIVE_EXPECTED_CHAIN_ID"
  printf '%s' "$chain"
}

golive_expected_codehash() {
  local identity="${1:-$GOLIVE_DEFAULT_IDENTITY}"
  [[ -f "$identity" ]] || golive_die "identity file missing: $identity"
  python3 -c "import json;print(json.load(open('$identity'))['patched_runtime_hash'])"
}

golive_require_codehash() {
  local executor="$1"
  local rpc="$2"
  local identity="${3:-$GOLIVE_DEFAULT_IDENTITY}"
  local expected observed
  expected="$(golive_expected_codehash "$identity")"
  observed="$(cast codehash "$executor" --rpc-url "$rpc")" \
    || golive_die "cast codehash failed"
  [[ "$observed" == "$expected" ]] \
    || golive_die "codehash mismatch on $executor: observed=$observed expected=$expected"
  printf '%s' "$observed"
}

golive_universe_meta_path() {
  local csv="${1:-$GOLIVE_DEFAULT_UNIVERSE}"
  local dir base
  dir="$(dirname "$csv")"
  base="$(basename "$csv" .csv)"
  # Prefer stem.meta.json (pool_universe.meta.json) over csv.meta.json.
  if [[ -f "$dir/${base}.meta.json" ]]; then
    printf '%s' "$dir/${base}.meta.json"
  elif [[ -f "${csv}.meta.json" ]]; then
    printf '%s' "${csv}.meta.json"
  else
    golive_die "universe meta missing next to $csv (need ${base}.meta.json)"
  fi
}

golive_require_universe_fingerprint() {
  local csv="${1:-$GOLIVE_DEFAULT_UNIVERSE}"
  local meta fingerprint pool_count chain_id
  [[ -f "$csv" ]] || golive_die "pool universe missing: $csv"
  meta="$(golive_universe_meta_path "$csv")"
  # WHI-1410: CSV + meta always self-agree after a regeneration, so compare
  # both against the committed pin; the CSV sha binds the file the bot loads.
  local pin pinned_fp pinned_sha csv_sha fields
  pin="$(golive_repo_root)/$GOLIVE_UNIVERSE_PIN"
  [[ -f "$pin" ]] || golive_die "universe pin missing: $pin"
  # Paths go in via argv, never into the Python source (quotes/backslashes).
  fields="$(python3 - "$meta" "$pin" "$csv" <<'PY'
import hashlib, json, sys
meta_path, pin_path, csv_path = sys.argv[1:4]
def load(path):
    try:
        with open(path) as f:
            doc = json.load(f)
    except (OSError, ValueError) as e:
        sys.exit(f"cannot read JSON {path!r}: {e}")
    if not isinstance(doc, dict):
        sys.exit(f"{path!r} is not a JSON object")
    return doc
meta, pin = load(meta_path), load(pin_path)
try:
    with open(csv_path, "rb") as f:
        csv_sha = hashlib.sha256(f.read()).hexdigest()
except OSError as e:
    sys.exit(f"cannot read {csv_path!r}: {e}")
vals = [meta.get("fingerprint") or "", meta.get("pool_count") or 0,
        meta.get("chain_id") or 0, pin.get("fingerprint") or "",
        pin.get("csv_sha256") or "", csv_sha]
for v in vals:  # one value per line; empty values stay empty lines
    print(" ".join(str(v).splitlines()))
PY
)" || golive_die "universe preflight could not read meta/pin/csv ($meta, $pin, $csv)"
  { read -r fingerprint; read -r pool_count; read -r chain_id
    read -r pinned_fp; read -r pinned_sha; read -r csv_sha; } <<<"$fields"
  [[ -n "$fingerprint" ]] || golive_die "universe meta $meta has empty fingerprint"
  [[ "$chain_id" == "$GOLIVE_EXPECTED_CHAIN_ID" ]] \
    || golive_die "universe chain_id=$chain_id, expected $GOLIVE_EXPECTED_CHAIN_ID"
  [[ "$pool_count" =~ ^[0-9]+$ && "$pool_count" -gt 0 ]] || golive_die "universe pool_count is not a positive integer: $pool_count"
  [[ -n "$pinned_fp" && -n "$pinned_sha" ]] || golive_die "universe pin $pin lacks fingerprint/csv_sha256"
  [[ "$fingerprint" == "$pinned_fp" ]] \
    || golive_die "universe fingerprint $fingerprint ($meta) != pinned $pinned_fp ($pin); deploy the committed universe or commit the new one with its pin"
  [[ "$csv_sha" == "$pinned_sha" ]] \
    || golive_die "universe csv sha256 $csv_sha ($csv) != pinned $pinned_sha ($pin); CSV and meta disagree or the CSV was edited"
  # Export for callers that want to log them.
  GOLIVE_UNIVERSE_FINGERPRINT="$fingerprint"
  GOLIVE_UNIVERSE_POOL_COUNT="$pool_count"
  GOLIVE_UNIVERSE_META="$meta"
  printf '%s' "$fingerprint"
}

# Derive (pool, poolType) lines from data/pool_universe.csv.
# poolType: agni-v2→0, agni-v3→1, moe→2 (matches PoolType on ArbitrageExecutor).
golive_pools_from_universe() {
  local csv="${1:-$GOLIVE_DEFAULT_UNIVERSE}"
  [[ -f "$csv" ]] || golive_die "pool universe missing: $csv"
  python3 - "$csv" <<'PY'
import csv, sys
path = sys.argv[1]
proto = {"agni-v2": "0", "agni-v3": "1", "moe": "2"}
rows = []
with open(path, newline="") as f:
    for r in csv.DictReader(f):
        p = (r.get("protocol") or "").strip()
        pool = (r.get("pool") or "").strip().lower()
        if p not in proto:
            sys.stderr.write(f"unknown protocol label in universe: {p!r}\n")
            sys.exit(2)
        if not pool.startswith("0x") or len(pool) != 42:
            sys.stderr.write(f"bad pool address in universe: {pool!r}\n")
            sys.exit(2)
        rows.append((pool, proto[p]))
rows.sort()
for pool, t in rows:
    print(f"{pool} {t}")
PY
}

# Assert a pools file matches the universe on (pool, poolType) exactly.
# Both sides are lowercased + sorted. Empty extra/missing → die with a summary.
golive_assert_pools_match_universe() {
  local pools_file="$1"
  local csv="${2:-$GOLIVE_DEFAULT_UNIVERSE}"
  [[ -f "$pools_file" ]] || golive_die "pools file missing: $pools_file"
  local expected actual
  expected="$(mktemp)"
  actual="$(mktemp)"
  # shellcheck disable=SC2064
  trap "rm -f '$expected' '$actual'" RETURN
  golive_pools_from_universe "$csv" | awk '{print tolower($1)" "$2}' | sort >"$expected"
  awk 'NF>=2 {print tolower($1)" "$2}' "$pools_file" | sort >"$actual"
  if ! cmp -s "$expected" "$actual"; then
    local only_exp only_act
    only_exp="$(comm -23 "$expected" "$actual" | wc -l | tr -d ' ')"
    only_act="$(comm -13 "$expected" "$actual" | wc -l | tr -d ' ')"
    echo "registry ⟷ universe mismatch for $pools_file vs $csv:" >&2
    echo "  only-in-universe: $only_exp" >&2
    echo "  only-in-pools:    $only_act" >&2
    echo "  first divergences (universe | pools):" >&2
    comm -3 "$expected" "$actual" | head -20 >&2
    golive_die "pool set must match data/pool_universe.csv on (pool, poolType)"
  fi
  local n
  n="$(wc -l <"$expected" | tr -d ' ')"
  echo "  registry ⟷ universe: $n pools match on (pool, poolType)"
}

# Assert every line of a canary/subset file lies in the universe intersection.
golive_assert_pools_subset_of_universe() {
  local subset_file="$1"
  local csv="${2:-$GOLIVE_DEFAULT_UNIVERSE}"
  [[ -f "$subset_file" ]] || golive_die "pools subset missing: $subset_file"
  local universe tmp
  universe="$(mktemp)"
  tmp="$(mktemp)"
  # shellcheck disable=SC2064
  trap "rm -f '$universe' '$tmp'" RETURN
  golive_pools_from_universe "$csv" | awk '{print tolower($1)" "$2}' | sort >"$universe"
  awk 'NF>=2 {print tolower($1)" "$2}' "$subset_file" | sort >"$tmp"
  local missing
  missing="$(comm -13 "$universe" "$tmp")"
  if [[ -n "$missing" ]]; then
    echo "canary pools not in universe intersection:" >&2
    echo "$missing" | head -20 >&2
    golive_die "canary pool set must be a subset of data/pool_universe.csv"
  fi
  local n
  n="$(wc -l <"$tmp" | tr -d ' ')"
  [[ "$n" -gt 0 ]] || golive_die "canary pool set is empty"
  echo "  canary pool set: $n pools ⊆ universe"
}

golive_write_pools_from_universe() {
  local out="$1"
  local csv="${2:-$GOLIVE_DEFAULT_UNIVERSE}"
  mkdir -p "$(dirname "$out")"
  golive_pools_from_universe "$csv" >"$out"
  echo "  wrote $out ($(wc -l <"$out" | tr -d ' ') pools) from $csv"
}
