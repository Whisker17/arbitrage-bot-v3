//! Crash-consistent loss ledger and consecutive-revert streak (WHI-524).

use alloy::primitives::{B256, U256};
use serde::{Deserialize, Serialize};

use super::config::BreakerConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum TerminalKind {
    Execute = 0,
    Cancel = 1,
}

impl TerminalKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Execute),
            1 => Some(Self::Cancel),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountingRecord {
    pub nonce: u64,
    pub tx_hash: B256,
    pub block_number: u64,
    pub block_hash: B256,
    pub kind: TerminalKind,
    pub success: bool,
    pub actual_cost: U256,
    pub execution_layer_only: bool,
    pub onchain_min_profit: U256,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReversalRecord {
    pub nonce: u64,
    pub tx_hash: B256,
    pub block_number: u64,
    pub block_hash: B256,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreakEntry {
    pub nonce: u64,
    pub tx_hash: B256,
    pub block_number: u64,
    pub block_hash: B256,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreakerStats {
    pub consecutive_reverts: u32,
    pub window_loss_wei: U256,
    pub charged_entries: u64,
    pub paused: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LedgerState {
    /// Canonical charged keys (nonce, tx_hash) → accounting.
    charges: std::collections::BTreeMap<(u64, B256), AccountingRecord>,
    /// Full consecutive Execute-failure tail since last Execute success.
    streak_tail: Vec<StreakEntry>,
    paused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChargeEffect {
    Applied { trip_streak: bool, trip_loss: bool, incomplete_fee: bool },
    Idempotent,
}

impl LedgerState {
    pub fn stats(&self, cfg: &BreakerConfig, head_block: u64) -> BreakerStats {
        BreakerStats {
            consecutive_reverts: self.streak_tail.len() as u32,
            window_loss_wei: self.window_loss(cfg, head_block),
            charged_entries: self.charges.len() as u64,
            paused: self.paused,
        }
    }

    pub fn streak_tail(&self) -> &[StreakEntry] {
        &self.streak_tail
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    pub fn window_loss(&self, cfg: &BreakerConfig, head_block: u64) -> U256 {
        let mut sum = U256::ZERO;
        for rec in self.charges.values() {
            if head_block.saturating_sub(rec.block_number) < cfg.loss_window_blocks {
                sum = sum.saturating_add(rec.actual_cost);
            }
        }
        sum
    }

    /// Charge a canonical-included receipt. Non-included callers must not invoke this.
    pub fn charge(
        &mut self,
        cfg: &BreakerConfig,
        record: AccountingRecord,
        head_block: u64,
    ) -> ChargeEffect {
        let key = (record.nonce, record.tx_hash);
        if self.charges.contains_key(&key) {
            return ChargeEffect::Idempotent;
        }
        let incomplete_fee = record.execution_layer_only;
        self.charges.insert(key, record.clone());
        self.apply_streak(&record);
        let trip_streak = self.streak_tail.len() as u32 >= cfg.max_consecutive_reverts;
        let trip_loss = self.window_loss(cfg, head_block) > U256::from(cfg.max_loss_per_window_wei);
        if trip_streak || trip_loss || incomplete_fee {
            self.paused = true;
        }
        ChargeEffect::Applied {
            trip_streak,
            trip_loss,
            incomplete_fee,
        }
    }

    pub fn reverse(&mut self, reversal: &ReversalRecord) -> bool {
        let key = (reversal.nonce, reversal.tx_hash);
        let removed = self.charges.remove(&key).is_some();
        if removed {
            self.recompute_streak_from_charges();
        }
        removed
    }

    fn apply_streak(&mut self, record: &AccountingRecord) {
        match record.kind {
            TerminalKind::Cancel => {
                // Cancels do not clear or extend the Execute failure streak.
            }
            TerminalKind::Execute if record.success => {
                self.streak_tail.clear();
            }
            TerminalKind::Execute => {
                self.streak_tail.push(StreakEntry {
                    nonce: record.nonce,
                    tx_hash: record.tx_hash,
                    block_number: record.block_number,
                    block_hash: record.block_hash,
                });
                self.streak_tail.sort_by_key(|e| e.nonce);
            }
        }
    }

    fn recompute_streak_from_charges(&mut self) {
        let mut executes: Vec<&AccountingRecord> = self
            .charges
            .values()
            .filter(|r| r.kind == TerminalKind::Execute)
            .collect();
        executes.sort_by_key(|r| r.nonce);
        self.streak_tail.clear();
        for rec in executes {
            if rec.success {
                self.streak_tail.clear();
            } else {
                self.streak_tail.push(StreakEntry {
                    nonce: rec.nonce,
                    tx_hash: rec.tx_hash,
                    block_number: rec.block_number,
                    block_hash: rec.block_hash,
                });
            }
        }
    }

    /// Drop charged entries that fall outside the loss window (does not touch streak).
    pub fn prune_window(&mut self, cfg: &BreakerConfig, head_block: u64) {
        self.charges.retain(|_, rec| {
            head_block.saturating_sub(rec.block_number) < cfg.loss_window_blocks
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::breaker::BreakerConfig;

    fn cfg() -> BreakerConfig {
        BreakerConfig::with_caps(1_000, 1, 1)
    }

    fn revert(nonce: u64, block: u64) -> AccountingRecord {
        AccountingRecord {
            nonce,
            tx_hash: B256::from(U256::from(nonce)),
            block_number: block,
            block_hash: B256::repeat_byte((block % 255) as u8),
            kind: TerminalKind::Execute,
            success: false,
            actual_cost: U256::from(100u64),
            execution_layer_only: false,
            onchain_min_profit: U256::ZERO,
        }
    }

    #[test]
    fn charges_full_cost_and_trips_on_streak() {
        let cfg = cfg();
        let mut ledger = LedgerState::default();
        assert!(matches!(
            ledger.charge(&cfg, revert(1, 10), 10),
            ChargeEffect::Applied {
                trip_streak: false,
                ..
            }
        ));
        ledger.charge(&cfg, revert(2, 11), 11);
        let effect = ledger.charge(&cfg, revert(3, 12), 12);
        assert!(matches!(
            effect,
            ChargeEffect::Applied {
                trip_streak: true,
                ..
            }
        ));
        assert!(ledger.is_paused());
        assert_eq!(ledger.stats(&cfg, 12).consecutive_reverts, 3);
        assert_eq!(ledger.stats(&cfg, 12).window_loss_wei, U256::from(300u64));
    }

    #[test]
    fn repeat_charge_is_idempotent() {
        let cfg = cfg();
        let mut ledger = LedgerState::default();
        let rec = revert(1, 10);
        ledger.charge(&cfg, rec.clone(), 10);
        assert_eq!(ledger.charge(&cfg, rec, 10), ChargeEffect::Idempotent);
        assert_eq!(ledger.stats(&cfg, 10).charged_entries, 1);
    }

    #[test]
    fn success_and_cancel_charge_full_cost_success_clears_streak() {
        let cfg = cfg();
        let mut ledger = LedgerState::default();
        ledger.charge(&cfg, revert(1, 10), 10);
        let success = AccountingRecord {
            success: true,
            actual_cost: U256::from(50u64),
            onchain_min_profit: U256::from(10u64),
            ..revert(2, 11)
        };
        ledger.charge(&cfg, success, 11);
        assert_eq!(ledger.stats(&cfg, 11).consecutive_reverts, 0);
        let cancel = AccountingRecord {
            kind: TerminalKind::Cancel,
            success: true,
            actual_cost: U256::from(20u64),
            ..revert(3, 12)
        };
        ledger.charge(&cfg, cancel, 12);
        assert_eq!(ledger.stats(&cfg, 12).window_loss_wei, U256::from(170u64));
        assert_eq!(ledger.stats(&cfg, 12).consecutive_reverts, 0);
    }

    #[test]
    fn incomplete_fee_pauses() {
        let cfg = cfg();
        let mut ledger = LedgerState::default();
        let mut rec = revert(1, 10);
        rec.execution_layer_only = true;
        let effect = ledger.charge(&cfg, rec, 10);
        assert!(matches!(
            effect,
            ChargeEffect::Applied {
                incomplete_fee: true,
                ..
            }
        ));
        assert!(ledger.is_paused());
    }

    #[test]
    fn reversal_of_non_last_failure_recomputes_streak() {
        let cfg = cfg();
        let mut ledger = LedgerState::default();
        ledger.charge(&cfg, revert(1, 10), 12);
        ledger.charge(&cfg, revert(2, 11), 12);
        ledger.charge(&cfg, revert(3, 12), 12);
        assert_eq!(ledger.stats(&cfg, 12).consecutive_reverts, 3);
        ledger.reverse(&ReversalRecord {
            nonce: 2,
            tx_hash: B256::from(U256::from(2u64)),
            block_number: 11,
            block_hash: B256::repeat_byte(11),
            reason: "reorg".into(),
        });
        assert_eq!(ledger.stats(&cfg, 12).consecutive_reverts, 2);
        assert_eq!(ledger.streak_tail()[0].nonce, 1);
        assert_eq!(ledger.streak_tail()[1].nonce, 3);
    }

    #[test]
    fn streak_survives_window_prune() {
        let mut cfg = cfg();
        cfg.loss_window_blocks = 5;
        cfg.max_consecutive_reverts = 3;
        let mut ledger = LedgerState::default();
        ledger.charge(&cfg, revert(1, 1), 1);
        ledger.charge(&cfg, revert(2, 2), 2);
        ledger.prune_window(&cfg, 20);
        assert_eq!(ledger.stats(&cfg, 20).window_loss_wei, U256::ZERO);
        assert_eq!(ledger.stats(&cfg, 20).consecutive_reverts, 2);
        let effect = ledger.charge(&cfg, revert(3, 20), 20);
        assert!(matches!(
            effect,
            ChargeEffect::Applied {
                trip_streak: true,
                ..
            }
        ));
    }

    #[test]
    fn shuffled_arrival_is_nonce_deterministic_for_streak() {
        let cfg = cfg();
        let mut a = LedgerState::default();
        let mut b = LedgerState::default();
        for rec in [revert(3, 12), revert(1, 10), revert(2, 11)] {
            a.charge(&cfg, rec, 12);
        }
        for rec in [revert(1, 10), revert(2, 11), revert(3, 12)] {
            b.charge(&cfg, rec, 12);
        }
        assert_eq!(a.streak_tail(), b.streak_tail());
    }
}
