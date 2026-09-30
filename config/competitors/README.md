# Competitor watchlist (WHI-1581)

`mantle_top_arbitrageurs.json` pins the Mantle arbitrage senders we compare against.
Peer-comparison work fetches **these addresses'** arbs directly, instead of
rediscovering bots on every run.

## What is in it

- **Members.** 25 sender addresses:
  - the **top 5 senders** by 30-day qualified-arb count (`group: "individual"`);
  - **20 senders in one behavioural group**, `suspected_operator_A`.
- **Profile per member.** The 30-day and 7-day arb counts, first and last seen, then the top-4 hop counts, protocol-family hop mixes, DEX routes and settlement assets. Also the tx types, the effective-tip and tx-index percentiles, and the gas-used p50.
- **Coverage.** Together the members account for 8,734 of 10,169 qualified arbs (85.9%) in 2026-08-31 → 2026-09-30.

`suspected_operator_A` is an **inference from behaviour only**. Its members share:
- ~220–270 arbs each;
- all Legacy transactions;
- a tip p50 of 1.9–2.9 gwei;
- a tx index p50 of 2;
- 100% WMNT settlement;
- a `v3>v3`-dominant, Agni/FusionX route mix.

It is not an identity or ownership claim. The Dune pack does no address clustering.

## Where the numbers come from

All numbers come from the WHI-1406 Dune pack (`scripts/dunesql/`, private saved queries).
The exact execution ids are recorded in the file under `source`:

| field | query | window (UTC, half-open) | execution |
|---|---|---|---|
| profile, 30-day counts | `03_bot_strategy_profile` (8781231) | 2026-08-31 00:00 → 2026-09-30 00:00 | `01M3S6FZ2009AAZF3E2433X7P5` |
| 7-day counts | `01_discover_bots` (8781227) | 2026-09-23 00:00 → 2026-09-30 00:00 | `01M3S6FZG0766CD431FN5NM2ZG` |

Both queries read the shared qualification backbone `00_qualified_arbs` (8781215). The qualification rules are in `scripts/dunesql/README.md`.

## Refreshing it

1. Re-run `03` for the new 30-day window and `01` for the new 7-day window. Both take `start_time`, `end_time` and `from_block`/`to_block` = `-1`.
2. Apply the rule stored in the file under `selection`: the top 5 by `arb_tx_count`, plus every sender matching `group_rule`. Adjust the rule if the group's footprint moves, and say so in the PR.
3. Update `source`, `window_totals`, `coverage` and `members`.
4. `cargo test --locked --test competitor_watchlist` checks:
   - the addresses are well formed, lower-case and unique;
   - the required counts are present;
   - group members satisfy the group rule;
   - the coverage totals agree.

## Using it

To fetch a member's arbs, run `02_arb_detail_feed` (8781229) with `bot_address_filter` = the member's lower-case address, for the comparison window. For a UTC day's peer comparison against our shadow ledger, feed the result to `peer_attribution` (`evidence/peer-attribution/README.md`).
