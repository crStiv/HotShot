use async_std::{
    future::{timeout, TimeoutError},
    sync::{Condvar, Mutex},
    task::spawn,
};

use crate::network_node::{
    gen_multiaddr, ClientRequest, ConnectionData, NetworkError, NetworkEvent, NetworkNode,
    NetworkNodeConfig, NetworkNodeConfigBuilder, NetworkNodeConfigBuilderError, NetworkNodeType,
};
use flume::{Receiver, RecvError, SendError, Sender};
use futures::{select, Future, FutureExt, future::join_all};
use libp2p::{Multiaddr, PeerId};
use rand::{seq::IteratorRandom, thread_rng};
use snafu::{ResultExt, Snafu};
use std::{fmt::Debug, sync::Arc, time::Duration};
use tracing::{info, info_span, instrument, Instrument};

/// A handle containing:
/// - A reference to the state
/// - Controls for the swarm
#[derive(Debug)]
pub struct NetworkNodeHandle<S> {
    /// notifies that a state change has occurred
    pub state_changed: Condvar,
    /// the state of the replica
    pub state: Arc<Mutex<S>>,
    /// send an action to the networkbehaviour
    pub send_network: Sender<ClientRequest>,
    /// receive an action from the networkbehaviour
    pub recv_network: Receiver<NetworkEvent>,
    /// kill the event handler for events from the swarm
    pub kill_switch: Sender<()>,
    /// receiving end of `kill_switch`
    pub recv_kill: Receiver<()>,
    /// the local address we're listening on
    pub listen_addr: Multiaddr,
    /// the peer id of the networkbehaviour
    pub peer_id: PeerId,
    /// the connection metadata associated with the networkbehaviour
    pub connection_state: Arc<Mutex<ConnectionData>>,
}

impl<S: Default + Debug> NetworkNodeHandle<S> {
    /// constructs a new node listening on `known_addr`
    #[instrument]
    pub async fn new(
        known_addrs: &[(PeerId, Multiaddr)],
        config: NetworkNodeConfig,
    ) -> Result<Self, NetworkNodeHandleError> {
        //`randomly assigned port
        let listen_addr = gen_multiaddr(0);
        let mut network = NetworkNode::new(config).await.context(NetworkSnafu)?;
        let peer_id = network.peer_id;
        let listen_addr = network
            .start(listen_addr, known_addrs)
            .await
            .context(NetworkSnafu)?;
        let (send_chan, recv_chan) = network.spawn_listeners().await.context(NetworkSnafu)?;
        let (kill_switch, recv_kill) = flume::bounded(1);

        send_chan
            .send_async(ClientRequest::Subscribe("global".to_string()))
            .await
            .context(SendSnafu)?;

        Ok(NetworkNodeHandle {
            state_changed: Condvar::new(),
            state: Arc::new(Mutex::new(S::default())),
            send_network: send_chan,
            recv_network: recv_chan,
            kill_switch,
            recv_kill,
            listen_addr,
            peer_id,
            connection_state: Arc::default(),
        })
    }

    /// Cleanly shuts down a swarm node
    /// This is done by sending a message to
    /// the swarm event handler to stop handling events
    /// and a message to the swarm itself to spin down
    #[instrument]
    pub async fn kill(&self) -> Result<(), NetworkError> {
        self.send_network
            .send_async(ClientRequest::Shutdown)
            .await
            .map_err(|_e| NetworkError::StreamClosed)?;
        self.kill_switch
            .send_async(())
            .await
            .map_err(|_e| NetworkError::StreamClosed)?;
        Ok(())
    }

    /// Spins up `num_of_nodes` nodes, connects them to each other
    /// and waits for connections to propagate to all nodes.
    #[instrument]
    pub async fn spin_up_swarms(
        num_of_nodes: usize,
        timeout_len: Duration,
        num_bootstrap: usize
    ) -> Result<Vec<Arc<Self>>, NetworkNodeHandleError> {
        let mut handles = Vec::new();
        let mut bootstrap_addrs = Vec::<(PeerId, Multiaddr)>::new();
        let mut connecting_futs = Vec::new();

        for i in 0..num_bootstrap {
            let node = Arc::new(NetworkNodeHandle::new(&bootstrap_addrs, NetworkNodeConfig::default()).await?);
            let addr  = node.listen_addr.clone();
            bootstrap_addrs.push((node.peer_id, addr));
            connecting_futs.push(Self::wait_to_connect(node.clone(), num_of_nodes, node.recv_network.clone(), i));
            handles.push(node);
        }

        let regular_node_config = NetworkNodeConfigBuilder::default()
            .node_type(NetworkNodeType::Regular)
            .min_num_peers(10usize)
            .max_num_peers(15usize)
            .build()
            .context(NodeConfigSnafu)?;

        for j in 0..(num_of_nodes - num_bootstrap) {
            let node =
                Arc::new(NetworkNodeHandle::new(&bootstrap_addrs, regular_node_config).await?);
            connecting_futs.push(Self::wait_to_connect(
                node.clone(),
                num_of_nodes,
                node.recv_network.clone(),
                num_bootstrap + j,
            ));

            handles.push(node);
        }

        timeout(
            timeout_len,
            join_all(connecting_futs.into_iter()),
        )
        .await
        .context(TimeoutSnafu)?;
        println!("Connected!");
        Ok(handles)
    }

    /// Wait for a node to connect to other nodes
    #[instrument]
    async fn wait_to_connect(
        node: Arc<NetworkNodeHandle<S>>,
        num_of_nodes: usize,
        chan: Receiver<NetworkEvent>,
        node_idx: usize,
    ) -> Result<(), NetworkNodeHandleError> {
        let mut connected_ok = false;
        let mut known_ok = false;
        while !(known_ok && connected_ok) {
            match chan.recv_async().await.context(RecvSnafu)? {
                NetworkEvent::UpdateConnectedPeers(pids) =>
                {
                    node.connection_state.lock().await.connected_peers = pids.clone();
                    // TODO when replaced with config, this should be > min num nodes in config
                    if pids.len() >= 3 * num_of_nodes / 4 {
                        connected_ok = true;
                    }
                }
                NetworkEvent::UpdateKnownPeers(pids) => {
                    node.connection_state.lock().await.known_peers = pids.clone();
                    if pids.len() >= 3 * num_of_nodes / 4 {
                        known_ok = true;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// Glue function that listens for events from the Swarm corresponding to `handle`
/// and calls `event_handler` when an event is observed.
/// The idea is that this function can be used independent of the actual state
/// we use
#[allow(clippy::panic)]
#[instrument(skip(event_handler))]
pub async fn spawn_handler<S: 'static + Send + Default + Debug, Fut>(
    handle: Arc<NetworkNodeHandle<S>>,
    event_handler: impl (Fn(NetworkEvent, Arc<NetworkNodeHandle<S>>) -> Fut)
        + std::marker::Sync
        + std::marker::Send
        + 'static,
) where
    Fut: Future<Output = Result<(), NetworkNodeHandleError>>
        + std::marker::Send
        + 'static
        + std::marker::Sync,
{
    let recv_kill = handle.recv_kill.clone();
    let recv_event = handle.recv_network.clone();
    spawn(
        async move {
            loop {
                select!(
                    _ = recv_kill.recv_async().fuse() => {
                        break;
                    },
                    event = recv_event.recv_async().fuse() => {
                        event_handler(event.context(RecvSnafu)?, handle.clone()).await?;
                    },
                );
            }
            Ok::<(), NetworkNodeHandleError>(())
        }
        .instrument(info_span!("Libp2p Counter Handler")),
    );
}

/// Given a slice of handles assumed to be larger than 0,
/// chooses one
/// # Panics
/// panics if handles is of length 0
pub fn get_random_handle<S>(handles: &[Arc<NetworkNodeHandle<S>>]) -> Arc<NetworkNodeHandle<S>> {
    handles.iter().choose(&mut thread_rng()).unwrap().clone()
}

/// error wrapper type for interacting with swarm handle
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum NetworkNodeHandleError {
    /// error generating network
    NetworkError {
        /// source of error
        source: NetworkError,
    },
    /// failure to serialize a message
    SerializationError {
        /// source of error
        source: Box<bincode::ErrorKind>,
    },
    /// failure to deserialize a message
    DeserializationError {},
    /// error sending request to network
    SendError {
        /// source of error
        source: SendError<ClientRequest>,
    },
    /// error receiving message from network
    RecvError {
        /// source of error
        source: RecvError,
    },
    /// Timeout spinning up handle
    #[snafu(display("Failed to spin up nodes. Hit timeout instead. {source:?}"))]
    TimeoutError {
        /// source of error
        source: TimeoutError,
    },
    /// Error building Node config
    NodeConfigError {
        /// source of error
        source: NetworkNodeConfigBuilderError,
    },
}
