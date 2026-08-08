#!/usr/bin/env bash
# Size-based segment rotation + total-bytes retention (WHI-952 / G-5).
#
# Mirrors src/ops/rotating_file.rs:
#   active stays at $1; rotated as $1.1, $1.2, … (higher N = older).
#   On rotate: shift N → N+1, active → .1, create empty active.
#   Retention deletes highest N until total size ≤ max_total.
#
# Usage (source from other scripts):
#   . scripts/lib/rotate_by_size.sh
#   rotate_by_size /path/to/live.log 67108864 536870912
#
# Or force-check after a write:
#   ensure_under_cap /path/to/live.log "$LOG_MAX_SEGMENT_BYTES" "$LOG_MAX_TOTAL_BYTES"

set -euo pipefail

# list_rotated_ns <active_path> → prints N values ascending
_list_rotated_ns() {
  local active="$1"
  local dir base
  dir="$(dirname -- "$active")"
  base="$(basename -- "$active")"
  # shellcheck disable=SC2012
  ls -1 "$dir" 2>/dev/null \
    | sed -n "s/^$(printf '%s' "$base" | sed 's/[.[\*^$()+?{|]/\\&/g')\\.\\([0-9][0-9]*\\)$/\\1/p" \
    | sort -n
}

_file_size() {
  local f="$1"
  if [[ -f "$f" ]]; then
    # portable: prefer stat -f (BSD) then stat -c (GNU)
    if stat -f%z "$f" >/dev/null 2>&1; then
      stat -f%z "$f"
    else
      stat -c%s "$f"
    fi
  else
    echo 0
  fi
}

total_bytes_for_path() {
  local active="$1"
  local total n
  total="$(_file_size "$active")"
  while IFS= read -r n; do
    [[ -z "$n" ]] && continue
    total=$(( total + $(_file_size "${active}.${n}") ))
  done < <(_list_rotated_ns "$active")
  echo "$total"
}

# rotate_active_file <active_path>
# Shifts existing .N up by one, renames active → .1. Active path is removed.
rotate_active_file() {
  local active="$1"
  [[ -f "$active" ]] || return 1
  local n
  # high → higher first
  while IFS= read -r n; do
    [[ -z "$n" ]] && continue
    mv -f -- "${active}.${n}" "${active}.$((n + 1))"
  done < <(_list_rotated_ns "$active" | sort -nr)
  mv -f -- "$active" "${active}.1"
}

# apply_retention <active_path> <max_total_bytes>
# Deletes oldest rotated segments until total ≤ max_total. Never deletes active.
apply_retention() {
  local active="$1"
  local max_total="$2"
  local n total
  while :; do
    total="$(total_bytes_for_path "$active")"
    if (( total <= max_total )); then
      break
    fi
    n="$(_list_rotated_ns "$active" | tail -1)"
    if [[ -z "$n" ]]; then
      break
    fi
    rm -f -- "${active}.${n}"
  done
}

# rotate_by_size <active_path> <max_segment_bytes> <max_total_bytes>
# If active size ≥ max_segment, rotate + retain. Returns 0 always (best-effort).
rotate_by_size() {
  local active="$1"
  local max_seg="$2"
  local max_total="$3"
  local size
  size="$(_file_size "$active")"
  if (( size >= max_seg && size > 0 )); then
    rotate_active_file "$active"
    : >"$active"
  fi
  apply_retention "$active" "$max_total"
}

# ensure_under_cap — alias used by run_live.sh after each restart.
ensure_under_cap() {
  rotate_by_size "$@"
}
