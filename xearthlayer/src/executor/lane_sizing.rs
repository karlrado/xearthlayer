//! Sizes the demand and prefetch lanes (#296).
//!
//! Demand keeps the full derived capacity, so prefetch costs FUSE nothing it
//! would have had with prefetch disabled. The prefetch lane is added on top,
//! scaled by `prefetch.capacity_share`. The pool formulas themselves stay in
//! `ResourcePoolConfig::default()` and are not configurable.

use super::download_config::DEFAULT_MAX_CONCURRENT_HTTP;
use super::resource_pool::{ResourcePoolConfig, BLOCKING_POOL_HEADROOM};

/// Capacity of both lanes, derived from the demand lane's pools.
#[derive(Clone, Debug)]
pub struct LaneSizing {
    pub demand: ResourcePoolConfig,
    pub prefetch: ResourcePoolConfig,
    /// Concurrent prefetch tile jobs allowed in flight.
    pub prefetch_tile_generation: usize,
    /// Prefetch share of the HTTP connection semaphore.
    pub prefetch_http_permits: usize,
}

impl LaneSizing {
    /// Sizes the prefetch lane as `share` of `demand`, never below one permit.
    pub fn derive(demand: ResourcePoolConfig, share: f64) -> Self {
        let scale = |n: usize| ((n as f64 * share).ceil() as usize).max(1);
        let prefetch = ResourcePoolConfig::new(
            scale(demand.network),
            scale(demand.disk_io),
            scale(demand.cpu),
        );
        let prefetch_tile_generation = 2 * (prefetch.cpu + prefetch.network);
        Self {
            prefetch_http_permits: scale(DEFAULT_MAX_CONCURRENT_HTTP),
            prefetch_tile_generation,
            prefetch,
            demand,
        }
    }

    /// Both lanes dispatch CPU and disk work through `spawn_blocking`, so the
    /// blocking pool must cover their sum (#227).
    pub fn blocking_threads_required(&self) -> usize {
        self.demand.cpu
            + self.demand.disk_io
            + self.prefetch.cpu
            + self.prefetch.disk_io
            + BLOCKING_POOL_HEADROOM
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_machine_at_default_share() {
        let sizing = LaneSizing::derive(ResourcePoolConfig::new(60, 64, 40), 0.35);
        assert_eq!(
            (
                sizing.prefetch.cpu,
                sizing.prefetch.network,
                sizing.prefetch.disk_io
            ),
            (14, 21, 23)
        );
        assert_eq!(sizing.prefetch_tile_generation, 70);
        assert_eq!(sizing.prefetch_http_permits, 359);
        assert_eq!(
            sizing.blocking_threads_required(),
            40 + 64 + 14 + 23 + BLOCKING_POOL_HEADROOM
        );
    }

    #[test]
    fn demand_keeps_its_full_capacity() {
        let demand = ResourcePoolConfig::new(60, 64, 40);
        let sizing = LaneSizing::derive(demand, 0.5);
        assert_eq!(
            (
                sizing.demand.cpu,
                sizing.demand.network,
                sizing.demand.disk_io
            ),
            (40, 60, 64)
        );
    }

    #[test]
    fn smallest_share_still_yields_a_working_lane() {
        let sizing = LaneSizing::derive(ResourcePoolConfig::new(8, 64, 6), 0.05);
        assert!(
            sizing.prefetch.cpu >= 1
                && sizing.prefetch.network >= 1
                && sizing.prefetch.disk_io >= 1
        );
        assert!(sizing.prefetch_tile_generation >= 1 && sizing.prefetch_http_permits >= 1);
    }
}
