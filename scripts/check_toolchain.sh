#!/usr/bin/env bash
# Verify installed rustc / forge / solc match repository pins in toolchain.toml.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TOOLCHAIN_FILE="${ROOT}/toolchain.toml"

die() {
  echo "error: $*" >&2
  exit 1
}

# Minimal TOML section reader for this repo's flat keys (no nested tables beyond one level).
read_pin() {
  local section="$1" key="$2"
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
  ' "$TOOLCHAIN_FILE"
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

echo "==> pinned versions"
echo "    rust:    $RUST_PIN"
echo "    solc:    $SOLC_PIN"
echo "    foundry: $FOUNDRY_PIN"

RUSTC_VER="$(rustc --version)"
echo "==> rustc --version: $RUSTC_VER"
# rustc 1.95.0 (hash date)
echo "$RUSTC_VER" | grep -Eq "rustc ${RUST_PIN//./\\.}($| )" \
  || die "rustc version mismatch: expected ${RUST_PIN}, got: ${RUSTC_VER}"

FORGE_VER="$(forge --version | head -n 1)"
echo "==> forge --version: $FORGE_VER"
# "forge Version: 1.7.1" or "forge 1.7.1"
echo "$FORGE_VER" | grep -Eq "${FOUNDRY_PIN//./\\.}" \
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
echo "$SOLC_VER" | grep -Eq "${SOLC_PIN//./\\.}" \
  || die "solc version mismatch: expected ${SOLC_PIN}, got: ${SOLC_VER}"

# Repo-visible Foundry pin must match too (defence in depth).
grep -Eq "solc_version\\s*=\\s*\"${SOLC_PIN//./\\.}\"" "${ROOT}/contracts/foundry.toml" \
  || die "contracts/foundry.toml missing solc_version = \"${SOLC_PIN}\""
grep -Eq "solc_version\\s*=\\s*\"${SOLC_PIN//./\\.}\"" "${ROOT}/contracts/executor/foundry.toml" \
  || die "contracts/executor/foundry.toml missing solc_version = \"${SOLC_PIN}\""

# Guard against regressing to host-specific absolute solc paths.
if grep -RIn --include='*.toml' --include='*.rs' --include='*.yml' --include='*.yaml' \
  -e '/opt/homebrew/bin/solc' -e '/usr/local/bin/solc' \
  "${ROOT}/build.rs" "${ROOT}/contracts" "${ROOT}/.github" 2>/dev/null; then
  die "host-specific solc absolute path still present in repo config"
fi

echo "==> toolchain check OK"
