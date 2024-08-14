//! Standalone crate for Traverse's node configuration and builder types.
//!
//! This contains mainly two types, [`TraverseNode`](node::TraverseNode) and
//! [`TraverseEvmConfig`](evm::TraverseEvmConfig).
//!
//! The [`TraverseNode`](node::TraverseNode) type implements the
//! [`NodeTypes`](reth_node_builder::NodeTypes) trait, and configures the engine types required for
//! the optimism engine API.
//!
//! The [`TraverseEvmConfig`](evm::TraverseEvmConfig) type implements the
//! [`ConfigureEvm`](reth_node_api::ConfigureEvm) and
//! [`ConfigureEvmEnv`](reth_node_api::ConfigureEvmEnv) traits, configuring the custom Traverse
//! precompiles and instructions.

#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![warn(unused_crate_dependencies)]

pub mod chainspec;
pub mod evm;
pub mod node;
pub mod rpc;
