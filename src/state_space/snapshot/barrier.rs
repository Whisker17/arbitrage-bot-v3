//! Fair reader/exclusive-transition barrier for executable identities.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const IDENTITY_PERMITS: usize = 1_000_000;

#[derive(Debug, Clone)]
pub struct IdentityBarrier {
    semaphore: Arc<Semaphore>,
}

impl Default for IdentityBarrier {
    fn default() -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(IDENTITY_PERMITS)),
        }
    }
}

impl IdentityBarrier {
    pub async fn acquire_lease(&self) -> IdentityReadLease {
        IdentityReadLease {
            _permit: Arc::clone(&self.semaphore)
                .acquire_owned()
                .await
                .expect("identity barrier is never closed"),
        }
    }

    pub async fn begin_transition(&self) -> IdentityTransitionGuard {
        IdentityTransitionGuard {
            _permit: Arc::clone(&self.semaphore)
                .acquire_many_owned(IDENTITY_PERMITS as u32)
                .await
                .expect("identity barrier is never closed"),
        }
    }
}

#[derive(Debug)]
pub struct IdentityReadLease {
    _permit: OwnedSemaphorePermit,
}

#[derive(Debug)]
pub struct IdentityTransitionGuard {
    _permit: OwnedSemaphorePermit,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn transition_order_and_lease_release_form_a_barrier() {
        let barrier = IdentityBarrier::default();
        let transition = barrier.begin_transition().await;
        let waiting_lease = {
            let barrier = barrier.clone();
            tokio::spawn(async move { barrier.acquire_lease().await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            !waiting_lease.is_finished(),
            "transition must block a new lease"
        );
        drop(transition);
        let lease = waiting_lease.await.unwrap();

        let waiting_transition = {
            let barrier = barrier.clone();
            tokio::spawn(async move { barrier.begin_transition().await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            !waiting_transition.is_finished(),
            "an acquired lease must delay invalidation"
        );
        drop(lease);
        waiting_transition.await.unwrap();
    }
}
