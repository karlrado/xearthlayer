//! Latency summary over one sample window.
//!
//! The no-harm measure for the prefetch lane is FUSE demand latency per
//! 60 s sample (#296). A window collects durations and `take()` closes the
//! sample, so each log line describes only its own interval.

/// Median, 95th percentile and maximum of one closed window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LatencySummary {
    pub median_us: u64,
    pub p95_us: u64,
    pub max_us: u64,
    pub count: usize,
}

#[derive(Clone, Debug, Default)]
pub struct LatencyWindow {
    samples: Vec<u64>,
}

impl LatencyWindow {
    pub fn record(&mut self, duration_us: u64) {
        self.samples.push(duration_us);
    }

    /// Closes the window: summarises it and starts a new one. `None` when
    /// nothing was recorded.
    pub fn take(&mut self) -> Option<LatencySummary> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted = std::mem::take(&mut self.samples);
        sorted.sort_unstable();
        let n = sorted.len();
        // Nearest rank: the smallest value with at least p of the samples at
        // or below it.
        let rank = |p: f64| sorted[((p * n as f64).ceil() as usize).clamp(1, n) - 1];
        Some(LatencySummary {
            median_us: rank(0.5),
            p95_us: rank(0.95),
            max_us: sorted[n - 1],
            count: n,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_window_has_no_summary() {
        assert!(LatencyWindow::default().take().is_none());
    }

    #[test]
    fn summary_is_nearest_rank_and_window_resets() {
        let mut w = LatencyWindow::default();
        for ms in 1..=100u64 {
            w.record(ms * 1000);
        }
        let s = w.take().unwrap();
        assert_eq!(
            (s.median_us, s.p95_us, s.max_us, s.count),
            (50_000, 95_000, 100_000, 100)
        );
        assert!(w.take().is_none(), "take() starts a new sample");
    }
}
