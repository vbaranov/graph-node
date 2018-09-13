use ethereum_types::Address;
use failure::*;
use futures::sync::mpsc::{channel, Receiver, Sender};
use futures::sync::oneshot;
use std::str::FromStr;
use std::sync::Mutex;
use std::thread;

use graph::components::ethereum::*;
use graph::components::store::Store;
use graph::components::subgraph::RuntimeHostEvent;
use graph::data::subgraph::DataSource;
use graph::prelude::{
    RuntimeHost as RuntimeHostTrait, RuntimeHostBuilder as RuntimeHostBuilderTrait, *,
};
use graph::util;

use module::{WasmiModule, WasmiModuleConfig};

#[derive(Clone)]
pub struct RuntimeHostConfig {
    subgraph_manifest: SubgraphManifest,
    data_source: DataSource,
}

pub struct RuntimeHostBuilder<T, L, S> {
    logger: Logger,
    ethereum_adapter: Arc<Mutex<T>>,
    link_resolver: Arc<L>,
    store: Arc<Mutex<S>>,
}

impl<T, L, S> RuntimeHostBuilder<T, L, S>
where
    T: EthereumAdapter,
    L: LinkResolver,
    S: Store,
{
    pub fn new(logger: &Logger, ethereum_adapter: Arc<Mutex<T>>, link_resolver: Arc<L>, store: Arc<Mutex<S>>) -> Self {
        RuntimeHostBuilder {
            logger: logger.new(o!("component" => "RuntimeHostBuilder")),
            ethereum_adapter,
            link_resolver,
            store,
        }
    }
}

impl<T, L, S> RuntimeHostBuilderTrait for RuntimeHostBuilder<T, L, S>
where
    T: EthereumAdapter,
    L: LinkResolver,
    S: Store + 'static,
{
    type Host = RuntimeHost;

    fn build(
        &mut self,
        subgraph_manifest: SubgraphManifest,
        data_source: DataSource
    ) -> Self::Host {
        RuntimeHost::new(
            &self.logger,
            self.ethereum_adapter.clone(),
            self.link_resolver.clone(),
            self.store.clone(),
            RuntimeHostConfig {
                subgraph_manifest,
                data_source,
            },
        )
    }
}

pub struct RuntimeHost {
    config: RuntimeHostConfig,
    logger: Logger,
    output: Option<Receiver<RuntimeHostEvent>>,
    eth_event_sender: Sender<(EthereumEvent, oneshot::Sender<Result<(), Error>>)>,
    // Never accessed, it's purpose is to signal cancelation on drop.
    _guard: oneshot::Sender<()>,
}

impl RuntimeHost {
    pub fn new<T, L, S>(
        logger: &Logger,
        ethereum_adapter: Arc<Mutex<T>>,
        link_resolver: Arc<L>,
        store: Arc<Mutex<S>>,
        config: RuntimeHostConfig,
    ) -> Self
    where
        T: EthereumAdapter,
        L: LinkResolver,
        S: Store + 'static,
    {
        let logger = logger.new(o!("component" => "RuntimeHost"));

        // Create channel for sending runtime host events
        let (event_sender, event_receiver) = channel(100);

        let wasmi_config = WasmiModuleConfig {
            subgraph: config.subgraph_manifest.clone(),
            data_source: config.data_source.clone(),
            event_sink: event_sender,
            ethereum_adapter: ethereum_adapter.clone(),
            link_resolver: link_resolver.clone(),
            store: store.clone(),
        };

        let name = config.data_source.name.clone();
        info!(logger, "Loading WASM runtime"; "data_source" => &name);

        // Create channel as cancelation guard.
        let (cancel_sender, cancel_receiver) = oneshot::channel();

        // Create channel for sending Ethereum events
        let (eth_event_sender, eth_event_receiver) = channel(100);

        // wasmi modules are not `Send` therefore they cannot be scheduled by
        // the regular tokio executor, so we create a dedicated thread inside
        // which we may wait on futures.
        let subscription_logger = logger.clone();
        thread::spawn(move || {
            let data_source = wasmi_config.data_source.clone();

            // Load the mappings as a WASM module
            let module = WasmiModule::new(&subscription_logger, wasmi_config);

            // Process one event at a time, blocking the thread when waiting for
            // the next event. Also check for a cancelation signal.
            Self::subscribe_to_events(
                &subscription_logger,
                data_source,
                module,
                eth_event_receiver,
            ).select(
                cancel_receiver
                    .into_stream()
                    .map(|_| panic!("sent into cancel guard"))
                    .map_err(|_| ()),
            )
                .for_each(|_| Ok(()))
                .wait()
                .ok();

            info!(subscription_logger, "shutting down WASM runtime"; "data_source" => name);
        });

        RuntimeHost {
            config,
            logger,
            output: Some(event_receiver),
            eth_event_sender,
            _guard: cancel_sender,
        }
    }

    /// Subscribe to all smart contract events of `data_source` contained in
    /// `subgraph`.
    fn subscribe_to_events<T, L, S>(
        logger: &Logger,
        data_source: DataSource,
        mut module: WasmiModule<T, L, S>,
        eth_event_receiver: Receiver<(EthereumEvent, oneshot::Sender<Result<(), Error>>)>,
    ) -> impl Stream<Item = (), Error = ()> + 'static
    where
        T: EthereumAdapter + 'static,
        L: LinkResolver + 'static,
        S: Store + 'static,
    {
        info!(logger, "Subscribe to events");

        let event_logger = logger.clone();

        eth_event_receiver.map(move |(event, on_complete)| {
            info!(event_logger, "Ethereum event received";
                      "signature" => event.event_signature.to_string(),
                    );

            if event.removed {
                // TODO why would this happen?
                error!(event_logger, "Event removed";
                          "block" => event.block.hash.unwrap().to_string());
            } else {
                let event_handler_opt = data_source
                    .mapping
                    .event_handlers
                    .iter()
                    .find(|event_handler| {
                        util::ethereum::string_to_h256(event_handler.event.as_str())
                            == event.event_signature
                    })
                    .to_owned();

                if let Some(event_handler) = event_handler_opt {
                    debug!(event_logger, "Call event handler";
                               "name" => &event_handler.handler,
                               "signature" => &event_handler.event);

                    module.handle_ethereum_event(event_handler.handler.as_str(), event);
                }
             }

            debug!(event_logger, "Done processing event in this RuntimeHost.");
            on_complete.send(Ok(())).unwrap();
        })
    }
}

impl EventProducer<RuntimeHostEvent> for RuntimeHost {
    fn take_event_stream(
        &mut self,
    ) -> Option<Box<Stream<Item = RuntimeHostEvent, Error = ()> + Send>> {
        self.output
            .take()
            .map(|s| Box::new(s) as Box<Stream<Item = RuntimeHostEvent, Error = ()> + Send>)
    }
}

impl EventConsumer<(EthereumEvent, oneshot::Sender<Result<(), Error>>)> for RuntimeHost {
    fn event_sink(
        &self,
    ) -> Box<
        Sink<SinkItem = (EthereumEvent, oneshot::Sender<Result<(), Error>>), SinkError = ()> + Send,
    > {
        Box::new(self.eth_event_sender.clone().sink_map_err(|_| ()))
    }
}

impl RuntimeHostTrait for RuntimeHost {
    fn subgraph_manifest(&self) -> &SubgraphManifest {
        &self.config.subgraph_manifest
    }

    // TODO cache this?
    fn event_filter(&self) -> Result<EthereumEventFilter, Error> {
        let data_source = &self.config.data_source;

        // Obtain the contract address of the data set
        let address = Address::from_str(data_source.source.address.as_str()).map_err(|_| {
            EthereumEventFilterError::InvalidContractAddress(
                data_source.name.clone(),
                data_source.source.address.clone(),
            )
        })?;

        // Load the main dataset contract
        let contract = data_source
            .mapping
            .abis
            .iter()
            .find(|abi| abi.name == data_source.source.abi)
            .ok_or(EthereumEventFilterError::AbiNotFound(
                data_source.name.clone(),
                data_source.source.abi.clone(),
            ))?
            .contract
            .clone();

        // Combine event handlers into one large event filter
        let mut missing_events = vec![];
        let mut event_filter = EthereumEventFilter::empty();
        for event_handler in data_source.mapping.event_handlers.iter() {
            match util::ethereum::contract_event_with_signature(
                &contract,
                event_handler.event.as_str(),
            ) {
                Some(event) => {
                    event_filter += EthereumEventFilter::from_single(address, event.to_owned())
                }
                None => missing_events.push(event_handler.event.clone()),
            };
        }

        if missing_events.is_empty() {
            Ok(event_filter)
        } else {
            Err(EthereumEventFilterError::EventsNotFound(
                data_source.name.clone(),
                Strings(missing_events),
            ))?
        }
    }
}
