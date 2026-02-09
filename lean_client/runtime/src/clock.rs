//! One of primary non-deterministic source providers - time.
//!
//! The point of this is to abstract time as a non-deterministic source, so it
//! can be simulated in tests.

use std::time::{Duration, SystemTime};

use anyhow::{Result, anyhow};
use strum::{EnumCount, FromRepr};

/// NOTE: if this ever becomes a fractional number of seconds (i.e. 2.5, 0.5,
/// etc.), don't forget to update `current_slot` functionality too.
const SLOT_DURATION: Duration = Duration::from_secs(4);

/// We can't divide duration, as `div`` operator is conditionally const. So
/// instead, we define it as constant value, and check its correctness in tests
/// below.
///
/// When [#143874] gets stabilized, this constant can be replaced with:
/// ```rust,compile_fail
/// SLOT_DURATION / Interval::COUNT as u32
/// ```
///
/// TODO(rust-update): check if [#143874] is stabilized, when upgrading from
/// 1.92.0.
const DURATION_PER_INTERVAL: Duration = Duration::from_secs(1);

pub type Slot = u64;

#[derive(Debug, FromRepr, EnumCount)]
#[repr(u8)]
pub enum Interval {
    BlockProposal = 0,
    AttestationBroadcast = 1,
    SafeTargetUpdate = 2,
    AttestationAcceptance = 3,
}

pub trait Clock {
    /// Returns time elapsed since genesis time. Returns None if genesis is in
    /// the future.
    fn time_since_genesis(&self) -> Option<Duration>;

    fn checked_current_slot(&self) -> Option<Slot> {
        let time = self.time_since_genesis()?;

        Some(time.as_secs() / SLOT_DURATION.as_secs())
    }

    fn current_slot(&self) -> Slot {
        self.checked_current_slot()
            .expect("genesis time is in the future")
    }

    fn checked_current_interval(&self) -> Option<Interval> {
        let elapsed = self.time_since_genesis()?;

        let time_in_slot = elapsed.as_millis() % SLOT_DURATION.as_millis();

        let interval = time_in_slot / DURATION_PER_INTERVAL.as_millis();

        Interval::from_repr(interval.try_into().ok()?)
    }

    fn current_interval(&self) -> Interval {
        self.checked_current_interval()
            .expect("genesis time is in the future")
    }
}

pub struct SystemClock(SystemTime);

impl SystemClock {
    pub fn new(genesis_timestamp: u64) -> Result<Self> {
        SystemTime::UNIX_EPOCH
            .checked_add(Duration::from_secs(genesis_timestamp))
            .ok_or(anyhow!("invalid timestamp"))
            .map(Self)
    }

    pub fn genesis_time(&self) -> SystemTime {
        self.0
    }
}

impl Clock for SystemClock {
    fn time_since_genesis(&self) -> Option<Duration> {
        self.genesis_time().elapsed().ok()
    }
}

#[cfg(test)]
mod tests {
    use strum::EnumCount;

    use crate::clock::{DURATION_PER_INTERVAL, Interval, SLOT_DURATION};

    #[test]
    fn configuration_is_valid() {
        assert_eq!(
            DURATION_PER_INTERVAL,
            SLOT_DURATION / Interval::COUNT as u32
        );
    }
}
