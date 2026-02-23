use std::time::{SystemTime, UNIX_EPOCH};

/// Number of intervals per slot for forkchoice processing
pub const INTERVALS_PER_SLOT: u64 = 5;

/// Fixed duration of a single slot in seconds
pub const SECONDS_PER_SLOT: u64 = 4;

/// Fixed duration of a single slot in milliseconds
pub const MILLISECONDS_PER_SLOT: u64 = SECONDS_PER_SLOT * 1000;

/// Milliseconds per forkchoice processing interval
pub const MILLISECONDS_PER_INTERVAL: u64 = MILLISECONDS_PER_SLOT / INTERVALS_PER_SLOT;

/// Converts wall-clock time to consensus slots and intervals.
///
/// The slot clock bridges wall-clock time to the discrete slot-based time
/// model used by consensus. Every node must agree on slot boundaries to
/// coordinate block proposals and attestations.
///
/// All time values are in seconds (Unix timestamps) except where milliseconds
/// are needed for sub-second interval boundaries.
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

    /// Milliseconds elapsed since genesis (0 if before genesis).
    fn milliseconds_since_genesis(&self) -> u64 {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("System time is before Unix epoch")
            .as_millis() as u64;
        let genesis_ms = self.genesis_time * 1000;
        if now_ms < genesis_ms {
            return 0;
        }
        now_ms - genesis_ms
    }

    /// Get the current slot number (0 if before genesis).
    pub fn current_slot(&self) -> u64 {
        self.seconds_since_genesis() / SECONDS_PER_SLOT
    }

    /// Get the current interval within the slot (0-4).
    pub fn current_interval(&self) -> u64 {
        let ms_into_slot = self.milliseconds_since_genesis() % MILLISECONDS_PER_SLOT;
        ms_into_slot / MILLISECONDS_PER_INTERVAL
    }

    /// Get total intervals elapsed since genesis.
    ///
    /// This is the value expected by the store time type.
    pub fn total_intervals(&self) -> u64 {
        self.milliseconds_since_genesis() / MILLISECONDS_PER_INTERVAL
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

        let elapsed_ms = (elapsed * 1000.0) as u64;
        let time_into_interval_ms = elapsed_ms % MILLISECONDS_PER_INTERVAL;
        let ms_until_next = MILLISECONDS_PER_INTERVAL - time_into_interval_ms;
        ms_until_next as f64 / 1000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slot_clock_basic() {
        let genesis_time = 1000;
        let clock = SlotClock::new(genesis_time);

        assert_eq!(clock.genesis_time(), genesis_time);
    }

    #[test]
    fn test_constants() {
        assert_eq!(SECONDS_PER_SLOT, 4);
        assert_eq!(INTERVALS_PER_SLOT, 5);
        assert_eq!(MILLISECONDS_PER_SLOT, 4000);
        assert_eq!(MILLISECONDS_PER_INTERVAL, 800);
    }
}
