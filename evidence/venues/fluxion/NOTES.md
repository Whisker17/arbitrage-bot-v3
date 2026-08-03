# Fluxion

## Search evidence
- `git grep -ni fluxion` → no matches
- `rg --no-ignore -ni fluxion data/ config/` → no matches
- No factory, deployer, pool, or Protocol tag in tracked seeds

## Verdict
**`not_found`**

No on-chain factory address was available to probe. Classification is relative to the operator seed set + repo corpus on Mantle mainnet (chain id 5000), not a full-chain factory crawl.

## bot_action
Ignore. If an operator later supplies a factory address, re-open classification.
