# WHI-1546: daily-digest operator acceptance record (sanitized)

This record closes the release review's R1-F6 finding: the daily-digest feature's operator acceptance items were unverified. It covers three items:
- the retention check;
- a real `--send-test`;
- the operator's review of real cards.

It also records one explicitly owner-accepted deviation (AC13 chronology, §5).

**Sanitization.** No webhook URL, token, tail fragment, keyword value or `.env` value appears in this file. Environment variables are named but never valued.

**Host and time.** Host: `arb-bot-jp`. All times are UTC.

**How the evidence was gathered.** Collection was agent-performed, with the owner's authorization for exactly the steps listed. The owner statements (§2 and §4) are quoted verbatim, as relayed by the release orchestrator from structured owner questions.

**What is not evidence.** No runbook, exit code or mock test is presented here as proof of an operator step. Only the live-host observations and the owner's own statements below are evidence.

## 1. Retention check (feature AC8): DONE, using the fixed helper
- **When:** 2026-09-29, about 19:12Z. Read-only: the script was piped over ssh, and nothing was installed on the host.
- **Helper:** `scripts/golive/check_ledger_retention.sh` as fixed by the retention-bound fix, commit `2e62b9f`. Its sha256, `d7a1032a32f41c2d0388bc55ef1bd1d580c368188169dcceb64404db262a704f`, is identical at `837aafe`.
- **Target:** the live shadow ledger `/opt/arbitrage-bot-v3-rc2/evidence/shadow/whi-1423/ledger.jsonl`, with 2 segments.
- **Results:**
  - `2026-09-28`: **`RESULT: appears fully covered by retained history`**. Earliest activity 2026-09-27T02:43:20Z; latest 2026-09-29T19:10:38Z.
  - `2026-09-29`: partial. This was expected, because the day was still in progress at the time of the check.
- **Why the fixed helper matters:** the pre-fix helper could report a false positive. The fixed helper derives its bounds from activity rows, which is why this re-evaluation was run.

## 2. Operator review of real cards (feature AC14): DONE for the rc2 cards (option A1)
- **Question asked of the owner:** "你是否已经查看 Lark 中 2026-09-27 和 2026-09-28 两天的真实日报，并确认内容和显示符合预期？" ("Have you looked at the real daily reports in Lark for 2026-09-27 and 2026-09-28, and confirmed that their content and display are as expected?")
- **Owner answer, verbatim:** **「已查看，符合预期」** ("viewed, as expected"). The selected option stated that the owner had actually inspected both cards, and that their content, layout and alarm meaning met expectations.
- **When:** the answer was recorded by the release orchestrator by about 2026-09-30 00:37Z. The authoritative timestamp is the orchestrator session's structured-question response. No exact minute is claimed here.
- **Scope:**
  - The scheduled daily cards for days **2026-09-27** (fired 2026-09-28 00:10Z) and **2026-09-28** (fired 2026-09-29 00:10Z).
  - Both were produced by the deployed **rc2** digest binary, sha256 `3ad466b207f09e38ab45a412c2fc967050b662dce3b7bdfef0550ba1e2062b0c`. That binary predates the notifier fix batch.
- **What this review does NOT cover:**
  - It is **not** a review of a card rendered by the fixed code at `837aafe`. The scheduled unit still runs the rc2 binary.
  - It is **not** the runbook's "dry-run reviewed before the first scheduled fire". That step did not happen before the first fire; the owner instead reviewed the real delivered cards afterwards.

## 3. Send-test identity (feature AC13, real-use part)
- **Tag:** annotated `v0.2.2-rc3`, tag object `0058ca4137b60e709e46b7d8eb387152dc2c573a`, pointing at commit `837aafeb647b1c7086a098cf6c9d788237c36228`. It was created and pushed 2026-09-30 about 00:40Z and verified with `git ls-remote`, where `^{}` resolves to `837aafe`.
- **Source checkout:** a fresh clone of that tag only, into a new isolated directory `/opt/arbitrage-bot-v3-rc3`. It was verified before the build: HEAD = `837aafe`, tag = `0058ca41…`, porcelain empty.
- **Build:** `nice -n 15 cargo build --locked --release --bin lark_daily_digest`, with rustc 1.95.0 (59807616e 2026-04-14) and cargo 1.95.0. It exited 0 after 4m 54s.
- **Binary:** `/opt/arbitrage-bot-v3-rc3/bin/lark_daily_digest-v0.2.2-rc3`, sha256 **`689abe8a7e278ddb0597fb855ae471c4823fd9bcaecf387612f3cd6badf2d697`**.
  - It is not wired into any systemd unit.
  - The scheduled digest still runs the rc2 binary through its existing drop-in.
- **Command shape.** One ssh invocation. The child environment was whitelisted, and exactly two variable names were read from `/opt/arbitrage-bot-v3/.env`; there was no blanket source:
  ```
  env -i PATH=/usr/bin:/bin bash -c 'set -a; . <(grep -E "^(LARK_WEBHOOK_URL|LARK_KEYWORD)=" /opt/arbitrage-bot-v3/.env); set +a; exec /opt/arbitrage-bot-v3-rc3/bin/lark_daily_digest-v0.2.2-rc3 --send-test' 2>&1 | <URL/tail redaction filter>
  ```
  - Environment names: `LARK_WEBHOOK_URL`, `LARK_KEYWORD`. The name check found exactly 2 names, and no values were printed.
  - No signer or private-key variables were inherited.
- **Checkpoints:** a durable `PLANNED` checkpoint was written before the send (2026-09-30T00:46:43Z), both off-host and on the host. The `ATTEMPTED` checkpoint was written immediately after the send.
- **Invocations:** exactly **one**, at 2026-09-30T00:46:56Z.

### Sanitized result
The webhook tail label is masked as well.
```
2026-09-30T00:46:56.885349Z INFO notify.lark: lark digest card delivered webhook=open.larksuite.com …<tail-redacted> attempts=1
send-test webhook=open.larksuite.com/<tail-redacted> …<tail-redacted> sent=true attempts=1 status=Some(200) error=None
exit=0
```
The result had `attempts=1`: there was no retry, so there was no possible duplicate from the built-in retry. The output also had no error text.

### Non-mutation check (before 00:46:15Z / after 00:47:19Z)

| Check | Before | After | Same? |
|---|---|---|---|
| marker `data/lark_daily_digest/state.marker` sha256 | `a0a764f496b11bbb3022c173ff5a4e9c638e30325954dcb66bfd955780d89939` | same | yes |
| marker mtime / size | 2026-09-30 00:10:29.764041246 / 10 B | same | yes |
| `state.marker.lock` | absent | absent | yes |
| `lark-daily-digest.service` InvocationID | `5876eaf6170c4cee9ce8178446142c31` | same | yes |
| ExecMainExitTimestamp / ActiveState | 2026-09-30 00:10:29 / inactive | same | yes |
| timer NEXT / LAST | 2026-10-01 00:10:00 / 2026-09-30 00:10:28 | same | yes |
| unit journal line count | 50 | 50 | yes |
| shadow bot `pgrep -x bot` | 1275022 | 1275022 | yes |
| bot exe sha256 / start | `f55b341b5e49452b413875f2a18d2a5ed475067b713f30e57a59648ac8de621c` / 2026-09-27 02:40:39 | same | yes |

- **Timing:** the send ran well outside the ±10 min window around the 00:10Z scheduled fire.
- **Untouched:** the digest service, timer, drop-in, marker, lock and shadow bot were not modified, restarted or triggered.

## 4. Owner delivery confirmation
- **Question:** a structured question naming the usual Lark digest chat, one grey TEST card, and a delivery time of about 2026-09-30 00:47Z (08:47 HKT).
- **Owner answer, verbatim:** **「收到一张，无重复」** ("one received, no duplicate").
- **What it confirms:** delivery of exactly one card from the single invocation in §3. No repeat send was made.

## 5. AC13 chronology: owner-accepted deviation
- **Original step MISSED.** The feature's acceptance item was "a real `--send-test` before the first scheduled fire". That step **was MISSED**. Scheduled cards had already been delivered, on 2026-09-28 and 2026-09-29 at 00:10Z, before any send-test was run. The same holds for the runbook's "dry-run reviewed before the first scheduled fire" (§2).
- **What was done instead:** a **late transport verification** (§3–§4), on 2026-09-30, using the fixed code.
- **Owner decision, verbatim:** 「接受如实记录的补验」 ("accept the late check, recorded as it happened"). This is an explicit, owner-accepted historical deviation.
- **What this record claims:** it does **not** claim that the original pre-first-fire chronology happened.

## 6. Status of the operator items

| Item | Status | Evidence |
|---|---|---|
| Retention check (AC8) | done, fixed helper | §1 |
| Real send-test (AC13 real-use) | late transport verification done; original pre-first-fire step MISSED, deviation owner-accepted | §3–§5 |
| Operator review of real cards (AC14) | done for the rc2 cards of 2026-09-27 and 2026-09-28; the fixed-code card is not yet reviewed | §2 |

**Out of scope:** switching the scheduled digest to the rc3 binary is a separate deploy decision. It must follow the notifier fix batch's rule to drain old invocations first.
