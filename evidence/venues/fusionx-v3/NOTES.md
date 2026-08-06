# FusionX V3

## Identity
- **Family:** Uniswap V3–style CL (FusionX fork; Agni-compatible ABI surface)
- **Factory:** `0x530d2766D1988CC1c000C8b7d00334c14B69AD71`
- **CREATE2 deployer (poolDeployer):** `0x8790c2C3BA67223D83C8FCF2a5E3C650059987b4`
- **init_code_hash:** `0x1bce652aaa6528355d7a339037433a20cd28410e3967635ba8d2ddb037440dbf`
- **Sample pool:** `0xD3d3127D9654f806370da592eb292eA0a347f0e3` (WMNT/WETH fee=2500)
- CSV `Protocol=FusionX` rows (e.g. `0x8A6A1ED0…`) resolve to this factory with `fee()` matching CSV fee tier.

## On-chain evidence (pinned block 98797253)
- `factory.poolDeployer()` → deployer above
- `pool.factory()` → FusionX V3 factory
- `pool.fee()` / `token0` / `token1` / `slot0` / `liquidity` succeed (same ABI as Agni)
- CREATE2 reproduction matches sample pool
- Swap topic0 shared with UniV3/Agni (`0xc42079f9…`)
- **Different** init_code_hash and deployer from Agni — not the same factory; same *interface family*

## Bot status
- Differential fixture exists (`uniswap_v3_fusionx_wmnt_weth_2500.json`)
- Live bot V3 seed applies `.with_protocol_filter("agni")` → **FusionX V3 rows excluded**
- Not a `SelectedProtocol`

## Verdict
**`drop_in_univ3_or_agni`** (ABI/event/math surface)

## bot_action
Discovery-only / non-executable until an explicit FusionX V3 (or multi-factory Agni-compatible) identity is wired. Safe to list as a **second-pass** factory for WHI-536 once product wants it; do not silently merge into Agni factory enumeration (different CREATE2 domain).

## Census expansion (2026-08-06, block 98950889)

### Census `kind` is wrong
`pool_census.json` tags every FusionX V3 pool as `kind: "algebra"`. Live accessors contradict that:

| accessor | `0x262255f4…` (USDT/WMNT, top uncovered arb pool) | Agni control `0x1858d52c…` |
| --- | --- | --- |
| `slot0()` | OK | OK |
| `globalState()` | MISSING | MISSING |
| `liquidity()` | OK | OK |
| `tickSpacing()` | 10 | 10 |
| `fee()` | 500 | 500 |
| `factory()` | `0x530d2766…` | `0x25780dc8…` |
| `slot0.feeProtocol` | 222825800 (uint32 width) | 222825800 |

`feeProtocol = 222825800` does not fit `uint8`; matches the note at `tests/differential.rs` that FusionX/Pancake-style V3 forks use **uint32 feeProtocol**. The bot's existing Agni/V3 reader already handles that width (control pool returns the identical value).

**Confirmed verdict remains `drop_in_univ3_or_agni`.** Missing piece is venue identity + factory registration, not math.

### bot_action (updated)
**Promote from discovery-only to first-pass enumerate** for WHI-536 / universe growth: this factory alone accounts for 5,609 arb legs in the 30d set and the single highest-marginal pool (`0x262255f4…`). Still requires multi-factory V3 identity (do not merge CREATE2 domain into Agni).
