//! Non-reversible endpoint fingerprints for shareable reports.
//!
//! Never emit raw URLs or API keys into the probe report or stdout.

use alloy::primitives::{keccak256, B256};

/// Stable, non-reversible fingerprint of an endpoint URL.
///
/// Uses keccak256 of the full URL string and truncates to 16 hex chars
/// (8 bytes) — enough to correlate runs without revealing the secret.
pub fn endpoint_fingerprint(url: &str) -> String {
    let digest: B256 = keccak256(url.as_bytes());
    digest.as_slice()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// True if `text` appears to contain a credential-bearing URL fragment that
/// must never land in a report (scheme + host, or common API-key query params).
pub fn text_leaks_endpoint(text: &str, http_url: &str, ws_url: &str) -> bool {
    if !http_url.is_empty() && text.contains(http_url) {
        return true;
    }
    if !ws_url.is_empty() && text.contains(ws_url) {
        return true;
    }
    // Common credential shapes even when full URL was not passed in.
    for marker in ["api_key=", "apikey=", "api-key=", "x-api-key=", "token="] {
        if text.to_ascii_lowercase().contains(marker) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_stable_and_truncated() {
        let a = endpoint_fingerprint("https://secret.example/path?api_key=abc");
        let b = endpoint_fingerprint("https://secret.example/path?api_key=abc");
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn fingerprint_differs_for_different_urls() {
        let a = endpoint_fingerprint("https://a.example");
        let b = endpoint_fingerprint("https://b.example");
        assert_ne!(a, b);
    }

    #[test]
    fn fingerprint_does_not_contain_url_plaintext() {
        let url = "https://mantle.publicnode.com/secret-token-xyz";
        let fp = endpoint_fingerprint(url);
        assert!(!fp.contains("publicnode"));
        assert!(!fp.contains("secret"));
        assert!(!url.contains(&fp)); // digest is not a substring of the URL
    }

    #[test]
    fn leak_detector_catches_full_url_and_api_key_markers() {
        let http = "https://rpc.example/v1/KEY123";
        let ws = "wss://rpc.example/ws/KEY123";
        assert!(text_leaks_endpoint(http, http, ws));
        assert!(text_leaks_endpoint("status api_key=foo", http, ws));
        assert!(!text_leaks_endpoint("ok fingerprint=ab12", http, ws));
    }
}
