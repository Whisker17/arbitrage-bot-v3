#!/usr/bin/env bash
# Read one pin from toolchain.toml (or another flat TOML of the same shape).
# Usage: read_toolchain_pin.sh <section> <key> [path-to-toml]
# Prints the unquoted string value to stdout. Exit 1 if missing.
set -euo pipefail

SECTION="${1:-}"
KEY="${2:-}"
FILE="${3:-}"

if [[ -z "$SECTION" || -z "$KEY" ]]; then
  echo "usage: $0 <section> <key> [path-to-toml]" >&2
  exit 1
fi

if [[ -z "$FILE" ]]; then
  ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
  FILE="${ROOT}/toolchain.toml"
fi

[[ -f "$FILE" ]] || {
  echo "error: missing $FILE" >&2
  exit 1
}

VALUE="$(
  awk -v section="[$SECTION]" -v key="$KEY" '
    $0 == section { in_section = 1; next }
    /^\[/ { in_section = 0 }
    in_section && $1 == key {
      if (match($0, /"[^"]+"/)) {
        print substr($0, RSTART + 1, RLENGTH - 2)
        exit
      }
    }
  ' "$FILE"
)"

if [[ -z "$VALUE" ]]; then
  echo "error: missing [${SECTION}].${KEY} in $FILE" >&2
  exit 1
fi

printf '%s\n' "$VALUE"
