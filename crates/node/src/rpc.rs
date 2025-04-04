//! Traverse rpc logic.
//!
//! `eth_` namespace overrides:
//!
//! - `eth_getProof` will _ONLY_ return the storage proofs _WITHOUT_ an account proof _IF_ targeting
//!   the withdrawal contract. Otherwise, it fallbacks to default behaviour.

use alloy_eips::BlockId;
use alloy_primitives::{Address, B256};
use alloy_rpc_types::serde_helpers::JsonStorageKey;
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
};
use traverse_common::WITHDRAWAL_CONTRACT;
use reth_errors::RethError;
use reth_rpc_eth_api::{
    helpers::{EthState, FullEthApi},
    FromEthApiError,
};
use reth_rpc_eth_types::EthApiError;
use reth_trie_common::AccountProof;
use tracing::trace;

/// Traverse `eth_` RPC namespace overrides.
#[cfg_attr(not(test), rpc(server, namespace = "eth"))]
#[cfg_attr(test, rpc(server, client, namespace = "eth"))]
pub trait EthApiOverride {
    /// Returns the account and storage values of the specified account including the Merkle-proof.
    /// This call can be used to verify that the data you are pulling from is not tampered with.
    #[method(name = "getProof")]
    async fn get_proof(
        &self,
        address: Address,
        keys: Vec<JsonStorageKey>,
        block_number: Option<BlockId>,
    ) -> RpcResult<EIP1186AccountProofResponse>;
}

/// Implementation of the `eth_` namespace override
#[derive(Debug)]
pub struct EthApiExt<Eth> {
    eth_api: Eth,
}

impl<E> EthApiExt<E> {
    /// Create a new `EthApiExt` module.
    pub const fn new(eth_api: E) -> Self {
        Self { eth_api }
    }
}

#[async_trait]
impl<Eth> EthApiOverrideServer for EthApiExt<Eth>
where
    Eth: FullEthApi + Send + Sync + 'static,
{
    async fn get_proof(
        &self,
        address: Address,
        keys: Vec<JsonStorageKey>,
        block_number: Option<BlockId>,
    ) -> RpcResult<EIP1186AccountProofResponse> {
        trace!(target: "rpc::eth", ?address, ?keys, ?block_number, "Serving eth_getProof");

        // If we are targeting the withdrawal contract, then we only need to provide the storage
        // proofs for withdrawal.
        if address == WITHDRAWAL_CONTRACT {
            let _permit = self
                .eth_api
                .acquire_owned()
                .await
                .map_err(RethError::other)
                .map_err(EthApiError::Internal)?;

            return self
                .eth_api
                .spawn_blocking_io(move |this| {
                    let b256_keys: Vec<B256> = keys.iter().map(|k| k.as_b256()).collect();
                    let state = this.state_at_block_id(block_number.unwrap_or_default())?;

                    let proofs = state
                        .storage_multiproof(WITHDRAWAL_CONTRACT, &b256_keys, Default::default())
                        .map_err(EthApiError::from_eth_err)?;

                    let account_proof = AccountProof {
                        address,
                        storage_root: proofs.root,
                        storage_proofs: b256_keys
                            .into_iter()
                            .map(|k| proofs.storage_proof(k))
                            .collect::<Result<_, _>>()
                            .map_err(RethError::other)
                            .map_err(EthApiError::Internal)?,
                        ..Default::default()
                    };
                    Ok(account_proof.into_eip1186_response(keys))
                })
                .await
                .map_err(Into::into);
        }

        EthState::get_proof(&self.eth_api, address, keys, block_number)
            .map_err(Into::into)?
            .await
            .map_err(Into::into)
    }
}
