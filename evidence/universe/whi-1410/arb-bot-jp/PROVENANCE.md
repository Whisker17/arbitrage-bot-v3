# arb-bot-jp production universe capture (WHI-1410 AC-2)

The universe the production host `arb-bot-jp` was running, copied **read-only**
(no write, no restart, no config change on the host). These bytes are the
host's files unchanged. Nothing here was reconstructed or regenerated.

| file | bytes | host mtime (host TZ +0800) | sha256 |
| -- | -- | -- | -- |
| `pool_universe.csv` | 20205 | 2026-09-21 02:01:53.691 | `bbd60ac3b37fdd345313cb8145f0234d7054eb6a35ed4c4da29fb0fb49ce8067` |
| `pool_universe.meta.json` | 906 | 2026-09-21 02:01:53.695 | `71ac61077afc53a705f36a50eb0efa7e22177f2920d7c060e7bcffd2322b3193` |
| `pool_universe.quarantine.json` | 3745 | 2026-09-21 02:01:53.695 | `d4ad9c88b3b77c3eb8f4d4a7b5b6ebe2102c1e7866ebed7fd4fc27166e9d6028` |

Meta: `chain_id` 5000, `snapshot_block` 100871945
(`0x3e32a75c…dc34f5`, timestamp 1789874202), `pool_count` 109,
`per_protocol` `{agni-v3: 76, moe: 33}` (**no `agni-v2` key**), fingerprint
`0xee1d40b8c1f0c42fe8c9348cc6d575516ef71cf05f5aaedd80931030eef7748b`.

## How it was captured

Host path `/opt/arbitrage-bot-v3/data/` (the deploy dir has no `.git`).
SSH alias `arb-bot-jp` from the operator workstation's `~/.ssh/config`
(`BatchMode=yes`, key auth; no credential is stored in this repo).

1. 2026-09-26T02:23:24Z (implementer run 2fb9c604): read-only listing +
   `sha256sum data/pool_universe.*` on the host.
2. 2026-09-26T02:23:40Z: copy, preserving mtimes:

   ```bash
   scp -o BatchMode=yes -p \
     arb-bot-jp:/opt/arbitrage-bot-v3/data/pool_universe.csv \
     arb-bot-jp:/opt/arbitrage-bot-v3/data/pool_universe.meta.json \
     arb-bot-jp:/opt/arbitrage-bot-v3/data/pool_universe.quarantine.json .
   ```

3. 2026-09-26T02:55:42Z (continuation run, host clock): re-read on the host,
   hashes unchanged:

   ```bash
   ssh -o BatchMode=yes arb-bot-jp 'cd /opt/arbitrage-bot-v3 && \
     sha256sum data/pool_universe.csv data/pool_universe.meta.json \
       data/pool_universe.quarantine.json data/poolLists.csv data/poolLists_moe.csv; \
     stat -c "%n %s %y" data/pool_universe.* target/release/universe_gen; \
     ls -la data/poolLists_v2.csv'
   ```

   Same three sha256 as the table. Other host facts from the same read:

   - `data/poolLists.csv` sha256
     `a737700def85cc8ac43f6ff961dd3eb7c2b90ab36259acc834f08b7a432374d6` and
     `data/poolLists_moe.csv`
     `de3a7e4a6c20ab026749231deba187d67f96484ca7021c756a38574c6cb4cda6`: byte-identical to the repo's committed seeds.
   - `data/poolLists_v2.csv`: **`No such file or directory`**.
   - `target/release/universe_gen` built 2026-09-20 11:16:29 +0800, i.e.
     before the regeneration. `target/release/bot` 2026-09-21 02:06:19 +0800.
   - No universe_gen log or shell history for the 02:01 run was found on the
     host (`/root/.bash_history` has no `universe_gen` line). The exact
     command line cannot be recovered. See `../README.md` for the byte-exact
     reproduction that stands in for it.

## Diff against the committed universe (`data/pool_universe.csv`, 130 pools)

Host 109 vs committed 130: 0 host-only, 21 committed-only, 0 shared rows
differing. That is a strict subset. Removed: 5 `agni-v2`, 11 `agni-v3`,
5 `moe`. None of the 21 is in the host quarantine file (20 entries:
19 `tick_data_batch_abi_incompatible` (WHI-938), 1 `valuation_unavailable`,
all `agni-v3`).
