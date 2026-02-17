use anyhow::{Error, Result};
use fork_choice::store::Store;

use tokio::sync::{broadcast, mpsc};
use tokio_stream::StreamExt as _;

use crate::{
    chain::{ChainInput, ChainMessage, ChainService},
    clock::SystemClock,
    event::{Effect, Event},
    validator::{KeyManager, ValidatorConfig, ValidatorInput, ValidatorMessage, ValidatorOutput, ValidatorService},
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

    async fn execute(self) -> Result<()> {
        // Event broadcast channel - EventSources publish Events here
        let (event_tx, _event_rx) = broadcast::channel::<Event>(128);

        // Message channels for inter-service communication
        let (to_chain_tx, to_chain_rx) = mpsc::unbounded_channel::<ChainMessage>();
        let (to_validator_tx, to_validator_rx) = mpsc::unbounded_channel::<ValidatorMessage>();

        // Effect channel - services send Effects here for execution
        let (effect_tx, mut effect_rx) = mpsc::unbounded_channel::<Effect>();

        let mut ticks = self.clock.ticks()?;

        // Spawn event collector task.
        // Collects events from all EventSources (currently just clock ticks)
        // and broadcasts them to all services.
        let event_tx_clone = event_tx.clone();
        tokio::spawn(async move {
            loop {
                let event = tokio::select! {
                    Some(tick) = ticks.next() => {
                        Event::Tick(tick?)
                    }
                    else => {
                        break Ok::<_, Error>(())
                    }
                };

                let _ = event_tx_clone.send(event);
            }
        });

        // Spawn ChainService actor
        // Receives Events and ChainMessages, outputs ValidatorMessages
        let chain_event_rx = event_tx.subscribe();
        let to_validator_tx_chain = to_validator_tx.clone();
        let store = self.store;
        tokio::spawn(async move {
            let mut chain = ChainService::new(store);
            let mut chain_event_rx = chain_event_rx;
            let mut to_chain_rx = to_chain_rx;

            loop {
                tokio::select! {
                    Ok(event) = chain_event_rx.recv() => {
                        let outputs = chain.handle_input(ChainInput::Event(event));
                        for msg in outputs {
                            let _ = to_validator_tx_chain.send(msg);
                        }
                    }
                    Some(msg) = to_chain_rx.recv() => {
                        let outputs = chain.handle_input(ChainInput::Message(msg));
                        for msg in outputs {
                            let _ = to_validator_tx_chain.send(msg);
                        }
                    }
                }
            }
        });

        // Spawn ValidatorService actor (only if configured)
        // Receives ValidatorMessages, outputs ChainMessages and Effects
        if let Some(validator_config) = self.validator_config {
            let to_chain_tx_validator = to_chain_tx;
            let effect_tx_validator = effect_tx.clone();
            let key_manager = self.key_manager;
            tokio::spawn(async move {
                let mut validator = ValidatorService::new(validator_config, key_manager);
                let mut to_validator_rx = to_validator_rx;

                loop {
                    tokio::select! {
                        Some(msg) = to_validator_rx.recv() => {
                            let outputs = validator.handle_input(ValidatorInput::Message(msg));
                            for output in outputs {
                                match output {
                                    ValidatorOutput::Effect(effect) => {
                                        let _ = effect_tx_validator.send(effect);
                                    }
                                    ValidatorOutput::Message(chain_msg) => {
                                        let _ = to_chain_tx_validator.send(chain_msg);
                                    }
                                }
                            }
                        }
                    }
                }
            });
        }

        // Main loop: process Effects from services
        loop {
            tokio::select! {
                Some(effect) = effect_rx.recv() => {
                    // Execute effects (gossip to network, etc.)
                    Self::execute_effect(effect).await?;
                }
                else => break,
            }
        }

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
