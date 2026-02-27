//! One of primary non-deterministic source providers - time.
//!
//! The point of this is to abstract time as a non-deterministic source, so it
//! can be simulated in tests.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Error, Result, anyhow};
use enum_iterator::Sequence;
use strum::FromRepr;
use tokio::{sync::mpsc, time::Instant};
use tokio_stream::{Stream, StreamExt, wrappers::IntervalStream};

use crate::environment::EventSource;

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

#[derive(Debug, Clone, Copy)]
pub struct Tick {
    pub slot: Slot,
    pub interval: Interval,
}

impl Tick {
    #[must_use]
    pub const fn start_of_slot(slot: Slot) -> Self {
        Self::new(slot, Interval::BlockProposal)
    }

    pub const fn new(slot: Slot, interval: Interval) -> Self {
        Self { slot, interval }
    }

    fn next(self) -> Result<Self> {
        let Self { slot, interval } = self;

        let next_slot = match interval.next() {
            Some(_) => slot,
            None => slot.checked_add(1).ok_or(anyhow!("out of slots"))?,
        };

        let next_interval = enum_iterator::next_cycle(&interval);

        Ok(Self::new(next_slot, next_interval))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Sequence, FromRepr)]
#[repr(u8)]
pub enum Interval {
    BlockProposal,
    AttestationBroadcast,
    SafeTargetUpdate,
    AttestationAcceptance,
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

    pub fn ticks(&self) -> Result<impl Stream<Item = Result<Tick>> + use<>> {
        let now_instant = Instant::now();
        let now_system_time = SystemTime::now();

        let (mut next_tick, next_instant) = next_tick_with_instant(
            now_instant,
            now_system_time,
            self.genesis_time()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )?;

        let interval = tokio::time::interval_at(next_instant.into(), SLOT_DURATION);

        Ok(IntervalStream::new(interval).map(move |_| {
            let current_tick = next_tick;
            next_tick = current_tick.next()?;
            Ok(current_tick)
        }))
    }
}

impl EventSource for SystemClock {
    // Clock has no effects
    type Effect = ();

    // Clock emits only tick events.
    type Event = Tick;

    async fn run(
        &mut self,
        tx: mpsc::UnboundedSender<Self::Event>,
        _: mpsc::UnboundedReceiver<Self::Effect>,
    ) -> Result<()> {
        let mut ticks = self.ticks()?;

        tokio::spawn(async move {
            while let Some(tick) = ticks.next().await {
                let tick = tick?;
                tx.send(tick)?;
            }

            Ok::<_, Error>(())
        });

        Ok(())
    }
}

fn next_tick_with_instant(
    now_instant: Instant,
    now_system_time: SystemTime,
    genesis_time: u64,
) -> Result<(Tick, Instant)> {
    let unix_epoch_to_now = now_system_time.duration_since(UNIX_EPOCH)?;
    let unix_epoch_to_genesis = Duration::from_secs(genesis_time);

    let mut next_tick;
    let mut now_to_next_tick;

    if unix_epoch_to_now <= unix_epoch_to_genesis {
        next_tick = Tick::start_of_slot(0);
        now_to_next_tick = unix_epoch_to_genesis
            .checked_sub(unix_epoch_to_now)
            .expect("the difference from genesis to now fits in Duration");
    } else {
        let genesis_to_now = unix_epoch_to_now
            .checked_sub(unix_epoch_to_genesis)
            .expect("the difference from now to genesis fits in Duration");
        let slots_since_genesis = genesis_to_now.as_secs() / SLOT_DURATION.as_secs();
        let genesis_to_current_slot =
            Duration::from_secs(slots_since_genesis * SLOT_DURATION.as_secs());
        let current_slot_to_now = genesis_to_now
            .checked_sub(genesis_to_current_slot)
            .expect("the difference from now to current slot fits in Duration");

        next_tick = Tick::start_of_slot(slots_since_genesis);
        now_to_next_tick = Duration::ZERO;

        while now_to_next_tick < current_slot_to_now {
            next_tick = next_tick.next()?;
            now_to_next_tick += DURATION_PER_INTERVAL;
        }

        now_to_next_tick -= current_slot_to_now;
    }

    let next_instant = now_instant
        .checked_add(now_to_next_tick)
        .ok_or(anyhow!("next instant overflow"))?;

    Ok((next_tick, next_instant))
}

impl Clock for SystemClock {
    fn time_since_genesis(&self) -> Option<Duration> {
        self.genesis_time().elapsed().ok()
    }
}

#[cfg(test)]
mod tests {
    use enum_iterator::Sequence;

    use crate::clock::{DURATION_PER_INTERVAL, Interval, SLOT_DURATION};

    #[test]
    fn configuration_is_valid() {
        assert_eq!(
            DURATION_PER_INTERVAL,
            SLOT_DURATION / Interval::CARDINALITY as u32
        );
    }
}
