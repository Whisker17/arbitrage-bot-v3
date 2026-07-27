//! E2E bootstrap/manifest/sign-permit state machine and the harness-facing
//! send facade (WHI-555).
//!
//! Lifecycle: validated startup -> [`E2eBootstrapAuthority`] mints one-shot
//! [`BootstrapActionPermit`]s for `deploy | config | initial-seed` ->
//! `finalize()` consumes the authority and returns a [`VerifiedE2eManifest`]
//! -> the manifest mints one-shot [`E2eSignPermit`]s for `arb | trigger |
//! cancel` -> `sign()` consumes a permit and returns an opaque
//! [`BroadcastableE2eSubmission`] -> `broadcast()` consumes the submission.
//!
//! Every constructor for an authority, manifest, or permit is either private
//! or visible only within `crate::execution::e2e` (see
//! [`ValidatedE2eStartup`]'s doc comment), and the signer (`EthereumWallet`)
//! and raw provider send method live only inside the private
//! [`SendTransport`] shared by [`E2eBootstrapAuthority`] and
//! [`VerifiedE2eManifest`]. Every public operation either mints a permit or
//! consumes one; nothing here hands back the signer, the provider, or a
//! reusable "just send anything" capability.

use std::sync::Arc;

use alloy::eips::Encodable2718;
use alloy::network::{EthereumWallet, NetworkWallet};
use alloy::primitives::{keccak256, Address, Bytes, B256, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::{BlockNumberOrTag, TransactionRequest};
use rand::RngCore;

use super::digest::{
    bootstrap_request_digest, cancel_request_digest, trigger_request_digest, BootstrapAction,
    BootstrapRequestDigest, E2eSignAction,
};
use super::env_guard::ValidatedE2eStartup;
use super::error::E2eCapabilityError;
use super::provider_identity::{
    reject_invalid_chain_id, validate_provider_identity, ProviderIdentityDigest,
    ValidatedE2eProvider,
};
use crate::execution::fee_context::FeePlan;
use crate::execution::intent::{
    ChainNonceView, IntentStateMachine, PreparedPayload, SignedSubmission as IntentSignedSubmission,
};
use crate::execution::pipeline::{DurableSubmissionHook, PreparedPipelineHead};
use crate::state_space::SnapshotId;

const MANIFEST_DIGEST_DOMAIN_V1: &[u8] = b"whisker-arb/e2e-manifest-digest/v1";

fn manifest_digest(
    provider_identity_digest: ProviderIdentityDigest,
    chain_id: u64,
    executor_address: Address,
    signer_address: Address,
) -> B256 {
    let mut preimage = Vec::new();
    preimage.extend_from_slice(MANIFEST_DIGEST_DOMAIN_V1);
    preimage.push(0x00);
    preimage.extend_from_slice(provider_identity_digest.0.as_slice());
    preimage.extend_from_slice(&chain_id.to_be_bytes());
    preimage.extend_from_slice(executor_address.as_slice());
    preimage.extend_from_slice(signer_address.as_slice());
    keccak256(&preimage)
}

/// Private signer + provider transport. Never exposed publicly: this is the
/// exclusive "signer and raw provider send method" owner required by
/// WHI-555 step 6.
struct SendTransport {
    wallet: EthereumWallet,
    signer_address: Address,
    provider: DynProvider,
    chain_id: u64,
    executor_address: Address,
}

impl SendTransport {
    async fn sign_and_wrap(
        &self,
        tx: TransactionRequest,
    ) -> Result<(Bytes, B256), E2eCapabilityError> {
        let envelope = <EthereumWallet as NetworkWallet<alloy::network::Ethereum>>::sign_request(
            &self.wallet,
            tx,
        )
        .await
        .map_err(|_| E2eCapabilityError::LocalSignFailed)?;
        let tx_hash = *envelope.tx_hash();
        let raw = Bytes::from(envelope.encoded_2718());
        Ok((raw, tx_hash))
    }

    async fn broadcast_raw(&self, raw: &Bytes) -> Result<B256, E2eCapabilityError> {
        let pending = self
            .provider
            .send_raw_transaction(raw.as_ref())
            .await
            .map_err(|_| E2eCapabilityError::BroadcastFailed)?;
        Ok(*pending.tx_hash())
    }
}

fn fresh_session_id() -> B256 {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    B256::from(bytes)
}

/// One-shot capability to send a single bootstrap transaction
/// (`deploy | config | initial-seed`). Consumed by value by
/// [`E2eBootstrapAuthority::sign_bootstrap`]; Rust ownership makes reuse a
/// compile error, not a runtime check.
///
/// ```compile_fail
/// use amms::execution::e2e::BootstrapActionPermit;
/// fn forge() -> BootstrapActionPermit {
///     // Every field is private to this module: no external caller can
///     // construct one from a struct literal.
///     BootstrapActionPermit { ..panic!("private fields") }
/// }
/// ```
#[derive(Debug)]
#[must_use]
pub struct BootstrapActionPermit {
    action: BootstrapAction,
    digest: BootstrapRequestDigest,
    tx: TransactionRequest,
    tx_nonce: u64,
    provider_identity_digest: ProviderIdentityDigest,
    chain_id: u64,
    signer_address: Address,
    session_id: B256,
}

impl BootstrapActionPermit {
    pub fn action(&self) -> BootstrapAction {
        self.action
    }

    pub fn digest(&self) -> BootstrapRequestDigest {
        self.digest
    }

    pub fn tx_nonce(&self) -> u64 {
        self.tx_nonce
    }

    /// Fresh per-mint random id, for the caller's own audit/correlation logs.
    /// Not re-validated at send time: `tx_nonce` and `digest` (which folds in
    /// the tx bytes, including its nonce) are what `sign_bootstrap` actually
    /// checks against this instance's live state; `session_id` only needs to
    /// be unique per mint, which `fresh_session_id`'s randomness already
    /// guarantees without a mutable "seen ids" registry.
    pub fn session_id(&self) -> B256 {
        self.session_id
    }
}

/// Validated-startup-gated authority that mints bootstrap permits. The only
/// production constructor is [`E2eBootstrapAuthority::establish`], which
/// performs a live `eth_chainId` + genesis-block read against `provider`
/// itself — tying the derived [`ProviderIdentityDigest`] to that exact
/// instance, per WHI-555 step 2 ("a fresh ... nonce for each validated
/// provider instance"). [`E2eBootstrapAuthority::establish_with_identity`]
/// (an already-validated identity, no live network call) exists only behind
/// the `e2e-test-util` feature for this module's own tests and
/// `tests/e2e_capabilities.rs`.
pub struct E2eBootstrapAuthority {
    transport: SendTransport,
    provider_identity: ValidatedE2eProvider,
}

/// Deliberately omits `transport` (the wallet): a derived `Debug` would let
/// `format!("{:?}", _)` reach the signer without ever going through a
/// permit-consuming operation.
impl std::fmt::Debug for E2eBootstrapAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("E2eBootstrapAuthority")
            .field("chain_id", &self.transport.chain_id)
            .field("executor_address", &self.transport.executor_address)
            .field("provider_identity", &self.provider_identity)
            .finish_non_exhaustive()
    }
}

impl E2eBootstrapAuthority {
    /// Fetch live chain id + genesis hash from `provider` itself, validate
    /// them, derive the provider identity, and establish the authority. This
    /// is the only path that binds the identity to the exact provider
    /// instance used for every subsequent send — a caller cannot construct a
    /// valid identity from one provider and reuse it against a different
    /// one.
    pub async fn establish<P: Provider + Clone + 'static>(
        startup: ValidatedE2eStartup,
        provider: P,
    ) -> Result<Self, E2eCapabilityError> {
        let chain_id = provider
            .get_chain_id()
            .await
            .map_err(|_| E2eCapabilityError::ProviderReadFailed)?;
        // Fail fast on a wrong chain id without spending a second RPC round
        // trip on the genesis block. This is the same check
        // `validate_provider_identity` runs below (shared via
        // `reject_invalid_chain_id` so the policy lives in exactly one
        // place); the only reason to run it again here is to skip the
        // genesis-block read when it's already moot.
        reject_invalid_chain_id(chain_id)?;
        let genesis_hash = provider
            .get_block_by_number(BlockNumberOrTag::Number(0))
            .await
            .map_err(|_| E2eCapabilityError::ProviderReadFailed)?
            .ok_or(E2eCapabilityError::GenesisBlockUnavailable(chain_id))?
            .header
            .hash;
        let provider_identity = validate_provider_identity(chain_id, genesis_hash)?;
        Ok(Self::establish_with_identity_impl(
            startup,
            provider_identity,
            provider.erased(),
        ))
    }

    /// Test-only: accepts an already-validated identity instead of fetching
    /// one live, so tests can avoid depending on a mock provider's exact
    /// `eth_chainId`/`eth_getBlockByNumber` response shape. Gated behind
    /// `e2e-test-util` (see `Cargo.toml`) — no non-test caller can skip the
    /// live validation [`Self::establish`] performs.
    #[cfg(feature = "e2e-test-util")]
    pub fn establish_with_identity(
        startup: ValidatedE2eStartup,
        provider_identity: ValidatedE2eProvider,
        provider: DynProvider,
    ) -> Self {
        Self::establish_with_identity_impl(startup, provider_identity, provider)
    }

    fn establish_with_identity_impl(
        startup: ValidatedE2eStartup,
        provider_identity: ValidatedE2eProvider,
        provider: DynProvider,
    ) -> Self {
        let signer_address = startup.signer_address();
        let executor_address = startup.executor_address();
        let wallet = EthereumWallet::from(startup.signer);
        Self {
            transport: SendTransport {
                wallet,
                signer_address,
                provider,
                chain_id: provider_identity.chain_id(),
                executor_address,
            },
            provider_identity,
        }
    }

    pub fn signer_address(&self) -> Address {
        self.transport.signer_address
    }

    pub fn executor_address(&self) -> Address {
        self.transport.executor_address
    }

    pub fn chain_id(&self) -> u64 {
        self.transport.chain_id
    }

    pub fn provider_identity_digest(&self) -> ProviderIdentityDigest {
        self.provider_identity.digest()
    }

    /// Mint a one-shot permit for `action`, binding it to `tx`'s domain-
    /// separated digest, its nonce, this instance's provider identity and
    /// chain id, the signer address, and a fresh random session id.
    pub fn mint_bootstrap_permit(
        &self,
        action: BootstrapAction,
        tx: TransactionRequest,
    ) -> Result<BootstrapActionPermit, E2eCapabilityError> {
        let from = self.transport.signer_address;
        let tx_nonce = tx.nonce.ok_or(E2eCapabilityError::MissingTxNonce)?;
        let digest = bootstrap_request_digest(action, &tx, from)?;
        Ok(BootstrapActionPermit {
            action,
            digest,
            tx,
            tx_nonce,
            provider_identity_digest: self.provider_identity.digest(),
            chain_id: self.transport.chain_id,
            signer_address: from,
            session_id: fresh_session_id(),
        })
    }

    /// Consume a bootstrap permit: re-validate every binding against this
    /// authority's current state, then sign. Returns the same opaque
    /// [`BroadcastableE2eSubmission`] the post-manifest sign path returns, so
    /// bootstrap sends get the identical byte/hash-substitution protection at
    /// [`Self::broadcast_bootstrap`]. Bootstrap actions never touch the
    /// nonce-intent state machine (only `arb` does), so unlike
    /// [`VerifiedE2eManifest::sign`] there is no SM cleanup obligation here.
    pub async fn sign_bootstrap(
        &self,
        permit: BootstrapActionPermit,
    ) -> Result<BroadcastableE2eSubmission, E2eCapabilityError> {
        if permit.chain_id != self.transport.chain_id {
            return Err(E2eCapabilityError::ChainIdMismatch {
                expected: self.transport.chain_id,
                actual: permit.chain_id,
            });
        }
        if permit.provider_identity_digest != self.provider_identity.digest() {
            return Err(E2eCapabilityError::ProviderIdentityMismatch);
        }
        if permit.signer_address != self.transport.signer_address {
            return Err(E2eCapabilityError::SignerMismatch {
                expected: self.transport.signer_address,
                actual: permit.signer_address,
            });
        }
        if permit.tx.nonce != Some(permit.tx_nonce) {
            return Err(E2eCapabilityError::PermitNonceMismatch {
                expected: permit.tx_nonce,
                actual: permit.tx.nonce,
            });
        }
        let fresh_digest =
            bootstrap_request_digest(permit.action, &permit.tx, permit.signer_address)?;
        if fresh_digest.0 != permit.digest.0 {
            return Err(E2eCapabilityError::DigestMismatch);
        }

        let (raw, tx_hash) = self.transport.sign_and_wrap(permit.tx).await?;
        Ok(BroadcastableE2eSubmission {
            action: SubmissionAction::Bootstrap(permit.action),
            raw,
            tx_hash,
            digest: permit.digest.0,
            nonce: permit.tx_nonce,
            execute_meta: None,
        })
    }

    /// Consume `submission`: verify `keccak256(raw) == tx_hash` before ever
    /// calling `eth_sendRawTransaction`.
    pub async fn broadcast_bootstrap(
        &self,
        submission: BroadcastableE2eSubmission,
    ) -> Result<B256, E2eCapabilityError> {
        broadcast_submission(&self.transport, submission).await
    }

    /// Consume the authority and mint the [`VerifiedE2eManifest`]. After this
    /// call `self` no longer exists, so no further bootstrap permit can ever
    /// be minted from it — enforced by ownership, not a runtime flag.
    pub fn finalize(self) -> VerifiedE2eManifest {
        let digest = manifest_digest(
            self.provider_identity.digest(),
            self.transport.chain_id,
            self.transport.executor_address,
            self.transport.signer_address,
        );
        VerifiedE2eManifest {
            transport: self.transport,
            provider_identity: self.provider_identity,
            manifest_digest: digest,
        }
    }
}

/// Execute-pipeline metadata carried through an `arb` [`E2eSignPermit`] so its
/// eventual [`BroadcastableE2eSubmission`] view exposes the fields the landed
/// durable hook / nonce-intent state machine need — including `min_profit`
/// and `deadline`, both required by `DurableSubmissionHook::on_signed`'s
/// signature, so a caller can drive durable recording from the view alone —
/// without exposing raw signed bytes, and so `sign()`/`Drop` can act on the
/// exact `IntentStateMachine`/nonce-view pair that reserved this nonce.
#[derive(Clone)]
struct ExecuteSubmissionMeta {
    fee_plan: FeePlan,
    payload: PreparedPayload,
    calldata_digest: B256,
    submitted_at: SnapshotId,
    min_profit: U256,
    deadline: U256,
    sm: Arc<IntentStateMachine>,
    chain: ChainNonceView,
}

/// `IntentStateMachine` has no `Debug` impl; every other field does, so
/// derive-and-omit isn't available — hand-roll it instead.
impl std::fmt::Debug for ExecuteSubmissionMeta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecuteSubmissionMeta")
            .field("fee_plan", &self.fee_plan)
            .field("payload", &self.payload)
            .field("calldata_digest", &self.calldata_digest)
            .field("submitted_at", &self.submitted_at)
            .field("min_profit", &self.min_profit)
            .field("deadline", &self.deadline)
            .field("chain", &self.chain)
            .finish_non_exhaustive()
    }
}

/// Unifies bootstrap and post-manifest action kinds for
/// [`BroadcastableE2eSubmission`]/[`SignedSubmissionView`], so both lifecycle
/// stages share one opaque-submission / broadcast-integrity implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SubmissionAction {
    Bootstrap(BootstrapAction),
    Sign(E2eSignAction),
}

impl SubmissionAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bootstrap(action) => action.as_str(),
            Self::Sign(action) => action.as_str(),
        }
    }
}

/// One-shot capability to sign a single `arb | trigger | cancel` transaction.
/// Consumed by value by [`VerifiedE2eManifest::sign`].
///
/// For `arb`, this permit inherits `mint_arb_permit`'s obligation to the
/// `Preparing` intent it carries in `execute_meta` (see
/// [`crate::execution::pipeline::PreparedPipelineHead::into_open_parts`]):
/// exactly one of `sign()` or [`Drop`] must resolve it, mirroring how
/// `PreparedPipelineHead` itself guarantees this.
///
/// ```compile_fail
/// use amms::execution::e2e::E2eSignPermit;
/// fn forge() -> E2eSignPermit {
///     E2eSignPermit { ..panic!("private fields") }
/// }
/// ```
#[derive(Debug)]
#[must_use]
pub struct E2eSignPermit {
    action: E2eSignAction,
    digest: B256,
    tx: TransactionRequest,
    nonce: u64,
    provider_identity_digest: ProviderIdentityDigest,
    chain_id: u64,
    executor_address: Address,
    manifest_digest: B256,
    signer_address: Address,
    execute_meta: Option<ExecuteSubmissionMeta>,
    /// Set by `sign()` before its first fallible step, so [`Drop`] never
    /// double-handles cleanup `sign()` has already taken responsibility for
    /// (mirroring `PreparedPipelineHead::consumed`).
    consumed: bool,
}

impl E2eSignPermit {
    pub fn action(&self) -> E2eSignAction {
        self.action
    }

    pub fn digest(&self) -> B256 {
        self.digest
    }

    pub fn nonce(&self) -> u64 {
        self.nonce
    }
}

/// Last-resort cleanup for an `arb` permit that was minted and then dropped
/// without ever reaching `sign()` (or reaching it and failing before it
/// marked itself consumed). A `Preparing` intent holds a live nonce; leaking
/// one stalls the nonce lane until restart — exactly the failure mode
/// `PreparedPipelineHead::drop` exists to prevent, which this mirrors.
impl Drop for E2eSignPermit {
    fn drop(&mut self) {
        if self.consumed {
            return;
        }
        let Some(meta) = &self.execute_meta else {
            return;
        };
        let abort = meta.sm.abort_prepare(self.nonce);
        let reconcile = meta.sm.reconcile(meta.chain.clone());
        tracing::error!(
            target: "execution.e2e",
            nonce = self.nonce,
            abort_error = ?abort.err(),
            reconcile_error = ?reconcile.err(),
            "E2eSignPermit (arb) dropped without sign() ever consuming it; ran \
             best-effort abort_prepare + reconcile so the Preparing intent does not \
             leak its nonce"
        );
    }
}

/// Read-only, borrowed view of a signed E2E submission's non-secret fields —
/// everything the landed durable hook / nonce-intent state machine need to
/// record the attempt, without exposing raw signed bytes or send authority.
/// `execute_meta` is populated only for the `arb` action.
pub struct SignedSubmissionView<'a> {
    pub action: SubmissionAction,
    pub tx_hash: B256,
    pub digest: B256,
    pub nonce: u64,
    pub execute_meta: Option<ExecuteSubmissionMetaView<'a>>,
}

/// Borrowed Execute-specific fields, only present for the `arb` action.
pub struct ExecuteSubmissionMetaView<'a> {
    pub fee_plan: &'a FeePlan,
    pub payload: &'a PreparedPayload,
    pub calldata_digest: B256,
    pub submitted_at: SnapshotId,
    /// Needed, alongside `deadline`, to call
    /// `DurableSubmissionHook::on_signed(signed, min_profit, deadline)`
    /// from this view alone.
    pub min_profit: U256,
    pub deadline: U256,
}

/// Opaque signed submission. Not a tuple, and carries no reusable broadcast
/// authority: the only ways to consume it are [`broadcast_submission`]
/// (reached only through [`E2eBootstrapAuthority::broadcast_bootstrap`] or
/// [`VerifiedE2eManifest::broadcast`], each of which owns the transport that
/// can actually send it) or [`BroadcastableE2eSubmission::view`] (a borrowed,
/// read-only projection that never exposes `raw`).
///
/// Deliberately has no [`Drop`] impl of its own. For `arb`, by the time this
/// type exists `sign` has already durably recorded the attempt into the
/// intent state machine (`IntentState::Submitted`), which is exactly the
/// same "signed but not yet observed on-chain" state production's own
/// `finalize_execute_submission` leaves an attempt in immediately after
/// signing — reconciling it is the harness/orchestration layer's ordinary
/// receipt-tracking job (out of scope here; see WHI-525), not a leak this
/// type's absence-of-`Drop` introduces.
#[must_use]
pub struct BroadcastableE2eSubmission {
    action: SubmissionAction,
    raw: Bytes,
    tx_hash: B256,
    digest: B256,
    nonce: u64,
    execute_meta: Option<ExecuteSubmissionMeta>,
}

/// Deliberately omits `raw`: a derived `Debug` would let `format!("{:?}", _)`
/// extract the signed bytes without ever calling `broadcast`, defeating "no
/// reusable broadcast authority" through a side door.
impl std::fmt::Debug for BroadcastableE2eSubmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BroadcastableE2eSubmission")
            .field("action", &self.action)
            .field("tx_hash", &self.tx_hash)
            .field("digest", &self.digest)
            .field("nonce", &self.nonce)
            .finish_non_exhaustive()
    }
}

impl BroadcastableE2eSubmission {
    pub fn tx_hash(&self) -> B256 {
        self.tx_hash
    }

    /// Borrowed, read-only view: everything the durable hook / nonce-intent
    /// state machine need, `execute_meta` populated only for `arb`. Raw
    /// signed bytes and broadcast authority are not reachable through it.
    pub fn view(&self) -> SignedSubmissionView<'_> {
        let execute_meta = self
            .execute_meta
            .as_ref()
            .map(|meta| ExecuteSubmissionMetaView {
                fee_plan: &meta.fee_plan,
                payload: &meta.payload,
                calldata_digest: meta.calldata_digest,
                submitted_at: meta.submitted_at,
                min_profit: meta.min_profit,
                deadline: meta.deadline,
            });
        SignedSubmissionView {
            action: self.action,
            tx_hash: self.tx_hash,
            digest: self.digest,
            nonce: self.nonce,
            execute_meta,
        }
    }
}

/// Shared broadcast path for both lifecycle stages: verify
/// `keccak256(raw) == tx_hash` — rejecting any byte/hash substitution — then,
/// and only then, hand `raw` to the transport's `eth_sendRawTransaction`.
async fn broadcast_submission(
    transport: &SendTransport,
    submission: BroadcastableE2eSubmission,
) -> Result<B256, E2eCapabilityError> {
    if keccak256(submission.raw.as_ref()) != submission.tx_hash {
        return Err(E2eCapabilityError::SubmissionIntegrityFailed);
    }
    transport.broadcast_raw(&submission.raw).await
}

/// The harness-facing facade for post-manifest sends. Owns the same private
/// [`SendTransport`] the bootstrap authority used, transferred via
/// [`E2eBootstrapAuthority::finalize`].
pub struct VerifiedE2eManifest {
    transport: SendTransport,
    provider_identity: ValidatedE2eProvider,
    manifest_digest: B256,
}

impl VerifiedE2eManifest {
    pub fn manifest_digest(&self) -> B256 {
        self.manifest_digest
    }

    pub fn signer_address(&self) -> Address {
        self.transport.signer_address
    }

    pub fn executor_address(&self) -> Address {
        self.transport.executor_address
    }

    pub fn chain_id(&self) -> u64 {
        self.transport.chain_id
    }

    pub fn provider_identity_digest(&self) -> ProviderIdentityDigest {
        self.provider_identity.digest()
    }

    /// Mint an `arb` permit. The digest parameter is exclusively the
    /// [`crate::execution::FinalRequestDigest`] taken from consuming `head` —
    /// no raw digest bytes and no independently re-encoded `FinalRequest` are
    /// ever accepted.
    ///
    /// Consumes `head` via `into_open_parts`, **not** `into_closed_outcome`:
    /// the latter aborts the SM's `Preparing` intent and releases its nonce,
    /// which would be wrong here — this permit is going to be signed and
    /// broadcast for real. The `Preparing` intent stays live until `sign`
    /// calls `IntentStateMachine::record_submission_with_min_profit` right
    /// after signing succeeds (or, on any failure before that point,
    /// `abort_prepare` runs as cleanup — mirroring the production Execute
    /// tail's own failure-path cleanup).
    pub fn mint_arb_permit(
        &self,
        head: PreparedPipelineHead,
    ) -> Result<E2eSignPermit, E2eCapabilityError> {
        let digest = head.digest();
        let request = head.request();
        let tx = request.transaction.clone();
        let from = request.from();
        let min_profit = request.min_profit();
        let deadline = request.deadline();
        let fee_plan = request.fee_plan.clone();
        let payload = request.payload.clone();
        let calldata_digest = request.calldata_digest;
        let submitted_at = request.submitted_at;

        if from != self.transport.signer_address {
            // `head` is dropped here, unconsumed: `PreparedPipelineHead`'s own
            // `Drop` impl runs the best-effort `abort_prepare` + `reconcile`
            // cleanup, so this failure path never leaks the nonce.
            return Err(E2eCapabilityError::SignerMismatch {
                expected: self.transport.signer_address,
                actual: from,
            });
        }

        let (sm, chain, nonce) = head.into_open_parts();

        Ok(E2eSignPermit {
            action: E2eSignAction::Arb,
            digest: digest.0,
            tx,
            nonce,
            provider_identity_digest: self.provider_identity.digest(),
            chain_id: self.transport.chain_id,
            executor_address: self.transport.executor_address,
            manifest_digest: self.manifest_digest,
            signer_address: from,
            execute_meta: Some(ExecuteSubmissionMeta {
                fee_plan,
                payload,
                calldata_digest,
                submitted_at,
                min_profit,
                deadline,
                sm,
                chain,
            }),
            consumed: false,
        })
    }

    /// Mint a `trigger` permit for a caller-supplied unsigned transaction.
    /// The digest is always computed by this module from `tx`, never
    /// accepted directly from the caller.
    pub fn mint_trigger_permit(
        &self,
        tx: TransactionRequest,
    ) -> Result<E2eSignPermit, E2eCapabilityError> {
        let digest = trigger_request_digest(&tx, self.transport.signer_address)?;
        self.mint_action_permit(E2eSignAction::Trigger, tx, digest.0)
    }

    /// Mint a `cancel` permit for a caller-supplied unsigned transaction.
    pub fn mint_cancel_permit(
        &self,
        tx: TransactionRequest,
    ) -> Result<E2eSignPermit, E2eCapabilityError> {
        let digest = cancel_request_digest(&tx, self.transport.signer_address)?;
        self.mint_action_permit(E2eSignAction::Cancel, tx, digest.0)
    }

    /// Shared field-assembly for [`Self::mint_trigger_permit`]/
    /// [`Self::mint_cancel_permit`] — the two differ only in which
    /// domain-separated digest function computed `digest`.
    fn mint_action_permit(
        &self,
        action: E2eSignAction,
        tx: TransactionRequest,
        digest: B256,
    ) -> Result<E2eSignPermit, E2eCapabilityError> {
        let from = self.transport.signer_address;
        let nonce = tx.nonce.ok_or(E2eCapabilityError::MissingTxNonce)?;
        Ok(E2eSignPermit {
            action,
            digest,
            tx,
            nonce,
            provider_identity_digest: self.provider_identity.digest(),
            chain_id: self.transport.chain_id,
            executor_address: self.transport.executor_address,
            manifest_digest: self.manifest_digest,
            signer_address: from,
            execute_meta: None,
            consumed: false,
        })
    }

    /// Consume `permit`: re-validate every bound field against this
    /// manifest's current state, then sign.
    ///
    /// `manifest_digest` alone already folds in `provider_identity_digest`,
    /// `chain_id`, `executor_address`, and `signer_address` (see
    /// [`manifest_digest`]), so an honestly-minted permit can never fail the
    /// `manifest_digest` check while passing the others. The individual
    /// checks stay, and stay ordered before it, for two reasons the acceptance
    /// criteria name directly: (1) each corresponds to a distinct adversarial
    /// fixture ("wrong signer", "wrong chain", "different provider session"
    /// are each tested independently — see this module's tests — and each
    /// deserves its own error variant for diagnosis); (2) white-box tests
    /// tamper individual permit fields directly (no public API can), which
    /// can desynchronize a field from `manifest_digest` in ways an honest
    /// mint never would.
    ///
    /// Takes `durable`, called before `record_submission_with_min_profit` for
    /// the `arb` action — the same order and the same
    /// `DurableSubmissionHook` seam
    /// [`crate::execution::pipeline::record_signed_submission`] uses in
    /// production — because [`SignedSubmissionView`] cannot honestly drive
    /// that call itself: `DurableSubmissionHook::on_signed` needs
    /// `&intent::SignedSubmission`, whose `raw` field the view deliberately
    /// never exposes. Pass
    /// [`crate::execution::pipeline::NoopDurableHook`] if the caller has
    /// nothing to record durably; `trigger`/`cancel` permits never call
    /// `durable` at all (they carry no `execute_meta`, so there is nothing
    /// to record into the intent state machine).
    ///
    /// `permit` is marked consumed only once every fallible step —
    /// including the local-signing `.await` — has actually completed. Until
    /// then, dropping `permit` for *any* reason (an early `return`, or this
    /// whole `sign` future itself being dropped/cancelled mid-`.await` by a
    /// caller's timeout or `select!`) runs [`E2eSignPermit`]'s own `Drop`
    /// cleanup — so there is exactly one cleanup path, not two to keep in
    /// sync.
    pub async fn sign(
        &self,
        mut permit: E2eSignPermit,
        durable: &impl DurableSubmissionHook,
    ) -> Result<BroadcastableE2eSubmission, E2eCapabilityError> {
        if permit.chain_id != self.transport.chain_id {
            return Err(E2eCapabilityError::ChainIdMismatch {
                expected: self.transport.chain_id,
                actual: permit.chain_id,
            });
        }
        if permit.provider_identity_digest != self.provider_identity.digest() {
            return Err(E2eCapabilityError::ProviderIdentityMismatch);
        }
        if permit.manifest_digest != self.manifest_digest {
            return Err(E2eCapabilityError::ManifestIdentityMismatch);
        }
        if permit.executor_address != self.transport.executor_address {
            return Err(E2eCapabilityError::ExecutorMismatch {
                expected: self.transport.executor_address,
                actual: permit.executor_address,
            });
        }
        if permit.signer_address != self.transport.signer_address {
            return Err(E2eCapabilityError::SignerMismatch {
                expected: self.transport.signer_address,
                actual: permit.signer_address,
            });
        }
        if permit.tx.nonce != Some(permit.nonce) {
            return Err(E2eCapabilityError::PermitNonceMismatch {
                expected: permit.nonce,
                actual: permit.tx.nonce,
            });
        }

        match permit.action {
            E2eSignAction::Trigger => {
                let fresh = trigger_request_digest(&permit.tx, permit.signer_address)?;
                if fresh.0 != permit.digest {
                    return Err(E2eCapabilityError::DigestMismatch);
                }
            }
            E2eSignAction::Cancel => {
                let fresh = cancel_request_digest(&permit.tx, permit.signer_address)?;
                if fresh.0 != permit.digest {
                    return Err(E2eCapabilityError::DigestMismatch);
                }
            }
            // `arb`'s digest was already bound at mint time from a consumed
            // `PreparedPipelineHead`; recomputing it here would be exactly
            // the "re-encode FinalRequest" WHI-555 forbids.
            E2eSignAction::Arb => {}
        }

        // Cloned rather than moved out of `permit`: `E2eSignPermit` has a
        // `Drop` impl, and a type with `Drop` can't be partially moved.
        // `permit` is still fully intact (`consumed` still `false`) across
        // this `.await`, so if this whole future is dropped here, `Drop`
        // runs the exact same cleanup an explicit failure branch would.
        let (raw, tx_hash) = self.transport.sign_and_wrap(permit.tx.clone()).await?;

        if let Some(meta) = &permit.execute_meta {
            let signed = IntentSignedSubmission {
                raw: raw.clone(),
                tx_hash,
                fee_plan: meta.fee_plan.clone(),
                payload: meta.payload.clone(),
                calldata_digest: meta.calldata_digest,
                nonce: permit.nonce,
                submitted_at: meta.submitted_at,
            };
            durable
                .on_signed(&signed, meta.min_profit, meta.deadline)
                .map_err(|e| E2eCapabilityError::DurableHookFailed(e.to_string()))?;
            meta.sm.record_submission_with_min_profit(&signed, meta.min_profit)?;
        }

        // Every fallible step is now behind us: mark consumed so `Drop`
        // no-ops. `execute_meta` (and the `Arc<IntentStateMachine>` it
        // carries) is cloned into the returned submission deliberately —
        // `BroadcastableE2eSubmission` doesn't need its own cleanup
        // obligation, because by this point the intent is durably `Submitted`
        // (see the doc comment on `BroadcastableE2eSubmission` for why a
        // never-broadcast submission is not a leak of the kind `Drop` here
        // guards against).
        permit.consumed = true;

        Ok(BroadcastableE2eSubmission {
            action: SubmissionAction::Sign(permit.action),
            raw,
            tx_hash,
            digest: permit.digest,
            nonce: permit.nonce,
            execute_meta: permit.execute_meta.clone(),
        })
    }

    /// Consume `submission`: verify `keccak256(raw) == tx_hash` (rejecting
    /// any byte/hash substitution) before ever calling
    /// `eth_sendRawTransaction`.
    pub async fn broadcast(
        &self,
        submission: BroadcastableE2eSubmission,
    ) -> Result<B256, E2eCapabilityError> {
        broadcast_submission(&self.transport, submission).await
    }
}

#[cfg(all(test, feature = "e2e-test-util"))]
mod tests {
    use super::*;
    use crate::execution::e2e::env_guard::{
        validate_e2e_startup, MapEnvSource, ENV_E2E_EXECUTOR_ADDRESS, ENV_E2E_PRIVATE_KEY,
        ENV_E2E_RPC_URL,
    };
    use crate::execution::pipeline::NoopDurableHook;
    use crate::execution::e2e::provider_identity::{MANTLE_SEPOLIA_CHAIN_ID, MANTLE_SEPOLIA_GENESIS_HASH};
    use alloy::network::TransactionBuilder;
    use alloy::providers::ProviderBuilder;
    use alloy::transports::mock::Asserter;
    use std::collections::BTreeMap;

    const KEY_A: &str = "0000000000000000000000000000000000000000000000000000000000000001";
    const KEY_B: &str = "0000000000000000000000000000000000000000000000000000000000000002";

    fn test_startup(private_key_hex: &str, executor: Address) -> ValidatedE2eStartup {
        let mut map = BTreeMap::new();
        map.insert(
            ENV_E2E_RPC_URL.to_string(),
            "https://rpc.sepolia.mantle.xyz".to_string(),
        );
        map.insert(ENV_E2E_PRIVATE_KEY.to_string(), private_key_hex.to_string());
        map.insert(ENV_E2E_EXECUTOR_ADDRESS.to_string(), executor.to_string());
        validate_e2e_startup(&MapEnvSource(map)).expect("test startup must validate")
    }

    fn mock_provider() -> DynProvider {
        ProviderBuilder::new()
            .connect_mocked_client(Asserter::new())
            .erased()
    }

    /// Each call mints a fresh random provider-session nonce (exactly like a
    /// real process restart or provider reconstruction would), so two
    /// authorities built this way always differ on provider identity even
    /// with identical `chain_id`/`genesis_hash`. Use this directly only when
    /// that is the dimension under test; otherwise share one
    /// `ValidatedE2eProvider` via [`establish_authority_with_identity`] so the
    /// test isolates the field it actually means to vary.
    fn establish_authority(
        private_key_hex: &str,
        chain_id: u64,
        genesis_hash: B256,
        executor: Address,
    ) -> E2eBootstrapAuthority {
        let provider_identity = validate_provider_identity(chain_id, genesis_hash)
            .expect("test chain identity must validate");
        establish_authority_with_identity(private_key_hex, provider_identity, executor)
    }

    fn establish_authority_with_identity(
        private_key_hex: &str,
        provider_identity: ValidatedE2eProvider,
        executor: Address,
    ) -> E2eBootstrapAuthority {
        let startup = test_startup(private_key_hex, executor);
        E2eBootstrapAuthority::establish_with_identity(startup, provider_identity, mock_provider())
    }

    fn fixture_tx(nonce: u64, from: Address) -> TransactionRequest {
        TransactionRequest::default()
            .with_chain_id(MANTLE_SEPOLIA_CHAIN_ID)
            .with_from(from)
            .with_nonce(nonce)
            .with_max_priority_fee_per_gas(1)
            .with_max_fee_per_gas(2)
            .with_gas_limit(21_000)
            .with_to(Address::repeat_byte(0x9))
            .with_value(U256::ZERO)
    }

    #[tokio::test]
    async fn bootstrap_permit_signs_and_finalize_hands_off_to_a_manifest() {
        let authority = establish_authority(
            KEY_A,
            MANTLE_SEPOLIA_CHAIN_ID,
            MANTLE_SEPOLIA_GENESIS_HASH,
            Address::repeat_byte(0xE2),
        );
        let signer = authority.signer_address();
        let permit = authority
            .mint_bootstrap_permit(BootstrapAction::Deploy, fixture_tx(0, signer))
            .expect("well-formed bootstrap permit must mint");

        let submission = authority
            .sign_bootstrap(permit)
            .await
            .expect("well-bound bootstrap permit must sign");
        assert_eq!(
            submission.view().action,
            SubmissionAction::Bootstrap(BootstrapAction::Deploy)
        );

        // finalize() takes `self` by value: after this line `authority` no longer
        // exists, so no further bootstrap permit can be minted from it. That is
        // enforced by the compiler, not by this assertion — this just proves the
        // manifest it hands back is usable.
        let manifest = authority.finalize();
        assert_eq!(manifest.signer_address(), signer);
    }

    #[tokio::test]
    async fn bootstrap_permit_for_the_wrong_signer_is_rejected_even_under_the_same_provider_identity(
    ) {
        // Share one provider identity across both authorities so the *only*
        // difference between them is the signer — otherwise two independently
        // `establish`ed authorities would also differ on provider identity
        // (each mints its own fresh random session nonce), and that check
        // runs before the signer check, masking what this test means to prove.
        let shared_identity =
            validate_provider_identity(MANTLE_SEPOLIA_CHAIN_ID, MANTLE_SEPOLIA_GENESIS_HASH)
                .unwrap();
        let authority_a =
            establish_authority_with_identity(KEY_A, shared_identity, Address::repeat_byte(0xE2));
        let authority_b_wrong_signer =
            establish_authority_with_identity(KEY_B, shared_identity, Address::repeat_byte(0xE2));

        let signer_a = authority_a.signer_address();
        let permit = authority_a
            .mint_bootstrap_permit(BootstrapAction::Deploy, fixture_tx(0, signer_a))
            .unwrap();
        let err = authority_b_wrong_signer
            .sign_bootstrap(permit)
            .await
            .expect_err("permit minted for a different signer must be rejected");
        assert!(matches!(err, E2eCapabilityError::SignerMismatch { .. }));
    }

    #[tokio::test]
    async fn bootstrap_permit_from_before_a_process_restart_is_rejected_after() {
        let authority_a = establish_authority(
            KEY_A,
            MANTLE_SEPOLIA_CHAIN_ID,
            MANTLE_SEPOLIA_GENESIS_HASH,
            Address::repeat_byte(0xE2),
        );
        // Same key/chain/genesis as A, but a fresh `validate_provider_identity` call
        // mints a new random session nonce — simulating what a process restart or
        // provider reconstruction would produce.
        let authority_restarted = establish_authority(
            KEY_A,
            MANTLE_SEPOLIA_CHAIN_ID,
            MANTLE_SEPOLIA_GENESIS_HASH,
            Address::repeat_byte(0xE2),
        );

        let signer_a = authority_a.signer_address();
        let permit = authority_a
            .mint_bootstrap_permit(BootstrapAction::Config, fixture_tx(1, signer_a))
            .unwrap();
        let err = authority_restarted
            .sign_bootstrap(permit)
            .await
            .expect_err("permit minted before a process restart must not validate after");
        assert_eq!(err, E2eCapabilityError::ProviderIdentityMismatch);
    }

    #[tokio::test]
    async fn manifest_rejects_permits_across_manifests_with_different_executors() {
        // Share one provider identity so the only difference between the two
        // manifests is the executor address (see the note on
        // `establish_authority` about why independently-established
        // authorities aren't suitable for isolating a single field).
        let shared_identity =
            validate_provider_identity(MANTLE_SEPOLIA_CHAIN_ID, MANTLE_SEPOLIA_GENESIS_HASH)
                .unwrap();
        let manifest_a =
            establish_authority_with_identity(KEY_A, shared_identity, Address::repeat_byte(0xE2))
                .finalize();
        let manifest_b_different_executor =
            establish_authority_with_identity(KEY_A, shared_identity, Address::repeat_byte(0xE3))
                .finalize();

        let signer = manifest_a.signer_address();
        let permit = manifest_a
            .mint_trigger_permit(fixture_tx(0, signer))
            .expect("trigger permit must mint");

        let err = manifest_b_different_executor
            .sign(permit, &NoopDurableHook)
            .await
            .expect_err("a permit minted under one manifest must not sign under another");
        // Manifest digest folds in the executor address, so the two manifests differ
        // on manifest digest before they'd even reach the executor-address check.
        assert_eq!(err, E2eCapabilityError::ManifestIdentityMismatch);
    }

    #[tokio::test]
    async fn cancel_permit_round_trips_through_sign_and_its_view_never_exposes_raw_bytes() {
        let manifest = establish_authority(
            KEY_A,
            MANTLE_SEPOLIA_CHAIN_ID,
            MANTLE_SEPOLIA_GENESIS_HASH,
            Address::repeat_byte(0xE2),
        )
        .finalize();
        let signer = manifest.signer_address();
        let permit = manifest
            .mint_cancel_permit(fixture_tx(0, signer))
            .expect("cancel permit must mint");
        let digest = permit.digest();

        let submission = manifest
            .sign(permit, &NoopDurableHook)
            .await
            .expect("well-bound cancel permit must sign");
        let view = submission.view();
        assert_eq!(view.action, SubmissionAction::Sign(E2eSignAction::Cancel));
        assert_eq!(view.digest, digest);
        assert_eq!(view.nonce, 0);
        assert!(
            view.execute_meta.is_none(),
            "cancel has no Execute metadata"
        );
        // `SignedSubmissionView` has no `raw`/bytes field at all — this is a
        // compile-time guarantee, not a runtime one; there is nothing to assert here
        // beyond "the view type does not offer it", which the type signature above
        // already proves.
    }

    #[tokio::test]
    async fn tampered_submission_fails_the_integrity_check_before_any_broadcast_attempt() {
        let manifest = establish_authority(
            KEY_A,
            MANTLE_SEPOLIA_CHAIN_ID,
            MANTLE_SEPOLIA_GENESIS_HASH,
            Address::repeat_byte(0xE2),
        )
        .finalize();
        let signer = manifest.signer_address();
        let permit = manifest.mint_trigger_permit(fixture_tx(0, signer)).unwrap();
        let mut submission = manifest.sign(permit, &NoopDurableHook).await.unwrap();

        // White-box tamper: substitute the tx hash so it no longer matches
        // keccak256(raw). No public API can construct or mutate this field —
        // this test only exists to prove the check inside `broadcast` actually
        // fires, using same-module access unavailable to any real caller.
        submission.tx_hash = B256::repeat_byte(0xFF);

        let err = manifest
            .broadcast(submission)
            .await
            .expect_err("hash-substituted submission must never reach send_raw_transaction");
        assert_eq!(err, E2eCapabilityError::SubmissionIntegrityFailed);
    }

    #[tokio::test]
    async fn manifest_sign_rejects_a_permit_whose_stored_digest_no_longer_matches_its_tx() {
        let manifest = establish_authority(
            KEY_A,
            MANTLE_SEPOLIA_CHAIN_ID,
            MANTLE_SEPOLIA_GENESIS_HASH,
            Address::repeat_byte(0xE2),
        )
        .finalize();
        let signer = manifest.signer_address();
        let mut permit = manifest.mint_trigger_permit(fixture_tx(0, signer)).unwrap();

        // White-box tamper: swap in a digest that no longer matches `permit.tx`.
        // Same rationale as above — no public constructor allows this.
        permit.digest = B256::repeat_byte(0xAB);

        let err = manifest
            .sign(permit, &NoopDurableHook)
            .await
            .expect_err("a permit whose digest no longer matches its tx must be rejected");
        assert_eq!(err, E2eCapabilityError::DigestMismatch);
    }

    #[tokio::test]
    async fn manifest_sign_rejects_a_permit_bound_to_a_different_chain_id() {
        let manifest = establish_authority(
            KEY_A,
            MANTLE_SEPOLIA_CHAIN_ID,
            MANTLE_SEPOLIA_GENESIS_HASH,
            Address::repeat_byte(0xE2),
        )
        .finalize();
        let signer = manifest.signer_address();
        let mut permit = manifest.mint_trigger_permit(fixture_tx(0, signer)).unwrap();

        // White-box tamper: only one chain id (5003) ever validates via the
        // public API, so two independently-established manifests can never
        // differ on chain id — this directly forces the "wrong chain"
        // fixture the acceptance criteria calls for, exercising the same
        // `sign()` check a hypothetical future multi-chain misconfiguration
        // would hit.
        permit.chain_id = 1;

        let err = manifest
            .sign(permit, &NoopDurableHook)
            .await
            .expect_err("a permit bound to a different chain id must be rejected");
        assert_eq!(
            err,
            E2eCapabilityError::ChainIdMismatch {
                expected: MANTLE_SEPOLIA_CHAIN_ID,
                actual: 1,
            }
        );
    }

    #[tokio::test]
    async fn manifest_sign_rejects_a_permit_whose_tx_nonce_was_tampered_after_minting() {
        let manifest = establish_authority(
            KEY_A,
            MANTLE_SEPOLIA_CHAIN_ID,
            MANTLE_SEPOLIA_GENESIS_HASH,
            Address::repeat_byte(0xE2),
        )
        .finalize();
        let signer = manifest.signer_address();
        let mut permit = manifest.mint_trigger_permit(fixture_tx(0, signer)).unwrap();

        // White-box tamper: mutate the tx's nonce without updating
        // `permit.nonce` (no public API allows this) — proves `sign()`'s
        // explicit nonce-consistency check catches a substituted nonce, not
        // just a substituted digest.
        permit.tx.nonce = Some(7);

        let err = manifest
            .sign(permit, &NoopDurableHook)
            .await
            .expect_err("a permit whose tx nonce was tampered after minting must be rejected");
        assert_eq!(
            err,
            E2eCapabilityError::PermitNonceMismatch {
                expected: 0,
                actual: Some(7),
            }
        );
    }

    #[tokio::test]
    async fn bootstrap_sign_rejects_a_permit_bound_to_a_different_chain_id() {
        let authority = establish_authority(
            KEY_A,
            MANTLE_SEPOLIA_CHAIN_ID,
            MANTLE_SEPOLIA_GENESIS_HASH,
            Address::repeat_byte(0xE2),
        );
        let signer = authority.signer_address();
        let mut permit = authority
            .mint_bootstrap_permit(BootstrapAction::Deploy, fixture_tx(0, signer))
            .unwrap();

        permit.chain_id = 1;

        let err = authority
            .sign_bootstrap(permit)
            .await
            .expect_err("a bootstrap permit bound to a different chain id must be rejected");
        assert_eq!(
            err,
            E2eCapabilityError::ChainIdMismatch {
                expected: MANTLE_SEPOLIA_CHAIN_ID,
                actual: 1,
            }
        );
    }
}
