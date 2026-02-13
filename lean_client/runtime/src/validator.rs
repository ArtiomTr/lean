use crate::clock::{Interval, Slot, Tick};

pub struct ValidatorService {}

pub enum ValidatorEvent {
    Tick(Tick),
}

impl ValidatorService {
    pub fn handle_event(&self, event: ValidatorEvent) {
        match event {
            ValidatorEvent::Tick(Tick { slot, interval }) => {
                match interval {
                    Interval::BlockProposal => {
                        self.maybe_produce_block(slot);
                    }
                    Interval::AttestationBroadcast => {
                        // sign attestations here
                    }
                    _ => {
                        // validator service doesn't care about any other ticks
                    }
                }
            }
        }
    }

    /// Produce a block if we are the proposer for this slot.
    ///
    /// Checks the proposer schedule against our validator registry. If one of
    /// our validators should propose, produces and emits the block.
    fn maybe_produce_block(&self, slot: Slot) {}
}
