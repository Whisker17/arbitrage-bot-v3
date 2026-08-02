# WHI-745 — Mantle mainnet RPC qualification

**Date:** 2026-08-01  
**Probe:** `rpc_probe` 1.0.0 @ `20d7c5f` (WHI-744)  
**Command shape:** `--blocks 128 --duration 600` at merged CSV universe width  
**Outcome:** **No qualified candidate.** Escalation to owner (issue step 6).

## Credential / sourcing notes

- No `MANTLE_RPC_CANDIDATE_*` keys were present in the owner `.env`.
- Per WHI-745, the existing `MANTLE_RPC_URL` / `MANTLE_RPC_WS_URL` pair was treated as
  candidate **`owner-primary`** without renaming owner keys.
- Endpoints were never written into this tree, this report, or Linear comments —
  only labels and probe fingerprints appear below.
- HTTP and WS hosts for `owner-primary` are **different Mantle product surfaces**
  (distinct hostnames). That is a mixed-pair configuration of the kind WHI-526
  already flagged as unsafe for gate windows.

## Comparison table (labels only)

| Label | HTTP fp | WS fp | A multi-addr logs | B 0x7e receipts | C continuity | D headers | E WS stability | Qualified |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `owner-primary` | `c668fc669cea94d5` | `4d5cf2e8e3c6cc9b` | **PASS** | **PASS** | **FAIL** `http_ws_header_disagreement` | **FAIL** `incomplete_block_header` | **FAIL** `ws_silent_stall` | **no** |

Report file: [`owner-primary.json`](./owner-primary.json).

## Measured numbers (`owner-primary`)

### Check A — multi-address `eth_getLogs`

| Metric | Value |
| --- | --- |
| Address-set width (effective) | **220** |
| Derivation | v2=28 + v3/agni=21 + moe=192 → unique=220 × multiplier=1.0 |
| HTTP 429 count | 0 |
| HTTP 413 count | 0 |
| Failure count | 0 |
| Latency | 351 ms |
| Logs returned in window | 0 (empty is OK; the call succeeded) |

Check A ran at the **merged universe's real width** (220 unique pool addresses from
the same CSV sources as `bot`).

### Check B — type `0x7e` receipt decode

| Metric | Value |
| --- | --- |
| Raw fetch failures | 0 |
| Provider-side failures | 0 |
| Receipts sampled | 190 |
| Type `0x7e` count | 128 (≥ min 1) |
| Alloy typed decode failures | 128 (expected; `REQUIRE_ALLOY_TYPED_RECEIPT_DECODE=false`) |

### Check C — block continuity / HTTP↔WS agreement

| Metric | Value |
| --- | --- |
| HTTP samples | 128 |
| WS samples (for continuity cross-check) | **0** |
| Cross-compared / agreed | 0 / 0 |
| Failure | `http_ws_header_disagreement` — *no overlapping heights between HTTP sample and WS sample* |

> **Corrected 2026-08-02 — the original text of this paragraph mis-attributed the
> failure to HTTP.** See "Attribution correction" at the end of this document. The
> `-32001 rpc method is not whitelisted` responses came from the **WS** surface, not
> HTTP. The measurements on this very table already showed it: `http_samples = 128`
> against `ws_samples = 0`. HTTP sampled cleanly; WS returned nothing.

Header sampling over the **WS** surface logged repeated `-32001 rpc method is not
whitelisted`. HTTP header reads, receipts (Check B) and multi-address logs (Check A)
all succeeded. With zero WS samples there were no overlapping heights to compare, so
C reports a hard failure — but on missing data, not on a genuine HTTP↔WS disagreement.

### Check D — header completeness

| Metric | Value |
| --- | --- |
| Samples | **0** |
| Completeness ratio | 0.0 |
| Failure | `incomplete_block_header` — *no headers sampled* |

### Check E — sustained WS stability (600 s)

| Metric | Value |
| --- | --- |
| Elapsed | 600 s |
| Heads received | 299 |
| Disconnects | 0 |
| Silent stalls (gap > 4 s) | **96** (max allowed 0) |
| Tip number gaps | 0 |
| Failure | `ws_silent_stall` |

WS stayed connected but stalled far beyond Mantle's ~2 s block cadence. Unusable
as a tip feed for a multi-hour gate.

## Selection / escalation

**No winner.** Per WHI-745 step 6 this is an explicit stop:

- Do **not** start WHI-535 on `owner-primary`.
- Do **not** relax probe thresholds to manufacture a pass.
- Owner action required: supply one or more **matched HTTP+WS pairs from a single
  production-grade provider** (full method whitelist including multi-address
  `eth_getLogs`, `eth_getBlockReceipts` / raw receipts, and `eth_getBlockBy*`,
  plus a stable newHeads subscription), under:

  ```
  MANTLE_RPC_CANDIDATE_N_LABEL=...
  MANTLE_RPC_CANDIDATE_N_HTTP=...
  MANTLE_RPC_CANDIDATE_N_WS=...
  ```

  Then re-run:

  ```bash
  cargo run --release --bin rpc_probe -- \
    --http "$MANTLE_RPC_CANDIDATE_N_HTTP" \
    --ws "$MANTLE_RPC_CANDIDATE_N_WS" \
    --blocks 128 --duration 600 \
    --out "evidence/rpc/<label>.json"
  ```

### Why this pair fails the *probe* (owner-facing)

> **Corrected 2026-08-02.** This section originally read "Why this pair fails the
> gate" and listed an HTTP whitelist gap. Both were wrong. See "Attribution
> correction" below: the endpoint is usable by the bot, and points 1–2 as first
> written did not describe a real obstacle to it.

1. **Mixed surfaces** — HTTP and WS fingerprints differ; hosts are not a matched
   pair. WHI-526 recorded missing-block failure modes for mixed providers. This
   remains a real hazard in principle, but it is one the code can close: WHI-762
   pins every per-block read to the announced block hash and skips on mismatch,
   which makes a mixed pair safe by construction rather than by procurement.
2. **WS method whitelist gap** — `eth_getBlockByNumber` over **WS** returns
   `-32001 not whitelisted`, which is why the probe's header sampler collected zero
   samples and C/D failed. **The bot does not make that call over WS**, so this does
   not block it. HTTP header reads work and return `baseFeePerGas` / `gasLimit`.
3. **WS tip quality** — 96 silent stalls in 600 s against a zero-stall threshold.
   The threshold shape is wrong (see the addendum), but the burstiness is real and
   would distort latency measurement (WHI-537). It does not block a signerless
   dry run.

### Partial credit (what *does* work)

If a future matched pair reuses the same HTTP surface that passed A/B:

- Multi-address `eth_getLogs` at width **220** with **zero** 429/413.
- Raw type-`0x7e` receipts delivered and counted (128 in the sample window).

That is necessary but not sufficient. C/D/E must also pass on the **same** pair.

## Rate-limit / burn envelope

**No selected endpoint** — envelope for a multi-hour WHI-535 window is **not
sized**. Partial observation on the non-qualified HTTP surface only:

- Check A: single multi-address `eth_getLogs` over 8 blocks at 220 addresses
  succeeded in ~351 ms with no 429.
- That says nothing about sustained block/receipt polling load, WS credit burn,
  or monthly caps. Size the plan only after a `qualified: true` report exists.

## Historical public endpoints (not re-probed here)

`setup_env.sh` previously presented these as equal choices for mainnet:

| Surface | Gate suitability | Basis |
| --- | --- | --- |
| `rpc.mantle.xyz` (public) | **Not for gate runs** | Documented failure modes in WHI-526 shadow STATUS; free-tier / public RPC |
| `mantle.publicnode.com` | **Not for gate runs** | WHI-526: 429 + type `0x7e` decode issues |
| `rpc.ankr.com/mantle` | **Not for gate runs** | Public / free-tier; not production-qualified for merged universe |

These were **not** owner-supplied candidates for this run. They remain labeled
unqualified for gate/shadow in `setup_env.sh` so they are not presented as equal
to a probe-qualified pair. Re-qualify any new paid plan with `rpc_probe` before
WHI-535.

## Acceptance mapping

| Criterion | Status |
| --- | --- |
| Every supplied candidate has a committed report under `evidence/rpc/` | **yes** (`owner-primary.json`) |
| At least one `qualified: true`, or step-6 escalation with per-check failures | **escalation** (this file + Linear comment) |
| Check A at merged universe width, width stated | **yes** (220) |
| HTTP+WS validated as a pair | **yes** (pair failed C/E together) |
| No URL / API key / credential in committed artifacts | **yes** (fingerprints + labels only) |
| `.env` not read wholesale into transcripts | **yes** (key-by-key extract of the two RPC vars only) |
| Rate-limit envelope for selected endpoint | **N/A** — none selected |
| `setup_env.sh` no longer presents known-unqualified as gate-suitable | **yes** (this PR) |

---

## Addendum (2026-08-01) — report files committed; two additional candidates

`owner-primary.json` was already committed with this document. Two further probe
runs against public endpoints were left untracked and are committed here, so that
WHI-761 has a fixed pre-fix baseline for all three to regression-test against.

Those two were **not** owner-supplied candidates and are recorded for comparison
only:

| Label | A multi-addr logs | B 0x7e receipts | C continuity | D headers | E WS stability | Qualified |
| --- | --- | --- | --- | --- | --- | --- |
| `owner-primary` | **PASS** | PASS (waived) | FAIL | FAIL | FAIL | no |
| `chainlist-drpc` | **PASS** | FAIL | FAIL | FAIL | FAIL | no |
| `chainlist-publicnode` | FAIL | PASS (waived) | **PASS** | **PASS** | FAIL | no |

### Cross-candidate reading

Comparing the three changes the diagnosis in two ways:

1. **The `-32001 rpc method is not whitelisted` gap is surface-specific, not universal.**
   `chainlist-publicnode` passes C and D, so full block-header reads are servable on
   Mantle. `owner-primary`'s HTTP surface serves `eth_getLogs` and receipts but
   refuses `eth_getBlockBy*`. That is a **method-whitelist configuration** on an
   otherwise strong surface — not a reason to replace the provider. The concrete
   owner action is to have `eth_getBlockByNumber` / `eth_getBlockByHash` (and any
   header read the snapshot protocol needs) added to the allowed method set.

2. **`owner-primary` already passes the two checks that are hardest to satisfy.**
   Multi-address `eth_getLogs` at the merged universe's real width (220 addresses,
   351 ms, zero 429/413) and raw `0x7e` receipt delivery both pass. `publicnode`
   fails A outright. If the whitelist gap is closed, `owner-primary` plausibly
   clears A, B, C(HTTP side) and D, leaving only E.

### On check E

Every candidate fails E, which on its own would suggest a threshold problem — and
the threshold (`stall_threshold_secs=4`, `max_ws_stalls=0`) is indeed absolute where
it should be a rate. But the `owner-primary` numbers do not simply exonerate the
feed: 299 heads over 600 s with `tip_number_gaps: 0` means **no block was missed**,
while 96 inter-head gaps exceeded 4 s. For the mean to remain ~2.0 s, delivery must
be **bursty** — quiet periods followed by several heads at once — rather than smooth
at Mantle's cadence. Bursty tip delivery is tolerable for a signerless dry run but
would distort any latency measurement (WHI-537) and is a real defect for
latency-sensitive operation.

WHI-761 owns re-expressing E as a rate/percentile and fixing the C/D zero-sample
reporting. Re-run all three after that lands before drawing a final conclusion.

---

## Attribution correction (2026-08-02)

**This document originally stated that the owner's HTTP surface refuses block-header
reads. That is false, and the error was acted on before it was caught.** Two earlier
paragraphs have been corrected in place and are marked; this section is the record.

### What was measured directly

| Transport | `eth_getBlockByNumber` |
| --- | --- |
| HTTP (`MANTLE_RPC_URL`) | **works** — returns a full block including `baseFeePerGas` and `gasLimit` |
| WS (`MANTLE_RPC_WS_URL`) | `-32001 rpc method is not whitelisted` |

The whitelist gap is on **WS**. This document's own check-C numbers already implied
it — `http_samples = 128` against `ws_samples = 0` — and were not cross-checked
against the prose.

### Why the probe's verdict does not describe the bot

The bot never issues `eth_getBlockByNumber` over WS:

- `src/bin/bot.rs:584` — `StateSpaceBuilder::new(http.clone())`; startup tip reads use HTTP.
- `src/bin/bot.rs:690` — `run_multi_protocol_watch_loop(http_erased, …)`; the per-block
  canonical header fetch at `src/service/block_loop.rs:586` uses HTTP.
- `src/bin/bot.rs:660` — WS carries **only** `subscribe_heads_once` → `subscribe_blocks()`,
  which works (299 heads in the E window).

A live run of `cargo run --release --bin bot -- --protocols agni-v3,moe --watch`
confirmed it: the HTTP provider connected, no `-32001` appeared anywhere, and startup
proceeded past RPC entirely, halting only at WHI-529's settlement-asset check for an
unrelated reason (the endpoint resolved to Sepolia 5003 — see WHI-776).

### Consequences

1. **`owner-primary` is not disqualified by C/D.** Those checks tested a call the bot
   does not make on that transport.
2. **No infra whitelist change is required**, and **no multi-provider split is
   required.** Both were recommended on the strength of this error.
3. **WHI-761 gains a defect-0**: the probe must mirror the bot's transport-per-method
   usage. Fixing only the zero-sample reporting would leave the false negative intact
   and merely reword it.

Superseded by this section: the earlier claim that "both the probe fix and the
whitelist change are required." Only the probe fix is required.

### Still open, unaffected by this correction

- Check A's result stands: multi-address `eth_getLogs` at 220 addresses, 351 ms,
  zero 429/413. That is genuine and is the hardest requirement to meet.
- Check E's burstiness stands: 96 stalls versus `chainlist-publicnode`'s 1 over a
  comparable window. The zero-tolerance threshold is the wrong shape, but the two
  feeds are not equivalent and a revised criterion should still separate them.
- No endpoint has been re-probed since these fixes were identified. Re-run all three
  after WHI-761 lands before treating any verdict here as final.
