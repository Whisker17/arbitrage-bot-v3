#!/usr/bin/env bash
# Install the pinned solc via svm (CI helper; also usable locally).
#
# Usage: scripts/ci_install_solc.sh <solc-version>
#
# PATH propagation:
# - In GitHub Actions: writes the svm bin dir to $GITHUB_PATH so *later steps*
#   in the same job see `solc`. (A subprocess `export PATH=...` does not affect
#   the parent step or subsequent steps — GITHUB_PATH is the real mechanism.)
# - Within this process: PATH is updated so the trailing `solc --version` works.
# - Locally after the script exits: PATH is unchanged. Use:
#     eval "$(scripts/ci_install_solc.sh <ver> --print-path-export)"
#   Diagnostics go to stderr so only the export line is captured on stdout.
set -euo pipefail

SOLC_VER="${1:-}"
PRINT_PATH_EXPORT=0
if [[ "${2:-}" == "--print-path-export" ]]; then
  PRINT_PATH_EXPORT=1
fi

[[ -n "$SOLC_VER" ]] || {
  echo "usage: $0 <solc-version> [--print-path-export]" >&2
  exit 1
}

# Resolve the directory that holds the versioned solc binary (Linux + macOS svm layouts).
resolve_svm_dir() {
  local ver="$1" d
  for d in \
    "${HOME}/.svm/${ver}" \
    "${HOME}/Library/Application Support/svm/${ver}"
  do
    if [[ -x "${d}/solc-${ver}" || -x "${d}/solc" ]]; then
      printf '%s\n' "$d"
      return 0
    fi
  done
  return 1
}

# svm-rs installs versioned binaries as solc-<version>.
if ! command -v svm >/dev/null 2>&1; then
  cargo install --locked svm-rs --version 0.5.17 || cargo install svm-rs
fi

# Install if missing. `svm install` can exit non-zero with "not a terminal"
# after reporting "already installed" when stdout is not a TTY — tolerate that
# when the binary is already present. Avoid `svm use` (same TTY issue).
#
# Intentionally do NOT write svm's machine-global `.global-version` — that would
# clobber other projects on a developer machine. We put the versioned binary
# dir first on PATH instead (and check_toolchain prefers solc-<ver> too).
if ! resolve_svm_dir "$SOLC_VER" >/dev/null; then
  svm install "$SOLC_VER" >&2 || true
fi

SVM_DIR="$(resolve_svm_dir "$SOLC_VER" || true)"
if [[ -z "${SVM_DIR}" ]]; then
  # Last attempt: force install even if the previous one complained about TTY.
  svm install "$SOLC_VER" >&2 || true
  SVM_DIR="$(resolve_svm_dir "$SOLC_VER" || true)"
fi
if [[ -z "${SVM_DIR}" ]]; then
  echo "error: solc ${SOLC_VER} not found under ~/.svm or ~/Library/Application Support/svm after install" >&2
  exit 1
fi

# Ensure a plain `solc` name exists next to solc-<version> (local to this pin dir).
if [[ -x "${SVM_DIR}/solc-${SOLC_VER}" && ! -e "${SVM_DIR}/solc" ]]; then
  ln -sf "solc-${SOLC_VER}" "${SVM_DIR}/solc"
fi

# GitHub Actions: persist for later steps in this job.
if [[ -n "${GITHUB_PATH:-}" ]]; then
  echo "$SVM_DIR" >> "$GITHUB_PATH"
fi

# In-process only: put the *versioned* binary dir first so we do not hit a
# cargo/bin/solc proxy. Does not affect the caller's shell (see --print-path-export).
PATH="${SVM_DIR}:${PATH}"
export PATH

command -v solc >/dev/null 2>&1 || {
  echo "error: solc not on PATH after svm install ${SOLC_VER} (dir=${SVM_DIR})" >&2
  exit 1
}

# Diagnostics on stderr so `eval "$(... --print-path-export)"` only sees the export line.
echo "==> solc --version (${SVM_DIR})" >&2
solc --version >&2

if [[ "$PRINT_PATH_EXPORT" -eq 1 ]]; then
  # For local: eval "$(./scripts/ci_install_solc.sh 0.8.26 --print-path-export)"
  # Only this line is on stdout (safe to eval).
  printf 'export PATH=%q:${PATH}\n' "$SVM_DIR"
fi
