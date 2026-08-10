//! Observed on-chain arbitrage coverage (WHI-906).
//!
//! Offline analysis: intersect real atomic-arb pool paths against a frozen
//! universe, then rank candidate additions by **marginal fully-executable
//! gain**. The TVL floor remains a safety filter — selection for where
//! arbitrage happens is driven by this ranking, not by TVL.
//!
//! Dataset stays external; this module is pure logic + file I/O helpers.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Schema version for [`ObservedArbCoverage`] embedded in universe meta.
pub const OBSERVED_ARB_COVERAGE_SCHEMA_VERSION: u32 = 1;

/// Report schema written by the CLI / `universe_gen` hook.
pub const ARB_COVERAGE_REPORT_SCHEMA_VERSION: u32 = 1;

/// Kinds the WHI-906 acceptance criteria treat as adapter-required
/// (`algebra`, `izi`, `solidly`). Drop-in today: `v2`, `v3`, `lb`.
///
/// Note: WHI-765 later reclassified some Algebra-tagged factories as UniV3
/// drop-ins; that venue matrix does not change the separation contract of
/// this report — operators still see `adapter_class` derived from census
/// `kind` so the ranking can be filtered without re-reading the matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterClass {
    DropIn,
    AdapterRequired,
    Unknown,
}

impl AdapterClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DropIn => "drop_in",
            Self::AdapterRequired => "adapter_required",
            Self::Unknown => "unknown",
        }
    }
}

/// Classify a census AMM-family label.
pub fn adapter_class(kind: Option<&str>) -> AdapterClass {
    match kind.map(|k| k.trim().to_ascii_lowercase()).as_deref() {
        Some("v2") | Some("v3") | Some("lb") => AdapterClass::DropIn,
        Some("algebra") | Some("izi") | Some("solidly") => AdapterClass::AdapterRequired,
        Some("") | None => AdapterClass::Unknown,
        Some(_) => AdapterClass::Unknown,
    }
}

/// One arbitrage path: ordered pool addresses (any casing; normalized on load).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArbPath {
    pub pools: Vec<String>,
    pub block: Option<u64>,
}

/// Census row for a pool address (subset of `pool_census.json`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolCensusEntry {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub factory: Option<String>,
    #[serde(default, rename = "t0")]
    pub token0: Option<String>,
    #[serde(default, rename = "t1")]
    pub token1: Option<String>,
    #[serde(default, rename = "s0")]
    pub symbol0: Option<String>,
    #[serde(default, rename = "s1")]
    pub symbol1: Option<String>,
    /// Swap events observed on this pool over the census window. Activity
    /// signal only — never a substitute for TVL (WHI-999).
    #[serde(default)]
    pub swaps: Option<u64>,
}

impl PoolCensusEntry {
    pub fn pair_label(&self) -> Option<String> {
        match (
            self.symbol0.as_deref().filter(|s| !s.is_empty()),
            self.symbol1.as_deref().filter(|s| !s.is_empty()),
        ) {
            (Some(a), Some(b)) => Some(format!("{a}/{b}")),
            _ => None,
        }
    }
}

/// Intersection summary of held pools vs observed arb paths.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoverageSummary {
    pub total_arbs: usize,
    pub touching: usize,
    pub fully_executable: usize,
    pub distinct_pools: usize,
    pub distinct_pools_covered: usize,
    pub block_from: Option<u64>,
    pub block_to: Option<u64>,
}

impl CoverageSummary {
    pub fn touching_pct(&self) -> f64 {
        pct(self.touching, self.total_arbs)
    }

    pub fn fully_executable_pct(&self) -> f64 {
        pct(self.fully_executable, self.total_arbs)
    }
}

/// One step of the greedy marginal-gain ranking.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GreedyStep {
    pub rank: usize,
    pub pool: String,
    pub marginal_gain: usize,
    pub cumulative_fully: usize,
    pub cumulative_pct: f64,
    pub kind: Option<String>,
    pub factory: Option<String>,
    pub pair: Option<String>,
    pub adapter_class: AdapterClass,
}

/// Compact record stored next to the universe fingerprint (meta sidecar).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservedArbCoverage {
    pub schema_version: u32,
    pub total_arbs: u64,
    pub touching: u64,
    pub fully_executable: u64,
    /// Percent of arbs that touch ≥1 held pool (one decimal, e.g. 49.9).
    pub touching_pct: f64,
    /// Percent fully executable (one decimal, e.g. 4.4).
    pub fully_executable_pct: f64,
    pub distinct_pools_in_arbs: u64,
    pub distinct_pools_covered: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_from: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_to: Option<u64>,
    /// Operator label only (basename), never a secret path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arb_dataset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub greedy_top_n: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub greedy_cumulative_pct_at_top_n: Option<f64>,
}

impl ObservedArbCoverage {
    pub fn from_summary(
        summary: &CoverageSummary,
        arb_dataset: Option<String>,
        greedy: &[GreedyStep],
    ) -> Self {
        let (greedy_top_n, greedy_cum) = if greedy.is_empty() {
            (None, None)
        } else {
            (
                Some(greedy.len() as u32),
                greedy.last().map(|s| round1(s.cumulative_pct)),
            )
        };
        Self {
            schema_version: OBSERVED_ARB_COVERAGE_SCHEMA_VERSION,
            total_arbs: summary.total_arbs as u64,
            touching: summary.touching as u64,
            fully_executable: summary.fully_executable as u64,
            touching_pct: round1(summary.touching_pct()),
            fully_executable_pct: round1(summary.fully_executable_pct()),
            distinct_pools_in_arbs: summary.distinct_pools as u64,
            distinct_pools_covered: summary.distinct_pools_covered as u64,
            block_from: summary.block_from,
            block_to: summary.block_to,
            arb_dataset,
            greedy_top_n,
            greedy_cumulative_pct_at_top_n: greedy_cum,
        }
    }
}

/// Full operator report (JSON).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArbCoverageReport {
    pub schema_version: u32,
    pub universe_pool_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub universe_fingerprint: Option<String>,
    pub coverage: CoverageSummary,
    pub touching_pct: f64,
    pub fully_executable_pct: f64,
    pub greedy: Vec<GreedyStep>,
    pub greedy_drop_in: Vec<GreedyStep>,
    pub greedy_adapter_required: Vec<GreedyStep>,
    pub greedy_unknown: Vec<GreedyStep>,
    /// Compact record suitable for embedding next to the universe fingerprint.
    pub observed: ObservedArbCoverage,
}

#[derive(Debug, Error)]
pub enum ArbCoverageError {
    #[error("I/O error on {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("CSV error on {path}: {source}")]
    Csv {
        path: String,
        #[source]
        source: csv::Error,
    },
    #[error("JSON error on {path}: {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("{0}")]
    Other(String),
}

/// Lowercase hex address; strips whitespace. Does not validate length.
pub fn normalize_address(s: &str) -> String {
    s.trim().to_ascii_lowercase()
}

/// Percent of `part` / `whole` as 0..=100. Empty whole → 0.0.
pub fn pct(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        0.0
    } else {
        (part as f64) * 100.0 / (whole as f64)
    }
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// Intersect held pool set with arb paths.
pub fn compute_coverage(held: &HashSet<String>, arbs: &[ArbPath]) -> CoverageSummary {
    let mut touching = 0usize;
    let mut fully = 0usize;
    let mut distinct: HashSet<String> = HashSet::new();
    let mut block_from: Option<u64> = None;
    let mut block_to: Option<u64> = None;

    for arb in arbs {
        if arb.pools.is_empty() {
            continue;
        }
        let mut any = false;
        let mut all = true;
        for p in &arb.pools {
            distinct.insert(p.clone());
            if held.contains(p) {
                any = true;
            } else {
                all = false;
            }
        }
        if any {
            touching += 1;
        }
        if all {
            fully += 1;
        }
        if let Some(b) = arb.block {
            block_from = Some(block_from.map_or(b, |m| m.min(b)));
            block_to = Some(block_to.map_or(b, |m| m.max(b)));
        }
    }

    let distinct_pools = distinct.len();
    let distinct_pools_covered = distinct.iter().filter(|p| held.contains(*p)).count();

    CoverageSummary {
        total_arbs: arbs.iter().filter(|a| !a.pools.is_empty()).count(),
        touching,
        fully_executable: fully,
        distinct_pools,
        distinct_pools_covered,
        block_from,
        block_to,
    }
}

/// Greedy: at each step add the non-held pool that unlocks the most *additional*
/// fully-covered arbs (last missing pool in the path). Ties break by
/// lexicographically smallest pool address. When no single add unlocks any
/// arb, fall back to the pool appearing most often among remaining missing
/// sets (still with address tie-break) so multi-hop gaps can close.
pub fn greedy_rank(
    held: &HashSet<String>,
    arbs: &[ArbPath],
    census: &HashMap<String, PoolCensusEntry>,
    top_n: usize,
) -> Vec<GreedyStep> {
    if top_n == 0 {
        return Vec::new();
    }

    let total = arbs.iter().filter(|a| !a.pools.is_empty()).count();
    let mut current = held.clone();
    let base_fully = compute_coverage(&current, arbs).fully_executable;

    // Missing pool sets for arbs not yet fully covered.
    let mut missing_lists: Vec<HashSet<String>> = arbs
        .iter()
        .filter(|a| !a.pools.is_empty())
        .filter_map(|a| {
            let miss: HashSet<String> = a
                .pools
                .iter()
                .filter(|p| !current.contains(*p))
                .cloned()
                .collect();
            if miss.is_empty() {
                None
            } else {
                Some(miss)
            }
        })
        .collect();

    let mut steps = Vec::with_capacity(top_n);
    let mut cumulative = base_fully;

    for rank in 1..=top_n {
        // Count last-missing unlocks.
        let mut unlock_counts: BTreeMap<String, usize> = BTreeMap::new();
        for miss in &missing_lists {
            let remaining: HashSet<&String> = miss.difference(&current).collect();
            if remaining.len() == 1 {
                let p = (*remaining.into_iter().next().unwrap()).clone();
                *unlock_counts.entry(p).or_insert(0) += 1;
            }
        }

        let (pool, gain) = if let Some((pool, gain)) = unlock_counts
            .iter()
            .max_by(|(pa, ga), (pb, gb)| ga.cmp(gb).then_with(|| pb.cmp(pa)))
        {
            // max_by: higher gain wins; on equal gain, smaller address wins
            // (pb.cmp(pa) so that when ga==gb and pa < pb, Ordering::Greater
            // keeps pa).
            (pool.clone(), *gain)
        } else {
            // Fallback: most frequent remaining pool.
            let mut appear: BTreeMap<String, usize> = BTreeMap::new();
            for miss in &missing_lists {
                for p in miss.difference(&current) {
                    *appear.entry(p.clone()).or_insert(0) += 1;
                }
            }
            match appear
                .iter()
                .max_by(|(pa, ga), (pb, gb)| ga.cmp(gb).then_with(|| pb.cmp(pa)))
            {
                Some((pool, _)) => (pool.clone(), 0usize),
                None => break,
            }
        };

        current.insert(pool.clone());
        cumulative += gain;

        // Drop arbs now fully covered; shrink remaining missing sets.
        missing_lists.retain_mut(|miss| {
            miss.remove(&pool);
            !miss.is_empty()
        });

        let entry = census.get(&pool);
        let kind = entry.and_then(|e| e.kind.clone());
        let factory = entry.and_then(|e| e.factory.clone());
        let pair = entry.and_then(|e| e.pair_label());
        let class = adapter_class(kind.as_deref());

        steps.push(GreedyStep {
            rank,
            pool,
            marginal_gain: gain,
            cumulative_fully: cumulative,
            cumulative_pct: pct(cumulative, total),
            kind,
            factory,
            pair,
            adapter_class: class,
        });
    }

    steps
}

/// Build the full report (pure).
pub fn build_report(
    held: &HashSet<String>,
    arbs: &[ArbPath],
    census: &HashMap<String, PoolCensusEntry>,
    top_n: usize,
    universe_fingerprint: Option<String>,
    arb_dataset: Option<String>,
) -> ArbCoverageReport {
    let coverage = compute_coverage(held, arbs);
    let greedy = greedy_rank(held, arbs, census, top_n);
    let greedy_drop_in: Vec<_> = greedy
        .iter()
        .filter(|s| s.adapter_class == AdapterClass::DropIn)
        .cloned()
        .collect();
    let greedy_adapter_required: Vec<_> = greedy
        .iter()
        .filter(|s| s.adapter_class == AdapterClass::AdapterRequired)
        .cloned()
        .collect();
    let greedy_unknown: Vec<_> = greedy
        .iter()
        .filter(|s| s.adapter_class == AdapterClass::Unknown)
        .cloned()
        .collect();
    let observed = ObservedArbCoverage::from_summary(&coverage, arb_dataset, &greedy);
    ArbCoverageReport {
        schema_version: ARB_COVERAGE_REPORT_SCHEMA_VERSION,
        universe_pool_count: held.len(),
        universe_fingerprint,
        coverage,
        touching_pct: observed.touching_pct,
        fully_executable_pct: observed.fully_executable_pct,
        greedy,
        greedy_drop_in,
        greedy_adapter_required,
        greedy_unknown,
        observed,
    }
}

// ── I/O ────────────────────────────────────────────────────────────────────

/// Load pool addresses from a unified universe CSV (`pool` column).
pub fn load_held_pools_from_csv(path: &Path) -> Result<HashSet<String>, ArbCoverageError> {
    let file = File::open(path).map_err(|source| ArbCoverageError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut rdr = csv::Reader::from_reader(file);
    let mut held = HashSet::new();
    for rec in rdr.deserialize::<HashMap<String, String>>() {
        let row = rec.map_err(|source| ArbCoverageError::Csv {
            path: path.display().to_string(),
            source,
        })?;
        let pool = row.get("pool").map(|s| s.as_str()).ok_or_else(|| {
            ArbCoverageError::Other(format!(
                "universe CSV missing required `pool` column: {}",
                path.display()
            ))
        })?;
        let n = normalize_address(pool);
        if n.is_empty() {
            return Err(ArbCoverageError::Other(format!(
                "empty pool address in {}",
                path.display()
            )));
        }
        held.insert(n);
    }
    Ok(held)
}

#[derive(Debug, Deserialize)]
struct ArbJsonLine {
    path: Vec<String>,
    #[serde(default)]
    block: Option<u64>,
}

/// Load arb paths from JSONL (`path` array + optional `block`).
pub fn load_arbs_jsonl(path: &Path) -> Result<Vec<ArbPath>, ArbCoverageError> {
    let file = File::open(path).map_err(|source| ArbCoverageError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line.map_err(|source| ArbCoverageError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parsed: ArbJsonLine =
            serde_json::from_str(line).map_err(|source| ArbCoverageError::Json {
                path: format!("{}:line {}", path.display(), i + 1),
                source,
            })?;
        let pools: Vec<String> = parsed
            .path
            .into_iter()
            .map(|p| normalize_address(&p))
            .filter(|p| !p.is_empty())
            .collect();
        if pools.is_empty() {
            continue;
        }
        out.push(ArbPath {
            pools,
            block: parsed.block,
        });
    }
    Ok(out)
}

/// Load `pool_census.json` (map of address → entry).
pub fn load_census(path: &Path) -> Result<HashMap<String, PoolCensusEntry>, ArbCoverageError> {
    let file = File::open(path).map_err(|source| ArbCoverageError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let raw: HashMap<String, PoolCensusEntry> =
        serde_json::from_reader(BufReader::new(file)).map_err(|source| ArbCoverageError::Json {
            path: path.display().to_string(),
            source,
        })?;
    let mut out = HashMap::with_capacity(raw.len());
    for (k, v) in raw {
        out.insert(normalize_address(&k), v);
    }
    Ok(out)
}

/// Write a pretty JSON report.
pub fn write_report(path: &Path, report: &ArbCoverageReport) -> Result<(), ArbCoverageError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|source| ArbCoverageError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }
    }
    let file = File::create(path).map_err(|source| ArbCoverageError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut w = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut w, report).map_err(|source| ArbCoverageError::Json {
        path: path.display().to_string(),
        source,
    })?;
    w.write_all(b"\n").map_err(|source| ArbCoverageError::Io {
        path: path.display().to_string(),
        source,
    })?;
    Ok(())
}

/// Basename label for meta (never a full path).
pub fn dataset_label(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Load inputs, build the report, write JSON. Shared by the CLI and
/// `universe_gen` so the operator path cannot drift.
pub fn run_coverage_report(
    universe_csv: &Path,
    arbs_path: &Path,
    census_path: &Path,
    top_n: usize,
    universe_fingerprint: Option<String>,
    out_path: &Path,
) -> Result<ArbCoverageReport, ArbCoverageError> {
    let held = load_held_pools_from_csv(universe_csv)?;
    let arbs = load_arbs_jsonl(arbs_path)?;
    let census = load_census(census_path)?;
    let report = build_report(
        &held,
        &arbs,
        &census,
        top_n,
        universe_fingerprint,
        Some(dataset_label(arbs_path)),
    );
    write_report(out_path, &report)?;
    Ok(report)
}

/// Companion coverage path: `{stem}.coverage.json` next to the universe CSV.
pub fn coverage_path_for(csv_path: &Path) -> PathBuf {
    if let Some(stem) = csv_path.file_stem() {
        if let Some(parent) = csv_path.parent() {
            return parent.join(format!("{}.coverage.json", stem.to_string_lossy()));
        }
    }
    csv_path.with_extension("coverage.json")
}

/// Human-readable table for stdout.
pub fn format_report_text(report: &ArbCoverageReport) -> String {
    let c = &report.coverage;
    let mut out = String::new();
    out.push_str(&format!(
        "arb coverage (universe pools={})\n",
        report.universe_pool_count
    ));
    if let Some(ref fp) = report.universe_fingerprint {
        out.push_str(&format!("  fingerprint: {fp}\n"));
    }
    if let (Some(a), Some(b)) = (c.block_from, c.block_to) {
        out.push_str(&format!("  blocks: {a}–{b}\n"));
    }
    out.push_str(&format!(
        "  total_arbs:              {}\n\
         \x20 touching (≥1 held):     {} ({:.1} %)\n\
         \x20 fully executable:       {} ({:.1} %)\n\
         \x20 distinct pools in arbs: {}\n\
         \x20 distinct pools covered: {}\n",
        c.total_arbs,
        c.touching,
        report.touching_pct,
        c.fully_executable,
        report.fully_executable_pct,
        c.distinct_pools,
        c.distinct_pools_covered,
    ));
    out.push_str("\ngreedy ranking (marginal fully-executable unlocks):\n");
    out.push_str(
        "  #  pool                                        gain  cum%   class              kind     factory                                    pair\n",
    );
    for s in &report.greedy {
        out.push_str(&format!(
            "  {:>2} {}  {:>4}  {:>5.2}  {:<18} {:<8} {:<42} {}\n",
            s.rank,
            s.pool,
            s.marginal_gain,
            s.cumulative_pct,
            s.adapter_class.as_str(),
            s.kind.as_deref().unwrap_or("—"),
            s.factory.as_deref().unwrap_or("—"),
            s.pair.as_deref().unwrap_or("—"),
        ));
    }
    out.push_str(&format!(
        "\n  split of top {}: drop_in={}  adapter_required={}  unknown={}\n",
        report.greedy.len(),
        report.greedy_drop_in.len(),
        report.greedy_adapter_required.len(),
        report.greedy_unknown.len(),
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn pool(b: u8) -> String {
        format!("0x{:040x}", b as u64)
    }

    fn arb(pools: &[&str]) -> ArbPath {
        ArbPath {
            pools: pools.iter().map(|p| normalize_address(p)).collect(),
            block: Some(100),
        }
    }

    #[test]
    fn adapter_class_separates_required_from_drop_in() {
        assert_eq!(adapter_class(Some("v3")), AdapterClass::DropIn);
        assert_eq!(adapter_class(Some("V2")), AdapterClass::DropIn);
        assert_eq!(adapter_class(Some("lb")), AdapterClass::DropIn);
        assert_eq!(adapter_class(Some("algebra")), AdapterClass::AdapterRequired);
        assert_eq!(adapter_class(Some("izi")), AdapterClass::AdapterRequired);
        assert_eq!(adapter_class(Some("solidly")), AdapterClass::AdapterRequired);
        assert_eq!(adapter_class(Some("weird")), AdapterClass::Unknown);
        assert_eq!(adapter_class(None), AdapterClass::Unknown);
    }

    #[test]
    fn intersection_counts_touching_and_fully_executable() {
        // Held: p1, p2
        let held: HashSet<_> = [pool(1), pool(2)].into_iter().collect();
        let arbs = vec![
            arb(&[&pool(1), &pool(2)]),             // fully
            arb(&[&pool(1), &pool(3)]),             // touching
            arb(&[&pool(4), &pool(5)]),             // none
            arb(&[&pool(2), &pool(1), &pool(1)]),   // fully (dup ok)
        ];
        let s = compute_coverage(&held, &arbs);
        assert_eq!(s.total_arbs, 4);
        assert_eq!(s.fully_executable, 2);
        assert_eq!(s.touching, 3);
        assert_eq!(s.distinct_pools, 5);
        assert_eq!(s.distinct_pools_covered, 2);
        assert_eq!(s.block_from, Some(100));
        assert_eq!(s.block_to, Some(100));
        assert!((s.fully_executable_pct() - 50.0).abs() < 1e-9);
        assert!((s.touching_pct() - 75.0).abs() < 1e-9);
    }

    #[test]
    fn greedy_unlocks_last_missing_pool_first() {
        // Paths: (1,3), (1,4), (2,3), (5,6) — held {1,2}
        // Adding 3 unlocks two arbs (1,3) and (2,3); adding 4 unlocks one.
        let held: HashSet<_> = [pool(1), pool(2)].into_iter().collect();
        let arbs = vec![
            arb(&[&pool(1), &pool(3)]),
            arb(&[&pool(1), &pool(4)]),
            arb(&[&pool(2), &pool(3)]),
            arb(&[&pool(5), &pool(6)]),
        ];
        let mut census = HashMap::new();
        census.insert(
            pool(3),
            PoolCensusEntry {
                kind: Some("algebra".into()),
                factory: Some("0xfac".into()),
                symbol0: Some("A".into()),
                symbol1: Some("B".into()),
                ..Default::default()
            },
        );
        census.insert(
            pool(4),
            PoolCensusEntry {
                kind: Some("v3".into()),
                symbol0: Some("X".into()),
                symbol1: Some("Y".into()),
                ..Default::default()
            },
        );
        census.insert(
            pool(5),
            PoolCensusEntry {
                kind: Some("izi".into()),
                ..Default::default()
            },
        );
        census.insert(
            pool(6),
            PoolCensusEntry {
                kind: Some("izi".into()),
                ..Default::default()
            },
        );

        let steps = greedy_rank(&held, &arbs, &census, 3);
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].pool, pool(3));
        assert_eq!(steps[0].marginal_gain, 2);
        assert_eq!(steps[0].cumulative_fully, 2); // was 0 base
        assert_eq!(steps[0].adapter_class, AdapterClass::AdapterRequired);
        assert_eq!(steps[0].pair.as_deref(), Some("A/B"));

        assert_eq!(steps[1].pool, pool(4));
        assert_eq!(steps[1].marginal_gain, 1);
        assert_eq!(steps[1].adapter_class, AdapterClass::DropIn);

        // After 3 and 4, remaining is (5,6) — no single unlock; fallback pick
        // one of them with gain 0, then next would unlock.
        assert_eq!(steps[2].marginal_gain, 0);
        assert!(steps[2].pool == pool(5) || steps[2].pool == pool(6));
    }

    #[test]
    fn greedy_tie_breaks_by_lexicographic_address() {
        let held: HashSet<_> = [pool(1)].into_iter().collect();
        // Two candidates each unlock exactly one arb.
        let arbs = vec![
            arb(&[&pool(1), &pool(0x0b)]), // 0x0b
            arb(&[&pool(1), &pool(0x0a)]), // 0x0a smaller
        ];
        let steps = greedy_rank(&held, &arbs, &HashMap::new(), 1);
        assert_eq!(steps[0].pool, pool(0x0a));
        assert_eq!(steps[0].marginal_gain, 1);
    }

    #[test]
    fn build_report_splits_adapter_classes() {
        let held: HashSet<_> = [pool(1)].into_iter().collect();
        let arbs = vec![
            arb(&[&pool(1), &pool(2)]),
            arb(&[&pool(1), &pool(3)]),
        ];
        let mut census = HashMap::new();
        census.insert(
            pool(2),
            PoolCensusEntry {
                kind: Some("algebra".into()),
                ..Default::default()
            },
        );
        census.insert(
            pool(3),
            PoolCensusEntry {
                kind: Some("v3".into()),
                ..Default::default()
            },
        );
        let report = build_report(
            &held,
            &arbs,
            &census,
            2,
            Some("0xfp".into()),
            Some("arbs_month.jsonl".into()),
        );
        assert_eq!(report.coverage.total_arbs, 2);
        assert_eq!(report.greedy_adapter_required.len(), 1);
        assert_eq!(report.greedy_drop_in.len(), 1);
        assert_eq!(
            report.observed.arb_dataset.as_deref(),
            Some("arbs_month.jsonl")
        );
        assert_eq!(report.observed.total_arbs, 2);
        assert!(format_report_text(&report).contains("factory"));
    }

    #[test]
    fn load_arbs_and_census_round_trip() {
        let dir = TempDir::new().unwrap();
        let arbs_path = dir.path().join("arbs.jsonl");
        {
            let mut f = File::create(&arbs_path).unwrap();
            writeln!(
                f,
                r#"{{"block":10,"path":["0xAAA0000000000000000000000000000000000001","0xBBB0000000000000000000000000000000000002"]}}"#
            )
            .unwrap();
            writeln!(
                f,
                r#"{{"block":20,"path":["0xaaa0000000000000000000000000000000000001"]}}"#
            )
            .unwrap();
        }
        let arbs = load_arbs_jsonl(&arbs_path).unwrap();
        assert_eq!(arbs.len(), 2);
        assert_eq!(arbs[0].pools[0], "0xaaa0000000000000000000000000000000000001");
        assert_eq!(arbs[0].block, Some(10));

        let census_path = dir.path().join("census.json");
        {
            let mut f = File::create(&census_path).unwrap();
            write!(
                f,
                r#"{{"0xAAA0000000000000000000000000000000000001":{{"kind":"v3","factory":"0xfac","s0":"A","s1":"B"}}}}"#
            )
            .unwrap();
        }
        let census = load_census(&census_path).unwrap();
        assert!(census.contains_key("0xaaa0000000000000000000000000000000000001"));
        assert_eq!(
            census["0xaaa0000000000000000000000000000000000001"]
                .pair_label()
                .as_deref(),
            Some("A/B")
        );
    }

    #[test]
    fn load_held_from_universe_csv() {
        let dir = TempDir::new().unwrap();
        let csv_path = dir.path().join("u.csv");
        {
            let mut f = File::create(&csv_path).unwrap();
            writeln!(f, "protocol,factory,pool,token0,token1,fee_tier,bin_step,creation_block")
                .unwrap();
            writeln!(
                f,
                "agni-v3,0xfac,0xAbC0000000000000000000000000000000000001,0xt0,0xt1,500,,"
            )
            .unwrap();
        }
        let held = load_held_pools_from_csv(&csv_path).unwrap();
        assert!(held.contains("0xabc0000000000000000000000000000000000001"));
    }

    #[test]
    fn load_held_fails_closed_without_pool_column() {
        let dir = TempDir::new().unwrap();
        let csv_path = dir.path().join("bad.csv");
        {
            let mut f = File::create(&csv_path).unwrap();
            writeln!(f, "protocol,factory,address").unwrap();
            writeln!(f, "agni-v3,0xfac,0xabc").unwrap();
        }
        let err = load_held_pools_from_csv(&csv_path).unwrap_err();
        assert!(
            err.to_string().contains("pool"),
            "expected pool-column error, got: {err}"
        );
    }
}
