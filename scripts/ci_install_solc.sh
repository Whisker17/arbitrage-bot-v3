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
#     export PATH="$HOME/.svm/<ver>:$PATH"
#   or re-run with:  eval "$(scripts/ci_install_solc.sh <ver> --print-path-export)"
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

# svm-rs installs versioned binaries as solc-<version>.
if ! command -v svm >/dev/null 2>&1; then
  cargo install --locked svm-rs --version 0.5.17 || cargo install svm-rs
fi

svm install "$SOLC_VER"
svm use "$SOLC_VER"

SVM_DIR="${HOME}/.svm/${SOLC_VER}"
if [[ -d "$SVM_DIR" ]]; then
  # GitHub Actions: persist for later steps in this job.
  if [[ -n "${GITHUB_PATH:-}" ]]; then
    echo "$SVM_DIR" >> "$GITHUB_PATH"
  fi
  if [[ -x "${SVM_DIR}/solc-${SOLC_VER}" && ! -e "${SVM_DIR}/solc" ]]; then
    ln -sf "solc-${SOLC_VER}" "${SVM_DIR}/solc"
  fi
  # In-process only (this script's own solc --version). Does not affect the caller.
  PATH="${SVM_DIR}:${PATH}"
  export PATH
fi

command -v solc >/dev/null 2>&1 || {
  echo "error: solc not on PATH after svm install ${SOLC_VER}" >&2
  exit 1
}

echo "==> solc --version"
solc --version

if [[ "$PRINT_PATH_EXPORT" -eq 1 ]]; then
  # For local: eval "$(./scripts/ci_install_solc.sh 0.8.26 --print-path-export)"
  printf 'export PATH=%q:${PATH}\n' "$SVM_DIR"
fi
