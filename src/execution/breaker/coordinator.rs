//! Durable intent coordinator: sole WAL/sequence owner (WHI-524).

use std::path::Path;
use std::sync::{Arc, Mutex};

use alloy::primitives::{Address, B256, U256};

use super::alert::{AlertEvent, AlertSink};
use super::config::BreakerConfig;
use super::ledger::{
    AccountingRecord, BreakerStats, ChargeEffect, LedgerState, ReversalRecord, TerminalKind,
};
use super::operator::{
    ControlKind, OperatorError, OperatorVerifier, SignedOperatorCommand, StaticOperatorVerifier,
};
use super::pause_ctrl::PauseController;
use super::store::{SecureStore, StoreError};
use super::wal::{
    decode_frame, encode_frame, WalPayload, WalRecord, WalTag,
};
use crate::execution::pause::Paused;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScopeId {
    pub chain_id: u64,
    pub executor: Address,
    pub signer: Address,
}

impl ScopeId {
    pub fn dir_name(&self) -> String {
        format!(
            "{}-{}-{}",
            self.chain_id,
            encode_hex(self.executor.as_slice()),
            encode_hex(self.signer.as_slice())
        )
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Wal(#[from] super::wal::WalError),
    #[error(transparent)]
    Operator(#[from] OperatorError),
    #[error("paused")]
    Paused,
    #[error("restart reconciliation failed: {0}")]
    RestartRevalidation(String),
    #[error("init requires a non-zero canonical anchor")]
    InitAnchorInvalid,
    #[error("operator command scope mismatch")]
    ScopeMismatch,
    #[error("partial wal tail")]
    PartialWalTail,
    #[error("partial wal frame")]
    PartialWalFrame,
    #[error("wal seq gap: expected {expected}, got {got}")]
    WalSeqGap { expected: u64, got: u64 },
}

/// Canonical chain view used at restart to revalidate the WAL anchor and streak tail.
pub trait CanonicalChainView: Send + Sync {
    fn block_hash(&self, block_number: u64) -> Result<Option<B256>, String>;
    fn signer_nonces(&self) -> Result<(u64 /*finalized*/, u64 /*pending*/), String>;
}

/// Baselines committed by signed `Init` (Rev5 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitAnchor {
    pub executor_codehash: B256,
    pub block_number: u64,
    pub block_hash: B256,
    pub finalized_nonce: u64,
    pub pending_nonce: u64,
}

impl InitAnchor {
    pub fn validate(&self) -> Result<(), CoordinatorError> {
        if self.executor_codehash == B256::ZERO || self.block_hash == B256::ZERO {
            return Err(CoordinatorError::InitAnchorInvalid);
        }
        if self.pending_nonce < self.finalized_nonce {
            return Err(CoordinatorError::InitAnchorInvalid);
        }
        Ok(())
    }
}

impl From<Paused> for CoordinatorError {
    fn from(_: Paused) -> Self {
        Self::Paused
    }
}

/// Blocking coordinator. Callers must not hold async/SM locks across its methods.
pub struct DurableIntentCoordinator {
    scope: ScopeId,
    store: SecureStore,
    cfg: BreakerConfig,
    alerts: Arc<dyn AlertSink>,
    pause: PauseController,
    operator: StaticOperatorVerifier,
    state: Mutex<CoordState>,
}

struct CoordState {
    next_seq: u64,
    prev_digest: B256,
    ledger: LedgerState,
    last_control_seq: u64,
    head_block: u64,
    init_anchor: Option<InitAnchor>,
    /// Nonces with durable SubmissionPrepared and no terminal/reversal yet.
    open_submissions: std::collections::BTreeSet<u64>,
}

/// Facade bundling pause + coordinator for service wiring.
pub struct BreakerRuntime {
    pub coordinator: Arc<DurableIntentCoordinator>,
    pub pause: PauseController,
}

impl DurableIntentCoordinator {
    pub fn open(
        root: &Path,
        scope: ScopeId,
        cfg: BreakerConfig,
        alerts: Arc<dyn AlertSink>,
        operator: Address,
    ) -> Result<Arc<Self>, CoordinatorError> {
        let store = SecureStore::open(root, &scope.dir_name())?;
        let pause = PauseController::new(Arc::clone(&alerts), cfg.broadcast_timeout_ms);
        let mut state = CoordState {
            next_seq: 1,
            prev_digest: B256::ZERO,
            ledger: LedgerState::default(),
            last_control_seq: 0,
            head_block: 0,
            init_anchor: None,
            open_submissions: std::collections::BTreeSet::new(),
        };
        let wal = store.read_wal()?;
        let mut initialized = false;
        if !wal.is_empty() {
            replay_wal(&wal, &mut state, &pause, &mut initialized)?;
        }
        // Missing/corrupt semantics: always start paused; virgin needs signed init.
        pause.pause("startup");
        if initialized {
            pause.mark_initialized();
        }
        Ok(Arc::new(Self {
            scope,
            store,
            cfg,
            alerts,
            pause,
            operator: StaticOperatorVerifier { allowed: operator },
            state: Mutex::new(state),
        }))
    }

    pub fn pause_controller(&self) -> PauseController {
        self.pause.clone()
    }

    pub fn breaker_stats(&self) -> BreakerStats {
        let st = self.state.lock().expect("coord");
        st.ledger.stats(&self.cfg, st.head_block)
    }

    pub fn set_head_block(&self, head: u64) {
        self.state.lock().expect("coord").head_block = head;
    }

    pub fn append_submission_prepared(
        &self,
        nonce: u64,
        tx_hash: B256,
        signed_raw_tx: Vec<u8>,
        kind: u8,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
        gas_limit: u64,
        deadline: U256,
        min_profit: U256,
        final_request_digest: B256,
        execution_identity: B256,
    ) -> Result<(), CoordinatorError> {
        self.append(
            WalTag::SubmissionPrepared,
            WalPayload::SubmissionPrepared {
                nonce,
                tx_hash,
                signed_raw_tx,
                kind,
                max_fee_per_gas,
                max_priority_fee_per_gas,
                gas_limit,
                deadline,
                min_profit,
                final_request_digest,
                execution_identity,
            },
        )?;
        self.state
            .lock()
            .expect("coord")
            .open_submissions
            .insert(nonce);
        Ok(())
    }

    pub fn append_broadcast_outcome(
        &self,
        nonce: u64,
        tx_hash: B256,
        accepted: bool,
        detail: impl Into<String>,
    ) -> Result<(), CoordinatorError> {
        self.append(
            WalTag::BroadcastOutcome,
            WalPayload::BroadcastOutcome {
                nonce,
                tx_hash,
                accepted,
                detail: detail.into(),
            },
        )?;
        Ok(())
    }

    pub fn commit_terminal(
        &self,
        record: AccountingRecord,
    ) -> Result<ChargeEffect, CoordinatorError> {
        let kind = record.kind as u8;
        self.append(
            WalTag::TerminalAccounting,
            WalPayload::TerminalAccounting {
                nonce: record.nonce,
                tx_hash: record.tx_hash,
                block_number: record.block_number,
                block_hash: record.block_hash,
                kind,
                success: record.success,
                actual_cost: record.actual_cost,
                execution_layer_only: record.execution_layer_only,
                onchain_min_profit: record.onchain_min_profit,
            },
        )?;
        let mut st = self.state.lock().expect("coord");
        st.open_submissions.remove(&record.nonce);
        let head = st.head_block;
        let effect = st.ledger.charge(&self.cfg, record, head);
        match &effect {
            ChargeEffect::Applied {
                trip_streak,
                trip_loss,
                incomplete_fee,
            } => {
                if *incomplete_fee {
                    self.alerts.alert(AlertEvent::IncompleteFeeAccounting {
                        detail: "execution_layer_only receipt".into(),
                    });
                    self.pause.pause("incomplete_fee");
                    self.append_unlocked(
                        &mut st,
                        WalTag::PauseTrip,
                        WalPayload::PauseTrip {
                            reason: "incomplete_fee".into(),
                            paused: true,
                        },
                    )?;
                } else if *trip_streak {
                    self.alerts.alert(AlertEvent::BreakerTrip {
                        reason: "max_consecutive_reverts".into(),
                    });
                    self.pause.pause("streak");
                    self.append_unlocked(
                        &mut st,
                        WalTag::PauseTrip,
                        WalPayload::PauseTrip {
                            reason: "streak".into(),
                            paused: true,
                        },
                    )?;
                } else if *trip_loss {
                    self.alerts.alert(AlertEvent::BreakerTrip {
                        reason: "max_loss_per_window".into(),
                    });
                    self.pause.pause("loss_window");
                    self.append_unlocked(
                        &mut st,
                        WalTag::PauseTrip,
                        WalPayload::PauseTrip {
                            reason: "loss_window".into(),
                            paused: true,
                        },
                    )?;
                }
            }
            ChargeEffect::Idempotent => {}
        }
        Ok(effect)
    }

    pub fn commit_reversal(&self, reversal: ReversalRecord) -> Result<bool, CoordinatorError> {
        self.append(
            WalTag::Reversal,
            WalPayload::Reversal {
                nonce: reversal.nonce,
                tx_hash: reversal.tx_hash,
                block_number: reversal.block_number,
                block_hash: reversal.block_hash,
                reason: reversal.reason.clone(),
            },
        )?;
        let mut st = self.state.lock().expect("coord");
        let removed = st.ledger.reverse(&reversal);
        if removed {
            self.alerts.alert(AlertEvent::LedgerReversal {
                detail: reversal.reason,
            });
        }
        Ok(removed)
    }

    pub fn apply_operator_command(
        &self,
        signed: SignedOperatorCommand,
        init_anchor: Option<InitAnchor>,
    ) -> Result<Address, CoordinatorError> {
        let actor = self.operator.verify(&signed)?;
        if signed.command.chain_id != self.scope.chain_id
            || signed.command.executor != self.scope.executor
            || signed.command.signer != self.scope.signer
        {
            return Err(CoordinatorError::ScopeMismatch);
        }
        let mut st = self.state.lock().expect("coord");
        if signed.command.control_seq <= st.last_control_seq {
            return Err(OperatorError::SeqRegression.into());
        }
        match signed.command.kind {
            ControlKind::Init => {
                if self.pause.is_initialized() {
                    return Err(OperatorError::InitNotVirgin.into());
                }
                let anchor = init_anchor.ok_or(CoordinatorError::InitAnchorInvalid)?;
                anchor.validate()?;
                let init_payload = WalPayload::Init {
                    chain_id: self.scope.chain_id,
                    executor: self.scope.executor,
                    executor_codehash: anchor.executor_codehash,
                    signer: self.scope.signer,
                    block_number: anchor.block_number,
                    block_hash: anchor.block_hash,
                    finalized_nonce: anchor.finalized_nonce,
                    pending_nonce: anchor.pending_nonce,
                };
                self.append_unlocked(&mut st, WalTag::Init, init_payload)?;
                self.append_unlocked(
                    &mut st,
                    WalTag::OperatorControl,
                    WalPayload::OperatorControl {
                        control_seq: signed.command.control_seq,
                        kind: ControlKind::Init as u8,
                        actor,
                    },
                )?;
                st.init_anchor = Some(anchor);
                st.head_block = anchor.block_number;
                st.last_control_seq = signed.command.control_seq;
                self.pause.mark_initialized();
                self.alerts.alert(AlertEvent::Init {
                    actor: format!("{actor}"),
                });
            }
            ControlKind::Unpause => {
                if !self.pause.is_initialized() {
                    return Err(OperatorError::UnpauseBeforeInit.into());
                }
                self.append_unlocked(
                    &mut st,
                    WalTag::OperatorControl,
                    WalPayload::OperatorControl {
                        control_seq: signed.command.control_seq,
                        kind: ControlKind::Unpause as u8,
                        actor,
                    },
                )?;
                st.last_control_seq = signed.command.control_seq;
                self.pause.unpause(format!("{actor}"))?;
            }
            ControlKind::Recovery => {
                self.append_unlocked(
                    &mut st,
                    WalTag::Recovery,
                    WalPayload::Recovery {
                        control_seq: signed.command.control_seq,
                        actor,
                        detail: signed.command.detail.clone(),
                    },
                )?;
                st.last_control_seq = signed.command.control_seq;
                self.pause.mark_initialized();
                self.alerts.alert(AlertEvent::Recovery {
                    actor: format!("{actor}"),
                });
            }
        }
        self.store.write_pause_projection(self.pause.is_paused())?;
        Ok(actor)
    }

    /// Hash-pinned restart revalidation (Rev5 §4/§6).
    pub fn revalidate_against_chain(
        &self,
        chain: &dyn CanonicalChainView,
    ) -> Result<(), CoordinatorError> {
        let st = self.state.lock().expect("coord");
        if let Some(anchor) = st.init_anchor {
            match chain.block_hash(anchor.block_number) {
                Ok(Some(hash)) if hash == anchor.block_hash => {}
                Ok(Some(_)) | Ok(None) => {
                    drop(st);
                    self.fail_revalidation("init anchor block hash mismatch/missing");
                    return Err(CoordinatorError::RestartRevalidation(
                        "init anchor block hash mismatch/missing".into(),
                    ));
                }
                Err(e) => return Err(CoordinatorError::RestartRevalidation(e)),
            }

            let (finalized, _pending) = chain
                .signer_nonces()
                .map_err(CoordinatorError::RestartRevalidation)?;
            for n in anchor.pending_nonce..finalized {
                let covered = st.open_submissions.contains(&n)
                    || st.ledger.has_charge_for_nonce(n);
                if !covered {
                    drop(st);
                    self.fail_revalidation(&format!(
                        "unexplained nonce gap at {n} (finalized {finalized})"
                    ));
                    return Err(CoordinatorError::RestartRevalidation(
                        "unexplained nonce gap after restart".into(),
                    ));
                }
            }
        }

        let streak_checks: Vec<(u64, B256)> = st
            .ledger
            .streak_tail()
            .iter()
            .map(|e| (e.block_number, e.block_hash))
            .collect();
        let window_checks: Vec<(u64, B256)> = st
            .ledger
            .window_charges(&self.cfg, st.head_block)
            .into_iter()
            .map(|rec| (rec.block_number, rec.block_hash))
            .collect();
        drop(st);

        for (block_number, block_hash) in streak_checks.into_iter().chain(window_checks) {
            if let Err(msg) = Self::check_entry_hash(chain, block_number, block_hash) {
                self.fail_revalidation(&msg);
                return Err(CoordinatorError::RestartRevalidation(msg));
            }
        }
        Ok(())
    }

    fn check_entry_hash(
        chain: &dyn CanonicalChainView,
        block_number: u64,
        block_hash: B256,
    ) -> Result<(), String> {
        match chain.block_hash(block_number) {
            Ok(Some(hash)) if hash == block_hash => Ok(()),
            Ok(Some(_)) | Ok(None) => Err(format!(
                "block {block_number} hash mismatch/missing during restart revalidation"
            )),
            Err(e) => Err(e),
        }
    }

    fn fail_revalidation(&self, detail: &str) {
        self.pause.pause("restart_revalidation");
        self.alerts.alert(AlertEvent::RestartValidationFailure {
            detail: detail.into(),
        });
    }

    pub fn open_submission_nonces(&self) -> Vec<u64> {
        self.state
            .lock()
            .expect("coord")
            .open_submissions
            .iter()
            .copied()
            .collect()
    }

    pub fn init_anchor(&self) -> Option<InitAnchor> {
        self.state.lock().expect("coord").init_anchor
    }

    fn append(&self, tag: WalTag, payload: WalPayload) -> Result<(), CoordinatorError> {
        let mut st = self.state.lock().expect("coord");
        self.append_unlocked(&mut st, tag, payload)
    }

    fn append_unlocked(
        &self,
        st: &mut CoordState,
        tag: WalTag,
        payload: WalPayload,
    ) -> Result<(), CoordinatorError> {
        let record = WalRecord {
            seq: st.next_seq,
            tag,
            payload,
        };
        let (frame, digest) = encode_frame(st.prev_digest, &record);
        self.store.append_wal(&frame)?;
        st.prev_digest = digest;
        st.next_seq += 1;
        Ok(())
    }
}

fn replay_wal(
    bytes: &[u8],
    state: &mut CoordState,
    pause: &PauseController,
    initialized: &mut bool,
) -> Result<(), CoordinatorError> {
    let mut offset = 0;
    let mut prev = B256::ZERO;
    while offset < bytes.len() {
        if offset + 4 > bytes.len() {
            return Err(CoordinatorError::PartialWalTail);
        }
        let body_len = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let frame_len = 4 + body_len + 32;
        if offset + frame_len > bytes.len() {
            return Err(CoordinatorError::PartialWalFrame);
        }
        let frame = &bytes[offset..offset + frame_len];
        let (record, dig) = decode_frame(prev, frame)?;
        if record.seq != state.next_seq {
            return Err(CoordinatorError::WalSeqGap {
                expected: state.next_seq,
                got: record.seq,
            });
        }
        apply_record(state, pause, initialized, &record);
        state.next_seq += 1;
        prev = dig;
        state.prev_digest = dig;
        offset += frame_len;
    }
    Ok(())
}

fn apply_record(
    state: &mut CoordState,
    pause: &PauseController,
    initialized: &mut bool,
    record: &WalRecord,
) {
    match &record.payload {
        WalPayload::Init {
            executor_codehash,
            block_number,
            block_hash,
            finalized_nonce,
            pending_nonce,
            ..
        } => {
            *initialized = true;
            pause.mark_initialized();
            state.init_anchor = Some(InitAnchor {
                executor_codehash: *executor_codehash,
                block_number: *block_number,
                block_hash: *block_hash,
                finalized_nonce: *finalized_nonce,
                pending_nonce: *pending_nonce,
            });
            state.head_block = state.head_block.max(*block_number);
        }
        WalPayload::SubmissionPrepared { nonce, .. } => {
            state.open_submissions.insert(*nonce);
        }
        WalPayload::TerminalAccounting {
            nonce,
            tx_hash,
            block_number,
            block_hash,
            kind,
            success,
            actual_cost,
            execution_layer_only,
            onchain_min_profit,
        } => {
            state.open_submissions.remove(nonce);
            let kind = TerminalKind::from_u8(*kind).unwrap_or(TerminalKind::Execute);
            let _ = state.ledger.charge(
                &BreakerConfig::with_caps(u128::MAX, 1, 1),
                AccountingRecord {
                    nonce: *nonce,
                    tx_hash: *tx_hash,
                    block_number: *block_number,
                    block_hash: *block_hash,
                    kind,
                    success: *success,
                    actual_cost: *actual_cost,
                    execution_layer_only: *execution_layer_only,
                    onchain_min_profit: *onchain_min_profit,
                },
                state.head_block.max(*block_number),
            );
        }
        WalPayload::Reversal {
            nonce,
            tx_hash,
            block_number,
            block_hash,
            reason,
        } => {
            let _ = state.ledger.reverse(&ReversalRecord {
                nonce: *nonce,
                tx_hash: *tx_hash,
                block_number: *block_number,
                block_hash: *block_hash,
                reason: reason.clone(),
            });
        }
        WalPayload::PauseTrip { paused, .. } => {
            if *paused {
                pause.pause("wal_replay");
            }
        }
        WalPayload::OperatorControl {
            control_seq, kind, ..
        } => {
            state.last_control_seq = state.last_control_seq.max(*control_seq);
            if *kind == ControlKind::Init as u8 {
                *initialized = true;
                pause.mark_initialized();
            }
        }
        WalPayload::Recovery { control_seq, .. } => {
            state.last_control_seq = state.last_control_seq.max(*control_seq);
            *initialized = true;
            pause.mark_initialized();
        }
        _ => {}
    }
}

impl BreakerRuntime {
    pub fn open(
        root: &Path,
        scope: ScopeId,
        cfg: BreakerConfig,
        alerts: Arc<dyn AlertSink>,
        operator: Address,
    ) -> Result<Self, CoordinatorError> {
        let coordinator = DurableIntentCoordinator::open(root, scope, cfg, alerts, operator)?;
        let pause = coordinator.pause_controller();
        Ok(Self {
            coordinator,
            pause,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::breaker::alert::{AlertSink, RecordingAlertSink};
    use crate::execution::breaker::operator::{sign_command, ControlCommand};
    use crate::execution::pause::{AttemptKind, PauseGate};
    use alloy::signers::local::PrivateKeySigner;
    use std::collections::HashMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("amms-coord-{n}-{seq}"))
    }

    fn sample_anchor() -> InitAnchor {
        InitAnchor {
            executor_codehash: B256::repeat_byte(0xab),
            block_number: 100,
            block_hash: B256::repeat_byte(0xcd),
            finalized_nonce: 5,
            pending_nonce: 5,
        }
    }

    struct MockChain {
        hashes: HashMap<u64, B256>,
        finalized: u64,
        pending: u64,
    }

    impl CanonicalChainView for MockChain {
        fn block_hash(&self, block_number: u64) -> Result<Option<B256>, String> {
            Ok(self.hashes.get(&block_number).copied())
        }
        fn signer_nonces(&self) -> Result<(u64, u64), String> {
            Ok((self.finalized, self.pending))
        }
    }

    #[test]
    fn absent_state_accepts_signed_init_only() {
        let root = tmp();
        let sk = PrivateKeySigner::random();
        let alerts: Arc<dyn AlertSink> = Arc::new(RecordingAlertSink::default());
        let scope = ScopeId {
            chain_id: 5003,
            executor: Address::repeat_byte(1),
            signer: Address::repeat_byte(2),
        };
        let rt = BreakerRuntime::open(
            &root,
            scope,
            BreakerConfig::with_caps(1_000_000, 1, 1),
            Arc::clone(&alerts),
            sk.address(),
        )
        .unwrap();
        assert!(rt.pause.is_paused());
        assert!(rt.pause.begin_send(AttemptKind::Execute).is_err());
        // unpause before init fails
        let unpause = sign_command(
            &sk,
            ControlCommand {
                control_seq: 1,
                kind: ControlKind::Unpause,
                chain_id: scope.chain_id,
                executor: scope.executor,
                signer: scope.signer,
                detail: String::new(),
            },
        )
        .unwrap();
        assert!(rt
            .coordinator
            .apply_operator_command(unpause, None)
            .is_err());
        let init = sign_command(
            &sk,
            ControlCommand {
                control_seq: 1,
                kind: ControlKind::Init,
                chain_id: scope.chain_id,
                executor: scope.executor,
                signer: scope.signer,
                detail: "virgin".into(),
            },
        )
        .unwrap();
        let anchor = sample_anchor();
        rt.coordinator
            .apply_operator_command(init, Some(anchor))
            .unwrap();
        assert_eq!(rt.coordinator.init_anchor(), Some(anchor));
        let unpause2 = sign_command(
            &sk,
            ControlCommand {
                control_seq: 2,
                kind: ControlKind::Unpause,
                chain_id: scope.chain_id,
                executor: scope.executor,
                signer: scope.signer,
                detail: String::new(),
            },
        )
        .unwrap();
        rt.coordinator
            .apply_operator_command(unpause2, None)
            .unwrap();
        assert!(rt.pause.begin_send(AttemptKind::Execute).is_ok());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn init_rejects_zero_placeholders() {
        let root = tmp();
        let sk = PrivateKeySigner::random();
        let alerts: Arc<dyn AlertSink> = Arc::new(RecordingAlertSink::default());
        let scope = ScopeId {
            chain_id: 5003,
            executor: Address::repeat_byte(1),
            signer: Address::repeat_byte(2),
        };
        let rt = BreakerRuntime::open(
            &root,
            scope,
            BreakerConfig::with_caps(1_000_000, 1, 1),
            alerts,
            sk.address(),
        )
        .unwrap();
        let init = sign_command(
            &sk,
            ControlCommand {
                control_seq: 1,
                kind: ControlKind::Init,
                chain_id: scope.chain_id,
                executor: scope.executor,
                signer: scope.signer,
                detail: String::new(),
            },
        )
        .unwrap();
        let bad = InitAnchor {
            executor_codehash: B256::ZERO,
            block_number: 1,
            block_hash: B256::repeat_byte(1),
            finalized_nonce: 0,
            pending_nonce: 0,
        };
        assert!(matches!(
            rt.coordinator.apply_operator_command(init, Some(bad)),
            Err(CoordinatorError::InitAnchorInvalid)
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn terminal_charge_persists_across_reopen() {
        let root = tmp();
        let sk = PrivateKeySigner::random();
        let alerts: Arc<dyn AlertSink> = Arc::new(RecordingAlertSink::default());
        let scope = ScopeId {
            chain_id: 1,
            executor: Address::repeat_byte(3),
            signer: Address::repeat_byte(4),
        };
        let cfg = BreakerConfig::with_caps(10_000, 1, 1);
        {
            let rt = BreakerRuntime::open(&root, scope, cfg.clone(), Arc::clone(&alerts), sk.address())
                .unwrap();
            let init = sign_command(
                &sk,
                ControlCommand {
                    control_seq: 1,
                    kind: ControlKind::Init,
                    chain_id: scope.chain_id,
                    executor: scope.executor,
                    signer: scope.signer,
                    detail: String::new(),
                },
            )
            .unwrap();
            rt.coordinator
                .apply_operator_command(init, Some(sample_anchor()))
                .unwrap();
            rt.coordinator.set_head_block(100);
            for nonce in 1..=3u64 {
                rt.coordinator
                    .commit_terminal(AccountingRecord {
                        nonce,
                        tx_hash: B256::from(U256::from(nonce)),
                        block_number: 90 + nonce,
                        block_hash: B256::repeat_byte(nonce as u8),
                        kind: TerminalKind::Execute,
                        success: false,
                        actual_cost: U256::from(100u64),
                        execution_layer_only: false,
                        onchain_min_profit: U256::ZERO,
                    })
                    .unwrap();
            }
            assert!(rt.pause.is_paused());
            assert_eq!(rt.coordinator.breaker_stats().consecutive_reverts, 3);
        }
        let rt2 = BreakerRuntime::open(&root, scope, cfg, alerts, sk.address()).unwrap();
        assert_eq!(rt2.coordinator.breaker_stats().consecutive_reverts, 3);
        assert_eq!(rt2.coordinator.init_anchor(), Some(sample_anchor()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn restart_revalidation_trips_on_hash_mismatch_and_gap() {
        let root = tmp();
        let sk = PrivateKeySigner::random();
        let alerts: Arc<dyn AlertSink> = Arc::new(RecordingAlertSink::default());
        let scope = ScopeId {
            chain_id: 1,
            executor: Address::repeat_byte(5),
            signer: Address::repeat_byte(6),
        };
        let cfg = BreakerConfig::with_caps(10_000, 3, 300);
        let rt = BreakerRuntime::open(&root, scope, cfg, Arc::clone(&alerts), sk.address()).unwrap();
        let init = sign_command(
            &sk,
            ControlCommand {
                control_seq: 1,
                kind: ControlKind::Init,
                chain_id: scope.chain_id,
                executor: scope.executor,
                signer: scope.signer,
                detail: String::new(),
            },
        )
        .unwrap();
        let anchor = sample_anchor();
        rt.coordinator
            .apply_operator_command(init, Some(anchor))
            .unwrap();
        rt.coordinator.set_head_block(110);
        rt.coordinator
            .commit_terminal(AccountingRecord {
                nonce: 5,
                tx_hash: B256::repeat_byte(9),
                block_number: 105,
                block_hash: B256::repeat_byte(0xee),
                kind: TerminalKind::Execute,
                success: false,
                actual_cost: U256::from(1u64),
                execution_layer_only: false,
                onchain_min_profit: U256::ZERO,
            })
            .unwrap();

        let mut hashes = HashMap::new();
        hashes.insert(anchor.block_number, anchor.block_hash);
        hashes.insert(105, B256::repeat_byte(0xee));
        let ok_chain = MockChain {
            hashes: hashes.clone(),
            finalized: 6,
            pending: 6,
        };
        assert!(rt.coordinator.revalidate_against_chain(&ok_chain).is_ok());

        // Streak/window hash mismatch → pause + error.
        let mut bad = hashes.clone();
        bad.insert(105, B256::repeat_byte(0xff));
        let bad_chain = MockChain {
            hashes: bad,
            finalized: 6,
            pending: 6,
        };
        assert!(rt.coordinator.revalidate_against_chain(&bad_chain).is_err());
        assert!(rt.pause.is_paused());

        // Unexplained gap: finalized advanced with no WAL coverage.
        let gap_chain = MockChain {
            hashes,
            finalized: 8,
            pending: 8,
        };
        assert!(rt.coordinator.revalidate_against_chain(&gap_chain).is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
