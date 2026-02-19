use anyhow::{Error, Result};
use fork_choice::store::Store;

use tokio::{
    select,
    sync::{broadcast, mpsc, oneshot},
};
use tokio_stream::StreamExt as _;
use tracing::{Span, warn};

use crate::{
    chain::{ChainInput, ChainMessage, ChainService},
    clock::SystemClock,
    simulation::{Effect, Event, EventSource, Message, Service, ServiceInput, SpannedEvent},
    validator::{
        KeyManager, ValidatorConfig, ValidatorInput, ValidatorMessage, ValidatorOutput,
        ValidatorService,
    },
};

pub struct RealSimulator {
    clock: SystemClock,
    store: Store,
    validator_config: Option<ValidatorConfig>,
    key_manager: Option<KeyManager>,
}

impl RealSimulator {
    pub fn new(
        genesis: u64,
        store: Store,
        validator_config: Option<ValidatorConfig>,
        key_manager: Option<KeyManager>,
    ) -> Result<Self> {
        Ok(Self {
            clock: SystemClock::new(genesis)?,
            store,
            validator_config,
            key_manager,
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

    fn spawn_event_source<T: EventSource>(
        mut source: T,
        tx: mpsc::UnboundedSender<Event>,
        map_event: impl Fn(T::Event) -> Event + 'static + Send,
    ) -> mpsc::UnboundedSender<T::Effect> {
        let (temp_tx, mut temp_rx) = mpsc::unbounded_channel();

        tokio::task::spawn(async move {
            while let Some(event) = temp_rx.recv().await {
                let event = map_event(event);
                tx.send(event);
            }
        });

        let (out_tx, out_rx) = mpsc::unbounded_channel();

        source.run(temp_tx, out_rx);

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
                    message_tx.send(message);
                }

                for effect in output.effects {
                    effect_tx.send(effect);
                }
            }
        });

        mailbox_tx
    }

    async fn execute(self) -> Result<()> {
        // Event broadcast channel - EventSources publish Events here
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        Self::spawn_event_source(self.clock, event_tx, |event| Event::Tick(event));

        let (message_tx, mut message_rx) = mpsc::unbounded_channel();
        let (effect_tx, mut effect_rx) = mpsc::unbounded_channel();

        let chain_mailbox = Self::spawn_service(
            ChainService::new(self.store),
            message_tx.clone(),
            effect_tx.clone(),
        );
        let validator_mailbox = self
            .validator_config
            .and_then(|config| {
                self.key_manager
                    .map(|manager| ValidatorService::new(config, manager))
            })
            .map(|service| Self::spawn_service(service, message_tx.clone(), effect_tx.clone()));

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

        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                chain_mailbox.send(ServiceInput::Event(event.clone()));
                validator_mailbox
                    .as_ref()
                    .map(|mb| mb.send(ServiceInput::Event(event)));
            }
        });

        tokio::spawn(async move {
            while let Some(event) = effect_rx.recv().await {
                // TODO: when some effects will be created, move them here
            }
        });

        Ok(())
    }

    async fn execute_effect(effect: Effect) -> Result<()> {
        match effect {
            Effect::GossipBlock(_block) => {
                // Send to network service when implemented
                Ok(())
            }
            Effect::GossipAttestation(_attestation) => {
                // Send to network service when implemented
                Ok(())
            }
        }
    }
}
