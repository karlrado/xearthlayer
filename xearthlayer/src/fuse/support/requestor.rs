//! Neutral DDS-generation request pipeline.
//!
//! [`DdsRequestor`] encapsulates the request lifecycle shared by every
//! backend: coordinate conversion, optional coalescing, timeout and
//! cancellation, and response validation. It carries no kernel concept;
//! the backend supplies the client, timeout, and context through the small
//! set of required methods.
//!
//! See `docs/dev/windows-backend-split.md` §3.5 for the design.

use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn, Instrument};

use crate::coord::TileCoord;
use crate::executor::DdsClient;
use crate::fuse::coalesce::{CoalesceResult, CoalescedResult, RequestCoalescer};
use crate::fuse::{get_default_placeholder, validate_dds_or_placeholder, DdsFilename};

use super::attrs::chunk_to_tile_coords;

/// Trait for requesting DDS texture generation.
///
/// This trait encapsulates the common pattern for requesting DDS textures
/// from the async pipeline, handling timeouts, cancellation, and validation.
///
/// # Implementation Notes
///
/// The default implementation handles:
/// 1. Converting chunk coordinates to tile coordinates
/// 2. Submitting requests to the DdsClient (new daemon architecture)
/// 3. Timeout handling with cancellation
/// 4. Response validation to prevent X-Plane crashes
///
/// # Migration
///
/// Implementations should provide `dds_client()`. The legacy `dds_handler()`
/// method exists for backward compatibility but should not be used in new code.
#[allow(async_fn_in_trait)] // Internal trait, Send bounds not needed
pub trait DdsRequestor {
    /// Get the DdsClient for submitting requests (new daemon architecture).
    ///
    /// This is the primary method for DDS generation. Implementations should
    /// return their DdsClient here. The client sends requests to the job
    /// executor daemon which handles tile generation asynchronously.
    fn dds_client(&self) -> &Arc<dyn DdsClient>;

    /// Get the timeout duration for DDS generation.
    fn generation_timeout(&self) -> Duration;

    /// Get the context label for logging.
    fn context_label(&self) -> &'static str;

    /// Get the optional tile request callback.
    fn tile_request_callback(&self) -> Option<&crate::prefetch::TileRequestCallback>;

    /// Hook invoked after a DDS response is received.
    ///
    /// Default is a no-op; `Fuse3OrthoUnionFS` overrides it to detect
    /// prefetch state divergence (#176). Kept on the trait so the check sits
    /// at the one place every FUSE implementation receives a response.
    fn on_dds_response(&self, _tile: TileCoord, _cache_hit: bool) {}

    /// Get the optional request coalescer for deduplicating concurrent requests.
    ///
    /// When this returns `Some`, requests for the same tile are coalesced at the
    /// FUSE layer. Only one request is sent to the executor; all others wait for
    /// the same result. This prevents duplicate work during X-Plane's burst
    /// loading patterns.
    ///
    /// When `None` (default), no coalescing is performed at the FUSE layer.
    fn request_coalescer(&self) -> Option<&Arc<RequestCoalescer>> {
        None
    }

    /// Get the optional metrics client for reporting FUSE-level metrics.
    ///
    /// When `Some`, coalesced requests and other FUSE-specific metrics are
    /// reported to the metrics system.
    fn metrics_client(&self) -> Option<&crate::metrics::MetricsClient> {
        None
    }

    /// Request DDS generation from the async pipeline.
    ///
    /// This method handles the full request lifecycle:
    /// 1. Converts coordinates and notifies tile callback
    /// 2. **Coalescing check**: If a coalescer is configured, duplicate requests
    ///    for the same tile wait for the in-flight result instead of starting new work
    /// 3. Submits request via DdsClient (daemon architecture)
    /// 4. Awaits response with timeout
    /// 5. Validates DDS data before returning
    ///
    /// # Request Coalescing
    ///
    /// If `request_coalescer()` returns `Some`, requests are deduplicated at the
    /// FUSE layer. X-Plane commonly requests the same tile multiple times during
    /// scene loading - coalescing ensures only one actual generation runs.
    ///
    /// # Arguments
    ///
    /// * `coords` - Parsed DDS filename coordinates
    ///
    /// # Returns
    ///
    /// Generated DDS data, or placeholder on error/timeout
    async fn request_dds_impl(&self, coords: &DdsFilename) -> Bytes {
        // Defence in depth (R8): lit names are routed to the lit source before
        // they get here. If one arrives anyway, it must never reach the
        // coalescer or the client, whose keys would collide with the ortho tile.
        if coords.kind == crate::fuse::TextureKind::Lit {
            return crate::night::minimal_black_dds();
        }

        let tile = chunk_to_tile_coords(coords);
        let context_label = self.context_label();
        let timeout = self.generation_timeout();

        // Notify tile request callback for FUSE inference (fast, non-blocking)
        if let Some(callback) = self.tile_request_callback() {
            callback(tile);
        }

        let using_coalescer = self.request_coalescer().is_some();

        debug!(
            chunk_row = coords.row,
            chunk_col = coords.col,
            chunk_zoom = coords.zoom,
            tile_row = tile.row,
            tile_col = tile.col,
            tile_zoom = tile.zoom,
            context = context_label,
            using_coalescer,
            "Requesting DDS generation"
        );

        // Request coalescing: if enabled, check for in-flight requests
        let data = if let Some(coalescer) = self.request_coalescer() {
            let coalesce_result = coalescer.register(tile);

            match coalesce_result {
                CoalesceResult::Coalesced(mut rx) => {
                    // Another request is in-flight - wait for its result
                    debug!(
                        tile = ?tile,
                        context = context_label,
                        "Request coalesced - waiting for in-flight result"
                    );

                    // Report coalesced request to metrics
                    if let Some(metrics) = self.metrics_client() {
                        metrics.job_coalesced();
                    }

                    match tokio::time::timeout(timeout, rx.recv()).await {
                        Ok(Ok(result)) => {
                            debug!(
                                tile = ?tile,
                                cache_hit = result.cache_hit,
                                duration_ms = result.duration.as_millis(),
                                context = context_label,
                                "Coalesced request completed"
                            );
                            result.into_data()
                        }
                        Ok(Err(_)) | Err(_) => {
                            // Broadcast channel error or timeout - return placeholder
                            warn!(
                                tile = ?tile,
                                context = context_label,
                                "Coalesced request failed - using placeholder"
                            );
                            get_default_placeholder()
                        }
                    }
                }
                CoalesceResult::NewRequest { tile, .. } => {
                    // This is the first request - do the actual work
                    let start = std::time::Instant::now();
                    let data = self.do_request(tile, timeout, context_label).await;
                    let duration = start.elapsed();

                    // Complete the coalescer so waiting requests receive the result
                    let result = CoalescedResult::new(data.clone(), false, duration);
                    coalescer.complete(tile, result);

                    data
                }
            }
        } else {
            // No coalescing - send request directly
            self.do_request(tile, timeout, context_label).await
        };

        // Critical: Validate DDS before returning to X-Plane
        // Invalid DDS data causes X-Plane to crash
        let validation_context =
            format!("{}({},{},{})", context_label, tile.row, tile.col, tile.zoom);
        validate_dds_or_placeholder(data, &validation_context)
    }

    /// Perform the actual DDS request (without coalescing).
    ///
    /// Sends the request to the job executor daemon via DdsClient.
    async fn do_request(
        &self,
        tile: TileCoord,
        timeout: Duration,
        context_label: &'static str,
    ) -> Bytes {
        let client = self.dds_client();
        let cancellation_token = CancellationToken::new();

        // Submit request via DdsClient
        let rx = client.request_dds(tile, cancellation_token.clone());

        // Await response with timeout (instrumented for profiling)
        let dds_await_span = tracing::debug_span!(target: "profiling", "dds_await", tile_row = tile.row, tile_col = tile.col,);
        match tokio::time::timeout(timeout, rx)
            .instrument(dds_await_span)
            .await
        {
            Ok(Ok(response)) => {
                debug!(
                    tile_row = tile.row,
                    tile_col = tile.col,
                    tile_zoom = tile.zoom,
                    cache_hit = response.cache_hit,
                    duration_ms = response.duration.as_millis(),
                    context = context_label,
                    "DDS request completed"
                );
                self.on_dds_response(tile, response.cache_hit);
                response.data
            }
            Ok(Err(_)) => {
                // Channel closed - daemon shutdown
                error!(
                    context = context_label,
                    "DDS daemon channel closed unexpectedly"
                );
                cancellation_token.cancel();
                get_default_placeholder()
            }
            Err(_) => {
                // Timeout - cancel the request
                // Enhanced logging for stall diagnosis
                error!(
                    tile_row = tile.row,
                    tile_col = tile.col,
                    tile_zoom = tile.zoom,
                    timeout_secs = timeout.as_secs(),
                    context = context_label,
                    "TIMEOUT: DDS generation exceeded {}s - possible executor stall. \
                     Tile: ({},{},{}) Context: {}. Returning placeholder.",
                    timeout.as_secs(),
                    tile.row,
                    tile.col,
                    tile.zoom,
                    context_label
                );
                cancellation_token.cancel();
                get_default_placeholder()
            }
        }
    }
}
