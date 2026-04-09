use std::{collections::VecDeque, ops::Range};

use fork_choice::Store;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

use crate::{
    chain::{ChainMessage, ChainService},
    environment::{Message, Service, ServiceInput, ServiceOutput},
    network::NetworkMessage,
    validator::{ValidatorMessage, ValidatorService},
    Effect, Event, HttpMessage, HttpService, KeyManager, NetworkService, ValidatorConfig,
};

pub trait Strategy {
    fn select(&mut self, range: Range<usize>) -> usize;
}

pub struct NodeSimulator<S: Strategy> {
    strategy: S,

    chain: ChainService,
    network: NetworkService,
    http: HttpService,
    validator: ValidatorService,

    chain_inbox: (Option<ServiceOutput>, VecDeque<ServiceInput<ChainMessage>>),
    network_inbox: (
        Option<ServiceOutput>,
        VecDeque<ServiceInput<NetworkMessage>>,
    ),
    http_inbox: (Option<ServiceOutput>, VecDeque<ServiceInput<HttpMessage>>),
    validator_inbox: (
        Option<ServiceOutput>,
        VecDeque<ServiceInput<ValidatorMessage>>,
    ),
}

impl<S: Strategy> NodeSimulator<S> {
    pub fn new(
        strategy: S,
        store: Store,
        validator_config: ValidatorConfig,
        key_manager: KeyManager,
    ) -> Self {
        Self {
            strategy,
            chain: ChainService::new(store),
            network: NetworkService::new(),
            http: HttpService::new(),
            validator: ValidatorService::new(validator_config, key_manager),

            chain_inbox: (None, VecDeque::new()),
            network_inbox: (None, VecDeque::new()),
            http_inbox: (None, VecDeque::new()),
            validator_inbox: (None, VecDeque::new()),
        }
    }

    pub fn feed(&mut self, event: Event) {
        self.chain_inbox
            .1
            .push_back(ServiceInput::Event(event.clone()));
        self.network_inbox
            .1
            .push_back(ServiceInput::Event(event.clone()));
        self.http_inbox
            .1
            .push_back(ServiceInput::Event(event.clone()));
        self.validator_inbox.1.push_back(ServiceInput::Event(event));
    }

    pub fn store(&self) -> &Store {
        self.chain.store()
    }

    pub fn step(&mut self) -> Vec<Effect> {
        let mut candidates = 0;
        if self.chain_inbox.0.is_some() || self.chain_inbox.1.len() > 0 {
            candidates += 1;
        }
        if self.network_inbox.0.is_some() || self.network_inbox.1.len() > 0 {
            candidates += 1;
        }
        if self.http_inbox.0.is_some() || self.http_inbox.1.len() > 0 {
            candidates += 1;
        }
        if self.validator_inbox.0.is_some() || self.validator_inbox.1.len() > 0 {
            candidates += 1;
        }

        let mut candidate = self.strategy.select(0..candidates);

        if self.chain_inbox.0.is_some() || self.chain_inbox.1.len() > 0 {
            candidate = candidate.saturating_sub(1);

            if candidate == 0 {
                match &mut self.chain_inbox {
                    (opt @ Some(_), _) => {
                        let output = opt.clone().expect("checked above");
                        *opt = None;

                        for message in output.messages {
                            self.put_message(message);
                        }

                        return output.effects.to_vec();
                    }
                    (_, messages) => {
                        let message = messages
                            .pop_front()
                            .expect("impossible - was checked above");

                        let output = self.chain.handle_input(message);

                        if output.effects.len() > 0 || output.messages.len() > 0 {
                            self.chain_inbox.0 = Some(output);
                        }

                        return Vec::new();
                    }
                }
            }
        }

        if self.network_inbox.0.is_some() || self.network_inbox.1.len() > 0 {
            candidate = candidate.saturating_sub(1);

            if candidate == 0 {
                match &mut self.network_inbox {
                    (opt @ Some(_), _) => {
                        let output = opt.clone().expect("checked above");
                        *opt = None;

                        for message in output.messages {
                            self.put_message(message);
                        }

                        return output.effects.to_vec();
                    }
                    (_, messages) => {
                        let message = messages
                            .pop_front()
                            .expect("impossible - was checked above");

                        let output = self.network.handle_input(message);

                        if output.effects.len() > 0 || output.messages.len() > 0 {
                            self.network_inbox.0 = Some(output);
                        }

                        return Vec::new();
                    }
                }
            }
        }

        if self.http_inbox.0.is_some() || self.http_inbox.1.len() > 0 {
            candidate = candidate.saturating_sub(1);

            if candidate == 0 {
                match &mut self.http_inbox {
                    (opt @ Some(_), _) => {
                        let output = opt.clone().expect("checked above");
                        *opt = None;

                        for message in output.messages {
                            self.put_message(message);
                        }

                        return output.effects.to_vec();
                    }
                    (_, messages) => {
                        let message = messages
                            .pop_front()
                            .expect("impossible - was checked above");

                        let output = self.http.handle_input(message);

                        if output.effects.len() > 0 || output.messages.len() > 0 {
                            self.http_inbox.0 = Some(output);
                        }

                        return Vec::new();
                    }
                }
            }
        }

        if self.validator_inbox.0.is_some() || self.validator_inbox.1.len() > 0 {
            candidate = candidate.saturating_sub(1);

            if candidate == 0 {
                match &mut self.validator_inbox {
                    (opt @ Some(_), _) => {
                        let output = opt.clone().expect("checked above");
                        *opt = None;

                        for message in output.messages {
                            self.put_message(message);
                        }

                        return output.effects.to_vec();
                    }
                    (_, messages) => {
                        let message = messages
                            .pop_front()
                            .expect("impossible - was checked above");

                        let output = self.validator.handle_input(message);

                        if output.effects.len() > 0 || output.messages.len() > 0 {
                            self.validator_inbox.0 = Some(output);
                        }

                        return Vec::new();
                    }
                }
            }
        }

        Vec::new()
    }

    pub fn has_pending(&self) -> bool {
        let mut candidates = 0;
        if self.chain_inbox.0.is_some() || self.chain_inbox.1.len() > 0 {
            candidates += 1;
        }
        if self.network_inbox.0.is_some() || self.network_inbox.1.len() > 0 {
            candidates += 1;
        }
        if self.http_inbox.0.is_some() || self.http_inbox.1.len() > 0 {
            candidates += 1;
        }
        if self.validator_inbox.0.is_some() || self.validator_inbox.1.len() > 0 {
            candidates += 1;
        }

        candidates > 0
    }

    fn put_message(&mut self, message: Message) {
        match message {
            Message::Chain(message) => self.chain_inbox.1.push_back(ServiceInput::Message(message)),
            Message::Network(message) => self
                .network_inbox
                .1
                .push_back(ServiceInput::Message(message)),
            Message::Http(message) => self.http_inbox.1.push_back(ServiceInput::Message(message)),
            Message::Validator(message) => self
                .validator_inbox
                .1
                .push_back(ServiceInput::Message(message)),
        }
    }
}

pub struct ChaChaStrategy {
    rand: ChaCha8Rng,
}

impl ChaChaStrategy {
    pub fn with_seed(seed: [u8; 32]) -> Self {
        Self {
            rand: ChaCha8Rng::from_seed(seed),
        }
    }
}

impl Strategy for ChaChaStrategy {
    fn select(&mut self, range: Range<usize>) -> usize {
        self.rand.random_range(range)
    }
}
