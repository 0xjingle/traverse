//! # Traverse Node types configuration
//!
//! The [`TraverseNode`] type implements the [`NodeTypes`] trait, and configures the engine types
//! required for the optimism engine API.

use crate::evm::TraverseEvmConfig;
use alloy_consensus::transaction::PooledTransaction;
use reth_evm::execute::BasicBlockExecutorProvider;
use reth_network::{
    transactions::{TransactionPropagationMode, TransactionsManagerConfig},
    NetworkHandle, NetworkManager, PeersInfo,
};
use reth_network_types::ReputationChangeWeights;
use reth_node_api::{FullNodeTypes, NodeTypesWithEngine, TxTy};
use reth_node_builder::{
    components::{
        ComponentsBuilder, ExecutorBuilder, NetworkBuilder, PayloadServiceBuilder,
        PoolBuilderConfigOverrides,
    },
    BuilderContext, Node, NodeAdapter, NodeComponentsBuilder, NodeTypes,
};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_node::{
    args::RollupArgs,
    node::{
        OpAddOns, OpConsensusBuilder, OpNetworkBuilder, OpPayloadBuilder, OpPoolBuilder, OpStorage,
    },
    OpEngineTypes, OpExecutionStrategyFactory, OpNetworkPrimitives,
};
use reth_optimism_primitives::OpPrimitives;
use reth_payload_builder::PayloadBuilderHandle;
use reth_transaction_pool::{
    PoolTransaction, SubPoolLimit, TransactionPool, TXPOOL_MAX_ACCOUNT_SLOTS_PER_SENDER,
};
use reth_trie_db::MerklePatriciaTrie;
use std::time::Duration;
use tracing::info;

/// Type configuration for a regular Traverse node.
#[derive(Debug, Clone, Default)]
pub struct TraverseNode {
    /// Additional Optimism args
    pub args: RollupArgs,
}

impl TraverseNode {
    /// Creates a new instance of the Optimism node type.
    pub const fn new(args: RollupArgs) -> Self {
        Self { args }
    }

    /// Returns the components for the given [`RollupArgs`].
    pub fn components<Node>(
        args: &RollupArgs,
    ) -> ComponentsBuilder<
        Node,
        OpPoolBuilder,
        TraversePayloadBuilder,
        TraverseNetworkBuilder,
        TraverseExecutorBuilder,
        OpConsensusBuilder,
    >
    where
        Node: FullNodeTypes<
            Types: NodeTypesWithEngine<
                Engine = OpEngineTypes,
                ChainSpec = OpChainSpec,
                Primitives = OpPrimitives,
            >,
        >,
    {
        ComponentsBuilder::default()
            .node_types::<Node>()
            .pool(OpPoolBuilder {
                pool_config_overrides: PoolBuilderConfigOverrides {
                    queued_limit: Some(SubPoolLimit::default() * 2),
                    pending_limit: Some(SubPoolLimit::default() * 2),
                    basefee_limit: Some(SubPoolLimit::default() * 2),
                    max_account_slots: Some(TXPOOL_MAX_ACCOUNT_SLOTS_PER_SENDER * 2),
                    ..Default::default()
                },
            })
            .payload(TraversePayloadBuilder::new(args.compute_pending_block))
            .network(TraverseNetworkBuilder::new(OpNetworkBuilder {
                disable_txpool_gossip: args.disable_txpool_gossip,
                disable_discovery_v4: !args.discovery_v4,
            }))
            .executor(TraverseExecutorBuilder::default())
            .consensus(OpConsensusBuilder::default())
    }
}

/// Configure the node types
impl NodeTypes for TraverseNode {
    type Primitives = OpPrimitives;
    type ChainSpec = OpChainSpec;
    type StateCommitment = MerklePatriciaTrie;
    type Storage = OpStorage;
}

impl NodeTypesWithEngine for TraverseNode {
    type Engine = OpEngineTypes;
}

impl<N> Node<N> for TraverseNode
where
    N: FullNodeTypes<
        Types: NodeTypesWithEngine<
            Engine = OpEngineTypes,
            ChainSpec = OpChainSpec,
            Primitives = OpPrimitives,
            Storage = OpStorage,
        >,
    >,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        OpPoolBuilder,
        TraversePayloadBuilder,
        TraverseNetworkBuilder,
        TraverseExecutorBuilder,
        OpConsensusBuilder,
    >;

    type AddOns =
        OpAddOns<NodeAdapter<N, <Self::ComponentsBuilder as NodeComponentsBuilder<N>>::Components>>;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        let Self { args } = self;
        Self::components(args)
    }

    fn add_ons(&self) -> Self::AddOns {
        Self::AddOns::builder().with_sequencer(self.args.sequencer_http.clone()).build()
    }
}

/// The Traverse evm and executor builder.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct TraverseExecutorBuilder;

impl<Node> ExecutorBuilder<Node> for TraverseExecutorBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = OpChainSpec, Primitives = OpPrimitives>>,
{
    type EVM = TraverseEvmConfig;
    type Executor = BasicBlockExecutorProvider<OpExecutionStrategyFactory<Self::EVM>>;

    async fn build_evm(
        self,
        ctx: &BuilderContext<Node>,
    ) -> eyre::Result<(Self::EVM, Self::Executor)> {
        let chain_spec = ctx.chain_spec();
        let evm_config = TraverseEvmConfig::new(chain_spec);
        let strategy_factory =
            OpExecutionStrategyFactory::new(ctx.chain_spec(), evm_config.clone());
        let executor = BasicBlockExecutorProvider::new(strategy_factory);

        Ok((evm_config, executor))
    }
}

/// The Traverse payload service builder.
///
/// This service wraps the default Optimism payload builder, but replaces the default evm config
/// with Traverse's own.
#[derive(Debug, Default, Clone)]
pub struct TraversePayloadBuilder {
    /// Inner Optimism payload builder service.
    inner: OpPayloadBuilder,
}

impl TraversePayloadBuilder {
    /// Create a new instance with the given `compute_pending_block` flag.
    pub fn new(compute_pending_block: bool) -> Self {
        Self { inner: OpPayloadBuilder::new(compute_pending_block) }
    }
}

impl<Node, Pool> PayloadServiceBuilder<Node, Pool> for TraversePayloadBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypesWithEngine<
            Engine = OpEngineTypes,
            ChainSpec = OpChainSpec,
            Primitives = OpPrimitives,
        >,
    >,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TxTy<Node::Types>>>
        + Unpin
        + 'static,
{
    async fn spawn_payload_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<PayloadBuilderHandle<OpEngineTypes>> {
        self.inner.spawn(TraverseEvmConfig::new(ctx.chain_spec()), ctx, pool)
    }
}

/// The default traverse network builder.
#[derive(Debug, Default, Clone)]
pub struct TraverseNetworkBuilder {
    inner: OpNetworkBuilder,
}

impl TraverseNetworkBuilder {
    /// Create a new instance based on the given op builder
    pub const fn new(network: OpNetworkBuilder) -> Self {
        Self { inner: network }
    }
}

impl<Node, Pool> NetworkBuilder<Node, Pool> for TraverseNetworkBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = OpChainSpec, Primitives = OpPrimitives>>,
    Pool: TransactionPool<
            Transaction: PoolTransaction<Consensus = TxTy<Node::Types>, Pooled = PooledTransaction>,
        > + Unpin
        + 'static,
{
    type Primitives = OpNetworkPrimitives;

    async fn build_network(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<NetworkHandle<OpNetworkPrimitives>> {
        let mut network_config = self.inner.network_config(ctx)?;
        // this is rolled with limited trusted peers and we want ignore any reputation slashing
        network_config.peers_config.reputation_weights = ReputationChangeWeights::zero();
        network_config.peers_config.backoff_durations.low = Duration::from_secs(5);
        network_config.peers_config.backoff_durations.medium = Duration::from_secs(5);
        network_config.peers_config.backoff_durations.high = Duration::from_secs(5);
        network_config.peers_config.max_backoff_count = u8::MAX;
        network_config.sessions_config.session_command_buffer = 750;
        network_config.sessions_config.session_event_buffer = 750;

        let txconfig = TransactionsManagerConfig {
            propagation_mode: TransactionPropagationMode::All,
            ..network_config.transactions_manager_config.clone()
        };
        let network = NetworkManager::builder(network_config).await?;
        let handle = ctx.start_network_with(network, pool, txconfig);
        info!(target: "reth::cli", enode=%handle.local_node_record(), "P2P networking initialized");
        Ok(handle)
    }
}
