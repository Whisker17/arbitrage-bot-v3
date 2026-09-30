//! WHI-1581: the committed competitor watchlist is well-formed and internally
//! consistent, so peer-comparison tooling can trust it without re-checking.

use std::collections::HashSet;

use serde_json::Value;

fn watchlist() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("config/competitors/mantle_top_arbitrageurs.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("watchlist readable"))
        .expect("watchlist is JSON")
}

fn u64_at(v: &Value, key: &str) -> u64 {
    v[key]
        .as_u64()
        .unwrap_or_else(|| panic!("`{key}` missing or not an unsigned integer in {v}"))
}

fn f64_pair(v: &Value, key: &str) -> (f64, f64) {
    let a = v[key].as_array().unwrap_or_else(|| panic!("`{key}` missing"));
    assert_eq!(a.len(), 2, "`{key}` must be [lo, hi]");
    (a[0].as_f64().unwrap(), a[1].as_f64().unwrap())
}

/// The `[key, count]` pairs of a top-k histogram, highest first.
fn top(m: &Value, key: &str) -> Vec<(String, u64)> {
    m[key]
        .as_array()
        .unwrap_or_else(|| panic!("`{key}` missing"))
        .iter()
        .map(|e| (e[0].as_str().unwrap().to_string(), e[1].as_u64().unwrap()))
        .collect()
}

#[test]
fn competitor_watchlist_is_well_formed_and_consistent() {
    let doc = watchlist();
    assert_eq!(doc["schema"], "whisker-arb/competitor-watchlist/v1");
    assert_eq!(u64_at(&doc, "chain_id"), 5000);
    for source in ["profile", "recent"] {
        let s = &doc["source"][source];
        assert!(u64_at(s, "query_id") > 0, "{source}");
        assert!(
            s["execution_id"].as_str().is_some_and(|e| !e.is_empty()),
            "{source} execution id"
        );
    }

    let totals = &doc["window_totals"];
    let (senders_30d, arbs_30d) = (u64_at(totals, "senders_30d"), u64_at(totals, "arbs_30d"));
    let arbs_7d = u64_at(totals, "arbs_7d");
    u64_at(totals, "senders_7d");

    let selection = &doc["selection"];
    let top_n = u64_at(selection, "top_n");
    let rule = &selection["group_rule"];
    let group_name = rule["name"].as_str().expect("group name");
    let (lo, hi) = f64_pair(rule, "arb_tx_count_30d_between");
    let (tip_lo, tip_hi) = f64_pair(rule, "effective_tip_gwei_p50_between");
    let tx_type_only = rule["tx_type_only"].as_str().expect("tx_type_only");
    let settlement_prefix = rule["settlement_only_prefix"].as_str().expect("settlement prefix");
    let dominant_mix = rule["dominant_hop_mix"].as_str().expect("dominant_hop_mix");

    let members = doc["members"].as_array().expect("members");
    assert!(!members.is_empty(), "an empty watchlist is unusable");
    let mut seen = HashSet::new();
    let (mut covered_30d, mut covered_7d) = (0u64, 0u64);
    let (mut individuals, mut group_members) = (Vec::new(), Vec::new());
    for m in members {
        let addr = m["bot_address"].as_str().expect("bot_address");
        assert!(
            addr.len() == 42
                && addr.starts_with("0x")
                && addr[2..].chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "malformed (or not lower-case) address {addr}"
        );
        assert!(seen.insert(addr.to_string()), "duplicate address {addr}");
        let n30 = u64_at(m, "arb_tx_count_30d");
        let n7 = u64_at(m, "arb_tx_count_7d");
        u64_at(m, "gas_used_p50");
        assert!(n30 > 0, "{addr}");
        covered_30d += n30;
        covered_7d += n7;
        match m["group"].as_str() {
            Some("individual") => individuals.push(n30),
            Some(g) if g == group_name => {
                // The complete stored predicate, checked on the recorded fields.
                assert!((lo..=hi).contains(&(n30 as f64)), "{addr}: count outside band");
                let types = top(m, "tx_type");
                assert!(
                    types.len() == 1 && types[0].0 == tx_type_only,
                    "{addr}: tx types {types:?}"
                );
                let settle = top(m, "settlement_top");
                assert!(
                    settle.len() == 1 && settle[0].0.starts_with(settlement_prefix),
                    "{addr}: settlement {settle:?}"
                );
                let tip_p50 = m["effective_tip_gwei_p10_p50_p90"][1].as_f64().expect("tip p50");
                assert!((tip_lo..=tip_hi).contains(&tip_p50), "{addr}: tip p50 {tip_p50}");
                assert_eq!(top(m, "hop_mix_top")[0].0, dominant_mix, "{addr}: dominant mix");
                group_members.push(n30);
            }
            other => panic!("{addr}: unknown group {other:?}"),
        }
    }

    assert_eq!(individuals.len() as u64, top_n, "top_n individuals");
    assert!(!group_members.is_empty(), "group rule selected nobody");
    let min_individual = individuals.iter().copied().min().unwrap();
    assert!(
        group_members.iter().all(|n| *n <= min_individual),
        "a group member outranks an individual; the top_n selection is wrong"
    );

    let coverage = &doc["coverage"];
    assert_eq!(u64_at(coverage, "members"), members.len() as u64);
    assert_eq!(u64_at(coverage, "individuals"), individuals.len() as u64);
    assert_eq!(u64_at(coverage, "group_members"), group_members.len() as u64);
    assert_eq!(u64_at(coverage, "arbs_30d_covered"), covered_30d);
    assert_eq!(u64_at(coverage, "arbs_7d_covered"), covered_7d);
    assert_eq!(u64_at(coverage, "arbs_30d_total"), arbs_30d);
    assert_eq!(u64_at(coverage, "arbs_7d_total"), arbs_7d);
    assert!(covered_30d <= arbs_30d && covered_7d <= arbs_7d);
    assert!(members.len() as u64 <= senders_30d);
}
