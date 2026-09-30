//! Per-host politeness: at most one request in flight per host, with a minimum gap
//! between requests (the configured delay, or robots.txt `Crawl-delay` if larger).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

/// robots.txt `Crawl-delay` values above this are clamped, so one host can't stall a worker for long.
pub const MAX_CRAWL_DELAY: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
struct HostState {
    in_flight: bool,
    next_allowed: Option<Instant>,
    crawl_delay: Option<Duration>,
}

#[derive(Debug)]
struct Inner {
    default_delay: Duration,
    hosts: Mutex<HashMap<String, HostState>>,
}

#[derive(Debug, Clone)]
pub struct HostGate {
    inner: Arc<Inner>,
}

/// Held while requesting from a host. Dropping it starts the host's cool-down.
#[derive(Debug)]
pub struct HostPermit {
    inner: Arc<Inner>,
    host: String,
}

impl HostGate {
    pub fn new(default_delay: Duration) -> Self {
        return Self {
            inner: Arc::new(Inner {
                default_delay,
                hosts: Mutex::new(HashMap::new()),
            }),
        };
    }

    /// Records a robots.txt `Crawl-delay` for `host` (clamped to [`MAX_CRAWL_DELAY`]).
    pub fn set_crawl_delay(&self, host: &str, delay: Option<Duration>) {
        let mut hosts = self.inner.hosts.lock().unwrap();
        hosts.entry(host.to_string()).or_default().crawl_delay =
            delay.map(|d| d.min(MAX_CRAWL_DELAY));
    }

    /// Waits until `host` is free and its cool-down has passed.
    pub async fn acquire(&self, host: &str) -> HostPermit {
        loop {
            let wait = {
                let mut hosts = self.inner.hosts.lock().unwrap();
                let state = hosts.entry(host.to_string()).or_default();
                let now = Instant::now();
                match (state.in_flight, state.next_allowed) {
                    (false, Some(t)) if t > now => t - now,
                    (false, _) => {
                        state.in_flight = true;
                        return HostPermit {
                            inner: self.inner.clone(),
                            host: host.to_string(),
                        };
                    }
                    // Another task holds the host; its cool-down is unknown until it finishes.
                    (true, _) => self.inner.default_delay.min(Duration::from_millis(100)),
                }
            };
            tokio::time::sleep(wait).await;
        }
    }
}

impl Drop for HostPermit {
    fn drop(&mut self) {
        let mut hosts = self.inner.hosts.lock().unwrap();
        if let Some(state) = hosts.get_mut(&self.host) {
            let delay = state
                .crawl_delay
                .unwrap_or_default()
                .max(self.inner.default_delay);
            state.in_flight = false;
            state.next_allowed = Some(Instant::now() + delay);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn spaces_requests_to_same_host() {
        let gate = HostGate::new(Duration::from_secs(1));
        let start = Instant::now();
        drop(gate.acquire("a.com").await);
        drop(gate.acquire("a.com").await);
        drop(gate.acquire("a.com").await);
        assert_eq!(start.elapsed().as_secs(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn different_hosts_do_not_wait() {
        let gate = HostGate::new(Duration::from_secs(1));
        let start = Instant::now();
        let _a = gate.acquire("a.com").await;
        let _b = gate.acquire("b.com").await;
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn crawl_delay_raises_but_never_lowers_the_gap() {
        let gate = HostGate::new(Duration::from_secs(1));
        gate.set_crawl_delay("slow.com", Some(Duration::from_secs(5)));
        gate.set_crawl_delay("fast.com", Some(Duration::from_millis(10)));
        gate.set_crawl_delay("huge.com", Some(Duration::from_secs(3600)));

        for (host, expected) in [("slow.com", 5), ("fast.com", 1), ("huge.com", 30)] {
            let start = Instant::now();
            drop(gate.acquire(host).await);
            drop(gate.acquire(host).await);
            assert_eq!(start.elapsed().as_secs(), expected, "{host}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn one_in_flight_per_host() {
        let gate = HostGate::new(Duration::from_secs(1));
        let held = gate.acquire("a.com").await;
        let g = gate.clone();
        let waiter = tokio::spawn(async move {
            let start = Instant::now();
            let _p = g.acquire("a.com").await;
            return start.elapsed();
        });
        tokio::time::sleep(Duration::from_secs(3)).await;
        drop(held);
        let waited = waiter.await.unwrap();
        // 3s held + 1s cool-down (within one 100ms poll interval).
        assert!(
            waited >= Duration::from_secs(4) && waited <= Duration::from_millis(4100),
            "{waited:?}"
        );
    }
}
