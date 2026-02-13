use fork_choice::store::Store;

use crate::clock::{Interval, Tick};

pub struct ChainService {
    store: Store,
}

pub enum ChainEvent {
    Tick(Tick),
}

impl ChainService {
    fn handle_event(&self, event: ChainEvent) {
        match event {
            ChainEvent::Tick(Tick { slot, interval }) => match interval {
                Interval::BlockProposal => {
                    // Prepare for block proposal - determine current block
                    // proposer. Send determined block proposer to validator
                    // service, so it can build block if needed.
                    let head_state = self
                        .store
                        .states
                        .get(&self.store.head)
                        .expect("head state is always present");
                }
                _ => {}
            },
        }
    }
}
