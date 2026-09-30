//! Lock-free counters shared across the crawler (`Arc<Metrics>`). The 1s sampler that
//! turns these into rates and `metrics_samples` rows arrives in milestone 7
//! (see `brainstorms/09-metrics.md`).

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

#[derive(Debug, Default)]
pub struct Metrics {
    pub requests: AtomicU64,
    /// Response counts by status class: index 0 = 1xx … 4 = 5xx.
    pub status_classes: [AtomicU64; 5],
    pub fetch_errors: AtomicU64,
    /// Bytes as received: approximate headers plus the still-compressed body.
    pub bytes_rx_wire: AtomicU64,
    /// Body bytes after decompression.
    pub bytes_rx_body: AtomicU64,
    /// Approximate request size (request line plus our headers).
    pub bytes_tx: AtomicU64,
    /// Bodies downloaded but thrown away (wrong content type, too large, undecodable).
    pub bytes_wasted: AtomicU64,
    pub robots_fetches: AtomicU64,
    pub robots_denied: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MetricsSnapshot {
    pub requests: u64,
    pub status_classes: [u64; 5],
    pub fetch_errors: u64,
    pub bytes_rx_wire: u64,
    pub bytes_rx_body: u64,
    pub bytes_tx: u64,
    pub bytes_wasted: u64,
    pub robots_fetches: u64,
    pub robots_denied: u64,
}

impl Metrics {
    pub fn record_status(&self, status: u16) {
        if let Some(slot) = (status / 100)
            .checked_sub(1)
            .and_then(|i| self.status_classes.get(i as usize))
        {
            slot.fetch_add(1, Relaxed);
        }
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        return MetricsSnapshot {
            requests: self.requests.load(Relaxed),
            status_classes: std::array::from_fn(|i| self.status_classes[i].load(Relaxed)),
            fetch_errors: self.fetch_errors.load(Relaxed),
            bytes_rx_wire: self.bytes_rx_wire.load(Relaxed),
            bytes_rx_body: self.bytes_rx_body.load(Relaxed),
            bytes_tx: self.bytes_tx.load(Relaxed),
            bytes_wasted: self.bytes_wasted.load(Relaxed),
            robots_fetches: self.robots_fetches.load(Relaxed),
            robots_denied: self.robots_denied.load(Relaxed),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_classes_bucket_by_hundreds() {
        let m = Metrics::default();
        for s in [200, 204, 301, 404, 503, 99, 600] {
            m.record_status(s);
        }
        assert_eq!(m.snapshot().status_classes, [0, 2, 1, 1, 1]);
    }
}
