# WMNT storage-layout attestation — Mantle mainnet

Address: `0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8`

## Balance mapping slot

`balanceOf` mapping base slot: `0`.

Empirically discovered (not assumed) by brute-forcing candidate slots `0..20` with a
magic-value `--override-state-diff` probe against live Mantle mainnet and observing
which candidate's override was reflected back by a real `balanceOf` call:

```text
cast call 0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8 "balanceOf(address)(uint256)" \
  0x00000000000000000000000000000000deadbeef --rpc-url https://rpc.mantle.xyz \
  --override-state-diff 0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8:<slot(holder,0)>:<magic>
```

Candidate `0` matched exactly (consistent with a canonical WETH9-style layout, balance
mapping as the first declared storage variable). This is the same probe documented at
`src/execution/mainnet_fork_harness.rs`'s `WMNT_BALANCE_SLOT` constant; this document is
the named, digested attestation record that `WmntStorageShape::Direct::verified_artifact`
pins, so the two independently-maintained sources (this file and that constant) can be
cross-checked at startup by `wmnt_descriptor::check_wmnt_balance_slot_drift`.

## Verification status (honest disclosure)

**This is a live-probe result, not a source-code-verified layout.** WMNT has no
first-party Solidity source vendored into this repo's `contracts/` directory, so there is
no compiled `BuildEvidence`-style `storageLayout` to derive this slot from (contrast
`runtime_identity::BuildEvidence`, used for `ArbitrageExecutor`'s own storage layout).

An attempt to independently verify WMNT's storage layout against a public block-explorer
source (Mantlescan, Blockscout, Etherscan's v2 multi-chain API) was made and did not
succeed in this environment: Mantlescan's contract page returned HTTP 403, Blockscout's
smart-contract API returned HTTP 502, Mantlescan's legacy `getsourcecode` endpoint is
deprecated, and Etherscan's v2 API requires an API key not configured here. No mainnet RPC
endpoint is configured in this repo either (only `MANTLE_SEPOLIA_RPC_URL`), so a live
`eth_getCode`-based codehash cross-check was also not possible.

`runtime_codehash` in the committed descriptor is therefore `0x0…0` — an explicit "not yet
independently verified" sentinel, not a real (and therefore falsely-attested) codehash.
Closing this out for real requires either a working block-explorer API key or direct
access to WMNT's verified source, neither available in this session.
