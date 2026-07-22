//! Breaker threshold configuration (WHI-524).
//!
//! Defaults for consecutive-reverts / window / poll / broadcast timeout are
//! conservative pending WHI-525 evidence. The three WMNT risk caps are
//! mandatory with no defaults (same pattern as `IntentPolicy` fee caps).

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum BreakerConfigError {
    #[error("{0}")]
    Invalid(String),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BreakerConfig {
    /// Consecutive canonical Execute failures before pause. Default 3.
    pub max_consecutive_reverts: u32,
    /// Sliding window (blocks) for realized receipt-cost loss. Default 300.
    pub loss_window_blocks: u64,
    /// Max CLI/file → in-process pause effect latency (ms). Default 500.
    pub pause_poll_ms: u64,
    /// Bounded RPC handoff timeout (ms). Default 15_000.
    pub broadcast_timeout_ms: u64,
    /// Mandatory window loss cap (wei). No default.
    pub max_loss_per_window_wei: u128,
    /// Mandatory per-tx WMNT input cap (wei). No default.
    pub max_input_per_tx_wmnt_wei: u128,
    /// Mandatory total inventory WMNT cap (wei). No default.
    pub max_total_inventory_wmnt_wei: u128,
}

impl BreakerConfig {
    pub fn with_caps(
        max_loss_per_window_wei: u128,
        max_input_per_tx_wmnt_wei: u128,
        max_total_inventory_wmnt_wei: u128,
    ) -> Self {
        Self {
            max_consecutive_reverts: 3,
            loss_window_blocks: 300,
            pause_poll_ms: 500,
            broadcast_timeout_ms: 15_000,
            max_loss_per_window_wei,
            max_input_per_tx_wmnt_wei,
            max_total_inventory_wmnt_wei,
        }
    }

    pub fn from_env() -> Result<Self, String> {
        let max_loss_per_window_wei = std::env::var("MAX_LOSS_PER_WINDOW_WEI")
            .map_err(|_| "MAX_LOSS_PER_WINDOW_WEI is required".to_string())?
            .parse::<u128>()
            .map_err(|e| format!("MAX_LOSS_PER_WINDOW_WEI: {e}"))?;
        let max_input_per_tx_wmnt_wei = std::env::var("MAX_INPUT_PER_TX_WMNT_WEI")
            .map_err(|_| "MAX_INPUT_PER_TX_WMNT_WEI is required".to_string())?
            .parse::<u128>()
            .map_err(|e| format!("MAX_INPUT_PER_TX_WMNT_WEI: {e}"))?;
        let max_total_inventory_wmnt_wei = std::env::var("MAX_TOTAL_INVENTORY_WMNT_WEI")
            .map_err(|_| "MAX_TOTAL_INVENTORY_WMNT_WEI is required".to_string())?
            .parse::<u128>()
            .map_err(|e| format!("MAX_TOTAL_INVENTORY_WMNT_WEI: {e}"))?;

        let mut cfg = Self::with_caps(
            max_loss_per_window_wei,
            max_input_per_tx_wmnt_wei,
            max_total_inventory_wmnt_wei,
        );
        if let Ok(v) = std::env::var("MAX_CONSECUTIVE_REVERTS") {
            cfg.max_consecutive_reverts = v
                .parse()
                .map_err(|e| format!("MAX_CONSECUTIVE_REVERTS: {e}"))?;
        }
        if let Ok(v) = std::env::var("LOSS_WINDOW_BLOCKS") {
            cfg.loss_window_blocks = v
                .parse()
                .map_err(|e| format!("LOSS_WINDOW_BLOCKS: {e}"))?;
        }
        if let Ok(v) = std::env::var("PAUSE_POLL_MS") {
            cfg.pause_poll_ms = v.parse().map_err(|e| format!("PAUSE_POLL_MS: {e}"))?;
        }
        if let Ok(v) = std::env::var("BROADCAST_TIMEOUT_MS") {
            cfg.broadcast_timeout_ms = v
                .parse()
                .map_err(|e| format!("BROADCAST_TIMEOUT_MS: {e}"))?;
        }
        cfg.validate().map_err(|e| e.to_string())?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), BreakerConfigError> {
        if self.max_consecutive_reverts == 0 {
            return Err(BreakerConfigError::Invalid(
                "max_consecutive_reverts must be > 0".into(),
            ));
        }
        if self.loss_window_blocks == 0 {
            return Err(BreakerConfigError::Invalid(
                "loss_window_blocks must be > 0".into(),
            ));
        }
        if self.pause_poll_ms == 0 {
            return Err(BreakerConfigError::Invalid(
                "pause_poll_ms must be > 0".into(),
            ));
        }
        if self.broadcast_timeout_ms == 0 {
            return Err(BreakerConfigError::Invalid(
                "broadcast_timeout_ms must be > 0".into(),
            ));
        }
        if self.max_loss_per_window_wei == 0 {
            return Err(BreakerConfigError::Invalid(
                "max_loss_per_window_wei must be set (>0)".into(),
            ));
        }
        if self.max_input_per_tx_wmnt_wei == 0 {
            return Err(BreakerConfigError::Invalid(
                "max_input_per_tx_wmnt_wei must be set (>0)".into(),
            ));
        }
        if self.max_total_inventory_wmnt_wei == 0 {
            return Err(BreakerConfigError::Invalid(
                "max_total_inventory_wmnt_wei must be set (>0)".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_conservative_and_caps_required() {
        let cfg = BreakerConfig::with_caps(1, 2, 3);
        assert_eq!(cfg.max_consecutive_reverts, 3);
        assert_eq!(cfg.loss_window_blocks, 300);
        assert_eq!(cfg.pause_poll_ms, 500);
        assert_eq!(cfg.broadcast_timeout_ms, 15_000);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_zeros() {
        let mut cfg = BreakerConfig::with_caps(1, 2, 3);
        cfg.max_consecutive_reverts = 0;
        assert!(cfg.validate().is_err());
        cfg = BreakerConfig::with_caps(1, 2, 3);
        cfg.broadcast_timeout_ms = 0;
        assert!(cfg.validate().is_err());
        cfg = BreakerConfig::with_caps(0, 2, 3);
        assert!(cfg.validate().is_err());
    }
}
