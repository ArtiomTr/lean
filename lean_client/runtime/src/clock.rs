use anyhow::{Error, Result};
use clock::{SystemClock, Tick};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use crate::environment::EventSource;

impl EventSource for SystemClock {
    type Effect = ();
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
