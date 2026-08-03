# Agni V2 (label) — alias of FusionX V2

## Finding
There is **no** separate Agni-branded V2 factory on Mantle in committed seeds:

- `data/poolLists.csv` Protocol tags: only `Agni` and `FusionX`, and both use V3-style fee tiers on those rows
- `config/gas_profiles/approved_pools.mantle_mainnet.json` V2 entry is FusionX V2
- `src/bin/universe_gen.rs` documents that bot `SelectedProtocol::AgniV2` uses **FusionX V2 factory** as an **interim** venue until WHI-765
- Agni factory `0x25780dc8…` is V3-only (`feeAmountTickSpacing` works; `allPairsLength` reverts)

## Verdict
**Alias of FusionX V2** — not an independent venue row for factory enumeration.

## bot_action
Retire the interim “Agni V2 = FusionX V2” naming in a follow-up identity fix (out of scope for this research issue’s code changes). Matrix row kept for explicit alias.
