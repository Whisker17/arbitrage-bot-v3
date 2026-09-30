# Competitor watchlist (WHI-1581)

`mantle_top_arbitrageurs.json` pins the Mantle arbitrage senders we compare against.
Peer-comparison work fetches **these addresses'** arbs directly, instead of
rediscovering bots on every run.

## What is in it

- **Members.** 29 sender addresses:
  - the **top 5 senders** by 30-day qualified-arb count (`group: "individual"`, `selection.top_n`);
  - **24 other senders** matching the behavioural group rule `suspected_operator_A` (`selection.group_rule`).
- **Profile per member.** The 30-day and 7-day arb counts, first and last seen, then the top-4 hop counts, protocol-family hop mixes, DEX routes and settlement assets. Also the tx types, the effective-tip and tx-index percentiles, and the gas-used p50.
- **Coverage.** Together the members account for **9,684 of 10,169** qualified arbs (95.2%) in 2026-08-31 → 2026-09-30, and **1,921 of 1,994** in the last 7 days.

The group rule is the complete predicate stored in the file. A sender joins the group only if **all** of these hold:
- it is not in the top 5;
- it has 200–270 arbs in 30 days (the selection band; members actually fall between 220 and 266);
- it sent only Legacy transactions;
- it settled only in WMNT;
- its effective-tip p50 is 1.5–3.0 gwei (members: 1.88–2.85);
- its dominant protocol-family hop mix is `v3>v3`.

Tx index is deliberately **not** a criterion: members' p50 ranges 1.0–2.0, and an earlier draft that required exactly 2 wrongly excluded 4 matching senders. The rule selects exactly these 24 among all 64 senders of the recorded execution. The group is an **inference from behaviour only**: it is not an identity or ownership claim, and the Dune pack does no address clustering.

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

1. **Fetch a member's arbs.** Run `02_arb_detail_feed` (8781229) for the comparison window, with `bot_address_filter` = the member's lower-case address. Without a filter, keep the rows whose sender is in this file.
2. **Convert to collector events.** `peer_attribution --events` reads the `ground_truth_collector` event file, **not** Dune CSV. Rename the export's `executor_address` column to `to` (or `executor`) and run `ground_truth_collector collect`. The steps are in `scripts/dunesql/README.md` § Collector compatibility.
3. **Six-way attribution against our shadow ledger.** Follow the reproduction recipe in `evidence/peer-attribution/whi-1412/STATUS.md` § 8 Reproduce. It includes the ledger day cut, `--gas-profile` and the `--pre-state` mode. Tool reference: `evidence/peer-attribution/README.md`.
