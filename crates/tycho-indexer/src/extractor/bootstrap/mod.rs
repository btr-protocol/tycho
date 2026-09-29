//! Starting an extractor from a state snapshot at a finalized block instead of replaying its
//! package from the protocol's deployment.
//!
//! On a first start, the snapshot source enumerates the protocol's components and reads their
//! state at block `N` over RPC. The extractor processes the result as block `N`, so it goes
//! through the normal token loading, buffering and DB write paths, and the stream then starts at
//! `N + 1`. Once the extractor reaches `N + verify_after`, it compares the state it indexed for a
//! sample of components with fresh on-chain reads and fails on any difference.

use std::{collections::HashMap, sync::Arc};

use alloy::rpc::types::BlockId;
use async_trait::async_trait;
use num_bigint::{BigInt, Sign};
use prost::Message;
use serde::Deserialize;
use tycho_common::{
    models::{
        protocol::{ProtocolComponent, ProtocolComponentState},
        Chain, ComponentId, ProtocolType,
    },
    Bytes,
};
use tycho_ethereum::rpc::{config::RPCRetryConfig, EthereumRpcClient};
use tycho_protobuf::{convert::TryFromMessage, pb::tycho::evm::v1 as pb};

use crate::{
    extractor::{
        bootstrap::logs::{HyperSyncLogSource, LogSource, RpcLogSource},
        ExtractionError, Extractor,
    },
    pb::sf::substreams::{
        rpc::v2::{BlockScopedData, MapModuleOutput},
        v1::{module::input as module_input, Clock, Package},
    },
};

pub mod logs;
pub mod uniswap_v3;

/// Reads a protocol's components and their absolute state from the chain.
#[async_trait]
pub trait SnapshotSource: Send + Sync {
    /// Returns the components created in blocks `from..=to` (the extractor's `start_block` up to
    /// the bootstrap block), in the form the protocol's package
    /// emits them.
    async fn components(
        &self,
        from: u64,
        to: u64,
    ) -> Result<Vec<pb::ProtocolComponent>, ExtractionError>;

    /// Returns the module parameters that let the protocol's package stream from `block + 1`
    /// without processing earlier blocks, given the snapshot at `block`. A package whose modules
    /// hold no state built from history needs none.
    fn stream_params(
        &self,
        block: u64,
        components: &[ProtocolComponent],
        states: &[ProtocolComponentState],
    ) -> Result<HashMap<String, String>, ExtractionError>;

    /// Returns the attributes and balances of `components` at `block`, as the protocol's package
    /// would have accumulated them by then.
    async fn state(
        &self,
        components: &[ProtocolComponent],
        block: BlockId,
    ) -> Result<Vec<ProtocolComponentState>, ExtractionError>;
}

/// Extractor option to start from a state snapshot.
#[derive(Debug, Deserialize, Clone)]
pub struct BootstrapConfig {
    /// Finalized block whose state seeds the extractor. The stream starts at the next block.
    pub block: u64,
    /// RPC endpoint for the snapshot reads. Defaults to the indexer's RPC.
    #[serde(default)]
    pub rpc_url: Option<String>,
    /// HyperSync endpoint for component discovery, used when `HYPERSYNC_API_KEY` is set.
    /// Otherwise components come from `eth_getLogs`.
    #[serde(default)]
    pub hypersync_url: Option<String>,
    /// Blocks per `eth_getLogs` request.
    #[serde(default = "default_logs_block_range")]
    pub logs_block_range: u64,
    /// Calls per Multicall3 request.
    #[serde(default = "default_calls_per_request")]
    pub calls_per_request: usize,
    /// RPC requests kept in flight.
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    /// Blocks streamed past `block` before the indexed state is verified against the chain.
    #[serde(default = "default_verify_after")]
    pub verify_after: u64,
    /// Number of components the verification compares.
    #[serde(default = "default_verify_sample")]
    pub verify_sample: usize,
    pub source: SnapshotSourceConfig,
}

fn default_logs_block_range() -> u64 {
    10_000
}

fn default_calls_per_request() -> usize {
    200
}

fn default_concurrency() -> usize {
    4
}

fn default_verify_after() -> u64 {
    100
}

fn default_verify_sample() -> usize {
    16
}

/// The snapshot source of an extractor's protocol.
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SnapshotSourceConfig {
    UniswapV3(uniswap_v3::UniswapV3Config),
}

impl BootstrapConfig {
    /// Builds the configured snapshot source, reading over `rpc_url` when set and over
    /// `default_rpc` otherwise.
    pub fn build_source(
        &self,
        default_rpc: &EthereumRpcClient,
    ) -> Result<Arc<dyn SnapshotSource>, ExtractionError> {
        let rpc = match &self.rpc_url {
            Some(url) => EthereumRpcClient::new(url)
                .map_err(|e| ExtractionError::Setup(format!("Invalid bootstrap RPC URL: {e}")))?,
            None => default_rpc.clone(),
        }
        .with_retry(RPCRetryConfig::new(8, 500, 30_000));
        let logs: Box<dyn LogSource> =
            match (&self.hypersync_url, std::env::var("HYPERSYNC_API_KEY")) {
                (Some(url), Ok(key)) if !key.is_empty() => {
                    Box::new(HyperSyncLogSource::new(url, key))
                }
                _ => Box::new(RpcLogSource::new(
                    rpc.clone(),
                    self.logs_block_range,
                    self.concurrency,
                )),
            };
        match &self.source {
            SnapshotSourceConfig::UniswapV3(config) => {
                Ok(Arc::new(uniswap_v3::UniswapV3Source::new(
                    config.clone(),
                    rpc,
                    logs,
                    self.calls_per_request,
                    self.concurrency,
                )))
            }
        }
    }
}

/// Seeds `extractor` with the state `source` reads at `block`, as one block-`block` message.
///
/// Components come from the blocks `from..=block`. The snapshot is attributed to a single
/// transaction whose hash is the block hash, since no real transaction produced it.
#[allow(clippy::too_many_arguments)]
pub async fn bootstrap(
    extractor: &dyn Extractor,
    source: &dyn SnapshotSource,
    rpc: &EthereumRpcClient,
    chain: Chain,
    protocol_system: &str,
    protocol_types: &HashMap<String, ProtocolType>,
    from: u64,
    block: u64,
) -> Result<(Vec<ProtocolComponent>, Vec<ProtocolComponentState>), ExtractionError> {
    let header = rpc
        .eth_get_block_by_number(block.into())
        .await
        .map_err(|e| ExtractionError::Setup(format!("Failed to read block {block}: {e}")))?
        .header;
    let pb_block = pb::Block {
        hash: header.hash.to_vec(),
        parent_hash: header.parent_hash.to_vec(),
        number: block,
        ts: header.timestamp,
    };
    let tx = pb::Transaction {
        hash: header.hash.to_vec(),
        from: vec![0; 20],
        to: vec![0; 20],
        index: 0,
    };

    if from > block {
        return Err(ExtractionError::Setup(format!(
            "Bootstrap block {block} is before the extractor's start_block {from}"
        )));
    }
    let pb_components = source.components(from, block).await?;
    if pb_components.is_empty() {
        return Err(ExtractionError::Setup(format!(
            "No components created in blocks {from}..={block}; check the snapshot source config"
        )));
    }
    let model_block =
        tycho_common::models::blockchain::Block::try_from_message((pb_block.clone(), chain))?;
    let components = pb_components
        .iter()
        .map(|c| {
            ProtocolComponent::try_from_message((
                c.clone(),
                chain,
                protocol_system,
                protocol_types,
                Bytes::from(tx.hash.clone()),
                model_block.ts,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    tracing::info!(block, components = components.len(), "Reading snapshot state");
    let states = source
        .state(&components, block.into())
        .await?;

    let changes = pb::TransactionChanges {
        tx: Some(tx),
        component_changes: pb_components,
        entity_changes: states
            .iter()
            .map(|state| pb::EntityChanges {
                component_id: state.component_id.clone(),
                attributes: state
                    .attributes
                    .iter()
                    .map(|(name, value)| pb::Attribute {
                        name: name.clone(),
                        value: value.to_vec(),
                        change: pb::ChangeType::Creation.into(),
                    })
                    .collect(),
            })
            .collect(),
        balance_changes: states
            .iter()
            .flat_map(|state| {
                state
                    .balances
                    .iter()
                    .map(|(token, balance)| pb::BalanceChange {
                        token: token.to_vec(),
                        balance: balance.to_vec(),
                        component_id: state.component_id.as_bytes().to_vec(),
                        ..Default::default()
                    })
            })
            .collect(),
        ..Default::default()
    };
    let msg =
        pb::BlockChanges { block: Some(pb_block), changes: vec![changes], ..Default::default() };
    extractor
        .handle_tick_scoped_data(BlockScopedData {
            output: Some(MapModuleOutput {
                name: "snapshot".to_string(),
                map_output: Some(prost_types::Any {
                    type_url: "type.googleapis.com/tycho.evm.v1.BlockChanges".to_string(),
                    value: msg.encode_to_vec(),
                }),
                debug_info: None,
            }),
            clock: Some(Clock {
                id: hex::encode(header.hash),
                number: block,
                timestamp: Some(prost_types::Timestamp {
                    seconds: header.timestamp as i64,
                    nanos: 0,
                }),
            }),
            cursor: String::new(),
            final_block_height: block,
            ..Default::default()
        })
        .await?;
    tracing::info!(block, "Snapshot written; streaming from the next block");
    Ok((components, states))
}

/// Makes `package` stream from `block + 1` on top of a snapshot at `block`: every module starts
/// at `block + 1`, so no store replays earlier blocks, and each module named in `params` gets that
/// value as its `params` input.
///
/// Errors if a module named in `params` is missing or takes no parameters.
pub fn start_package_after(
    package: &mut Package,
    block: u64,
    params: &HashMap<String, String>,
) -> Result<(), ExtractionError> {
    let modules = package
        .modules
        .as_mut()
        .ok_or_else(|| ExtractionError::Setup("Package has no modules".to_string()))?;
    for module in &mut modules.modules {
        module.initial_block = module.initial_block.max(block + 1);
    }
    for (name, value) in params {
        let input = modules
            .modules
            .iter_mut()
            .find(|module| &module.name == name)
            .and_then(|module| {
                module
                    .inputs
                    .iter_mut()
                    .find_map(|input| match &mut input.input {
                        Some(module_input::Input::Params(params)) => Some(params),
                        _ => None,
                    })
            })
            .ok_or_else(|| {
                ExtractionError::Setup(format!("Package module {name} takes no parameters"))
            })?;
        input.value = value.clone();
    }
    Ok(())
}

/// Largest shortfall of an indexed balance below `balanceOf` that verification accepts.
const BALANCE_DUST: u64 = 1_000;

/// A pending comparison of indexed state with on-chain state at `block`.
#[derive(Clone)]
pub struct SnapshotCheck {
    pub block: u64,
    pub sample: usize,
    pub source: Arc<dyn SnapshotSource>,
}

/// Compares `expected` on-chain state with `indexed` state and describes every difference.
///
/// An attribute present on one side only must be zero on the other: additive attributes, such
/// as tick net liquidity, drop to zero instead of disappearing.
pub fn compare_states(
    expected: &[ProtocolComponentState],
    indexed: &HashMap<ComponentId, ProtocolComponentState>,
) -> Vec<String> {
    let int = |value: Option<&Bytes>| {
        value.map_or_else(BigInt::default, |v| BigInt::from_signed_bytes_be(v))
    };
    let uint = |value: Option<&Bytes>| {
        value.map_or_else(BigInt::default, |v| BigInt::from_bytes_be(Sign::Plus, v))
    };
    let mut mismatches = Vec::new();
    for state in expected {
        let empty =
            ProtocolComponentState::new(&state.component_id, HashMap::new(), HashMap::new());
        let found = indexed
            .get(&state.component_id)
            .unwrap_or(&empty);
        let mut names: Vec<&String> = state
            .attributes
            .keys()
            .chain(found.attributes.keys())
            .collect();
        names.sort();
        names.dedup();
        for name in names {
            let (want, got) = (state.attributes.get(name), found.attributes.get(name));
            if int(want) != int(got) {
                mismatches.push(format!(
                    "{} {name}: chain {} indexed {}",
                    state.component_id,
                    int(want),
                    int(got)
                ));
            }
        }
        // Event-derived balances miss tokens sent to a component without an event, so the
        // indexed balance may trail `balanceOf` by dust but never exceed it.
        for (token, balance) in &state.balances {
            let got = found.balances.get(token);
            let shortfall = uint(Some(balance)) - uint(got);
            if shortfall < BigInt::default() || shortfall > BigInt::from(BALANCE_DUST) {
                mismatches.push(format!(
                    "{} balance {token}: chain {} indexed {}",
                    state.component_id,
                    uint(Some(balance)),
                    uint(got)
                ));
            }
        }
    }
    mismatches
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(attrs: &[(&str, i64)], balance: u64) -> ProtocolComponentState {
        ProtocolComponentState::new(
            "pool",
            attrs
                .iter()
                .map(|(name, v)| {
                    (
                        name.to_string(),
                        BigInt::from(*v)
                            .to_signed_bytes_be()
                            .into(),
                    )
                })
                .collect(),
            HashMap::from([(Bytes::from(vec![1]), Bytes::from(balance.to_be_bytes().to_vec()))]),
        )
    }

    #[test]
    fn test_start_package_after_moves_modules_and_sets_params() {
        use crate::pb::sf::substreams::v1::{module, Module, Modules};
        let module = |name: &str, initial_block: u64, params: bool| Module {
            name: name.to_string(),
            initial_block,
            inputs: params
                .then(|| module::Input {
                    input: Some(module_input::Input::Params(module::input::Params::default())),
                })
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let mut package = Package {
            modules: Some(Modules {
                modules: vec![module("seed", 10, true), module("late", 500, false)],
                ..Default::default()
            }),
            ..Default::default()
        };
        let params = HashMap::from([("seed".to_string(), "block=101".to_string())]);

        start_package_after(&mut package, 100, &params).unwrap();

        let modules = &package
            .modules
            .as_ref()
            .unwrap()
            .modules;
        assert_eq!((modules[0].initial_block, modules[1].initial_block), (101, 500));
        assert!(matches!(
            &modules[0].inputs[0].input,
            Some(module_input::Input::Params(p)) if p.value == "block=101"
        ));
        let bad = HashMap::from([("late".to_string(), String::new())]);
        assert!(start_package_after(&mut package, 100, &bad).is_err());
    }

    #[test]
    fn test_compare_states_treats_missing_as_zero() {
        let expected = vec![state(&[("liquidity", 5), ("ticks/1/net-liquidity", 3)], 9)];
        let indexed = HashMap::from([(
            "pool".to_string(),
            state(
                &[("liquidity", 5), ("ticks/1/net-liquidity", 3), ("ticks/2/net-liquidity", 0)],
                9,
            ),
        )]);
        assert!(compare_states(&expected, &indexed).is_empty());
    }

    #[test]
    fn test_compare_states_reports_differences() {
        let expected = vec![state(&[("liquidity", 5), ("ticks/1/net-liquidity", 3)], 9)];
        let indexed = HashMap::from([(
            "pool".to_string(),
            state(&[("liquidity", 6), ("ticks/7/net-liquidity", 1)], 10),
        )]);
        let mismatches = compare_states(&expected, &indexed);
        assert_eq!(mismatches.len(), 4, "{mismatches:?}");
        // A balance trailing `balanceOf` by dust passes; attributes must match exactly.
        let dust = HashMap::from([(
            "pool".to_string(),
            state(&[("liquidity", 5), ("ticks/1/net-liquidity", 3)], 8),
        )]);
        assert!(compare_states(&expected, &dust).is_empty());
    }
}
