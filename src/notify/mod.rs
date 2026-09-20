//! WHI-1407 — daily Lark digest for the signerless Mantle dry-run.
//!
//! One-shot binary support library (`src/bin/lark_daily_digest.rs`). Deliberately
//! **not** wired into `bot.rs`'s `--watch` loop — see that binary's module doc for the
//! full contract. Submodule boundaries mirror the issue's stated seams so each one is
//! independently testable:
//!
//! * [`utc_date`] — dependency-free UTC calendar-day arithmetic (no `chrono`, matching
//!   this crate's existing convention — see `rpc_probe::runner::now_unix_label`).
//! * [`ledger_window`] — I/O: reads the shadow ledger's active file + numeric rotated
//!   segments in chronological order into plain data rows. The real ledger row types
//!   (`execution::shadow::ledger::Ledger*Row`) are `pub(crate)` to that module's parent
//!   only (see `execution::shadow_report`'s doc comment / `docs/DEFERRED_ISSUES.md`
//!   DI-31) — this module defines its own wire-mirror rows rather than reaching in.
//! * [`digest`] — pure aggregation (no I/O) over [`ledger_window`]'s rows into a
//!   [`digest::DigestAggregate`], for one UTC calendar day.
//! * [`lark`] — pure card-render function over a [`digest::DigestAggregate`], plus the
//!   `reqwest::blocking` delivery client (status + provider-body validation, bounded
//!   retries, redaction).
//! * [`state`] — day-keyed idempotency marker with a single-flight exclusive lock
//!   spanning read → POST → write.

pub mod digest;
pub mod lark;
pub mod ledger_window;
pub mod state;
pub mod utc_date;
