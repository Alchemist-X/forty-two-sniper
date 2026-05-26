use std::{sync::Arc, time::Duration};

use tokio::sync::Mutex;

#[derive(Clone, Debug)]
pub struct RpcRateLimiter {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    min_interval: Duration,
    next_slot: Mutex<std::time::Instant>,
}

impl RpcRateLimiter {
    pub fn new(max_requests_per_second: u32) -> Self {
        let max_requests_per_second = max_requests_per_second.max(1);
        let nanos = 1_000_000_000u64.div_ceil(max_requests_per_second as u64);

        Self {
            inner: Arc::new(Inner {
                min_interval: Duration::from_nanos(nanos),
                next_slot: Mutex::new(std::time::Instant::now()),
            }),
        }
    }

    pub async fn wait(&self) {
        let mut next_slot = self.inner.next_slot.lock().await;
        let now = std::time::Instant::now();

        if *next_slot > now {
            tokio::time::sleep(*next_slot - now).await;
        }

        *next_slot = std::time::Instant::now() + self.inner.min_interval;
    }
}
