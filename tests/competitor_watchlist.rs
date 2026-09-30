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

    let rule = &doc["selection"]["group_rule"];
    let group_name = rule["name"].as_str().expect("group name");
    let range = rule["arb_tx_count_30d_between"].as_array().expect("range");
    let (lo, hi) = (range[0].as_u64().unwrap(), range[1].as_u64().unwrap());
    let tx_index_p50 = rule["tx_index_p50"].as_f64().expect("tx_index_p50");

    let members = doc["members"].as_array().expect("members");
    let mut seen = HashSet::new();
    let mut covered = 0u64;
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
        u64_at(m, "arb_tx_count_7d");
        u64_at(m, "gas_used_p50");
        assert!(n30 > 0, "{addr}");
        covered += n30;
        match m["group"].as_str() {
            Some("individual") => {}
            Some(g) if g == group_name => {
                assert!((lo..=hi).contains(&n30), "{addr} outside the group count range");
                assert_eq!(m["tx_index_p10_p50_p90"][1].as_f64(), Some(tx_index_p50), "{addr}");
            }
            other => panic!("{addr}: unknown group {other:?}"),
        }
    }

    let coverage = &doc["coverage"];
    assert_eq!(u64_at(coverage, "members"), members.len() as u64);
    assert_eq!(u64_at(coverage, "arbs_30d_covered"), covered);
    assert_eq!(
        u64_at(coverage, "arbs_30d_total"),
        u64_at(&doc["window_totals"], "arbs_30d")
    );
    assert!(covered <= u64_at(coverage, "arbs_30d_total"));
}
