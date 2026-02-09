use crate::{clock::Clock, event::Event};

pub struct Node<C: Context> {
    context: C,
}

pub trait Context {
    type Clock: Clock;

    fn clock(&self) -> Self::Clock;
}

impl<C: Context> Node<C> {
    fn handle_event(&self, event: Event) {
        match event {
            Event::TickEvent { interval, slot } => {}
        }
    }
}
