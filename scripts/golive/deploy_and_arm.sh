#!/usr/bin/env bash
# WHI-953: the combined deploy+fund+unpause path is retired.
#
# It crossed the WHI-547 / WHI-548 boundary (fund + unpause in the same
# invocation as deploy) and required hot/guardian private keys to register
# roles that only need addresses.
#
# Use the split launchers instead:
#   scripts/golive/deploy_only.sh       # WHI-547: paused + unfunded
#   scripts/golive/fund_and_canary.sh   # WHI-548: after second human approval
#   scripts/golive/run_signerless_shadow.sh  # shadow (no --enable-sends)
set -euo pipefail
echo "ABORT: scripts/golive/deploy_and_arm.sh is retired (WHI-953)." >&2
echo "  Deploy (paused + unfunded):  scripts/golive/deploy_only.sh --dry-run" >&2
echo "  Fund + canary (2nd approve): scripts/golive/fund_and_canary.sh --dry-run" >&2
echo "  Signerless shadow:           scripts/golive/run_signerless_shadow.sh" >&2
exit 1
