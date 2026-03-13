use anyhow::{Result, anyhow, bail};
use clock::SystemClock;
use fork_choice::Store;
use futures::future::join_all;
use http_api::HttpServerConfig;
use networking::NetworkConfig;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::{
    chain::ChainService,
    environment::{Effect, Event, EventSource, Message, Service, ServiceInput},
    http::{HttpEventSource, HttpService},
    network::{NetworkEventSource, NetworkService},
    validator::{KeyManager, ValidatorConfig, ValidatorService},
};

type TaskHandle = tokio::task::JoinHandle<Result<()>>;

pub struct Node {
    clock: SystemClock,
    store: Store,
    validator_config: Option<ValidatorConfig>,
    key_manager: Option<KeyManager>,
    network_config: NetworkConfig,
    http_config: HttpServerConfig,
}

impl Node {
    pub fn new(
        genesis: u64,
        store: Store,
        validator_config: Option<ValidatorConfig>,
        key_manager: Option<KeyManager>,
        network_config: NetworkConfig,
        http_config: HttpServerConfig,
    ) -> Result<Self> {
        Ok(Self {
            clock: SystemClock::new(genesis)?,
            store,
            validator_config,
            key_manager,
            network_config,
            http_config,
        })
    }

    pub fn run(self) -> Result<()> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;

        runtime.block_on(self.execute())
    }

    fn spawn_event_source<T>(
        mut source: T,
        event_tx: mpsc::UnboundedSender<Event>,
        map_event: fn(T::Event) -> Event,
        shutdown: CancellationToken,
    ) -> (mpsc::UnboundedSender<T::Effect>, TaskHandle)
    where
        T: EventSource + Send + 'static,
    {
        let (source_event_tx, mut source_event_rx) = mpsc::unbounded_channel();
        let (effect_tx, effect_rx) = mpsc::unbounded_channel();

        let task_shutdown = shutdown.clone();
        let handle = tokio::spawn(async move {
            let mut source_task =
                tokio::spawn(async move { source.run(source_event_tx, effect_rx).await });

            let result = loop {
                tokio::select! {
                    _ = task_shutdown.cancelled() => {
                        source_task.abort();
                        drop(source_task.await);
                        break Ok(());
                    }
                    source_result = &mut source_task => {
                        break source_result.map_err(anyhow::Error::from)?;
                    }
                    event = source_event_rx.recv() => {
                        let Some(event) = event else {
                            break source_task.await.map_err(anyhow::Error::from)?;
                        };

                        event_tx
                            .send(map_event(event))
                            .map_err(|_| anyhow!("event router exited"))?;
                    }
                }
            };

            shutdown.cancel();
            result
        });

        (effect_tx, handle)
    }

    fn spawn_service<T: Service + Send + 'static>(
        mut service: T,
        message_tx: mpsc::UnboundedSender<Message>,
        effect_tx: mpsc::UnboundedSender<Effect>,
        shutdown: CancellationToken,
    ) -> (mpsc::UnboundedSender<ServiceInput<T::Message>>, TaskHandle) {
        let (mailbox_tx, mut mailbox_rx) = mpsc::unbounded_channel();

        let task_shutdown = shutdown.clone();
        let handle = tokio::spawn(async move {
            let result = async {
                loop {
                    tokio::select! {
                        _ = task_shutdown.cancelled() => {
                            return Ok(());
                        }
                        input = mailbox_rx.recv() => {
                            let Some(input) = input else {
                                if task_shutdown.is_cancelled() {
                                    return Ok(());
                                }

                                bail!("service mailbox closed");
                            };

                            let output = service.handle_input(input);

                            for message in output.messages {
                                message_tx
                                    .send(message)
                                    .map_err(|_| anyhow!("message router exited"))?;
                            }

                            for effect in output.effects {
                                effect_tx
                                    .send(effect)
                                    .map_err(|_| anyhow!("effect router exited"))?;
                            }
                        }
                    }
                }
            }
            .await;

            shutdown.cancel();
            result
        });

        (mailbox_tx, handle)
    }

    async fn execute(self) -> Result<()> {
        let shutdown = CancellationToken::new();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (message_tx, mut message_rx) = mpsc::unbounded_channel();
        let (effect_tx, mut effect_rx) = mpsc::unbounded_channel();

        let (network_effect_tx, network_source_task) = Self::spawn_event_source(
            NetworkEventSource::new(self.network_config),
            event_tx.clone(),
            Event::Network,
            shutdown.clone(),
        );

        let (http_effect_tx, http_source_task) = Self::spawn_event_source(
            HttpEventSource::new(self.http_config),
            event_tx.clone(),
            Event::Http,
            shutdown.clone(),
        );

        let (_clock_effect_tx, clock_source_task) =
            Self::spawn_event_source(self.clock, event_tx, Event::Tick, shutdown.clone());

        let (chain_mailbox, chain_task) = Self::spawn_service(
            ChainService::new(self.store),
            message_tx.clone(),
            effect_tx.clone(),
            shutdown.clone(),
        );

        let (network_mailbox, network_task) = Self::spawn_service(
            NetworkService::new(),
            message_tx.clone(),
            effect_tx.clone(),
            shutdown.clone(),
        );

        let (http_mailbox, http_task) = Self::spawn_service(
            HttpService::new(),
            message_tx.clone(),
            effect_tx.clone(),
            shutdown.clone(),
        );

        let (validator_mailbox, validator_task) = self
            .validator_config
            .zip(self.key_manager)
            .map(|(config, manager)| ValidatorService::new(config, manager))
            .map(|service| Self::spawn_service(service, message_tx, effect_tx, shutdown.clone()))
            .map_or((None, None), |(mailbox, handle)| {
                (Some(mailbox), Some(handle))
            });

        let router_shutdown = shutdown.clone();
        let router_task = tokio::spawn(async move {
            let result = async {
                loop {
                    tokio::select! {
                        _ = router_shutdown.cancelled() => {
                            return Ok(());
                        }
                        message = message_rx.recv() => {
                            let Some(message) = message else {
                                if router_shutdown.is_cancelled() {
                                    return Ok(());
                                }

                                bail!("message router channel closed");
                            };

                            match message {
                                Message::Chain(msg) => {
                                    chain_mailbox
                                        .send(ServiceInput::Message(msg))
                                        .map_err(|_| anyhow!("chain mailbox closed"))?;
                                }
                                Message::Validator(msg) => {
                                    if let Some(mailbox) = validator_mailbox.as_ref() {
                                        mailbox
                                            .send(ServiceInput::Message(msg))
                                            .map_err(|_| anyhow!("validator mailbox closed"))?;
                                    } else {
                                        warn!("validator service not configured");
                                    }
                                }
                                Message::Network(msg) => {
                                    network_mailbox
                                        .send(ServiceInput::Message(msg))
                                        .map_err(|_| anyhow!("network mailbox closed"))?;
                                }
                                Message::Http(msg) => {
                                    http_mailbox
                                        .send(ServiceInput::Message(msg))
                                        .map_err(|_| anyhow!("http mailbox closed"))?;
                                }
                            }
                        }
                        event = event_rx.recv() => {
                            let Some(event) = event else {
                                if router_shutdown.is_cancelled() {
                                    return Ok(());
                                }

                                bail!("event router channel closed");
                            };

                            match event {
                                Event::Network(network_event) => {
                                    network_mailbox
                                        .send(ServiceInput::Event(Event::Network(network_event)))
                                        .map_err(|_| anyhow!("network mailbox closed"))?;
                                }
                                Event::Http(http_event) => {
                                    http_mailbox
                                        .send(ServiceInput::Event(Event::Http(http_event)))
                                        .map_err(|_| anyhow!("http mailbox closed"))?;
                                }
                                Event::Tick(tick) => {
                                    chain_mailbox
                                        .send(ServiceInput::Event(Event::Tick(tick)))
                                        .map_err(|_| anyhow!("chain mailbox closed"))?;

                                    network_mailbox
                                        .send(ServiceInput::Event(Event::Tick(tick)))
                                        .map_err(|_| anyhow!("network mailbox closed"))?;

                                    if let Some(mailbox) = validator_mailbox.as_ref() {
                                        mailbox
                                            .send(ServiceInput::Event(Event::Tick(tick)))
                                            .map_err(|_| anyhow!("validator mailbox closed"))?;
                                    }
                                }
                            }
                        }
                        effect = effect_rx.recv() => {
                            let Some(effect) = effect else {
                                if router_shutdown.is_cancelled() {
                                    return Ok(());
                                }

                                bail!("effect router channel closed");
                            };

                            match effect {
                                Effect::Network(effect) => {
                                    network_effect_tx
                                        .send(effect)
                                        .map_err(|_| anyhow!("network event source effect mailbox closed"))?;
                                }
                                Effect::Http(effect) => {
                                    http_effect_tx
                                        .send(effect)
                                        .map_err(|_| anyhow!("http event source effect mailbox closed"))?;
                                }
                            }
                        }
                        _ = tokio::signal::ctrl_c() => {
                            router_shutdown.cancel();
                        }
                    }
                }
            }
            .await;

            router_shutdown.cancel();
            result
        });

        let mut handles = vec![
            network_source_task,
            http_source_task,
            clock_source_task,
            chain_task,
            network_task,
            http_task,
            router_task,
        ];

        if let Some(validator_task) = validator_task {
            handles.push(validator_task);
        }

        for result in join_all(handles).await {
            result.map_err(anyhow::Error::from).flatten()?;
        }

        Ok(())
    }
}
