#!/usr/bin/env bash
# Dispatch a prompt to a named agent role, in a fresh context, from any runtime.
#
# Skills name a ROLE (ORCHESTRATOR, IMPLEMENTER, REVIEWER) and a semantic effort
# (medium | high), never a model. The role -> runtime/model/effort mapping lives
# only in config/agent-roles.conf; this script translates it into the runtime's
# own flags. Contract: docs/agents/runtime.md.
#
# Usage:
#   scripts/agent-dispatch.sh <ROLE> <prompt-file|-> --effort <medium|high>
#   scripts/agent-dispatch.sh --probe [ROLE ...]
#
# The dispatched process inherits the caller's working directory; its result is
# the runtime's stdout.
#
# Exit codes:
#   0  success
#   2  usage error (unknown role, unknown/missing effort, missing prompt file)
#   3  ROLE UNAVAILABLE -- runtime, model or effort mapping not configured, unknown
#      runtime, or binary not on PATH. Never substituted: no other model, no
#      lower effort.
#   *  otherwise the dispatched runtime's own exit code (auth and call failures
#      included), unchanged and not retried.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONF="${AGENT_ROLES_CONF:-$REPO_ROOT/config/agent-roles.conf}"
ROLES="ORCHESTRATOR IMPLEMENTER REVIEWER"
RUNTIMES="claude codex pi"
USAGE="usage: agent-dispatch.sh <ROLE> <prompt-file|-> --effort <medium|high> | --probe [ROLE ...]"

usage() { echo "agent-dispatch: $*" >&2; exit 2; }
unavailable() { echo "agent-dispatch: $*" >&2; return 3; }

[ -f "$CONF" ] || usage "no role mapping at $CONF (see docs/agents/runtime.md)"
# shellcheck disable=SC1090
. "$CONF"

var() { eval "printf '%s' \"\${$1:-}\""; }

# A model or effort value is one plain token: no whitespace, quotes or leading dash,
# so it can never be read as an extra flag or break out of the codex -c value.
plain_value() {
    local value
    value="$(var "$2")"
    case "$value" in
        -*|*[!A-Za-z0-9._/:@+=-]*)
            unavailable "$1: $2 must be a single plain token, got '$value'"
            return 3 ;;
    esac
}

effort_key() {
    case "$2" in
        medium) printf '%s_EFFORT_MEDIUM' "$1" ;;
        high) printf '%s_EFFORT_HIGH' "$1" ;;
    esac
}

known_role() {
    case " $ROLES " in *" $1 "*) return 0 ;; esac
    usage "unknown role '$1' (known: $ROLES)"
}

# Check a role's static configuration. With an effort, also check that effort's
# mapping. Prints nothing; returns 3 with a reason on stderr.
check_role() {
    local role="$1" effort="${2:-}" runtime model
    runtime="$(var "${role}_RUNTIME")"
    model="$(var "${role}_MODEL")"
    [ -n "$runtime" ] || { unavailable "$role: ${role}_RUNTIME is unset in $CONF"; return 3; }
    case " $RUNTIMES " in
        *" $runtime "*) ;;
        *) unavailable "$role: unknown runtime '$runtime' (known: $RUNTIMES)"; return 3 ;;
    esac
    [ -n "$model" ] || { unavailable "$role: ${role}_MODEL is unset in $CONF"; return 3; }
    if [ "$runtime" = "pi" ] && [ "${model#*:}" != "$model" ]; then
        unavailable "$role: pi model '$model' embeds a thinking level; use ${role}_EFFORT_*"
        return 3
    fi
    if [ -n "$effort" ]; then
        local key
        key="$(effort_key "$role" "$effort")"
        [ -n "$(var "$key")" ] || {
            unavailable "$role: effort '$effort' is not supported ($key is unset in $CONF)"
            return 3
        }
        plain_value "$role" "$key" || return 3
    fi
    plain_value "$role" "${role}_MODEL" || return 3
    # The argv is fixed by the adapter below: no operator flags can override the
    # model, the effort or the fresh session. Refuse keys that used to allow it.
    local legacy
    for legacy in "${role}_EXTRA_ARGS" "${role}_CMD"; do
        [ -z "$(var "$legacy")" ] || {
            unavailable "$role: $legacy is not supported; configure permissions or sandbox in the runtime's own config (docs/agents/runtime.md)"
            return 3
        }
    done
    command -v "$runtime" >/dev/null 2>&1 || {
        unavailable "$role: '$runtime' is not on PATH"
        return 3
    }
}

# --- probe mode: static configuration and PATH only --------------------------
if [ "${1:-}" = "--probe" ]; then
    shift
    targets="${*:-$ROLES}"
    for role in $targets; do known_role "$role"; done
    failed=0
    for role in $targets; do
        efforts=""
        for e in medium high; do
            if check_role "$role" "$e" 2>/dev/null; then efforts="$efforts $e"; fi
        done
        if reason="$(check_role "$role" 2>&1)" && [ -n "$efforts" ]; then
            echo "ok        $role -> $(var "${role}_RUNTIME") $(var "${role}_MODEL") (effort:$efforts)"
        else
            [ -n "$reason" ] || reason="no effort mapping configured"
            echo "UNUSABLE  $role -- ${reason#agent-dispatch: }"
            failed=1
        fi
    done
    if [ -n "$(var IMPLEMENTER_MODEL)" ] && \
       [ "$(var IMPLEMENTER_MODEL)" = "$(var REVIEWER_MODEL)" ]; then
        echo "note: IMPLEMENTER and REVIEWER use the same model -- a different" \
             "vendor catches more (docs/agents/runtime.md)."
    fi
    echo "static check only: not an authentication or real-call result" \
         "(docs/agents/runtime.md § Preflight)."
    [ "$failed" -eq 0 ] || exit 3
    exit 0
fi

# --- dispatch mode -----------------------------------------------------------
[ "$#" -eq 4 ] && [ "$3" = "--effort" ] || usage "$USAGE"
ROLE="$1"; PROMPT_SRC="$2"; EFFORT="$4"
known_role "$ROLE"
case "$EFFORT" in
    medium|high) ;;
    *) usage "unknown effort '$EFFORT' (known: medium high)" ;;
esac
if [ "$PROMPT_SRC" != "-" ] && [ ! -f "$PROMPT_SRC" ]; then
    usage "prompt file not found: $PROMPT_SRC"
fi

check_role "$ROLE" "$EFFORT" || exit 3

RUNTIME="$(var "${ROLE}_RUNTIME")"
MODEL="$(var "${ROLE}_MODEL")"
NATIVE_EFFORT="$(var "$(effort_key "$ROLE" "$EFFORT")")"
case "$RUNTIME" in
    claude) ARGV=(claude -p --model "$MODEL" --effort "$NATIVE_EFFORT") ;;
    codex)  ARGV=(codex exec --model "$MODEL" -c "model_reasoning_effort=\"$NATIVE_EFFORT\"") ;;
    pi)     ARGV=(pi -p --model "$MODEL" --thinking "$NATIVE_EFFORT") ;;
esac

if [ "$RUNTIME" = "pi" ]; then
    # pi takes the prompt as an @file argument, not on stdin.
    if [ "$PROMPT_SRC" = "-" ]; then
        spool="$(mktemp)"
        cat > "$spool"
        rc=0
        "${ARGV[@]}" "@$spool" < /dev/null || rc=$?
        rm -f "$spool"
        exit "$rc"
    fi
    exec "${ARGV[@]}" "@$PROMPT_SRC" < /dev/null
fi

[ "$RUNTIME" = "codex" ] && ARGV+=(-)
if [ "$PROMPT_SRC" = "-" ]; then
    exec "${ARGV[@]}"
fi
exec "${ARGV[@]}" < "$PROMPT_SRC"
