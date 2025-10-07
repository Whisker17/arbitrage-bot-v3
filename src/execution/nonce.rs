use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug)]
pub struct NonceManager {
    next_nonce: AtomicU64,
}

impl NonceManager {
    pub fn new(initial_nonce: u64) -> Self {
        Self {
            next_nonce: AtomicU64::new(initial_nonce),
        }
    }

    /// Reserve and return the next nonce value
    pub fn reserve(&self) -> u64 {
        self.next_nonce.fetch_add(1, Ordering::SeqCst)
    }

    /// Peek current next nonce without increment
    pub fn peek(&self) -> u64 {
        self.next_nonce.load(Ordering::SeqCst)
    }

    /// Bump the next nonce (e.g., if chain used a higher one)
    pub fn sync_at_least(&self, chain_next_nonce: u64) {
        let mut current = self.next_nonce.load(Ordering::SeqCst);
        while chain_next_nonce > current {
            let res = self.next_nonce.compare_exchange(
                current,
                chain_next_nonce,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
            match res {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }
}
