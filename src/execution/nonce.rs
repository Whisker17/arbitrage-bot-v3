use std::sync::atomic::{AtomicU64, Ordering};

/// Private nonce counter for the intent state machine.
///
/// Visibility is intentional: only the intent module may mutate nonces.
/// Callers outside this module must go through `IntentStateMachine`.
#[derive(Debug)]
pub(super) struct NonceManager {
    next_nonce: AtomicU64,
}

impl NonceManager {
    pub(super) fn new(initial_nonce: u64) -> Self {
        Self {
            next_nonce: AtomicU64::new(initial_nonce),
        }
    }

    /// Reserve and return the next nonce value.
    pub(super) fn reserve(&self) -> u64 {
        self.next_nonce.fetch_add(1, Ordering::SeqCst)
    }

    /// Peek current next nonce without increment.
    pub(super) fn peek(&self) -> u64 {
        self.next_nonce.load(Ordering::SeqCst)
    }

    /// Bump the next nonce upward only (e.g. chain advanced past us).
    pub(super) fn sync_at_least(&self, chain_next_nonce: u64) {
        let mut current = self.next_nonce.load(Ordering::SeqCst);
        while chain_next_nonce > current {
            match self.next_nonce.compare_exchange(
                current,
                chain_next_nonce,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    /// Lower or set the next nonce. Only called from `reconcile()`.
    pub(super) fn set_next(&self, next_nonce: u64) {
        self.next_nonce.store(next_nonce, Ordering::SeqCst);
    }
}
