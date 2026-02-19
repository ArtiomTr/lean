use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use fork_choice::{handlers::on_tick, store::Store};
use tokio::time::{Duration, sleep};
use tracing::{debug, info};

use crate::clock::SlotClock;

/// Drives the consensus clock by periodically ticking the forkchoice store.
///
/// ChainService is the heartbeat of a consensus client. It ensures time
/// advances in the Store, triggering interval-specific actions like
/// attestation acceptance and safe target updates.
///
/// The service is intentionally minimal:
/// - Timer loop that wakes every interval
/// - Ticks the store forward to current time
/// - Updates the shared store reference
pub struct ChainService {
    /// Shared reference to the forkchoice store
    store: Arc<RwLock<Store>>,

    /// Clock for time calculation
    clock: SlotClock,

    /// Whether the service is running
    running: Arc<AtomicBool>,
}

impl ChainService {
    /// Create a new ChainService.
    ///
    /// The store is shared with other services (like ValidatorService) via Arc<RwLock>.
    pub fn new(store: Arc<RwLock<Store>>, clock: SlotClock) -> Self {
        Self {
            store,
            clock,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Main loop - tick the store every interval.
    ///
    /// This is the core of the chain service. It runs forever, sleeping
    /// until each interval boundary and then advancing the store's time.
    ///
    /// The loop continues until the service is stopped.
    ///
    /// NOTE: We track the last handled interval to avoid skipping intervals.
    /// If processing takes time and we end up in a new interval, we
    /// handle it immediately instead of sleeping past it.
    pub async fn run(&self) -> Result<()> {
        self.running.store(true, Ordering::SeqCst);

        // Catch up store time to current wall clock (post-genesis only).
        //
        // - Before genesis, returns None; the main loop handles the wait.
        // - After genesis, this ensures attestation validation accepts valid attestations
        // (the store's time would otherwise lag behind wall-clock).
        let mut last_handled_total_interval = self.initial_tick();

        while self.running.load(Ordering::SeqCst) {
            // Get current wall-clock time
            let current_time = self.clock.current_time();
            let genesis_time = self.clock.genesis_time();

            // Wait for genesis if we're before it
            if current_time < genesis_time {
                let sleep_duration = genesis_time - current_time;
                debug!(
                    sleep_secs = sleep_duration,
                    "Before genesis, sleeping until genesis"
                );
                sleep(Duration::from_secs(sleep_duration)).await;
                continue;
            }

            // Get current total interval count
            let total_interval = self.clock.total_intervals();

            // If we've already handled this interval, sleep until the next boundary
            let already_handled = last_handled_total_interval
                .map(|last| total_interval <= last)
                .unwrap_or(false);

            if already_handled {
                self.sleep_until_next_interval().await;

                // Check if stopped during sleep
                if !self.running.load(Ordering::SeqCst) {
                    break;
                }

                // Re-fetch interval after sleep
                //
                // If still the same (e.g., time didn't advance),
                // skip this iteration to avoid duplicate ticks
                let new_total_interval = self.clock.total_intervals();
                if let Some(last) = last_handled_total_interval {
                    if new_total_interval <= last {
                        continue;
                    }
                }
            }

            // Get current wall-clock time as Unix timestamp (may have changed after sleep)
            //
            // The store expects an absolute timestamp, not intervals.
            // It internally converts to intervals.
            let current_time = self.clock.current_time();

            // Tick the store forward to current time
            //
            // The store advances time interval by interval, performing
            // appropriate actions at each interval.
            //
            // This minimal service does not produce blocks.
            // Block production requires validator keys.
            {
                let mut store = self.store.write().expect("Store lock poisoned");
                on_tick(&mut store, current_time, false);

                info!(
                    slot = self.clock.current_slot(),
                    interval = self.clock.total_intervals(),
                    time = current_time,
                    head = %format!("{:?}", store.head),
                    finalized_slot = store.latest_finalized.slot.0,
                    "Tick"
                );
            }

            // Mark this interval as handled
            last_handled_total_interval = Some(total_interval);
        }

        info!("ChainService stopped");
        Ok(())
    }

    /// Perform initial tick to catch up store time to current wall clock.
    ///
    /// This is called once at startup to ensure the store's time reflects
    /// actual wall clock time, not just the genesis anchor time.
    ///
    /// Returns the interval that was handled, or None if before genesis.
    fn initial_tick(&self) -> Option<u64> {
        let current_time = self.clock.current_time();

        // Only tick if we're past genesis
        if current_time >= self.clock.genesis_time() {
            let mut store = self.store.write().expect("Store lock poisoned");
            on_tick(&mut store, current_time, false);
            debug!(
                time = current_time,
                interval = self.clock.total_intervals(),
                "Initial tick performed"
            );
            return Some(self.clock.total_intervals());
        }

        None
    }

    /// Sleep until the next interval boundary.
    ///
    /// Uses the clock to calculate precise sleep duration, ensuring tick
    /// timing is aligned with network consensus expectations.
    async fn sleep_until_next_interval(&self) {
        let sleep_time = self.clock.seconds_until_next_interval();
        if sleep_time > 0.0 {
            sleep(Duration::from_secs_f64(sleep_time)).await;
        }
    }

    /// Stop the service.
    ///
    /// Sets the running flag to false, causing the run() loop to exit
    /// after completing its current sleep cycle.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        info!("ChainService stop requested");
    }

    /// Check if the service is currently running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}
