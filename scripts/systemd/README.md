# WHI-1407 — daily Lark digest systemd units

`lark-daily-digest.service` (oneshot) + `lark-daily-digest.timer` (fixed
**00:10 UTC**, `Persistent=true`) run `cargo run --release --bin
lark_daily_digest` once a day against the shadow ledger. This process never
writes to the ledger and is fully independent of `bot --watch` — see
`src/bin/lark_daily_digest.rs`'s module doc for the full contract.

## Install (per host)

1. Edit the `EDIT:` placeholders in both unit files for this host's absolute
   deploy path, then copy them into `/etc/systemd/system/`:

   ```bash
   sudo cp scripts/systemd/lark-daily-digest.service /etc/systemd/system/
   sudo cp scripts/systemd/lark-daily-digest.timer /etc/systemd/system/
   sudo systemctl daemon-reload
   ```

2. Ensure `.env` (gitignored, same convention as `env.mainnet.example`) sets
   `LARK_WEBHOOK_URL` and `LARK_KEYWORD` at the `EnvironmentFile=` path named
   in the `.service` unit — the timer-triggered service does **not** inherit
   an interactive shell's environment.

3. **Before enabling the timer**, verify the real webhook in isolation
   (issue item 7 — run this against production once):

   ```bash
   cargo run --release --bin lark_daily_digest -- --send-test
   ```

   Confirm the labeled test card actually lands in the Lark chat. This does
   **not** consume the daily marker.

4. Verify the actual retained-ledger history on this host covers the
   reporting window — the ledger's 512MiB total-size cap alone is **not**
   evidence of a full day's retention (see the issue's Context note):

   ```bash
   scripts/golive/check_ledger_retention.sh /path/to/shadow_ledger.jsonl 2026-06-14
   ```

5. Dry-run the actual card once against the live ledger and have an operator
   review it before the first scheduled fire — every field must trace to a
   real ledger value or an explicit `N/A`/label, never a fabricated sample
   number:

   ```bash
   cargo run --release --bin lark_daily_digest -- --dry-run --ledger /path/to/shadow_ledger.jsonl
   ```

6. Enable the timer:

   ```bash
   sudo systemctl enable --now lark-daily-digest.timer
   systemctl list-timers lark-daily-digest.timer
   ```

## Recovery

A missed or failed day stays eligible for retry — the next scheduled fire
processes outstanding days in order (bounded per invocation; see
`notify::state::plan_backlog`). For an explicit one-off backfill:

```bash
cargo run --release --bin lark_daily_digest -- --date 2026-06-14 \
  --ledger /path/to/shadow_ledger.jsonl --state /path/to/state.marker
```

See `src/bin/lark_daily_digest.rs`'s module doc for the exact state-mutation
rule this recovery command follows (it never moves the marker backward).
