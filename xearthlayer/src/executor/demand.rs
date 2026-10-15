//! Counts FUSE demand jobs in flight and publishes whether any are.
//!
//! The demand lane takes a [`DemandGuard`] for every job it submits and the
//! guard is released however the job ends: success, failure, a FUSE timeout
//! killing it, or the submit itself failing. RAII rather than a call on each
//! path, because one missed exit pins the prefetch lane paused forever (the
//! permit leak in PR #298 was that shape of bug). Subscribers see the latest
//! state, never an event stream, so a remote lane converges after a lost
//! message (#296).

use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::watch;

use crate::metrics::MetricsClient;

#[derive(Clone)]
pub struct DemandTracker {
    inner: Arc<Inner>,
}

struct Inner {
    count: Mutex<usize>,
    outstanding: watch::Sender<bool>,
    metrics: OnceLock<MetricsClient>,
}

impl DemandTracker {
    pub fn new() -> Self {
        let (outstanding, _) = watch::channel(false);
        Self {
            inner: Arc::new(Inner {
                count: Mutex::new(0),
                outstanding,
                metrics: OnceLock::new(),
            }),
        }
    }

    /// Reports the in-flight count and pause state to the metrics daemon.
    /// Call before any guard is taken.
    pub fn with_metrics(self, client: MetricsClient) -> Self {
        let _ = self.inner.metrics.set(client);
        self
    }

    /// Latest-value view: `true` while any demand job is in flight.
    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.inner.outstanding.subscribe()
    }

    pub fn in_flight(&self) -> usize {
        *self.inner.count.lock().expect("demand count poisoned")
    }

    /// Marks one demand job in flight until the guard drops.
    pub fn begin(&self) -> DemandGuard {
        self.inner.adjust(true);
        DemandGuard {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Default for DemandTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Inner {
    /// Count and published state change under one lock so they never disagree.
    fn adjust(&self, up: bool) {
        let mut count = self.count.lock().expect("demand count poisoned");
        *count = if up {
            *count + 1
        } else {
            count.saturating_sub(1)
        };
        let was_outstanding = self.outstanding.send_replace(*count > 0);
        // Emitted under the lock so events arrive in the order the count
        // changed.
        if let Some(metrics) = self.metrics.get() {
            metrics.demand_in_flight(*count);
            if was_outstanding != (*count > 0) {
                metrics.prefetch_paused(*count > 0);
            }
        }
    }
}

/// One demand job in flight; dropping it ends the job's demand.
pub struct DemandGuard {
    inner: Arc<Inner>,
}

impl Drop for DemandGuard {
    fn drop(&mut self) {
        self.inner.adjust(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_tracker_reads_not_paused() {
        let tracker = DemandTracker::new();
        assert!(!*tracker.subscribe().borrow());
        assert_eq!(tracker.in_flight(), 0);
    }

    #[test]
    fn first_guard_publishes_paused_and_last_drop_publishes_resumed() {
        let tracker = DemandTracker::new();
        let rx = tracker.subscribe();
        let a = tracker.begin();
        assert!(*rx.borrow());
        let b = tracker.begin();
        drop(a);
        assert!(*rx.borrow(), "one guard is still live");
        drop(b);
        assert!(!*rx.borrow());
        assert_eq!(tracker.in_flight(), 0);
    }

    #[test]
    fn metrics_see_the_count_and_each_pause_transition() {
        use crate::metrics::MetricEvent;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tracker = DemandTracker::new().with_metrics(MetricsClient::new(tx));
        let a = tracker.begin();
        let b = tracker.begin();
        drop(a);
        drop(b);

        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        let counts: Vec<usize> = events
            .iter()
            .filter_map(|e| match e {
                MetricEvent::DemandInFlight { count } => Some(*count),
                _ => None,
            })
            .collect();
        let pauses: Vec<bool> = events
            .iter()
            .filter_map(|e| match e {
                MetricEvent::PrefetchPaused { paused } => Some(*paused),
                _ => None,
            })
            .collect();
        assert_eq!(counts, [1, 2, 1, 0]);
        assert_eq!(pauses, [true, false], "only the edges, not every change");
    }

    #[test]
    fn guards_dropped_on_other_threads_still_reach_zero() {
        let tracker = DemandTracker::new();
        let guards: Vec<_> = (0..64).map(|_| tracker.begin()).collect();
        std::thread::scope(|s| {
            for g in guards {
                s.spawn(move || drop(g));
            }
        });
        assert_eq!(tracker.in_flight(), 0);
        assert!(!*tracker.subscribe().borrow());
    }
}
