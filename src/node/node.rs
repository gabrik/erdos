use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    thread,
};

use futures::future;
use futures_util::stream::StreamExt;
use slog;
use tokio::{
    runtime::Builder,
    sync::{
        mpsc::{self, Receiver, Sender, UnboundedReceiver},
        Mutex,
    },
};

#[cfg(feature = "tcp_transport")]
use crate::communication::{ControlMessageCodec, MessageCodec};
#[cfg(feature = "tcp_transport")]
use tokio::net::TcpStream;
#[cfg(feature = "tcp_transport")]
use tokio_util::codec::Framed;

use crate::communication::{self, ControlMessage, ControlMessageHandler};

#[cfg(feature = "tcp_transport")]
use crate::communication::{
    receivers::{self, ControlReceiver, DataReceiver},
    senders::{self, ControlSender, DataSender},
};

#[cfg(feature = "zenoh_transport")]
use crate::communication::{
    zenoh_receivers::{
        self as receivers, ZenohControlReceiver as ControlReceiver,
        ZenohDataReceiver as DataReceiver,
    },
    zenoh_senders::{
        self as senders, ZenohControlSender as ControlSender, ZenohDataSender as DataSender,
    },
};

#[cfg(feature = "zenoh_zerocopy_transport")]
use crate::communication::{
    zenoh_shm_receivers::{
        self as receivers, ZenohShmControlReceiver as ControlReceiver,
        ZenohShmDataReceiver as DataReceiver,
    },
    zenoh_shm_senders::{
        self as senders, ZenohShmControlSender as ControlSender, ZenohShmDataSender as DataSender,
    },
};

use crate::dataflow::graph::{default_graph, Graph};
use crate::scheduler::{
    self,
    channel_manager::ChannelManager,
    endpoints_manager::{ChannelsToReceivers, ChannelsToSenders},
};
use crate::Configuration;

/// Unique index for a [`Node`].
pub type NodeId = usize;

/// Structure which executes a portion of an ERDOS application.
///
/// The [`Node`] contains a runtime which executes operators and manages
/// communication between operators via streams.
#[allow(dead_code)]
pub struct Node {
    /// Node's configuration parameters.
    config: Configuration,
    /// Unique node id.
    id: NodeId,
    /// Dataflow graph which the node will execute.
    dataflow_graph: Option<Graph>,
    /// Structure to be used to send `Sender` updates to receiver threads.
    channels_to_receivers: Arc<Mutex<ChannelsToReceivers>>,
    /// Structure to be used to send messages to sender threads.
    channels_to_senders: Arc<Mutex<ChannelsToSenders>>,
    /// Structure used to send and receive control messages.
    control_handler: ControlMessageHandler,
    /// Used to block `run_async` until setup is complete for the driver to continue running safely.
    initialized: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    /// Channel used to shut down the node.
    shutdown_tx: Sender<()>,
    shutdown_rx: Option<Receiver<()>>,
}

impl Node {
    /// Creates a new node.
    pub fn new(config: Configuration) -> Self {
        let id = config.index;
        let logger = config.logger.clone();
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
        Self {
            config,
            id,
            dataflow_graph: None,
            channels_to_receivers: Arc::new(Mutex::new(ChannelsToReceivers::new())),
            channels_to_senders: Arc::new(Mutex::new(ChannelsToSenders::new())),
            control_handler: ControlMessageHandler::new(logger),
            initialized: Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new())),
            shutdown_tx,
            shutdown_rx: Some(shutdown_rx),
        }
    }

    /// Runs an ERDOS node.
    ///
    /// The method never returns.
    pub fn run(&mut self) {
        slog::debug!(self.config.logger, "Node {}: running", self.id);
        // Set the dataflow graph if it hasn't been set already.
        if self.dataflow_graph.is_none() {
            self.dataflow_graph = Some(default_graph::clone());
        }
        // Build a runtime with n threads.
        let mut runtime = Builder::new()
            .threaded_scheduler()
            .core_threads(self.config.num_worker_threads)
            .thread_name(format!("node-{}", self.id))
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(self.async_run());
        slog::debug!(self.config.logger, "Node {}: finished running", self.id);
    }

    /// Runs an ERDOS node in a seperate OS thread.
    ///
    /// The method immediately returns.
    pub fn run_async(mut self) -> NodeHandle {
        // Clone to avoid move to other thread.
        let shutdown_tx = self.shutdown_tx.clone();
        // Copy dataflow graph to the other thread
        self.dataflow_graph = Some(default_graph::clone());
        let initialized = self.initialized.clone();
        let thread_handle = thread::spawn(move || {
            self.run();
        });
        // Wait for ERDOS to start up.
        let (lock, cvar) = &*initialized;
        let mut started = lock.lock().unwrap();
        while !*started {
            started = cvar.wait(started).unwrap();
        }

        NodeHandle {
            thread_handle,
            shutdown_tx,
        }
    }

    fn set_node_initialized(&mut self) {
        let (lock, cvar) = &*self.initialized;
        let mut started = lock.lock().unwrap();
        *started = true;
        cvar.notify_all();

        // slog::debug!(self.config.logger, "Node {}: done initializing.", self.id);
    }

    #[cfg(any(feature = "zenoh_transport", feature = "zenoh_zerocopy_transport"))]
    async fn get_control_streams(
        &mut self,
        zsession: Arc<zenoh::net::Session>,
        nodes: Vec<NodeId>,
    ) -> (Vec<ControlSender>, Vec<ControlReceiver>) {
        let mut control_receivers = Vec::new();
        let mut control_senders = Vec::new();

        for node_id in nodes {
            control_receivers.push(ControlReceiver::new(
                node_id,
                self.id,
                zsession.clone(),
                &mut self.control_handler,
            ));

            control_senders.push(ControlSender::new(
                node_id,
                self.id,
                zsession.clone(),
                &mut self.control_handler,
            ));
        }
        (control_senders, control_receivers)
    }

    #[cfg(any(feature = "zenoh_transport", feature = "zenoh_zerocopy_transport"))]
    async fn get_data_streams(
        &mut self,
        zsession: Arc<zenoh::net::Session>,
        nodes: Vec<NodeId>,
    ) -> (Vec<DataSender>, Vec<DataReceiver>) {
        let mut data_receivers = Vec::new();
        let mut data_senders = Vec::new();

        for node_id in nodes {
            data_receivers.push(
                DataReceiver::new(
                    node_id,
                    self.id,
                    zsession.clone(),
                    self.channels_to_receivers.clone(),
                    &mut self.control_handler,
                )
                .await,
            );

            data_senders.push(
                DataSender::new(
                    node_id,
                    self.id,
                    zsession.clone(),
                    self.channels_to_senders.clone(),
                    &mut self.control_handler,
                )
                .await,
            );
        }
        (data_senders, data_receivers)
    }

    /// Splits a vector of TCPStreams into `DataSender`s and `DataReceiver`s.
    #[cfg(feature = "tcp_transport")]
    async fn split_data_streams(
        &mut self,
        mut streams: Vec<(NodeId, TcpStream)>,
    ) -> (Vec<DataSender>, Vec<DataReceiver>) {
        let mut sink_halves = Vec::new();
        let mut stream_halves = Vec::new();
        while let Some((node_id, stream)) = streams.pop() {
            // Use the message codec to divide the TCP stream data into messages.
            let framed = Framed::new(stream, MessageCodec::new());
            let (split_sink, split_stream) = framed.split();
            // Create an ERDOS receiver for the stream half.
            stream_halves.push(
                DataReceiver::new(
                    node_id,
                    split_stream,
                    self.channels_to_receivers.clone(),
                    &mut self.control_handler,
                )
                .await,
            );

            // Create an ERDOS sender for the sink half.
            sink_halves.push(
                DataSender::new(
                    node_id,
                    split_sink,
                    self.channels_to_senders.clone(),
                    &mut self.control_handler,
                )
                .await,
            );
        }
        (sink_halves, stream_halves)
    }

    /// Splits a vector of TCPStreams into `ControlMessageHandler`, `ControlSender`s and `ControlReceiver`s.
    #[cfg(feature = "tcp_transport")]
    async fn split_control_streams(
        &mut self,
        streams: Vec<(NodeId, TcpStream)>,
    ) -> (Vec<ControlSender>, Vec<ControlReceiver>) {
        let mut control_receivers = Vec::new();
        let mut control_senders = Vec::new();

        for (node_id, stream) in streams {
            // Use the message codec to divide the TCP stream data into messages.
            let framed = Framed::new(stream, ControlMessageCodec::new());
            let (split_sink, split_stream) = framed.split();
            // Create an control receiver for the stream half.
            control_receivers.push(ControlReceiver::new(
                node_id,
                split_stream,
                &mut self.control_handler,
            ));
            // Create an control sender for the sink half.
            control_senders.push(ControlSender::new(
                node_id,
                split_sink,
                &mut self.control_handler,
            ));
        }

        (control_senders, control_receivers)
    }

    async fn wait_for_communication_layer_initialized(&mut self) -> Result<(), String> {
        let num_nodes = self.config.data_addresses.len();

        let mut control_senders_initialized = HashSet::new();
        control_senders_initialized.insert(self.id);
        let mut control_receivers_initialized = HashSet::new();
        control_receivers_initialized.insert(self.id);
        let mut data_senders_initialized = HashSet::new();
        data_senders_initialized.insert(self.id);
        let mut data_receivers_initialized = HashSet::new();
        data_receivers_initialized.insert(self.id);

        while control_senders_initialized.len() < num_nodes
            || control_receivers_initialized.len() < num_nodes
            || data_senders_initialized.len() < num_nodes
            || data_receivers_initialized.len() < num_nodes
        {
            let msg = self
                .control_handler
                .read_sender_or_receiver_initialized()
                .await
                .map_err(|e| format!("Error receiving control message: {:?}", e))?;
            match msg {
                ControlMessage::ControlSenderInitialized(node_id) => {
                    control_senders_initialized.insert(node_id);
                }
                ControlMessage::ControlReceiverInitialized(node_id) => {
                    control_receivers_initialized.insert(node_id);
                }
                ControlMessage::DataSenderInitialized(node_id) => {
                    data_senders_initialized.insert(node_id);
                }
                ControlMessage::DataReceiverInitialized(node_id) => {
                    data_receivers_initialized.insert(node_id);
                }
                _ => unreachable!(),
            };
        }
        Ok(())
    }

    async fn wait_for_local_operators_initialized(
        &mut self,
        mut rx_from_operators: UnboundedReceiver<ControlMessage>,
        num_local_operators: usize,
    ) {
        let mut initialized_operators = HashSet::new();
        while initialized_operators.len() < num_local_operators {
            if let Some(ControlMessage::OperatorInitialized(op_id)) = rx_from_operators.recv().await
            {
                initialized_operators.insert(op_id);
            }
        }
    }

    async fn broadcast_local_operators_initialized(&mut self) -> Result<(), String> {
        slog::debug!(
            self.config.logger,
            "Node {}: initialized all operators on this node.",
            self.id
        );
        self.control_handler
            .broadcast_to_nodes(ControlMessage::AllOperatorsInitializedOnNode(self.id))
            .map_err(|e| format!("Error broadcasting control message: {:?}", e))
    }

    async fn wait_for_all_operators_initialized(&mut self) -> Result<(), String> {
        let num_nodes = self.config.data_addresses.len();
        let mut initialized_nodes = HashSet::new();
        initialized_nodes.insert(self.id);
        while initialized_nodes.len() < num_nodes {
            match self
                .control_handler
                .read_all_operators_initialized_on_node_msg()
                .await
            {
                Ok(node_id) => {
                    initialized_nodes.insert(node_id);
                }
                Err(e) => {
                    return Err(format!("Error waiting for other nodes to set up: {:?}", e));
                }
            }
        }
        Ok(())
    }

    async fn run_operators(&mut self) -> Result<(), String> {
        self.wait_for_communication_layer_initialized().await?;

        let graph_ref = self
            .dataflow_graph
            .as_ref()
            .unwrap_or_else(|| panic!("Node {}: dataflow graph must be set.", self.id));
        let graph = scheduler::schedule(graph_ref);
        if let Some(filename) = &self.config.graph_filename {
            graph.to_dot(filename.as_str()).map_err(|e| e.to_string())?;
        }

        let channel_manager = ChannelManager::new(
            &graph,
            self.id,
            Arc::clone(&self.channels_to_receivers),
            Arc::clone(&self.channels_to_senders),
        )
        .await;
        // Execute operators scheduled on the current node.
        let channel_manager = Arc::new(std::sync::Mutex::new(channel_manager));
        let local_operators: Vec<_> = graph
            .get_operators()
            .into_iter()
            .filter(|op| op.node_id == self.id)
            .collect();

        let (operator_tx, rx_from_operators) = mpsc::unbounded_channel();
        let mut channels_to_operators = HashMap::new();

        let num_local_operators = local_operators.len();

        let mut join_handles = Vec::with_capacity(num_local_operators);
        for operator_info in local_operators {
            let name = operator_info
                .name
                .clone()
                .unwrap_or_else(|| format!("{}", operator_info.id));
            slog::debug!(
                self.config.logger,
                "Node {}: starting operator {}",
                self.id,
                name
            );
            let channel_manager_copy = Arc::clone(&channel_manager);
            let operator_tx_copy = operator_tx.clone();
            let (tx, rx) = mpsc::unbounded_channel();
            channels_to_operators.insert(operator_info.id, tx);
            // Launch the operator as a separate async task.
            let join_handle = tokio::spawn(async move {
                let mut operator_executor =
                    (operator_info.runner)(channel_manager_copy, operator_tx_copy, rx);
                operator_executor.execute().await;
            });
            join_handles.push(join_handle);
        }

        // Wait for all operators to finish setting up.
        self.wait_for_local_operators_initialized(rx_from_operators, num_local_operators)
            .await;
        // Setup driver on the current node.
        if let Some(driver) = graph.get_driver(self.id) {
            for setup_hook in driver.setup_hooks {
                (setup_hook)(Arc::clone(&channel_manager));
            }
        }
        // Broadcast all operators initialized on current node.
        self.broadcast_local_operators_initialized().await?;
        // Wait for all other nodes to finish setting up.
        self.wait_for_all_operators_initialized().await?;
        // Tell driver to run.
        self.set_node_initialized();
        // Tell all operators to run.
        for (op_id, tx) in channels_to_operators {
            tx.send(ControlMessage::RunOperator(op_id))
                .map_err(|e| format!("Error telling operator to run: {}", e))?;
        }
        // Wait for all operators to finish running.
        future::join_all(join_handles).await;
        Ok(())
    }

    async fn async_run(&mut self) {
        // Assign values used later to avoid lifetime errors.
        let num_nodes = self.config.data_addresses.len();
        let logger = self.config.logger.clone();

        #[cfg(any(feature = "zenoh_transport", feature = "zenoh_zerocopy_transport"))]
        let zconfig = zenoh::net::config::peer();

        #[cfg(any(feature = "zenoh_transport", feature = "zenoh_zerocopy_transport"))]
        let zsession = Arc::new(zenoh::net::open(zconfig).await.unwrap());

        // Spawning a task that can reply to evals, needed to verify the node are discovered
        #[cfg(any(feature = "zenoh_transport", feature = "zenoh_zerocopy_transport"))]
        let (ztx, mut zrx) = mpsc::channel(1);
        #[cfg(any(feature = "zenoh_transport", feature = "zenoh_zerocopy_transport"))]
        let self_id = self.id.clone();
        #[cfg(any(feature = "zenoh_transport", feature = "zenoh_zerocopy_transport"))]
        let z_handler_session = zsession.clone();
        #[cfg(any(feature = "zenoh_transport", feature = "zenoh_zerocopy_transport"))]
        let z_handler_fut =
            tokio::task::spawn(async move { query_handler(z_handler_session, self_id, ztx).await });

        #[cfg(any(feature = "zenoh_transport", feature = "zenoh_zerocopy_transport"))]
        zrx.recv().await;

        // Wait zenoh scouting
        #[cfg(any(feature = "zenoh_transport", feature = "zenoh_zerocopy_transport"))]
        wait_zenoh_nodes_discovered(num_nodes, self.id, zsession.clone())
            .await
            .unwrap();

        // Create TCPStreams between all node pairs.
        #[cfg(feature = "tcp_transport")]
        let control_streams = communication::create_tcp_streams(
            self.config.control_addresses.clone(),
            self.id,
            &self.config.logger,
        )
        .await;

        #[cfg(feature = "tcp_transport")]
        let data_streams = communication::create_tcp_streams(
            self.config.data_addresses.clone(),
            self.id,
            &self.config.logger,
        )
        .await;

        #[cfg(feature = "tcp_transport")]
        let (control_senders, control_receivers) =
            self.split_control_streams(control_streams).await;

        #[cfg(feature = "tcp_transport")]
        let (senders, receivers) = self.split_data_streams(data_streams).await;

        #[cfg(any(feature = "zenoh_transport", feature = "zenoh_zerocopy_transport"))]
        let (control_senders, control_receivers) = self
            .get_control_streams(zsession.clone(), get_nodes_ids(num_nodes, self.id))
            .await;

        #[cfg(any(feature = "zenoh_transport", feature = "zenoh_zerocopy_transport"))]
        let (senders, receivers) = self
            .get_data_streams(zsession.clone(), get_nodes_ids(num_nodes, self.id))
            .await;

        // Listen for shutdown message.
        let mut shutdown_rx = self.shutdown_rx.take().unwrap();
        let shutdown_fut = shutdown_rx.recv();
        // Execute threads that send data to other nodes.
        let control_senders_fut = senders::run_control_senders(control_senders);
        let senders_fut = senders::run_senders(senders);
        // Execute threads that receive data from other nodes.
        let control_recvs_fut = receivers::run_control_receivers(control_receivers);
        let recvs_fut = receivers::run_receivers(receivers);
        // Execute operators.
        let ops_fut = self.run_operators();
        // These threads only complete when a failure happens.
        if num_nodes <= 1 {
            // Senders and Receivers should return if there's only 1 node.
            if let Err(e) = tokio::try_join!(
                senders_fut,
                recvs_fut,
                control_senders_fut,
                control_recvs_fut,
            ) {
                slog::error!(
                    logger,
                    "Non-fatal network communication error; this should not happen! {:?}",
                    e
                );
            }

            #[cfg(feature = "tcp_transport")]
            tokio::select! {
                Err(e) = ops_fut => slog::error!(
                    logger,
                    "Error running operators on node {:?}: {:?}", self.id, e
                ),
                _ = shutdown_fut => slog::debug!(logger, "Node {}: shutting down", self.id),
            }

            #[cfg(any(feature = "zenoh_transport", feature = "zenoh_zerocopy_transport"))]
            tokio::select! {
                Err(e) = ops_fut => slog::error!(
                    logger,
                    "Error running operators on node {:?}: {:?}", self.id, e
                ),
                _ = shutdown_fut => slog::debug!(logger, "Node {}: shutting down", self.id),
                _ = z_handler_fut => slog::debug!(logger, "Node {}: shutting down Zenoh Query Handler", self.id),
            }
        } else {
            #[cfg(feature = "tcp_transport")]
            tokio::select! {
                Err(e) = senders_fut => slog::error!(logger, "Error with data senders: {:?}", e),
                Err(e) = recvs_fut => slog::error!(logger, "Error with data receivers: {:?}", e),
                Err(e) = control_senders_fut => slog::error!(logger, "Error with control senders: {:?}", e),
                Err(e) = control_recvs_fut => slog::error!(
                    self.config.logger,
                    "Error with control receivers: {:?}", e
                ),
                Err(e) = ops_fut => slog::error!(
                    logger,
                    "Error running operators on node {:?}: {:?}", self.id, e
                ),
                _ = shutdown_fut => slog::debug!(logger, "Node {}: shutting down", self.id),
            }

            #[cfg(any(feature = "zenoh_transport", feature = "zenoh_zerocopy_transport"))]
            tokio::select! {
                Err(e) = senders_fut => slog::error!(logger, "Error with data senders: {:?}", e),
                Err(e) = recvs_fut => slog::error!(logger, "Error with data receivers: {:?}", e),
                Err(e) = control_senders_fut => slog::error!(logger, "Error with control senders: {:?}", e),
                Err(e) = control_recvs_fut => slog::error!(
                    self.config.logger,
                    "Error with control receivers: {:?}", e
                ),
                Err(e) = ops_fut => slog::error!(
                    logger,
                    "Error running operators on node {:?}: {:?}", self.id, e
                ),
                _ = shutdown_fut => slog::debug!(logger, "Node {}: shutting down", self.id),
                _ = z_handler_fut => slog::debug!(logger, "Node {}: shutting down Zenoh Query Handler", self.id),
            }
        }
    }
}

#[cfg(any(feature = "zenoh_transport", feature = "zenoh_zerocopy_transport"))]
async fn query_handler(zsession: Arc<zenoh::net::Session>, id: NodeId, mut tx: Sender<()>) {
    let path = format!("/{}/info", id);
    let value = format!("{}", id);
    let mut queryable = zsession
        .declare_queryable(&path.clone().into(), zenoh::net::queryable::EVAL)
        .await
        .unwrap();
    tx.send(()).await.unwrap();

    while let Some(zquery) = queryable.stream().next().await {
        zquery
            .reply(zenoh::net::Sample {
                res_name: path.clone(),
                payload: value.as_bytes().into(),
                data_info: None,
            })
            .await;
    }
}

#[cfg(any(feature = "zenoh_transport", feature = "zenoh_zerocopy_transport"))]
async fn wait_zenoh_nodes_discovered(
    total_nodes: usize,
    node_id: NodeId,
    zsession: Arc<zenoh::net::Session>,
) -> Result<Vec<NodeId>, communication::CommunicationError> {
    let mut nodes = vec![];
    let mut n = 0;
    while nodes.len() < (total_nodes - 1) {
        if n != node_id {
            let path = format!("/{}/info", n);
            let mut replies = zsession
                .query(
                    &path.into(),
                    "",
                    zenoh::net::protocol::core::QueryTarget::default(),
                    zenoh::net::protocol::core::QueryConsolidation::default(),
                )
                .await
                .map_err(communication::CommunicationError::from)?;
            if let Some(reply) = replies.next().await {
                let z_data = reply.data.payload.to_vec();
                let s_id = String::from_utf8_lossy(&z_data);
                let id = s_id
                    .parse::<usize>()
                    .map_err(|_| communication::CommunicationError::DeserializeNotImplemented)?;
                nodes.push(id);
                n += 1;
            } else {
                std::hint::spin_loop();
            }
        } else {
            n += 1;
        }
    }
    Ok(nodes)
}

#[cfg(any(feature = "zenoh_transport", feature = "zenoh_zerocopy_transport"))]
fn get_nodes_ids(total_nodes: usize, node_id: NodeId) -> Vec<NodeId> {
    let mut nodes = vec![];
    for n in 0..total_nodes {
        if n != node_id {
            nodes.push(n);
        }
    }
    nodes
}

/// Handle to a [`Node`] running asynchronously.
pub struct NodeHandle {
    thread_handle: thread::JoinHandle<()>,
    shutdown_tx: Sender<()>,
}

// TODO: distinguish between shutting down the dataflow and shutting down the node.
impl NodeHandle {
    /// Waits for the associated [`Node`] to finish.
    pub fn join(self) -> Result<(), String> {
        self.thread_handle.join().map_err(|e| format!("{:?}", e))
    }
    /// Blocks until the [`Node`] shuts down.
    pub fn shutdown(mut self) -> Result<(), String> {
        // Error indicates node is already shutting down.
        self.shutdown_tx.try_send(()).ok();
        self.thread_handle.join().map_err(|e| format!("{:?}", e))
    }
}
