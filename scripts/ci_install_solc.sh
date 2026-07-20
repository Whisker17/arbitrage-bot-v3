#!/usr/bin/env bash
# Install the pinned solc via svm and put `solc` on PATH (CI helper).
# Usage: scripts/ci_install_solc.sh <solc-version>
# Reads nothing from the ambient env except HOME; version is always explicit.
set -euo pipefail

SOLC_VER="${1:-}"
[[ -n "$SOLC_VER" ]] || {
  echo "usage: $0 <solc-version>" >&2
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
  export PATH="${SVM_DIR}:${PATH}"
fi

command -v solc >/dev/null 2>&1 || {
  echo "error: solc not on PATH after svm install ${SOLC_VER}" >&2
  exit 1
}

echo "==> solc --version"
solc --version
