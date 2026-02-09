use anyhow::Result;

use crate::{clock::SystemClock, node::Context};

pub struct RealSimulator {
    clock: SystemClock
}

impl Context for RealSimulator {
    type Clock = SystemClock;

    fn clock(&self) -> &Self::Clock {
        &self.clock
    }
}

impl RealSimulator {
    fn new(genesis: u64) -> Result<Self> {
        Ok(Self {
            clock: SystemClock::new(genesis)?
        })
    }
}
