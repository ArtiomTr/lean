use crate::clock::Tick;

#[derive(Debug, Clone)]
pub enum Event {
    Tick(Tick),
}
