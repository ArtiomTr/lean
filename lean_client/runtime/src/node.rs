//! Real-time node: runs services and event sources on the tokio runtime.
//!
//! The `Node` wires together:
//! - `SystemClock` (EventSource) — emits `Event::Tick` at each consensus interval.
//! - `NetworkEventSource` (EventSource) — emits `Event::Network` for
//!   inbound gossip and consumes `Effect`s for outbound gossip and block requests.
//! - `ChainService` (Service) — owns the fork choice store; processes ticks and
//!   network events; handles block/attestation production requests from validators.
//! - `ValidatorService` (Service, optional) — drives block proposals and attestations.
//!
//! Effects produced by services are forwarded to the appropriate EventSource.
//! Currently all effects go to the `NetworkEventSource`; the clock has no effects.

use anyhow::{Error, Result};
use fork_choice::Store;
use tokio::sync::mpsc;
use tracing::warn;

use crate::{
    chain::ChainService,
    clock::SystemClock,
    environment::{Effect, Event, EventSource, Message, Service, ServiceInput},
    network::NetworkEventSource,
    validator::{KeyManager, ValidatorConfig, ValidatorService},
};

pub struct Node {
    clock: SystemClock,
    store: Store,
    validator_config: Option<ValidatorConfig>,
    key_manager: Option<KeyManager>,
    network_config: NetworkConfig,
}

impl Node {
    pub fn new(
        genesis: u64,
        store: Store,
        validator_config: Option<ValidatorConfig>,
        key_manager: Option<KeyManager>,
        network_config: NetworkConfig,
    ) -> Result<Self> {
        Ok(Self {
            clock: SystemClock::new(genesis)?,
            store,
            validator_config,
            key_manager,
            network_config,
        })
    }

    pub fn run(self) -> Result<()> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;

        runtime.block_on(async move {
            // Errors are logged inside `execute` itself via tracing.
            self.execute().await.ok();
        });

        Ok(())
    }

    fn spawn_event_source<T: EventSource>(
        mut source: T,
        tx: mpsc::UnboundedSender<Event>,
        map_event: impl Fn(T::Event) -> Event + 'static + Send,
    ) -> mpsc::UnboundedSender<T::Effect> {
        let (temp_tx, mut temp_rx) = mpsc::unbounded_channel();

        tokio::task::spawn(async move {
            while let Some(event) = temp_rx.recv().await {
                let event = map_event(event);
                // Ignore send errors: the event loop has exited.
                tx.send(event).ok();
            }
        });

        let (out_tx, out_rx) = mpsc::unbounded_channel();

        // Errors are logged inside the EventSource itself.
        source.run(temp_tx, out_rx).ok();

        out_tx
    }

    fn spawn_service<T: Service + 'static + Send>(
        mut service: T,
        message_tx: mpsc::UnboundedSender<Message>,
        effect_tx: mpsc::UnboundedSender<Effect>,
    ) -> mpsc::UnboundedSender<ServiceInput<T::Message>> {
        let (mailbox_tx, mut mailbox_rx) = mpsc::unbounded_channel();

        tokio::task::spawn(async move {
            while let Some(input) = mailbox_rx.recv().await {
                let output = service.handle_input(input);

                for message in output.messages {
                    // Ignore send errors: the router task has exited.
                    message_tx.send(message).ok();
                }

                for effect in output.effects {
                    effect_tx.send(effect).ok();
                }
            }
        });

        mailbox_tx
    }

    async fn execute(self) -> Result<()> {
        // Event broadcast channel — EventSources publish Events here.
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        // Clock: always present — drives slot/interval ticks.
        Self::spawn_event_source(self.clock, event_tx.clone(), Event::Tick);

        // Network: emits inbound blocks/attestations, consumes effects.
        // We keep the effect sender so we can route Effects to the network later.
        let network_effect_tx = Self::spawn_event_source(
            NetworkEventSource::new(self.network_config),
            event_tx.clone(),
            Event::Network,
        );

        let (message_tx, mut message_rx) = mpsc::unbounded_channel();
        let (effect_tx, mut effect_rx) = mpsc::unbounded_channel();

        let chain_mailbox = Self::spawn_service(
            ChainService::new(self.store),
            message_tx.clone(),
            effect_tx.clone(),
        );
        let validator_mailbox = self
            .validator_config
            .zip(self.key_manager)
            .map(|(config, manager)| ValidatorService::new(config, manager))
            .map(|service| Self::spawn_service(service, message_tx.clone(), effect_tx.clone()));

        // Route inter-service Messages to the correct service mailbox.
        {
            let chain_mailbox = chain_mailbox.clone();
            let validator_mailbox = validator_mailbox.clone();
            tokio::spawn(async move {
                while let Some(message) = message_rx.recv().await {
                    match message {
                        Message::Chain(chain_message) => {
                            chain_mailbox.send(ServiceInput::Message(chain_message))?;
                        }
                        Message::Validator(validator_message) => {
                            validator_mailbox
                                .as_ref()
                                .map(|mb| mb.send(ServiceInput::Message(validator_message)))
                                .unwrap_or_else(|| Ok(warn!("validator service not configured")))?;
                        }
                    }
                }

                Ok::<_, Error>(())
            });
        }

        // Broadcast Events from all EventSources to all Services.
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                chain_mailbox.send(ServiceInput::Event(event.clone())).ok();
                if let Some(ref mb) = validator_mailbox {
                    mb.send(ServiceInput::Event(event)).ok();
                }
            }
        });

        // Route Effects to the EventSource that handles them.
        // Currently all effects are network effects; the clock has no effects.
        tokio::spawn(async move {
            while let Some(effect) = effect_rx.recv().await {
                if network_effect_tx.send(effect).is_err() {
                    break;
                }
            }
        });

        Ok(())
    }
}
