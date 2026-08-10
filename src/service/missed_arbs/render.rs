//! Operator-facing Markdown rendering for the WHI-999 report.
//!
//! Split from the analysis so presentation changes and analytical changes do not
//! land in the same file. Everything here reads [`MissedArbReport`] and writes
//! text — including the cause shares, which the analysis emits as
//! `CauseRecount::shares_pct` so no percentage is re-derived here.

use std::collections::BTreeMap;

use alloy::primitives::U256;

use super::{MissedArbReport, UnlockStep, COLD_START_REFERENCE};

/// WMNT-wei → whole WMNT for display; `—` when unmeasured.
fn tvl_display(tvl: Option<&String>) -> String {
    match tvl.and_then(|s| s.parse::<U256>().ok()) {
        Some(v) => {
            let whole = v / U256::from(10u64).pow(U256::from(18u64));
            format!("{whole}")
        }
        None => "—".into(),
    }
}

/// `1:12 2:30` — hop position to occurrence count.
fn hop_positions_display(positions: &BTreeMap<u32, usize>) -> String {
    if positions.is_empty() {
        return "—".into();
    }
    positions
        .iter()
        .map(|(pos, n)| format!("{pos}:{n}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn dash(s: Option<&String>) -> &str {
    s.map(|v| v.as_str()).unwrap_or("—")
}

/// Render the operator-facing Markdown report.
pub fn render_markdown(report: &MissedArbReport) -> String {
    let mut out = String::new();
    let i = &report.inputs;
    let r = &report.cause_recount;
    let b = &report.baseline;

    out.push_str("# Backwards universe selection from missed arbs (WHI-999)\n\n");
    out.push_str(&format!(
        "Dataset `{}` × census `{}` vs the frozen {}-pool universe.\n\n",
        i.arb_dataset, i.census_dataset, i.universe_pool_count
    ));
    out.push_str("| Input | Value |\n| --- | --- |\n");
    if let (Some(a), Some(z)) = (i.block_from, i.block_to) {
        out.push_str(&format!("| Block range | {a} – {z} |\n"));
    }
    out.push_str(&format!("| Events classified | {} |\n", r.total_events));
    out.push_str(&format!(
        "| Source rows | {} ({} dropped: no decodable path) |\n",
        i.source_rows, i.skipped_empty_path
    ));
    if let Some(f) = &i.universe_fingerprint {
        out.push_str(&format!("| Universe fingerprint | `{f}` |\n"));
    }
    if let Some(sb) = i.universe_snapshot_block {
        out.push_str(&format!("| Universe snapshot block | {sb} |\n"));
    }
    out.push_str(&format!("| Settlement asset | `{}` |\n", i.settlement_asset));
    out.push_str(&format!("| Hop cap | {} |\n", i.max_hops));
    out.push_str(&format!(
        "| TVL floor | {} WMNT wei |\n",
        i.min_tvl_wmnt_wei
    ));
    out.push_str(&format!(
        "| Candidate TVL | {} |\n\n",
        match (i.tvl_measured, i.tvl_requested, i.tvl_block) {
            (true, _, Some(bk)) => format!("measured at block {bk}"),
            (true, _, None) => "measured".into(),
            (false, true, _) => "**requested but not measured** — no candidate pool \
                                 carried a census token pair to value (census gap, \
                                 not a clean floor)"
                .into(),
            (false, false, _) => "**not measured** (no `--measure-tvl`)".into(),
        }
    ));

    out.push_str("## Verdict\n\n");
    out.push_str(&format!("{}\n\n", report.verdict.statement));
    out.push_str(&format!("> {}\n\n", report.verdict.economics_caveat));

    out.push_str("## Cause recount (reconciles with WHI-957)\n\n");
    out.push_str("| Cause | Count | Share of non-aggregator |\n| --- | ---: | ---: |\n");
    for (label, suffix, n) in [
        ("not_in_universe", "", r.not_in_universe),
        (
            "in_universe_in_scope",
            " — WHI-957 `unattributable`",
            r.in_universe_in_scope,
        ),
        ("out_of_scope_hop_cap", "", r.out_of_scope_hop_cap),
        (
            "out_of_scope_non_wmnt_settlement",
            "",
            r.out_of_scope_non_wmnt_settlement,
        ),
    ] {
        let share = r.shares_pct.get(label).copied().unwrap_or(0.0);
        out.push_str(&format!("| `{label}`{suffix} | {n} | {share:.1}% |\n"));
    }
    out.push_str(&format!(
        "| `aggregator_misclass` (excluded from rates) | {} | — |\n\n",
        r.aggregator_misclass
    ));

    out.push_str("## Residual bound\n\n");
    out.push_str(&format!("{}\n\n", report.residual_bound.statement));
    out.push_str(&format!(
        "Residual events carrying a pool outside the universe: **{}**.\n\n",
        report.residual_bound.residual_events_with_missing_pool
    ));

    out.push_str("## In-scope baseline\n\n");
    out.push_str("| Metric | Value |\n| --- | ---: |\n");
    out.push_str(&format!("| In-scope arbs | {} |\n", b.in_scope_arbs));
    out.push_str(&format!(
        "| Reachable on today's universe | {} ({:.1}%) |\n",
        b.reachable_now, b.reachable_now_pct
    ));
    out.push_str(&format!(
        "| Blocked by missing pools | {} |\n",
        b.blocked_by_missing_pools
    ));
    out.push_str(&format!(
        "| Distinct pools in in-scope arbs | {} |\n",
        b.distinct_pools_in_in_scope_arbs
    ));
    out.push_str(&format!("| …held | {} |\n", b.distinct_pools_held));
    out.push_str(&format!("| …missing | {} |\n\n", b.distinct_missing_pools));

    out.push_str("## Which filter actually excludes the missing pools\n\n");
    out.push_str("Evaluated in admission order — a pool blocked by its venue is never also blamed on TVL.\n\n");
    out.push_str("| Exclusion cause | Missing pools | In-scope arbs touched |\n| --- | ---: | ---: |\n");
    for (cause, n) in &report.exclusions.pools_by_cause {
        let arbs = report
            .exclusions
            .in_scope_arbs_touched_by_cause
            .get(cause)
            .copied()
            .unwrap_or(0);
        out.push_str(&format!("| `{cause}` | {n} | {arbs} |\n"));
    }
    out.push_str("\nArbs double-count across causes when one path has several kinds of gap.\n\n");
    out.push_str("| Venue status | Missing pools | Work required |\n| --- | ---: | --- |\n");
    for (status, n) in &report.exclusions.pools_by_venue_status {
        let work = report
            .exclusions
            .work_required_by_venue_status
            .get(status)
            .map(|s| s.as_str())
            .unwrap_or("—");
        out.push_str(&format!("| `{status}` | {n} | {work} |\n"));
    }
    out.push_str(&format!(
        "\nIn-scope arbs whose **every** gap sits on a loadable venue: **{}** — the ceiling \
         reachable with no new adapter.\n\n",
        report.exclusions.in_scope_arbs_gap_fully_loadable
    ));

    push_ranking(
        &mut out,
        "## Ranking — loadable venues only (actionable today)",
        &report.ranking_loadable,
    );
    push_ranking(
        &mut out,
        "## Ranking — any venue (upper bound; needs adapters)",
        &report.ranking_any_venue,
    );

    out.push_str("## Candidate sets and admission cost\n\n");
    out.push_str(
        "| Restriction | Size | Added | Arbs unlocked | Reachable after | Pools | Cycles | Δcycles | Cycle × | Cold start | Adapter-blocked |\n\
         | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n",
    );
    for s in &report.candidate_sets {
        out.push_str(&format!(
            "| `{}` | {} | {} | +{} | {} ({:.1}%) | {} | {} | {:+} | {:.2}× | {:.0} s | {} |\n",
            s.restriction.as_str(),
            s.requested_size,
            s.pools_added,
            s.arbs_unlocked,
            s.reachable_after,
            s.reachable_after_pct,
            s.pool_count_after,
            s.cycle_count_after,
            s.cycle_count_delta,
            s.cycle_growth_factor,
            s.est_cold_start_secs,
            s.adapter_required_pools,
        ));
    }
    out.push_str(&format!(
        "\nCold start is estimated linearly from {COLD_START_REFERENCE}; cycle counts come from \
         the production enumerator.\n\n"
    ));

    let h = &report.hop_cap;
    out.push_str("## Hop-cap pricing\n\n");
    out.push_str("| Hops | Arbs |\n| ---: | ---: |\n");
    for (hops, n) in &h.arbs_above_cap_by_hop {
        out.push_str(&format!("| {hops} | {n} |\n"));
    }
    out.push_str(&format!("| **total above cap** | **{}** |\n\n", h.arbs_above_cap_total));
    out.push_str("| Metric | Value |\n| --- | ---: |\n");
    out.push_str(&format!(
        "| Arbs at exactly {} hops | {} |\n",
        i.max_hops + 1,
        h.arbs_at_cap_plus_one
    ));
    out.push_str(&format!(
        "| …already fully in universe | {} |\n",
        h.arbs_at_cap_plus_one_in_universe
    ));
    if let Some(n) = h.arbs_at_cap_plus_one_with_candidates {
        out.push_str(&format!("| …with the largest loadable set added | {n} |\n"));
    }
    out.push_str(&format!(
        "| Cycles at cap {} | {} |\n",
        i.max_hops, h.cycle_count_at_cap
    ));
    out.push_str(&format!(
        "| Cycles at cap {} | {} ({:.1}×) |\n",
        i.max_hops + 1,
        h.cycle_count_at_cap_plus_one,
        h.cycle_growth_factor
    ));
    out.push_str("| Cold start impact | none (same pool set) |\n\n");
    out.push_str(&format!("{}\n\n", h.note));

    out.push_str("## Missing pools by appearance\n\n");
    out.push_str(
        "| Pool | Venue | Pair | Kind | In-scope arbs | Sole blocker of | Hop positions | TVL (WMNT) | Exclusion |\n\
         | --- | --- | --- | --- | ---: | ---: | --- | ---: | --- |\n",
    );
    for m in &report.missing_pools {
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | {} | {} | {} | {} | `{}` |\n",
            m.pool,
            dash(m.venue.as_ref()),
            dash(m.pair.as_ref()),
            dash(m.kind.as_ref()),
            m.appears_in_in_scope_arbs,
            m.sole_blocker_of,
            hop_positions_display(&m.hop_positions),
            tvl_display(m.tvl_wmnt_wei.as_ref()),
            m.exclusion_cause.as_str(),
        ));
    }
    out.push_str(
        "\nHop positions read `position:count` over the ordered path — a pool that only ever \
         appears at hop 1 is a different kind of gap from a mid-cycle one.\n",
    );

    out.push_str("\n## Notes\n\n");
    for n in &report.notes {
        out.push_str(&format!("* {n}\n"));
    }
    out
}

fn push_ranking(out: &mut String, heading: &str, steps: &[UnlockStep]) {
    out.push_str(&format!("{heading}\n\n"));
    if steps.is_empty() {
        out.push_str("_No candidate pools in this class._\n\n");
        return;
    }
    out.push_str(
        "| # | Pool | Venue | Pair | TVL (WMNT) | Marginal | Cumulative | Reachable | Selection | Venue status |\n\
         | ---: | --- | --- | --- | ---: | ---: | ---: | ---: | --- | --- |\n",
    );
    for s in steps {
        out.push_str(&format!(
            "| {} | `{}` | {} | {} | {} | +{} | {} | {} ({:.1}%) | `{}` | `{}` |\n",
            s.rank,
            s.pool,
            dash(s.venue.as_ref()),
            dash(s.pair.as_ref()),
            tvl_display(s.tvl_wmnt_wei.as_ref()),
            s.marginal_arbs_unlocked,
            s.cumulative_arbs_unlocked,
            s.cumulative_reachable,
            s.cumulative_reachable_pct,
            s.selection.as_str(),
            s.venue_status.as_str(),
        ));
    }
    out.push_str(
        "\n`frequency_fallback` steps unlock nothing alone — they close one side of a \
         multi-pool gap.\n\n",
    );
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::missed_arbs::{
        CauseRecount, ExclusionBreakdown, HopCapPricing, InScopeBaseline, ReportInputs,
        ResidualBound, Verdict, MISSED_ARB_REPORT_SCHEMA_VERSION,
    };
    use std::collections::BTreeMap;

    fn report(tvl_measured: bool, tvl_requested: bool, tvl_block: Option<u64>) -> MissedArbReport {
        MissedArbReport {
            schema_version: MISSED_ARB_REPORT_SCHEMA_VERSION.into(),
            inputs: ReportInputs {
                arb_dataset: "arbs.jsonl".into(),
                census_dataset: "census.json".into(),
                universe_pool_count: 130,
                universe_fingerprint: None,
                universe_snapshot_block: None,
                settlement_asset: "0xwmnt".into(),
                max_hops: 3,
                min_tvl_wmnt_wei: "1000".into(),
                block_from: None,
                block_to: None,
                tvl_block,
                tvl_measured,
                tvl_requested,
                source_rows: 1,
                skipped_empty_path: 0,
            },
            cause_recount: CauseRecount {
                total_events: 1,
                aggregator_misclass: 0,
                out_of_scope_hop_cap: 0,
                out_of_scope_non_wmnt_settlement: 0,
                not_in_universe: 1,
                in_universe_in_scope: 0,
                analysis_denominator: 1,
                not_in_universe_pct: 100.0,
                shares_pct: BTreeMap::from([("not_in_universe".into(), 100.0)]),
            },
            residual_bound: ResidualBound {
                residual_count: 0,
                residual_events_with_missing_pool: 0,
                residual_events_fully_in_universe: 0,
                can_hide_not_in_universe: false,
                statement: "bound".into(),
            },
            baseline: InScopeBaseline {
                in_scope_arbs: 1,
                reachable_now: 0,
                reachable_now_pct: 0.0,
                blocked_by_missing_pools: 1,
                distinct_pools_in_in_scope_arbs: 1,
                distinct_pools_held: 0,
                distinct_missing_pools: 1,
            },
            exclusions: ExclusionBreakdown {
                pools_by_cause: BTreeMap::new(),
                pools_by_venue_status: BTreeMap::new(),
                work_required_by_venue_status: BTreeMap::new(),
                in_scope_arbs_touched_by_cause: BTreeMap::new(),
                in_scope_arbs_gap_fully_loadable: 0,
            },
            ranking_loadable: Vec::new(),
            ranking_any_venue: Vec::new(),
            missing_pools: Vec::new(),
            candidate_sets: Vec::new(),
            hop_cap: HopCapPricing {
                arbs_above_cap_by_hop: BTreeMap::new(),
                arbs_above_cap_total: 0,
                arbs_at_cap_plus_one: 0,
                arbs_at_cap_plus_one_in_universe: 0,
                arbs_at_cap_plus_one_with_candidates: None,
                cycle_count_at_cap: 1,
                cycle_count_at_cap_plus_one: 2,
                cycle_growth_factor: 2.0,
                note: "note".into(),
            },
            verdict: Verdict {
                best_loadable_reachable: 0,
                best_loadable_reachable_pct: 0.0,
                best_any_venue_reachable: 0,
                best_any_venue_reachable_pct: 0.0,
                best_loadable_arbs_per_day: 0.0,
                reachable_universe_exists: false,
                non_trivial_arbs_threshold_pct: 25.0,
                statement: "verdict".into(),
                economics_caveat: "caveat".into(),
            },
            notes: Vec::new(),
        }
    }

    /// The TVL header is the one place an unmeasured valuation could read as a
    /// clean floor, and the flag name drifted once already — pin all three states.
    #[test]
    fn tvl_header_distinguishes_measured_requested_and_absent() {
        let measured = render_markdown(&report(true, true, Some(42)));
        assert!(measured.contains("measured at block 42"), "{measured}");

        let requested = render_markdown(&report(false, true, None));
        assert!(
            requested.contains("requested but not measured"),
            "a requested-but-empty pass must not read as unmeasured: {requested}"
        );
        assert!(requested.contains("census gap"));

        let absent = render_markdown(&report(false, false, None));
        assert!(
            absent.contains("`--measure-tvl`"),
            "the header must name the current flag: {absent}"
        );
        assert!(!absent.contains("--rpc-url"), "stale flag name: {absent}");
    }

    #[test]
    fn cause_shares_come_from_the_report_not_a_recomputation() {
        // The renderer must format `shares_pct`; a value the analysis did not
        // emit reads as 0.0 rather than being silently re-derived.
        let mut r = report(true, true, Some(1));
        r.cause_recount
            .shares_pct
            .insert("not_in_universe".into(), 12.5);
        let md = render_markdown(&r);
        assert!(md.contains("| `not_in_universe` | 1 | 12.5% |"), "{md}");
    }

    #[test]
    fn hop_positions_render_as_position_colon_count() {
        assert_eq!(
            hop_positions_display(&BTreeMap::from([(1, 12), (3, 4)])),
            "1:12 3:4"
        );
        assert_eq!(hop_positions_display(&BTreeMap::new()), "—");
    }

    #[test]
    fn tvl_display_truncates_to_whole_wmnt_and_dashes_when_absent() {
        let wei = (U256::from(918u64) * U256::from(10u64).pow(U256::from(18u64))
            + U256::from(999u64))
        .to_string();
        assert_eq!(tvl_display(Some(&wei)), "918");
        assert_eq!(tvl_display(None), "—");
    }
}
