//! Testing harness for the hotshot repository
//!
//! To build a test environment you can create a [`TestLauncher`] instance. This launcher can be configured to have a custom networking layer, initial state, etc.
//!
//! Calling `TestLauncher::launch()` will turn this launcher into a [`TestRunner`], which can be used to start and stop nodes, send transacstions, etc.
//!
//! Node that `TestLauncher::launch()` is only available if the given `NETWORK`, `STATE` and `STORAGE` are correct.

#![warn(missing_docs)]

/// test launcher infrastructure
pub mod launcher;
/// implementations of various networking models
pub mod network_reliability;
/// structs and infra to describe the tests to be written
pub mod test_description;
/// set of commonly used test types for our tests
pub mod test_types;

pub use self::launcher::TestLauncher;

use either::Either;
use futures::future::LocalBoxFuture;
use hotshot::{
    traits::{NodeImplementation, TestableNodeImplementation},
    types::{HotShotHandle, SignatureKey},
    HotShot, HotShotError, HotShotInitializer, ViewRunner, H_256,
};
use hotshot_types::traits::election::ConsensusExchange;
use nll::nll_todo::nll_todo;

use hotshot_types::message::Message;
use hotshot_types::traits::node_implementation::{CommitteeNetwork, QuorumNetwork};
use hotshot_types::{
    data::LeafType,
    traits::{election::Membership, metrics::NoMetrics, node_implementation::NodeType},
    HotShotConfig,
};
use snafu::Snafu;
use std::{collections::HashMap, fmt::Debug, ops::Deref, sync::Arc};
use test_description::RoundCheckDescription;
use tracing::{debug, error, info, warn};

/// Wrapper for a function that takes a `node_id` and returns an instance of `T`.
pub type Generator<T> = Box<dyn Fn(u64) -> T + 'static>;

/// For now we only support a size of [`H_256`]. This can be changed in the future.
pub const N: usize = H_256;

/// Alias for `(Vec<S>, Vec<B>)`. Used in [`RoundResult`].
pub type StateAndBlock<S, B> = (Vec<S>, Vec<B>);

/// Result of running a round of consensus
#[derive(Debug, Default)]
// TODO do we need static here
pub struct RoundResult<TYPES: NodeType, LEAF: LeafType<NodeType = TYPES>> {
    /// Transactions that were submitted
    pub txns: Vec<TYPES::Transaction>,
    /// Nodes that committed this round
    pub success_nodes: HashMap<u64, StateAndBlock<LEAF::StateCommitmentType, LEAF::DeltasType>>,
    /// Nodes that failed to commit this round
    pub failed_nodes: HashMap<u64, HotShotError<TYPES>>,

    /// whether or not the round succeeded (for a custom defn of succeeded)
    pub success: bool,
}

/// context for a round
/// TODO eventually we want these to just be futures
/// that we poll when things are event driven
/// this context will be passed around
#[derive(Debug)]
pub struct RoundCtx<TYPES: NodeType, I: TestableNodeImplementation<TYPES>> {
    prior_round_results: Vec<RoundResult<TYPES, <I as NodeImplementation<TYPES>>::Leaf>>,
    views_since_progress: usize,
    total_failed_views: usize,
}

impl<TYPES: NodeType, I: TestableNodeImplementation<TYPES>> Default for RoundCtx<TYPES, I> {
    fn default() -> Self {
        Self {
            prior_round_results: Default::default(),
            views_since_progress: 0,
            total_failed_views: 0,
        }
    }
}

/// Type of function used for checking results after running a view of consensus
#[derive(Clone)]
pub struct RoundPostSafetyCheck<TYPES: NodeType, I: TestableNodeImplementation<TYPES>>(
    pub  Arc<
        dyn for<'a> Fn(
            &'a TestRunner<TYPES, I>,
            &'a mut RoundCtx<TYPES, I>,
            RoundResult<TYPES, <I as NodeImplementation<TYPES>>::Leaf>,
        ) -> LocalBoxFuture<'a, Result<(), ConsensusFailedError>>,
    >,
);

/// Type of function used for checking results after running a view of consensus
// #[derive(Clone)]
// pub struct RoundPostSafetyCheck<TYPES: NodeType, I: TestableNodeImplementation<TYPES>>(
//     pub  Arc<
//         dyn for<'a> Fn(
//             &'a TestRunner<TYPES, I>,
//             &'a mut RoundCtx<TYPES, I>,
//             RoundResult<TYPES, <I as NodeImplementation<TYPES>>::Leaf>,
//         ) -> LocalBoxFuture<'a, Result<(), ConsensusFailedError>>,
//     >,
// );

impl<TYPES: NodeType, I: TestableNodeImplementation<TYPES>> Deref
    for RoundPostSafetyCheck<TYPES, I>
{
    type Target = dyn for<'a> Fn(
        &'a TestRunner<TYPES, I>,
        &'a mut RoundCtx<TYPES, I>,
        RoundResult<TYPES, <I as NodeImplementation<TYPES>>::Leaf>,
    ) -> LocalBoxFuture<'a, Result<(), ConsensusFailedError>>;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

/// Type of function used for configuring a round of consensus
#[derive(Clone)]
pub struct RoundSetup<TYPES: NodeType, I: TestableNodeImplementation<TYPES>>(
    pub  Arc<
        dyn for<'a> Fn(
            &'a mut TestRunner<TYPES, I>,
            &'a RoundCtx<TYPES, I>,
        ) -> LocalBoxFuture<'a, Vec<TYPES::Transaction>>,
    >,
);

pub fn constrain<TYPES: NodeType, I: TestableNodeImplementation<TYPES>, F>(f: F) -> F
where
    F: for<'a> Fn(&'a mut TestRunner<TYPES, I>, &'a RoundCtx<TYPES, I>) -> LocalBoxFuture<'a, Vec<TYPES::Transaction>>,
{
    f
}

impl<TYPES: NodeType, I: TestableNodeImplementation<TYPES>> Deref for RoundSetup<TYPES, I> {
    type Target = dyn for<'a> Fn(
        &'a mut TestRunner<TYPES, I>,
        &'a RoundCtx<TYPES, I>,
    ) -> LocalBoxFuture<'a, Vec<TYPES::Transaction>>;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

/// Type of function used for checking safety before beginnning consensus
#[derive(Clone)]
pub struct RoundPreSafetyCheck<TYPES: NodeType, I: TestableNodeImplementation<TYPES>>(
    pub  Arc<
        dyn for<'a> Fn(
            &'a TestRunner<TYPES, I>,
            &'a RoundCtx<TYPES, I>,
        ) -> LocalBoxFuture<'a, Result<(), ConsensusFailedError>>,
    >,
);

impl<TYPES: NodeType, I: TestableNodeImplementation<TYPES>> Default
    for RoundPreSafetyCheck<TYPES, I>
{
    fn default() -> Self {
        Self(Arc::new(default_safety_check_pre))
    }
}

impl<TYPES: NodeType, I: TestableNodeImplementation<TYPES>> Deref
    for RoundPreSafetyCheck<TYPES, I>
{
    type Target = dyn for<'a> Fn(
        &'a TestRunner<TYPES, I>,
        &'a RoundCtx<TYPES, I>,
    ) -> LocalBoxFuture<'a, Result<(), ConsensusFailedError>>;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

/// functions to run a round of consensus
/// the control flow is: (1) pre safety check, (2) setup round, (3) post safety check
pub struct Round<TYPES: NodeType, I: TestableNodeImplementation<TYPES>> {
    /// Safety check before round is set up and run
    /// to ensure consistent state
    pub safety_check_pre: RoundPreSafetyCheck<TYPES, I>,

    /// Round set up
    pub setup_round: RoundSetup<TYPES, I>,

    /// Safety check after round is complete
    pub safety_check_post: RoundPostSafetyCheck<TYPES, I>,
}

pub fn default_safety_check_pre<'a, TYPES: NodeType, I: TestableNodeImplementation<TYPES>>(
    _asdf: &'a TestRunner<TYPES, I>,
    _ctx: &'a RoundCtx<TYPES, I>,
) -> LocalBoxFuture<'a, Result<(), ConsensusFailedError>> {
    use futures::FutureExt;
    async move { Ok(()) }.boxed()
}

pub fn default_setup_round<'a, TYPES: NodeType, TRANS, I: TestableNodeImplementation<TYPES>>(
    _asdf: &'a mut TestRunner<TYPES, I>,
    _ctx: &'a RoundCtx<TYPES, I>,
) -> LocalBoxFuture<'a, Vec<TRANS>> {
    use futures::FutureExt;
    async move { vec![] }.boxed()
}

pub fn default_safety_check_post<'a, TYPES: NodeType, I: TestableNodeImplementation<TYPES>>(
    _asdf: &'a TestRunner<TYPES, I>,
    _ctx: &'a mut RoundCtx<TYPES, I>,
    _result: RoundResult<TYPES, <I as NodeImplementation<TYPES>>::Leaf>,
) -> LocalBoxFuture<'a, Result<(), ConsensusFailedError>> {
    use futures::FutureExt;
    async move { Ok(()) }.boxed()
}

impl<TYPES: NodeType, I: TestableNodeImplementation<TYPES>> Default for Round<TYPES, I> {
    fn default() -> Self {
        Self {
            safety_check_post: RoundPostSafetyCheck(Arc::new(default_safety_check_post)),
            setup_round: RoundSetup(Arc::new(default_setup_round)),
            safety_check_pre: RoundPreSafetyCheck(Arc::new(default_safety_check_pre)),
        }
    }
}
impl<TYPES: NodeType, I: TestableNodeImplementation<TYPES>> Clone for Round<TYPES, I> {
    fn clone(&self) -> Self {
        Self {
            safety_check_pre: self.safety_check_pre.clone(),
            setup_round: self.setup_round.clone(),
            safety_check_post: self.safety_check_post.clone(),
        }
    }
}

/// The runner of a test network
/// spin up and down nodes, execute rounds
pub struct TestRunner<TYPES: NodeType, I: TestableNodeImplementation<TYPES>> {
    quorum_network_generator: Generator<QuorumNetwork<TYPES, I>>,
    committee_network_generator: Generator<CommitteeNetwork<TYPES, I>>,
    storage_generator: Generator<I::Storage>,
    default_node_config: HotShotConfig<TYPES::SignatureKey, TYPES::ElectionConfigType>,
    nodes: Vec<Node<TYPES, I>>,
    next_node_id: u64,
    round: Round<TYPES, I>,
}

struct Node<TYPES: NodeType, I: TestableNodeImplementation<TYPES>> {
    pub node_id: u64,
    pub handle: HotShotHandle<TYPES, I>,
}

impl<TYPES: NodeType, I: TestableNodeImplementation<TYPES>> TestRunner<TYPES, I> {
    pub(self) fn new(launcher: TestLauncher<TYPES, I>) -> Self {
        Self {
            quorum_network_generator: launcher.quorum_network,
            committee_network_generator: launcher.committee_network,
            storage_generator: launcher.storage,
            default_node_config: launcher.config,
            nodes: Vec::new(),
            next_node_id: 0,
            round: Round::default(),
        }
    }

    /// default setup for round
    pub fn default_before_round(_runner: &mut Self) -> Vec<TYPES::Transaction> {
        Vec::new()
    }
    /// default safety check
    pub fn default_safety_check(_runner: &Self, _results: RoundResult<TYPES, I::Leaf>) {}

    /// Add `count` nodes to the network. These will be spawned with the default node config and state
    pub async fn add_nodes(&mut self, count: usize) -> Vec<u64>
    where
        HotShot<TYPES::ConsensusType, TYPES, I>: ViewRunner<TYPES, I>,
    {
        let mut results = vec![];
        for _i in 0..count {
            let node_id = self.next_node_id;
            let quorum_network = (self.quorum_network_generator)(node_id);
            let committee_network = (self.committee_network_generator)(node_id);
            let storage = (self.storage_generator)(node_id);
            let config = self.default_node_config.clone();
            let initializer =
                HotShotInitializer::<TYPES, I::Leaf>::from_genesis(I::block_genesis()).unwrap();
            let node_id = self
                .add_node_with_config(
                    quorum_network,
                    committee_network,
                    storage,
                    initializer,
                    config,
                )
                .await;
            results.push(node_id);
        }

        results
    }

    /// replace round list
    #[allow(clippy::type_complexity)]
    pub fn with_round(&mut self, round: Round<TYPES, I>) {
        self.round = round;
    }

    /// Get the next node id that would be used for `add_node_with_config`
    pub fn next_node_id(&self) -> u64 {
        self.next_node_id
    }

    /// Add a node with the given config. This can be used to fine tweak the settings of this particular node. The internal `next_node_id` will be incremented after calling this function.
    ///
    /// For a simpler way to add nodes to this runner, see `add_nodes`
    pub async fn add_node_with_config(
        &mut self,
        quorum_network: QuorumNetwork<TYPES, I>,
        committee_network: CommitteeNetwork<TYPES, I>,
        storage: I::Storage,
        initializer: HotShotInitializer<TYPES, I::Leaf>,
        config: HotShotConfig<TYPES::SignatureKey, TYPES::ElectionConfigType>,
    ) -> u64
    where
        HotShot<TYPES::ConsensusType, TYPES, I>: ViewRunner<TYPES, I>,
    {
        let node_id = self.next_node_id;
        self.next_node_id += 1;

        let known_nodes = config.known_nodes.clone();
        let private_key = I::generate_test_key(node_id);
        let public_key = TYPES::SignatureKey::from_private(&private_key);
        let election_config = config.election_config.clone().unwrap_or_else(|| {
            <<I as NodeImplementation<TYPES>>::QuorumExchange as ConsensusExchange<
                TYPES,
                I::Leaf,
                Message<TYPES, I>,
            >>::Membership::default_election_config(config.total_nodes.get() as u64)
        });
        let quorum_exchange = I::QuorumExchange::create(
            known_nodes.clone(),
            election_config.clone(),
            quorum_network,
            public_key.clone(),
            private_key.clone(),
        );
        let committee_exchange = I::CommitteeExchange::create(
            known_nodes,
            election_config,
            committee_network,
            public_key.clone(),
            private_key.clone(),
        );
        let handle = HotShot::init(
            public_key,
            private_key,
            node_id,
            config,
            storage,
            quorum_exchange,
            committee_exchange,
            initializer,
            NoMetrics::boxed(),
        )
        .await
        .expect("Could not init hotshot");
        self.nodes.push(Node { handle, node_id });
        node_id
    }

    /// Iterate over the [`HotShotHandle`] nodes in this runner.
    pub fn nodes(&self) -> impl Iterator<Item = &HotShotHandle<TYPES, I>> + '_ {
        self.nodes.iter().map(|node| &node.handle)
    }

    /// repeatedly executes consensus until either:
    /// * `self.fail_threshold` rounds fail
    /// * `self.num_succeeds` rounds are successful
    /// (for a definition of success defined by safety checks)
    pub async fn execute_rounds(
        &mut self,
        num_success: usize,
        fail_threshold: usize,
    ) -> Result<(), ConsensusTestError> {
        let mut num_fails = 0;
        // the default context starts as empty
        let mut ctx = RoundCtx::<TYPES, I>::default();
        for i in 0..(num_success + fail_threshold) {
            if let Err(e) = self.execute_round(&mut ctx).await {
                num_fails += 1;
                error!("failed round {:?} of consensus with error: {:?}", i, e);
                if num_fails > fail_threshold {
                    error!("returning error");
                    return Err(ConsensusTestError::TooManyFailures);
                }
            }
        }
        Ok(())
    }

    /// Execute a single round of consensus
    /// This consists of the following steps:
    /// - checking the state of the hotshot
    /// - setting up the round (ex: submitting txns) or spinning up or down nodes
    /// - checking safety conditions to ensure that the round executed as expected
    pub async fn execute_round(
        &mut self,
        ctx: &mut RoundCtx<TYPES, I>,
    ) -> Result<(), ConsensusFailedError> {
        let Round {
            safety_check_pre,
            setup_round,
            safety_check_post,
        } = self.round.clone();

        safety_check_pre(self, ctx).await?;

        let txns = setup_round(self, ctx).await;
        let results = self.run_one_round(txns).await;
        safety_check_post(self, ctx, results).await?;
        Ok(())
    }

    /// Internal function that unpauses hotshots and waits for round to complete,
    /// returns a `RoundResult` upon successful completion, indicating what (if anything) was
    /// committed
    async fn run_one_round(
        &mut self,
        txns: Vec<TYPES::Transaction>,
    ) -> RoundResult<TYPES, I::Leaf> {
        let mut results = HashMap::new();

        info!("EXECUTOR: running one round");
        for handle in self.nodes() {
            handle.start_one_round().await;
        }
        info!("EXECUTOR: done running one round");
        let mut failures = HashMap::new();
        for node in &mut self.nodes {
            let result = node.handle.collect_round_events().await;
            info!(
                "EXECUTOR: collected node {:?} results: {:?}",
                node.node_id.clone(),
                result
            );
            match result {
                Ok((state, block)) => {
                    results.insert(node.node_id, (state, block));
                }
                Err(e) => {
                    failures.insert(node.node_id, e);
                }
            }
        }
        info!("All nodes reached decision");
        if !failures.is_empty() {
            error!(
                "Some failures this round. Failing nodes: {:?}. Successful nodes: {:?}",
                failures, results
            );
        }
        RoundResult {
            txns,
            success_nodes: results,
            failed_nodes: failures,
            success: nll_todo(),
        }
    }

    /// Gracefully shut down this system
    pub async fn shutdown_all(self) {
        for node in self.nodes {
            node.handle.shut_down().await;
        }
        debug!("All nodes should be shut down now.");
    }

    /// In-place shut down an individual node with id `node_id`
    /// # Errors
    /// returns [`ConsensusRoundError::NoSuchNode`] if the node idx is either
    /// - already shut down
    /// - does not exist
    pub async fn shutdown(&mut self, node_id: u64) -> Result<(), ConsensusFailedError> {
        let maybe_idx = self.nodes.iter().position(|n| n.node_id == node_id);
        if let Some(idx) = maybe_idx {
            let node = self.nodes.remove(idx);
            node.handle.shut_down().await;
            Ok(())
        } else {
            Err(ConsensusFailedError::NoSuchNode {
                node_ids: self.ids(),
                requested_id: node_id,
            })
        }
    }

    /// returns the requested handle specified by `id` if it exists
    /// else returns `None`
    pub fn get_handle(&self, id: u64) -> Option<HotShotHandle<TYPES, I>> {
        self.nodes.iter().find_map(|node| {
            if node.node_id == id {
                Some(node.handle.clone())
            } else {
                None
            }
        })
    }

    /// return curent node ids
    pub fn ids(&self) -> Vec<u64> {
        self.nodes.iter().map(|n| n.node_id).collect()
    }
}

impl<TYPES: NodeType, I: TestableNodeImplementation<TYPES>> TestRunner<TYPES, I> {
    /// Will validate that all nodes are on exactly the same state.
    /// TODO `views_since_failed` should be contained within ctx
    pub async fn validate_nodes(&self, desc: &RoundCheckDescription, views_since_failed: usize) {
        let mut leaves = HashMap::<I::Leaf, usize>::new();

        if desc.check_leaf {
            let mut result = None;
            // group all the leaves since thankfully leaf implements hash
            for node in self.nodes.iter() {
                let decide_leaf = node.handle.get_decided_leaf().await;
                match leaves.entry(decide_leaf) {
                    std::collections::hash_map::Entry::Occupied(mut o) => {
                        *o.get_mut() += 1;
                    }
                    std::collections::hash_map::Entry::Vacant(v) => {
                        v.insert(1);
                    }
                }
            }
            let collective = self.nodes().collect::<Vec<_>>().len() - desc.num_out_of_sync;
            for (leaf, num_nodes) in leaves {
                if num_nodes >= collective {
                    result = Some(leaf);
                }
            }
        }
    }

    /// Will validate that all nodes are on exactly the same state.
    pub async fn validate_node_states(&self) {
        let mut leaves = Vec::<I::Leaf>::new();
        for node in self.nodes.iter() {
            let decide_leaf = node.handle.get_decided_leaf().await;
            leaves.push(decide_leaf);
        }

        let (first_leaf, remaining) = leaves.split_first().unwrap();
        // Hack, needs to be fixed: https://github.com/EspressoSystems/HotShot/issues/295
        // Sometimes 1 of the nodes is not in sync with the rest
        // For now we simply check if n-2 nodes match the first node
        let mut mismatch_count = 0;

        for (idx, leaf) in remaining.iter().enumerate() {
            if first_leaf != leaf {
                eprintln!("Leaf dump for {idx:?}");
                eprintln!("\texpected: {first_leaf:#?}");
                eprintln!("\tgot:      {leaf:#?}");
                eprintln!("Node {idx} storage state does not match the first node");
                mismatch_count += 1;
            }
        }

        if mismatch_count == 0 {
            info!("All nodes are on the same decided leaf.");
            return;
        } else if mismatch_count == 1 {
            // Hack, needs to be fixed: https://github.com/EspressoSystems/HotShot/issues/295
            warn!("One node mismatch, but accepting this anyway.");
            return;
        } else if mismatch_count == self.nodes.len() - 1 {
            // It's probably the first node that is out of sync, check the `remaining` nodes for equality
            let mut all_other_nodes_match = true;

            // not stable yet: https://github.com/rust-lang/rust/issues/75027
            // for [left, right] in remaining.array_windows::<2>() {
            for slice in remaining.windows(2) {
                let (left, right) = if let [left, right] = slice {
                    (left, right)
                } else {
                    unimplemented!()
                };
                if left == right {
                    all_other_nodes_match = false;
                }
            }

            if all_other_nodes_match {
                warn!("One node mismatch, but accepting this anyway");
                return;
            }
        }

        // We tried to recover from n-1 nodes not match, but failed
        // The `eprintln` above will be shown in the output, so we can simply panic
        panic!("Node states do not match");
    }
}

// FIXME make these return some sort of generic error.
// corresponding issue: <https://github.com/EspressoSystems/hotshot/issues/181>
impl<TYPES: NodeType, I: TestableNodeImplementation<TYPES>> TestRunner<TYPES, I> {
    /// Add a random transaction to this runner.
    pub async fn add_random_transaction(
        &self,
        node_id: Option<usize>,
        rng: &mut dyn rand::RngCore,
    ) -> TYPES::Transaction {
        if self.nodes.is_empty() {
            panic!("Tried to add transaction, but no nodes have been added!");
        }

        use rand::seq::IteratorRandom;

        // we're assuming all nodes have the same leaf.
        // If they don't match, this is probably fine since
        // it should be caught by an assertion (and the txn will be rejected anyway)
        let leaf = self.nodes[0].handle.get_decided_leaf().await;

        let txn = I::leaf_create_random_transaction(&leaf, rng, 0);

        let node = if let Some(node_id) = node_id {
            self.nodes.get(node_id).unwrap()
        } else {
            // find a random handle to send this transaction from
            self.nodes.iter().choose(rng).unwrap()
        };

        node.handle
            .submit_transaction(txn.clone())
            .await
            .expect("Could not send transaction");
        txn
    }

    /// add `n` transactions
    /// TODO error handling to make sure entire set of transactions can be processed
    pub async fn add_random_transactions(
        &self,
        n: usize,
        rng: &mut dyn rand::RngCore,
    ) -> Option<Vec<TYPES::Transaction>> {
        let mut result = Vec::new();
        for _ in 0..n {
            result.push(self.add_random_transaction(None, rng).await);
        }
        Some(result)
    }
}

#[derive(Debug, Snafu)]
/// Error that is returned from [`TestRunner`] with methods related to transactions
pub enum TransactionError {
    /// There are no valid nodes online
    NoNodes,
    /// There are no valid balances available
    NoValidBalance,
    /// FIXME remove this entirely
    /// The requested node does not exist
    InvalidNode,
}

/// Overarchign errors encountered
/// when trying to reach consensus
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum ConsensusFailedError {
    /// Safety condition failed
    SafetyFailed {
        /// description of error
        description: String,
    },
    /// No node exists
    NoSuchNode {
        /// the existing nodes
        node_ids: Vec<u64>,
        /// the node requested
        requested_id: u64,
    },

    /// View times out with any node as the leader.
    TimedOutWithoutAnyLeader,

    NoTransactionsSubmitted,

    /// replicas timed out
    ReplicasTimedOut,

    /// States after a round of consensus is inconsistent.
    InconsistentAfterTxn,

    /// Unable to submit valid transaction
    TransactionError {
        /// source of error
        source: TransactionError,
    },
    /// Too many consecutive failures
    TooManyConsecutiveFailures,
    /// too many view failures overall
    TooManyViewFailures,
    /// inconsistent leaves
    InconsistentLeaves,
    InconsistentStates,
    InconsistentBlocks

}

/// An overarching consensus test failure
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum ConsensusTestError {
    /// Too many nodes failed
    TooManyFailures,
}
