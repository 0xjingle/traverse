//! # Traverse wallet.
//!
//! Implementations of a custom `wallet_` namespace for Traverse experiment 1.
//!
//! - `traverse_sendTransaction` that can perform service-sponsored [EIP-7702][eip-7702] delegations
//!   and send other service-sponsored transactions on behalf of EOAs with delegated code.
//!
//! # Restrictions
//!
//! `traverse_sendTransaction` has additional verifications in place to prevent some
//! rudimentary abuse of the service's funds. For example, transactions cannot contain any
//! `value`.
//!
//! [eip-5792]: https://eips.ethereum.org/EIPS/eip-5792
//! [eip-7702]: https://eips.ethereum.org/EIPS/eip-7702

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use alloy_network::{
    eip2718::Encodable2718, Ethereum, EthereumWallet, NetworkWallet, TransactionBuilder,
};
use alloy_primitives::{Address, Bytes, ChainId, TxHash, TxKind, U256};
use alloy_provider::{utils::Eip1559Estimation, Provider, WalletProvider};
use alloy_rpc_types::{BlockId, TransactionRequest};
use alloy_transport::Transport;
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
};
use metrics::Counter;
use metrics_derive::Metrics;

use reth_rpc_eth_api::helpers::{EthCall, EthTransactions, FullEthApi, LoadFee, LoadState};
use reth_storage_api::StateProviderFactory;
use serde::{Deserialize, Serialize};
use std::{marker::PhantomData, sync::Arc};
use tracing::{trace, warn};

use reth_optimism_primitives as _;
use reth_optimism_rpc as _;
use tokio::sync::Mutex;

/// An upstream is capable of estimating, signing, and propagating signed transactions for a
/// specific chain.
#[async_trait]
pub trait Upstream {
    /// Get the address of the account that sponsors transactions.
    fn default_signer_address(&self) -> Address;

    /// Get the code at a specific address.
    async fn get_code(&self, address: Address) -> Result<Bytes, TraverseWalletError>;

    /// Estimate the transaction request's gas usage and fees.
    async fn estimate(
        &self,
        tx: &TransactionRequest,
    ) -> Result<(u64, Eip1559Estimation), TraverseWalletError>;

    /// Sign the transaction request and send it to the upstream.
    async fn sign_and_send(&self, tx: TransactionRequest) -> Result<TxHash, TraverseWalletError>;
}

/// A wrapper around an Alloy provider for signing and sending sponsored transactions.
#[derive(Debug)]
pub struct AlloyUpstream<P, T> {
    provider: P,
    _transport: PhantomData<T>,
}

impl<P, T> AlloyUpstream<P, T> {
    /// Create a new [`AlloyUpstream`]
    pub const fn new(provider: P) -> Self {
        Self { provider, _transport: PhantomData }
    }
}

#[async_trait]
impl<P, T> Upstream for AlloyUpstream<P, T>
where
    P: Provider<T> + WalletProvider,
    T: Transport + Clone,
{
    fn default_signer_address(&self) -> Address {
        self.provider.default_signer_address()
    }

    async fn get_code(&self, address: Address) -> Result<Bytes, TraverseWalletError> {
        self.provider
            .get_code_at(address)
            .await
            .map_err(|err| TraverseWalletError::InternalError(err.into()))
    }

    async fn estimate(
        &self,
        tx: &TransactionRequest,
    ) -> Result<(u64, Eip1559Estimation), TraverseWalletError> {
        let (estimate, fee_estimate) =
            tokio::join!(self.provider.estimate_gas(tx), self.provider.estimate_eip1559_fees(None));

        Ok((
            estimate.map_err(|err| TraverseWalletError::InternalError(err.into()))?,
            fee_estimate.map_err(|err| TraverseWalletError::InternalError(err.into()))?,
        ))
    }

    async fn sign_and_send(&self, tx: TransactionRequest) -> Result<TxHash, TraverseWalletError> {
        self.provider
            .send_transaction(tx)
            .await
            .map_err(|err| TraverseWalletError::InternalError(err.into()))
            .map(|pending| *pending.tx_hash())
    }
}

/// A handle to a Reth upstream that signs transactions and injects them directly into the
/// transaction pool.
#[derive(Debug)]
pub struct RethUpstream<Provider, Eth> {
    provider: Provider,
    eth_api: Eth,
    wallet: EthereumWallet,
}

impl<Provider, Eth> RethUpstream<Provider, Eth> {
    /// Create a new [`RethUpstream`].
    pub const fn new(provider: Provider, eth_api: Eth, wallet: EthereumWallet) -> Self {
        Self { provider, eth_api, wallet }
    }
}

#[async_trait]
impl<Provider, Eth> Upstream for RethUpstream<Provider, Eth>
where
    Provider: StateProviderFactory + Send + Sync,
    Eth: FullEthApi + Send + Sync,
{
    fn default_signer_address(&self) -> Address {
        NetworkWallet::<Ethereum>::default_signer_address(&self.wallet)
    }

    async fn get_code(&self, address: Address) -> Result<Bytes, TraverseWalletError> {
        let state =
            self.provider.latest().map_err(|err| TraverseWalletError::InternalError(err.into()))?;

        Ok(state
            .account_code(&address)
            .ok()
            .flatten()
            .map(|code| code.0.bytes())
            .unwrap_or_default())
    }

    async fn estimate(
        &self,
        tx: &TransactionRequest,
    ) -> Result<(u64, Eip1559Estimation), TraverseWalletError> {
        let (estimate, fee_estimate) = tokio::join!(
            EthCall::estimate_gas_at(&self.eth_api, tx.clone(), BlockId::latest(), None),
            LoadFee::eip1559_fees(&self.eth_api, None, None)
        );

        Ok((
            estimate
                .map(|estimate| estimate.to())
                .map_err(|err| TraverseWalletError::InternalError(eyre::Report::new(err)))?,
            fee_estimate
                .map(|(base, prio)| Eip1559Estimation {
                    max_fee_per_gas: (base + prio).to(),
                    max_priority_fee_per_gas: prio.to(),
                })
                .map_err(|err| TraverseWalletError::InternalError(eyre::Report::new(err)))?,
        ))
    }

    async fn sign_and_send(
        &self,
        mut tx: TransactionRequest,
    ) -> Result<TxHash, TraverseWalletError> {
        let next_nonce = LoadState::next_available_nonce(
            &self.eth_api,
            NetworkWallet::<Ethereum>::default_signer_address(&self.wallet),
        )
        .await
        .map_err(|err| TraverseWalletError::InternalError(eyre::Report::new(err)))?;
        tx.nonce = Some(next_nonce);

        // build and sign
        let envelope =
            <TransactionRequest as TransactionBuilder<Ethereum>>::build::<EthereumWallet>(
                tx,
                &self.wallet,
            )
            .await
            .map_err(|err| TraverseWalletError::InternalError(err.into()))?;

        // this uses the internal `OpEthApi` to either forward the tx to the sequencer, or add it to
        // the txpool
        //
        // see: https://github.com/paradigmxyz/reth/blob/b67f004fbe8e1b7c05f84f314c4c9f2ed9be1891/crates/optimism/rpc/src/eth/transaction.rs#L35-L57
        EthTransactions::send_raw_transaction(&self.eth_api, envelope.encoded_2718().into())
            .await
            .map_err(|err| TraverseWalletError::InternalError(eyre::Report::new(err)))
    }
}

/// The capability to perform [EIP-7702][eip-7702] delegations, sponsored by the service.
///
/// The service will only perform delegations, and act on behalf of delegated accounts, if the
/// account delegates to one of the addresses specified within this capability.
///
/// [eip-7702]: https://eips.ethereum.org/EIPS/eip-7702
#[derive(Debug, Clone, Eq, PartialEq, Deserialize, Serialize)]
pub struct DelegationCapability {
    /// A list of valid delegation contracts.
    pub addresses: Vec<Address>,
}

/// Traverse `wallet_` RPC namespace.
#[cfg_attr(not(test), rpc(server, namespace = "wallet"))]
#[cfg_attr(test, rpc(server, client, namespace = "wallet"))]
pub trait TraverseWalletApi {
    /// Send a sponsored transaction.
    ///
    /// The transaction will only be processed if:
    ///
    /// - The transaction is an [EIP-7702][eip-7702] transaction.
    /// - The transaction is an [EIP-1559][eip-1559] transaction to an EOA that is currently
    ///   delegated to one of the addresses above
    /// - The value in the transaction is exactly 0.
    ///
    /// The service will sign the transaction and inject it into the transaction pool, provided it
    /// is valid. The nonce is managed by the service.
    ///
    /// [eip-7702]: https://eips.ethereum.org/EIPS/eip-7702
    /// [eip-1559]: https://eips.ethereum.org/EIPS/eip-1559
    #[method(name = "sendTransaction", aliases = ["traverse_sendTransaction"])]
    async fn send_transaction(&self, request: TransactionRequest) -> RpcResult<TxHash>;
}

/// Errors returned by the wallet API.
#[derive(Debug, thiserror::Error)]
pub enum TraverseWalletError {
    /// The transaction value is not 0.
    ///
    /// The value should be 0 to prevent draining the service.
    #[error("tx value not zero")]
    ValueNotZero,
    /// The from field is set on the transaction.
    ///
    /// Requests with the from field are rejected, since it is implied that it will always be the
    /// service.
    #[error("tx from field is set")]
    FromSet,
    /// The nonce field is set on the transaction.
    ///
    /// Requests with the nonce field set are rejected, as this is managed by the service.
    #[error("tx nonce is set")]
    NonceSet,
    /// The to field of the transaction was invalid.
    ///
    /// The destination is invalid if:
    ///
    /// - There is no bytecode at the destination, or
    /// - The bytecode is not an EIP-7702 delegation designator
    #[error("the destination of the transaction is not a delegated account")]
    IllegalDestination,
    /// The transaction request was invalid.
    ///
    /// This is likely an internal error, as most of the request is built by the service.
    #[error("invalid tx request")]
    InvalidTransactionRequest,
    /// The request was estimated to consume too much gas.
    ///
    /// The gas usage by each request is limited to counteract draining the services funds.
    #[error("request would use too much gas: estimated {estimate}")]
    GasEstimateTooHigh {
        /// The amount of gas the request was estimated to consume.
        estimate: u64,
    },
    /// An internal error occurred.
    #[error(transparent)]
    InternalError(#[from] eyre::Error),
}

impl From<TraverseWalletError> for jsonrpsee::types::error::ErrorObject<'static> {
    fn from(error: TraverseWalletError) -> Self {
        jsonrpsee::types::error::ErrorObject::owned::<()>(
            jsonrpsee::types::error::INVALID_PARAMS_CODE,
            error.to_string(),
            None,
        )
    }
}

/// Implementation of the Traverse `wallet_` namespace.
#[derive(Debug)]
pub struct TraverseWallet<T> {
    inner: Arc<TraverseWalletInner<T>>,
}

impl<T> TraverseWallet<T> {
    /// Create a new Traverse wallet module.
    pub fn new(upstream: T, chain_id: ChainId) -> Self {
        let inner = TraverseWalletInner {
            upstream,
            chain_id,
            permit: Default::default(),
            metrics: WalletMetrics::default(),
        };
        Self { inner: Arc::new(inner) }
    }

    #[allow(clippy::missing_const_for_fn)]
    fn chain_id(&self) -> ChainId {
        self.inner.chain_id
    }
}

#[async_trait]
impl<T> TraverseWalletApiServer for TraverseWallet<T>
where
    T: Upstream + Sync + Send + 'static,
{
    async fn send_transaction(&self, mut request: TransactionRequest) -> RpcResult<TxHash> {
        trace!(target: "rpc::wallet", ?request, "Serving traverse_sendTransaction");

        // validate fields common to eip-7702 and eip-1559
        if let Err(err) = validate_tx_request(&request) {
            self.inner.metrics.invalid_send_transaction_calls.increment(1);
            return Err(err.into());
        }

        // validate destination
        match (request.authorization_list.is_some(), request.to) {
            // if this is an eip-1559 tx, ensure that it is an account that delegates to a
            // whitelisted address
            (false, Some(TxKind::Call(addr))) => {
                let code = self.inner.upstream.get_code(addr).await?;
                match code.as_ref() {
                    // A valid EIP-7702 delegation
                    [0xef, 0x01, 0x00, address @ ..] => {
                        let addr = Address::from_slice(address);
                        // the delegation was cleared
                        if addr.is_zero() {
                            self.inner.metrics.invalid_send_transaction_calls.increment(1);
                            return Err(TraverseWalletError::IllegalDestination.into());
                        }
                    }
                    // Not an EIP-7702 delegation, or an empty (cleared) delegation
                    _ => {
                        self.inner.metrics.invalid_send_transaction_calls.increment(1);
                        return Err(TraverseWalletError::IllegalDestination.into());
                    }
                }
            }
            // if it's an eip-7702 tx, let it through
            (true, _) => (),
            // create tx's disallowed
            _ => {
                self.inner.metrics.invalid_send_transaction_calls.increment(1);
                return Err(TraverseWalletError::IllegalDestination.into());
            }
        }

        // we acquire the permit here so that all following operations are performed exclusively
        let _permit = self.inner.permit.lock().await;

        // set chain id
        request.chain_id = Some(self.chain_id());

        // set gas limit
        // note: we also set the `from` field here to correctly estimate for contracts that use e.g.
        // `tx.origin`
        request.from = Some(self.inner.upstream.default_signer_address());
        let (estimate, fee_estimate) = self
            .inner
            .upstream
            .estimate(&request)
            .await
            .inspect_err(|_| self.inner.metrics.invalid_send_transaction_calls.increment(1))?;
        if estimate >= 350_000 {
            self.inner.metrics.invalid_send_transaction_calls.increment(1);
            return Err(TraverseWalletError::GasEstimateTooHigh { estimate }.into());
        }
        request.gas = Some(estimate);

        // set gas price
        request.max_fee_per_gas = Some(fee_estimate.max_fee_per_gas);
        request.max_priority_fee_per_gas = Some(fee_estimate.max_priority_fee_per_gas);
        request.gas_price = None;

        // all checks passed, increment the valid calls counter
        self.inner.metrics.valid_send_transaction_calls.increment(1);

        Ok(self.inner.upstream.sign_and_send(request).await.inspect_err(
            |err| warn!(target: "rpc::wallet", ?err, "Error adding sponsored tx to pool"),
        )?)
    }
}

/// Implementation of the Traverse `wallet_` namespace.
#[derive(Debug)]
struct TraverseWalletInner<T> {
    upstream: T,
    chain_id: ChainId,
    /// Used to guard tx signing
    permit: Mutex<()>,
    /// Metrics for the `wallet_` RPC namespace.
    metrics: WalletMetrics,
}

fn validate_tx_request(request: &TransactionRequest) -> Result<(), TraverseWalletError> {
    // reject transactions that have a non-zero value to prevent draining the service.
    if request.value.is_some_and(|val| val > U256::ZERO) {
        return Err(TraverseWalletError::ValueNotZero);
    }

    // reject transactions that have from set, as this will be the service.
    if request.from.is_some() {
        return Err(TraverseWalletError::FromSet);
    }

    // reject transaction requests that have nonce set, as this is managed by the service.
    if request.nonce.is_some() {
        return Err(TraverseWalletError::NonceSet);
    }

    Ok(())
}

/// Metrics for the `wallet_` RPC namespace.
#[derive(Metrics)]
#[metrics(scope = "wallet")]
struct WalletMetrics {
    /// Number of invalid calls to `traverse_sendTransaction`
    invalid_send_transaction_calls: Counter,
    /// Number of valid calls to `traverse_sendTransaction`
    valid_send_transaction_calls: Counter,
}

#[cfg(test)]
mod tests {
    use crate::{validate_tx_request, TraverseWalletError};
    use alloy_primitives::{Address, U256};
    use alloy_rpc_types::TransactionRequest;

    #[test]
    fn no_value_allowed() {
        assert!(matches!(
            validate_tx_request(&TransactionRequest::default().value(U256::from(1))),
            Err(TraverseWalletError::ValueNotZero)
        ));

        assert!(matches!(
            validate_tx_request(&TransactionRequest::default().value(U256::from(0))),
            Ok(())
        ));
    }

    #[test]
    fn no_from_allowed() {
        assert!(matches!(
            validate_tx_request(&TransactionRequest::default().from(Address::ZERO)),
            Err(TraverseWalletError::FromSet)
        ));

        assert!(matches!(validate_tx_request(&TransactionRequest::default()), Ok(())));
    }

    #[test]
    fn no_nonce_allowed() {
        assert!(matches!(
            validate_tx_request(&TransactionRequest::default().nonce(1)),
            Err(TraverseWalletError::NonceSet)
        ));

        assert!(matches!(validate_tx_request(&TransactionRequest::default()), Ok(())));
    }
}
