# Traverse

## What is Traverse?

Traverse is a testnet OP Stack rollup aimed at enabling experimentation of bleeding edge Ethereum Research.
It is not a fork of reth.
Traverse implements traits provided by the [reth node builder API](https://paradigmxyz.github.io/reth/docs/reth_node_builder/index.html), allowing implementation of precompiles and instructions of experimental EIPs without forking the node.

### Traverse Local Development

Traverse can be run locally for development and testing purposes. To do this, the binary can be run with the `--dev` flag, which will start the node with a development configuration.

First, traverse should be built locally:

```bash
git clone https://github.com/0xjingle/traverse
cd traverse
cargo install --path bin/traverse
```

```bash
traverse node --chain genesis.json --dev --http --http.api all
```

This will start the node with a development configuration, and expose the HTTP API on `http://localhost:8545`.

To use EOF-enabled foundry, use [forge-eof](https://github.com/paradigmxyz/forge-eof) and follow installation instructions.

### Running Traverse

Running Traverse will require running additional infrastructure for the archival L1 node. These instructions are a guide for
running the Traverse OP-stack node only.

For instructions on running the full Traverse OP stack, including the L1 node, see the [Reth book section on running the OP stack](https://paradigmxyz.github.io/reth/run/optimism.html), using the `traverse` binary instead of `op-reth`.

#### Running the Traverse execution node

To run Traverse from source, clone the repository and run the following commands:

```bash
git clone https://github.com/0xjingle/traverse.git
cd traverse
cargo install --path bin/traverse
traverse node \
    --chain genesis.json \
    --rollup.sequencer-http <rollup-sequencer-http> \
    --http \
    --ws \
    --authrpc.port 9551 \
    --authrpc.jwtsecret /path/to/jwt.hex
```

#### Running op-node with the Traverse configuration

Once `traverse` is started, [`op-node`](https://github.com/ethereum-optimism/optimism/tree/develop/op-node) can be run with the
included `traverse-rollup.json`:

```bash
cd traverse/
op-node \
    --rollup.config rollup.json \
    --l1=<your-sepolia-L1-rpc> \
    --l2=http://localhost:9551 \
    --l2.jwt-secret=/path/to/jwt.hex \
    --rpc.addr=0.0.0.0 \
    --rpc.port=7000 \
    --l1.trustrpc
```
