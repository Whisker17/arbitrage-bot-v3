#!/usr/bin/env bash
# Verify installed rustc / forge / solc match repository pins in toolchain.toml,
# and that mirrored pin files (rust-toolchain.toml, foundry.toml) stay in sync.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TOOLCHAIN_FILE="${ROOT}/toolchain.toml"

die() {
  echo "error: $*" >&2
  exit 1
}

# Minimal TOML section reader for this repo's flat keys (no nested tables beyond one level).
read_pin() {
  local section="$1" key="$2" file="${3:-$TOOLCHAIN_FILE}"
  awk -v section="[$section]" -v key="$key" '
    $0 == section { in_section = 1; next }
    /^\[/ { in_section = 0 }
    in_section && $1 == key {
      # value = "..."
      if (match($0, /"[^"]+"/)) {
        print substr($0, RSTART + 1, RLENGTH - 2)
        exit
      }
    }
  ' "$file"
}

# Escape a version string for use as a literal in ERE (dots only — pins are dotted versions).
ere_literal() {
  printf '%s' "$1" | sed 's/\./\\./g'
}

# Match exact version token: not a superstring like 11.7.1 or 0.8.260.
version_matches() {
  local haystack="$1" version="$2"
  local lit
  lit="$(ere_literal "$version")"
  # Allow common prefixes/suffixes around an exact version token.
  echo "$haystack" | grep -Eq "(^|[^0-9.])${lit}([^0-9.]|$)"
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "'$1' not found on PATH"
}

[[ -f "$TOOLCHAIN_FILE" ]] || die "missing $TOOLCHAIN_FILE"

RUST_PIN="$(read_pin rust version)"
SOLC_PIN="$(read_pin solidity version)"
FOUNDRY_PIN="$(read_pin foundry version)"

[[ -n "$RUST_PIN" ]] || die "missing [rust].version in toolchain.toml"
[[ -n "$SOLC_PIN" ]] || die "missing [solidity].version in toolchain.toml"
[[ -n "$FOUNDRY_PIN" ]] || die "missing [foundry].version in toolchain.toml"

require_cmd rustc
require_cmd forge

echo "==> pinned versions (toolchain.toml)"
echo "    rust:    $RUST_PIN"
echo "    solc:    $SOLC_PIN"
echo "    foundry: $FOUNDRY_PIN"

# --- Mirrored pin files must match toolchain.toml (static SSOT checks) ---

RUST_TOOLCHAIN="${ROOT}/rust-toolchain.toml"
[[ -f "$RUST_TOOLCHAIN" ]] || die "missing rust-toolchain.toml"
RUST_CHANNEL="$(awk -F'"' '/^channel[[:space:]]*=/ { print $2; exit }' "$RUST_TOOLCHAIN")"
[[ -n "$RUST_CHANNEL" ]] || die "rust-toolchain.toml missing channel"
[[ "$RUST_CHANNEL" == "$RUST_PIN" ]] \
  || die "rust-toolchain.toml channel=${RUST_CHANNEL} != toolchain.toml [rust].version=${RUST_PIN}"
echo "==> rust-toolchain.toml channel matches ${RUST_PIN}"

for foundry_toml in \
  "${ROOT}/contracts/foundry.toml" \
  "${ROOT}/contracts/executor/foundry.toml"
do
  [[ -f "$foundry_toml" ]] || die "missing $foundry_toml"
  # Accept either solc_version = "x" or solc = "x" (version, not path).
  if ! grep -Eq "solc(_version)?[[:space:]]*=[[:space:]]*\"$(ere_literal "$SOLC_PIN")\"" "$foundry_toml"; then
    die "$(basename "$(dirname "$foundry_toml")")/$(basename "$foundry_toml") missing solc_version = \"${SOLC_PIN}\""
  fi
done
echo "==> foundry.toml solc_version pins match ${SOLC_PIN}"

# build.rs must not hardcode a second solc pin. solc is owned by
# contracts/foundry.toml + toolchain.toml only (no const / no --use).
if grep -nE 'const[[:space:]]+SOLC_VERSION|\.arg\("--use"\)' "${ROOT}/build.rs"; then
  die "build.rs must not hardcode solc version/--use; rely on contracts/foundry.toml solc_version"
fi
# Explicit: no absolute host solc paths in build.rs / foundry / CI config.
if grep -RIn --include='*.toml' --include='*.rs' --include='*.yml' --include='*.yaml' \
  -e '/opt/homebrew/bin/solc' -e '/usr/local/bin/solc' \
  "${ROOT}/build.rs" "${ROOT}/contracts" "${ROOT}/.github" 2>/dev/null; then
  die "host-specific solc absolute path still present in repo config"
fi
echo "==> build.rs does not hardcode solc pin or host path"

# --- Runtime tool versions ---

RUSTC_VER="$(rustc --version)"
echo "==> rustc --version: $RUSTC_VER"
version_matches "$RUSTC_VER" "$RUST_PIN" \
  || die "rustc version mismatch: expected ${RUST_PIN}, got: ${RUSTC_VER}"

FORGE_VER="$(forge --version | head -n 1)"
echo "==> forge --version: $FORGE_VER"
version_matches "$FORGE_VER" "$FOUNDRY_PIN" \
  || die "forge version mismatch: expected ${FOUNDRY_PIN}, got: ${FORGE_VER}"

# Resolve solc binary: PATH name `solc`, or svm-installed `solc-<version>`.
SOLC_BIN=""
if command -v solc >/dev/null 2>&1; then
  SOLC_BIN="$(command -v solc)"
else
  for candidate in \
    "${HOME}/.svm/${SOLC_PIN}/solc-${SOLC_PIN}" \
    "${HOME}/.svm/${SOLC_PIN}/solc" \
    "${HOME}/Library/Application Support/svm/${SOLC_PIN}/solc-${SOLC_PIN}" \
    "${HOME}/Library/Application Support/svm/${SOLC_PIN}/solc"
  do
    if [[ -x "$candidate" ]]; then
      SOLC_BIN="$candidate"
      break
    fi
  done
fi

if [[ -z "$SOLC_BIN" ]]; then
  die "solc ${SOLC_PIN} not found on PATH or under svm (install via: svm install ${SOLC_PIN} && svm use ${SOLC_PIN})"
fi

SOLC_VER="$("$SOLC_BIN" --version | tr '\n' ' ')"
echo "==> solc --version ($SOLC_BIN): $SOLC_VER"
version_matches "$SOLC_VER" "$SOLC_PIN" \
  || die "solc version mismatch: expected ${SOLC_PIN}, got: ${SOLC_VER}"

echo "==> toolchain check OK"
