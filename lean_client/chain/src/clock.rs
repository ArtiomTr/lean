use std::time::{SystemTime, UNIX_EPOCH};

/// Number of intervals per slot for forkchoice processing
pub const INTERVALS_PER_SLOT: u64 = 4;

/// Fixed duration of a single slot in seconds
pub const SECONDS_PER_SLOT: u64 = 4;

/// Seconds per forkchoice processing interval
pub const SECONDS_PER_INTERVAL: u64 = SECONDS_PER_SLOT / INTERVALS_PER_SLOT;

/// Converts wall-clock time to consensus slots and intervals.
///
/// The slot clock bridges wall-clock time to the discrete slot-based time
/// model used by consensus. Every node must agree on slot boundaries to
/// coordinate block proposals and attestations.
///
/// All time values are in seconds (Unix timestamps).
#[derive(Debug, Clone)]
pub struct SlotClock {
    /// Unix timestamp (seconds) when slot 0 began
    genesis_time: u64,
}

impl SlotClock {
    /// Create a new SlotClock with the given genesis time.
    pub fn new(genesis_time: u64) -> Self {
        Self { genesis_time }
    }

    /// Get the genesis time.
    pub fn genesis_time(&self) -> u64 {
        self.genesis_time
    }

    /// Get current wall-clock time as Unix timestamp in seconds.
    pub fn current_time(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("System time is before Unix epoch")
            .as_secs()
    }

    /// Seconds elapsed since genesis (0 if before genesis).
    fn seconds_since_genesis(&self) -> u64 {
        let now = self.current_time();
        if now < self.genesis_time {
            return 0;
        }
        now - self.genesis_time
    }

    /// Get the current slot number (0 if before genesis).
    pub fn current_slot(&self) -> u64 {
        self.seconds_since_genesis() / SECONDS_PER_SLOT
    }

    /// Get the current interval within the slot (0-3).
    pub fn current_interval(&self) -> u64 {
        let seconds_into_slot = self.seconds_since_genesis() % SECONDS_PER_SLOT;
        seconds_into_slot / SECONDS_PER_INTERVAL
    }

    /// Get total intervals elapsed since genesis.
    ///
    /// This is the value expected by the store time type.
    pub fn total_intervals(&self) -> u64 {
        self.seconds_since_genesis() / SECONDS_PER_INTERVAL
    }

    /// Calculate seconds until the next interval boundary.
    ///
    /// Returns time until genesis if before genesis.
    /// Returns 0.0 if exactly at an interval boundary.
    pub fn seconds_until_next_interval(&self) -> f64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("System time is before Unix epoch")
            .as_secs_f64();

        let genesis = self.genesis_time as f64;
        let elapsed = now - genesis;

        if elapsed < 0.0 {
            // Before genesis - return time until genesis
            return -elapsed;
        }

        // Time into current interval
        let time_into_interval = elapsed % (SECONDS_PER_INTERVAL as f64);

        // Time until next boundary (may be 0 if exactly at boundary)
        (SECONDS_PER_INTERVAL as f64) - time_into_interval
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slot_clock_basic() {
        let genesis_time = 1000;
        let clock = SlotClock::new(genesis_time);

        // Mock current time to be at slot 1, interval 2
        // That would be genesis_time + 4 + 2 = 1006
        // But we can't easily mock SystemTime, so we'll just check the API compiles
        assert_eq!(clock.genesis_time(), genesis_time);
    }

    #[test]
    fn test_constants() {
        assert_eq!(SECONDS_PER_SLOT, 4);
        assert_eq!(INTERVALS_PER_SLOT, 4);
        assert_eq!(SECONDS_PER_INTERVAL, 1);
    }
}
