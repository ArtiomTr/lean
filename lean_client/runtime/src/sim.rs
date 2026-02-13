use anyhow::{Error, Result};

use tokio::sync::{broadcast, mpsc};
use tokio_stream::StreamExt as _;

use crate::{clock::SystemClock, event::Event, node::Context};

pub struct RealSimulator {
    clock: SystemClock,
}

impl Context for RealSimulator {
    type Clock = SystemClock;

    fn clock(&self) -> &Self::Clock {
        &self.clock
    }
}

impl RealSimulator {
    pub fn new(genesis: u64) -> Result<Self> {
        Ok(Self {
            clock: SystemClock::new(genesis)?,
        })
    }

    pub fn run(self) -> Result<()> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;

        runtime.block_on(async move {
            self.execute().await;
        });

        Ok(())
    }

    async fn execute(self) -> Result<()> {
        let (tx, rx) = broadcast::channel(128);

        let mut ticks = self.clock.ticks()?;

        // Spawn event collector.
        // This thread collects all events from all external non-determinism
        // sources, and broadcast
        let handle = tokio::spawn(async move {
            loop {
                let event = tokio::select! {
                    Some(tick) = ticks.next() => {
                        Event::Tick(tick?)
                    }
                    else => {
                        break Ok::<_, Error>(())
                    }
                };

                tx.send(event)?;
            }
        });

        // Spawn each actor separately

        // Spawn network service

        todo!()
    }
}
