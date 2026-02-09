use crate::clock::{Interval, Slot};

pub enum Event {
    TickEvent { interval: Interval, slot: Slot },
}
