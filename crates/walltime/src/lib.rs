//! # Traverse walltime
//!
//! Returns the current walltime and the chain's tip timestamps.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use alloy_consensus::BlockHeader;
use futures::{Stream, StreamExt};
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
    types::{error::INTERNAL_ERROR_CODE, ErrorObject},
};
use reth_chain_state::CanonStateNotification;
use reth_node_api::NodePrimitives;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;

/// The traverse walltime endpoint.
#[derive(Debug, Clone)]
pub struct TraverseWallTime {
    inner: Arc<TraverseWallTimeInner>,
}

impl TraverseWallTime {
    /// Creates a new instance with the connected stream.
    pub fn spawn<St, N>(mut st: St) -> Self
    where
        St: Stream<Item = CanonStateNotification<N>> + Send + Unpin + 'static,
        N: NodePrimitives,
    {
        let walltime = Self { inner: Default::default() };
        let listener = walltime.clone();
        tokio::task::spawn(async move {
            while let Some(notification) = st.next().await {
                let tip = BlockTimeData {
                    wall_time_ms: unix_epoch_ms(),
                    block_timestamp: notification.tip().header().timestamp(),
                };
                *listener.inner.block_time_data.write().await = Some(tip);
            }
        });
        walltime
    }

    /// Returns the currently tracked [`BlockTimeData`] if any.
    async fn current_block_time(&self) -> Option<BlockTimeData> {
        *self.inner.block_time_data.read().await
    }
}

/// Implementation of the Traverse `traverse_getWallTimeData` endpoint.
#[derive(Debug, Default)]
struct TraverseWallTimeInner {
    /// Tracks the recent blocktime data
    block_time_data: RwLock<Option<BlockTimeData>>,
}

/// Data about the current time and the last block's.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WallTimeData {
    /// Wall time right now
    current_wall_time_ms: u64,
    /// Wall time of last block
    last_block_wall_time_ms: u64,
    /// Timestamp of last block (chain time)
    last_block_timestamp: u64,
}

/// Rpc endpoints
#[cfg_attr(not(test), rpc(server, namespace = "traverse"))]
#[cfg_attr(test, rpc(server, client, namespace = "traverse"))]
pub trait TraverseWallTimeRpcApi {
    /// Return the wall time and block timestamp of the latest block.
    #[method(name = "getWallTimeData")]
    async fn get_timedata(&self) -> RpcResult<WallTimeData>;
}

#[async_trait]
impl TraverseWallTimeRpcApiServer for TraverseWallTime {
    async fn get_timedata(&self) -> RpcResult<WallTimeData> {
        let Some(current) = self.current_block_time().await else {
            return Err(ErrorObject::owned(INTERNAL_ERROR_CODE, "node is not synced", None::<()>));
        };
        Ok(WallTimeData {
            current_wall_time_ms: unix_epoch_ms(),
            last_block_wall_time_ms: current.wall_time_ms,
            last_block_timestamp: current.block_timestamp,
        })
    }
}

/// Time data about the last block.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BlockTimeData {
    /// Wall time of last block
    wall_time_ms: u64,
    /// Timestamp of last block (chain time)
    block_timestamp: u64,
}

/// Returns the current unix epoch in milliseconds.
pub fn unix_epoch_ms() -> u64 {
    use std::time::SystemTime;
    let now = SystemTime::now();
    now.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_else(|err| panic!("Current time {now:?} is invalid: {err:?}"))
        .as_millis() as u64
}
