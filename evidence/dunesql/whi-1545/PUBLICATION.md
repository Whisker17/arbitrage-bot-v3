# Dune publication of the protocol-family `hop_mix` (2026-09-29)

This file summarizes the external publication record. The raw payloads stay outside the repo and are
referenced here by path and sha256.

- **Record directory** (`<PUB>` below): `…/subagent-artifacts/outputs/517c23bd-e4d2-42c1-9d5a-7f8a0f97ed2a/release-022/dune-publish/`
- **Record:** `<PUB>/PUBLICATION-RECORD.md`, which includes the read-only visibility addendum.
- **Hash list:** `<PUB>/SHA256SUMS`, with sha256 `4db2b9aa4dbd814eb4f73419eae3026f886a8f03352758cee23deaa95864a1e1`. All 16 entries verify with `shasum -a 256 -c SHA256SUMS`.
- **Authorization:** the owner's structured choice "更新 SQL，保留私有" (update SQL, keep private).
- **Scope:** an SQL-only update of 8781215 and then 8781229 (team 12632). Visibility, title, parameters and tags were preserved. 8781227, 8781231 and the dashboard were not modified.

## Versions and visibility

| query | file | before | after | is_private | evidence (in `<PUB>`) |
| --- | --- | --- | --- | --- | --- |
| 8781215 | `00_qualified_arbs.sql` | v11 | **v12** | true | `backup-8781215-v11.json`, `readback-8781215-v12.json` |
| 8781227 | `01_discover_bots.sql` | v4 (unchanged SQL) | v4 | true | `readonly-8781227-v4.json` (read-only check) |
| 8781229 | `02_arb_detail_feed.sql` | v4 | **v5** | true | `backup-8781229-v4.json`, `readback-8781229-v5.json` |
| 8781231 | `03_bot_strategy_profile.sql` | v7 (unchanged SQL) | v7 | true | `readonly-8781231-v7.json` (read-only check) |

- All four saved queries are private by owner decision (2026-09-29).
- All four have `is_temp: false`.
- The dashboard's visibility was not checked.

## Published SQL equals the repo

The published body is `body(file)`: the repo file with its leading `--` comment header and the
blank lines after it removed, and without the trailing newline.

| published body | chars | sha256 | equals `body(file)` at base `9842654` |
| --- | ---: | --- | --- |
| `new-8781215.sql` | 44,789 | `4c6cadfe4db1f6d46923edf746f9ed7bfd4838bd62811c22971867785b0f640c` | `00_qualified_arbs.sql` (117 header lines): yes |
| `new-8781229.sql` | 791 | `af97e867637c26ad6bbf5be995c9780b87833140d7bb7c20439e6c589b9a9c99` | `02_arb_detail_feed.sql` (26 header lines): yes |

- **Readback:** each post-update readback (`readback-*.json` `query`) is an exact byte match with its published body.
- **Dependents:** the `query` fields of the 01/03 read-only checks equal `body(file)` of `01`/`03`.
- **Headers only:** this change edits header comments only, so `body(file)` is unchanged and the saved SQL still equals the repo.

Reproduce all of this offline (no Dune access):

```bash
python3 evidence/dunesql/whi-1545/body_check.py <PUB>
```

## Validation executions (real Dune, medium engine)

| check | execution id | result | credits | payload (sha256 in `SHA256SUMS`) |
| --- | --- | --- | ---: | --- |
| V1: `00` on 2026-09-28 | `01M3PMEZC3E984Z86B6ZFCZHF3` | 252 rows, 252 distinct tx. The tx set equals the frozen export (`arb_detail_feed_20260928.csv`, sha256 `11ed6d1967a8f9a188c104a202975a925b3a3a757d60a9920af3c88a6d7f6cfe`). `hop_mix` counts equal `mirror_sept28.json` `hop_mix_counts` on all 43 keys (`unknown` 24). | 1.105 | `V1V2-exec-01M3PMEZC3E984Z86B6ZFCZHF3.raw.txt` (`cc00746c29dc053e29f9316c76456aaf1f6f7a623c7057daeaba50b17f89d214`) |
| V2: spot rows (same run) | ″ | `0x48f9322d…` `lb>lb`, `0x6eee6497…` `v2>v2` (both `merchant_moe;merchant_moe`). `0x82523d55…` `unknown` (`lb;unknown`). `0x87d95c63…` `v3>v3`. | – | ″ |
| V4/V5 on the day | ″ | 0 family/hop-count inconsistencies. Project → family pairs are only agni/fusionx/uniswap → v3 and merchant_moe → lb/v2. | – | ″ |
| V3/V4/V5: `00` on the default window 2026-06-01 → 2026-09-01 | `01M3PMHG6PGB4CHDF6EBPYQQQS` | 21,741 rows = 21,741 distinct tx; 90 bots. `unknown` 2,867 (13.19 %); 463 distinct mixes. V4: 0 inconsistencies. V5: 0 disallowed pairs. | 2.381 | `V3V4V5-exec-01M3PMHG6PGB4CHDF6EBPYQQQS.json.gz` (`3afd74a055c82e00e7749fcc9581676c7bad20e6919cfe8b2729f85dae0b8668`) |
| `01` (8781227) default | `01M3PMMBEG52PMFBP66R5NP4Z7` | 90 rows; sum(`arb_tx_count`) = 21,741. `hop_mix_distribution` has family keys. | 0.677 | `dep-01-exec-01M3PMMBEG52PMFBP66R5NP4Z7.json.gz` (`d5bc95ef78e07141cfa96e91b9cdbf8c70177e07fc242f3a1b4de1e9a1c5259c`) |
| `02` (8781229 v5) default | `01M3PMMASEN4KSNM8JZ2Q1CVZ2` | 21,741 rows, with the new `hop_count_bucket` / `ordered_families` columns. | 1.591 | not saved (see Limits) |
| `03` (8781231) default | `01M3PMMB5HZ9BDX441XNZYBMDN` | 90 rows = count of `01`. `hop_mix_distribution` has family keys with shares. | 3.257 | `dep-03-exec-01M3PMMB5HZ9BDX441XNZYBMDN.json.gz` (`cd7a29e135045f82fedba54a6aed05b01bb3e173c7e6e23889cf20d2c1de4fba`) |

- **Total cost:** 9.011 credits.
- **Qualification unchanged:** the default-window counts (21,741 tx, 90 bots) equal those in `scripts/dunesql/README.md` § "One successful execution window".
- **No side effects:** no temporary or new query objects were created, and no rollback was needed. Rollback SQL is kept at `rollback-8781215.sql` (`0c739d9de86a8b66804debc7a90adc617ec9a50ed7095a8533524e763150c125`) and `rollback-8781229.sql` (`c7903b0cc74752caed98ef587cb521c40fc19e5be8dc1eb608b735395431d061`).

## Limits

- **Independent re-check.** The V1–V5 and `01`/`03` figures above were recomputed from the saved payloads. The V1 tx set was compared against the frozen export.
- **`02` execution.** Its result payload was not saved, so its row count and columns rest on the orchestrator's record alone.
- **Dashboard.** Neither its text nor its visibility was reviewed.
