use anyhow::{Error, Result};
use fork_choice::store::Store;

use tokio::{
    select,
    sync::{broadcast, mpsc, oneshot},
};
use tokio_stream::StreamExt as _;
use tracing::Span;

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
        tx: broadcast::Sender<SpannedEvent>,
        map_event: impl Fn(T::Event) -> Event + 'static + Send,
    ) -> mpsc::UnboundedSender<T::Effect> {
        let (temp_tx, mut temp_rx) = mpsc::unbounded_channel();

        tokio::task::spawn(async move {
            while let Some(event) = temp_rx.recv().await {
                let event = map_event(event);
                tx.send(SpannedEvent::new(Span::none(), event));
            }
        });

        let (out_tx, out_rx) = mpsc::unbounded_channel();

        source.run(temp_tx, out_rx);

        out_tx
    }

    fn spawn_service<T: Service + 'static + Send>(
        mut service: T,
        messaging: mpsc::UnboundedSender<Message>,
    ) -> mpsc::UnboundedSender<ServiceInput<T::Message>> {
        let (mailbox_tx, mut mailbox_rx) = mpsc::unbounded_channel();

        tokio::task::spawn(async move {
            while let Some(input) = mailbox_rx.recv().await {
                let output = service.handle_input(input);
            }
        });

        mailbox_tx
    }

    async fn execute(self) -> Result<()> {
        // Event broadcast channel - EventSources publish Events here
        let (event_tx, _event_rx) = broadcast::channel::<SpannedEvent>(128);

        Self::spawn_event_source(self.clock, event_tx, |event| Event::Tick(event));

        let (message_tx, mut message_rx) = mpsc::unbounded_channel();

        let chain_mailbox = Self::spawn_service(ChainService::new(self.store), message_tx.clone());

        tokio::spawn(async move {
            while let Some(message) = message_rx.recv().await {
                match message {
                    Message::Chain(chain_message) => {
                        chain_mailbox.send(ServiceInput::Message(chain_message))?;
                    }
                    Message::Validator(validator_message) => {}
                }
            }

            Ok::<_, Error>(())
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
