//! # Traverse EVM configuration
//!
//! The [`TraverseEvmConfig`] type implements the [`ConfigureEvm`] and [`ConfigureEvmEnv`] traits,
//! configuring the custom Traverse precompiles and instructions.
//!
//! These trait implementations allow for custom precompiles and instructions to be implemented and
//! integrated in a reth node only with importing, without the need to fork the node or EVM
//! implementation.
//!
//! This currently configures the instructions defined in [EIP3074-instructions](https://github.com/paradigmxyz/eip3074-instructions), and the
//! precompiles defined by [`revm_precompile`].

use alloy_consensus::Header;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use op_alloy_consensus::EIP1559ParamError;
use reth_chainspec::{ChainSpec, EthereumHardfork};
use reth_evm::env::EvmEnv;
use reth_node_api::{ConfigureEvm, ConfigureEvmEnv, NextBlockEnvAttributes};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_forks::OpHardfork;
use reth_optimism_primitives::OpTransactionSigned;
use reth_primitives::transaction::FillTxEnv;
use reth_revm::{
    handler::register::EvmHandler,
    inspector_handle_register,
    precompile::PrecompileSpecId,
    primitives::{
        AnalysisKind, BlobExcessGasAndPrice, BlockEnv, CfgEnv, Env, HandlerCfg, OptimismFields,
        SpecId,
    },
    ContextPrecompiles, Database, Evm, EvmBuilder, GetInspector,
};
use revm_precompile::{
    secp256r1::{p256_verify, P256VERIFY as REVM_P256VERIFY},
    u64_to_address, PrecompileWithAddress,
};
use revm_primitives::{CfgEnvWithHandlerCfg, Precompile, TxEnv};
use std::sync::Arc;

/// P256 verify precompile address.
pub const P256VERIFY_ADDRESS: u64 = 0x14;

/// [RIP-7212](https://github.com/ethereum/RIPs/blob/master/RIPS/rip-7212.md#specification) secp256r1 precompile.
pub const P256VERIFY: PrecompileWithAddress =
    PrecompileWithAddress(u64_to_address(P256VERIFY_ADDRESS), Precompile::Standard(p256_verify));

/// Custom EVM configuration
#[derive(Debug, Clone)]
pub struct TraverseEvmConfig {
    chain_spec: Arc<OpChainSpec>,
}

impl TraverseEvmConfig {
    /// Creates a new Traverse EVM configuration with the given chain spec.
    pub const fn new(chain_spec: Arc<OpChainSpec>) -> Self {
        Self { chain_spec }
    }

    fn precompiles() -> impl Iterator<Item = PrecompileWithAddress> {
        [P256VERIFY, REVM_P256VERIFY].into_iter()
    }

    /// Sets the precompiles to the EVM handler
    ///
    /// This will be invoked when the EVM is created via [`ConfigureEvm::evm`] or
    /// [`ConfigureEvm::evm_with_inspector`]
    ///
    /// This will use the default mainnet precompiles and add additional precompiles.
    fn set_precompiles<EXT, DB>(handler: &mut EvmHandler<'_, EXT, DB>)
    where
        DB: Database,
    {
        // first we need the evm spec id, which determines the precompiles
        let spec_id = handler.cfg.spec_id;

        // install the precompiles
        handler.pre_execution.load_precompiles = Arc::new(move || {
            let mut loaded_precompiles: ContextPrecompiles<DB> =
                ContextPrecompiles::new(PrecompileSpecId::from_spec_id(spec_id));

            loaded_precompiles.extend(Self::precompiles());

            loaded_precompiles
        });
    }
}

impl ConfigureEvmEnv for TraverseEvmConfig {
    type Header = Header;
    type Transaction = OpTransactionSigned;
    type Error = EIP1559ParamError;

    fn fill_tx_env(&self, tx_env: &mut TxEnv, transaction: &OpTransactionSigned, sender: Address) {
        transaction.fill_tx_env(tx_env, sender);
    }

    fn fill_tx_env_system_contract_call(
        &self,
        env: &mut Env,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) {
        env.tx = TxEnv {
            caller,
            transact_to: TxKind::Call(contract),
            // Explicitly set nonce to None so revm does not do any nonce checks
            nonce: None,
            gas_limit: 30_000_000,
            value: U256::ZERO,
            data,
            // Setting the gas price to zero enforces that no value is transferred as part of the
            // call, and that the call will not count against the block's gas limit
            gas_price: U256::ZERO,
            // The chain ID check is not relevant here and is disabled if set to None
            chain_id: None,
            // Setting the gas priority fee to None ensures the effective gas price is derived from
            // the `gas_price` field, which we need to be zero
            gas_priority_fee: None,
            access_list: Vec::new(),
            // blob fields can be None for this tx
            blob_hashes: Vec::new(),
            max_fee_per_blob_gas: None,
            authorization_list: None,
            optimism: OptimismFields {
                source_hash: None,
                mint: None,
                is_system_transaction: Some(false),
                // The L1 fee is not charged for the EIP-4788 transaction, submit zero bytes for the
                // enveloped tx size.
                enveloped_tx: Some(Bytes::default()),
            },
        };

        // ensure the block gas limit is >= the tx
        env.block.gas_limit = U256::from(env.tx.gas_limit);

        // disable the base fee check for this call by setting the base fee to zero
        env.block.basefee = U256::ZERO;
    }

    fn fill_cfg_env(&self, cfg_env: &mut CfgEnvWithHandlerCfg, header: &Header) {
        let spec_id = revm_spec(&self.chain_spec, header);

        cfg_env.chain_id = self.chain_spec.chain().id();
        cfg_env.perf_analyse_created_bytecodes = AnalysisKind::Analyse;

        cfg_env.handler_cfg.spec_id = spec_id;
        cfg_env.handler_cfg.is_optimism = true;
    }

    fn fill_block_env(&self, block_env: &mut BlockEnv, header: &Self::Header, after_merge: bool) {
        block_env.number = U256::from(header.number);
        block_env.coinbase = header.beneficiary;
        block_env.timestamp = U256::from(header.timestamp);
        if after_merge {
            block_env.prevrandao = Some(header.mix_hash);
            block_env.difficulty = U256::ZERO;
        } else {
            block_env.difficulty = header.difficulty;
            block_env.prevrandao = None;
        }
        block_env.basefee = U256::from(header.base_fee_per_gas.unwrap_or_default());
        block_env.gas_limit = U256::from(header.gas_limit);

        // EIP-4844 excess blob gas of this block, introduced in Cancun
        if let Some(excess_blob_gas) = header.excess_blob_gas {
            block_env.set_blob_excess_gas_and_price(excess_blob_gas);
        }
    }

    fn next_cfg_and_block_env(
        &self,
        parent: &Self::Header,
        attributes: NextBlockEnvAttributes,
    ) -> Result<EvmEnv, Self::Error> {
        // configure evm env based on parent block
        let cfg_env = CfgEnv::default().with_chain_id(self.chain_spec.chain().id());

        // ensure we're not missing any timestamp based hardforks
        let spec_id = revm_spec(&self.chain_spec, parent);

        // if the parent block did not have excess blob gas (i.e. it was pre-cancun), but it is
        // cancun now, we need to set the excess blob gas to the default value
        let blob_excess_gas_and_price = parent
            .next_block_excess_blob_gas()
            .or_else(|| spec_id.is_enabled_in(SpecId::CANCUN).then_some(0)) // default excess blob gas is zero
            .map(BlobExcessGasAndPrice::new);

        let block_env = BlockEnv {
            number: U256::from(parent.number + 1),
            coinbase: attributes.suggested_fee_recipient,
            timestamp: U256::from(attributes.timestamp),
            difficulty: U256::ZERO,
            prevrandao: Some(attributes.prev_randao),
            gas_limit: U256::from(parent.gas_limit),
            // calculate basefee based on parent block's gas usage
            basefee: U256::from(
                parent
                    .next_block_base_fee(
                        self.chain_spec.base_fee_params_at_timestamp(attributes.timestamp),
                    )
                    .unwrap_or_default(),
            ),
            // calculate excess gas based on parent block's blob gas usage
            blob_excess_gas_and_price,
        };

        Ok((
            CfgEnvWithHandlerCfg {
                cfg_env,
                handler_cfg: HandlerCfg { spec_id, is_optimism: true },
            },
            block_env,
        )
            .into())
    }
}

impl ConfigureEvm for TraverseEvmConfig {
    type DefaultExternalContext<'a> = ();

    fn evm<DB: Database>(&self, db: DB) -> Evm<'_, Self::DefaultExternalContext<'_>, DB> {
        EvmBuilder::default()
            .with_db(db)
            .optimism()
            // add additional precompiles
            .append_handler_register(Self::set_precompiles)
            .build()
    }

    fn evm_with_inspector<DB, I>(&self, db: DB, inspector: I) -> Evm<'_, I, DB>
    where
        DB: Database,
        I: GetInspector<DB>,
    {
        EvmBuilder::default()
            .with_db(db)
            .with_external_context(inspector)
            .optimism()
            // add additional precompiles
            .append_handler_register(Self::set_precompiles)
            .append_handler_register(inspector_handle_register)
            .build()
    }

    fn default_external_context<'a>(&self) -> Self::DefaultExternalContext<'a> {}
}

/// Determine the revm spec ID from the current block and reth chainspec.
fn revm_spec(chain_spec: &ChainSpec, header: &Header) -> reth_revm::primitives::SpecId {
    let timestamp = header.timestamp;
    let number = header.number;
    if chain_spec.fork(EthereumHardfork::Prague).active_at_timestamp_or_number(timestamp, number) {
        reth_revm::primitives::OSAKA
    } else if chain_spec.fork(OpHardfork::Granite).active_at_timestamp_or_number(timestamp, number)
    {
        reth_revm::primitives::GRANITE
    } else if chain_spec.fork(OpHardfork::Fjord).active_at_timestamp_or_number(timestamp, number) {
        reth_revm::primitives::FJORD
    } else if chain_spec.fork(OpHardfork::Ecotone).active_at_timestamp_or_number(timestamp, number)
    {
        reth_revm::primitives::ECOTONE
    } else if chain_spec.fork(OpHardfork::Canyon).active_at_timestamp_or_number(timestamp, number) {
        reth_revm::primitives::CANYON
    } else if chain_spec.fork(OpHardfork::Regolith).active_at_timestamp_or_number(timestamp, number)
    {
        reth_revm::primitives::REGOLITH
    } else if chain_spec.fork(OpHardfork::Bedrock).active_at_timestamp_or_number(timestamp, number)
    {
        reth_revm::primitives::BEDROCK
    } else if chain_spec
        .fork(EthereumHardfork::Prague)
        .active_at_timestamp_or_number(timestamp, number)
    {
        reth_revm::primitives::PRAGUE
    } else if chain_spec
        .fork(EthereumHardfork::Cancun)
        .active_at_timestamp_or_number(timestamp, number)
    {
        reth_revm::primitives::CANCUN
    } else if chain_spec
        .fork(EthereumHardfork::Shanghai)
        .active_at_timestamp_or_number(timestamp, number)
    {
        reth_revm::primitives::SHANGHAI
    } else if chain_spec
        .fork(EthereumHardfork::Paris)
        .active_at_timestamp_or_number(timestamp, number)
    {
        reth_revm::primitives::MERGE
    } else if chain_spec
        .fork(EthereumHardfork::London)
        .active_at_timestamp_or_number(timestamp, number)
    {
        reth_revm::primitives::LONDON
    } else if chain_spec
        .fork(EthereumHardfork::Berlin)
        .active_at_timestamp_or_number(timestamp, number)
    {
        reth_revm::primitives::BERLIN
    } else if chain_spec
        .fork(EthereumHardfork::Istanbul)
        .active_at_timestamp_or_number(timestamp, number)
    {
        reth_revm::primitives::ISTANBUL
    } else if chain_spec
        .fork(EthereumHardfork::Petersburg)
        .active_at_timestamp_or_number(timestamp, number)
    {
        reth_revm::primitives::PETERSBURG
    } else if chain_spec
        .fork(EthereumHardfork::Byzantium)
        .active_at_timestamp_or_number(timestamp, number)
    {
        reth_revm::primitives::BYZANTIUM
    } else if chain_spec
        .fork(EthereumHardfork::SpuriousDragon)
        .active_at_timestamp_or_number(timestamp, number)
    {
        reth_revm::primitives::SPURIOUS_DRAGON
    } else if chain_spec
        .fork(EthereumHardfork::Tangerine)
        .active_at_timestamp_or_number(timestamp, number)
    {
        reth_revm::primitives::TANGERINE
    } else if chain_spec
        .fork(EthereumHardfork::Homestead)
        .active_at_timestamp_or_number(timestamp, number)
    {
        reth_revm::primitives::HOMESTEAD
    } else if chain_spec
        .fork(EthereumHardfork::Frontier)
        .active_at_timestamp_or_number(timestamp, number)
    {
        reth_revm::primitives::FRONTIER
    } else {
        panic!(
            "invalid hardfork chainspec: expected at least one hardfork, got {:?}",
            chain_spec.hardforks
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_chainspec::{Chain, ChainSpecBuilder, EthereumHardfork};
    use reth_primitives::ForkCondition;
    use revm_primitives::{BlockEnv, CfgEnv, SpecId};

    #[test]
    fn test_fill_cfg_and_block_env() {
        let mut cfg_env = CfgEnvWithHandlerCfg::new_with_spec_id(CfgEnv::default(), SpecId::LATEST);
        let mut block_env = BlockEnv::default();
        let header = Header::default();
        let chain_spec = Arc::new(OpChainSpec::new(
            ChainSpecBuilder::default()
                .chain(Chain::optimism_mainnet())
                .genesis(Default::default())
                .with_fork(EthereumHardfork::Frontier, ForkCondition::Block(0))
                .build(),
        ));

        TraverseEvmConfig::new(chain_spec.clone()).fill_cfg_and_block_env(
            &mut cfg_env,
            &mut block_env,
            &header,
        );

        assert_eq!(cfg_env.chain_id, chain_spec.chain().id());
    }

    #[test]
    fn test_p256verify_precompile_availability() {
        let evm = EvmBuilder::default()
            .with_empty_db()
            .optimism()
            // add additional precompiles
            .append_handler_register(TraverseEvmConfig::set_precompiles)
            .build();

        // loading the precompiles from pre execution instead of the evm context directly, as they
        // are only set pre-execution in the context
        let precompiles = evm.handler.pre_execution().load_precompiles();
        assert!(precompiles.contains(&u64_to_address(0x14)));
        assert!(precompiles.contains(&u64_to_address(0x100)));
    }
}
