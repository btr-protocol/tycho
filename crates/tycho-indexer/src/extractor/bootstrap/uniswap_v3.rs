//! Snapshot source for Uniswap V3 and its forks, matching the output of the
//! `ethereum-uniswap-v3-logs-only` package.

use std::collections::HashMap;

use alloy::{
    primitives::{aliases::I24, Address, Bytes as AlloyBytes, I256, U256},
    rpc::types::BlockId,
    sol,
    sol_types::{SolCall, SolEvent},
};
use async_trait::async_trait;
use num_bigint::BigInt;
use serde::Deserialize;
use tycho_common::{
    models::protocol::{ProtocolComponent, ProtocolComponentState},
    Bytes,
};
use tycho_ethereum::{erc20::balanceOfCall, rpc::EthereumRpcClient};
use tycho_protobuf::pb::tycho::evm::v1 as pb;

use crate::extractor::{
    bootstrap::{logs::LogSource, SnapshotSource},
    ExtractionError,
};

sol! {
    event PoolCreated(
        address indexed token0,
        address indexed token1,
        uint24 indexed fee,
        int24 tickSpacing,
        address pool
    );

    function liquidity() external view returns (uint128);
    function tickBitmap(int16 wordPosition) external view returns (uint256);
    function ticks(int24 tick) external view returns (
        uint128 liquidityGross,
        int128 liquidityNet,
        uint256 feeGrowthOutside0X128,
        uint256 feeGrowthOutside1X128,
        int56 tickCumulativeOutside,
        uint160 secondsPerLiquidityOutsideX128,
        uint32 secondsOutside,
        bool initialized
    );
    function slot0() external view;
}

const MIN_TICK: i32 = -887272;
const MAX_TICK: i32 = 887272;

/// How a fork packs and names its protocol fee.
#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolFeeLayout {
    /// `uint8 feeProtocol`, 4 bits per direction, emitted as `protocol_fees/token{0,1}` once
    /// set.
    #[default]
    Uniswap,
    /// `uint32 feeProtocol`, 16 bits per direction, emitted as
    /// `protocol_fees/{zero2one,one2zero}` from initialization on.
    Pancakeswap,
}

#[derive(Debug, Deserialize, Clone)]
pub struct UniswapV3Config {
    /// The factory whose `PoolCreated` logs define the components.
    pub factory: Address,
    /// The protocol type name the package gives the components.
    pub protocol_type_name: String,
    #[serde(default)]
    pub protocol_fee_layout: ProtocolFeeLayout,
}

/// Reads Uniswap V3 pools: `slot0`, `liquidity`, every `tickBitmap` word, `ticks(i)` for each
/// initialized tick, and the pool's token balances.
pub struct UniswapV3Source {
    config: UniswapV3Config,
    rpc: EthereumRpcClient,
    logs: Box<dyn LogSource>,
    calls_per_request: usize,
    concurrency: usize,
}

impl UniswapV3Source {
    pub fn new(
        config: UniswapV3Config,
        rpc: EthereumRpcClient,
        logs: Box<dyn LogSource>,
        calls_per_request: usize,
        concurrency: usize,
    ) -> Self {
        Self { config, rpc, logs, calls_per_request, concurrency }
    }

    async fn multicall(
        &self,
        calls: Vec<(Address, AlloyBytes)>,
        block: BlockId,
    ) -> Result<Vec<AlloyBytes>, ExtractionError> {
        self.rpc
            .multicall(&calls, block, self.calls_per_request, self.concurrency)
            .await
            .map_err(|e| ExtractionError::Setup(format!("Snapshot read at block {block}: {e}")))
    }
}

fn signed(value: impl Into<BigInt>) -> Bytes {
    value.into().to_signed_bytes_be().into()
}

fn uint(value: U256) -> BigInt {
    BigInt::from_bytes_be(num_bigint::Sign::Plus, &value.to_be_bytes::<32>())
}

fn decode_error(what: &str, pool: Address, e: impl std::fmt::Display) -> ExtractionError {
    ExtractionError::Setup(format!("Failed to decode {what} of pool {pool}: {e}"))
}

/// A pool's `slot0` fields that the packages index. Decoded by word so that the Uniswap
/// (`uint8`) and PancakeSwap (`uint32`) fee layouts share one read.
struct Slot0 {
    sqrt_price_x96: U256,
    tick: i32,
    fee_protocol: u32,
}

impl Slot0 {
    fn decode(data: &[u8], pool: Address) -> Result<Self, ExtractionError> {
        let word = |i: usize| {
            data.get(i * 32..(i + 1) * 32)
                .map(U256::from_be_slice)
                .ok_or_else(|| decode_error("slot0", pool, "short return data"))
        };
        let tick = I256::from_raw(word(1)?);
        Ok(Self {
            sqrt_price_x96: word(0)?,
            tick: i32::try_from(tick).map_err(|e| decode_error("slot0 tick", pool, e))?,
            fee_protocol: u32::try_from(word(5)?)
                .map_err(|e| decode_error("slot0 fee", pool, e))?,
        })
    }
}

/// The `tickBitmap` word positions a pool with `tick_spacing` can use.
fn word_range(tick_spacing: i32) -> std::ops::RangeInclusive<i16> {
    let word = |tick: i32| (tick.div_euclid(tick_spacing) >> 8) as i16;
    word(MIN_TICK)..=word(MAX_TICK)
}

/// The initialized ticks a `tickBitmap` word marks.
fn ticks_in_word(word_position: i16, bitmap: U256, tick_spacing: i32) -> Vec<i32> {
    (0..256)
        .filter(|bit| bitmap.bit(*bit))
        .map(|bit| ((i32::from(word_position) << 8) + bit as i32) * tick_spacing)
        .collect()
}

fn tick_spacing(component: &ProtocolComponent) -> Result<i32, ExtractionError> {
    component
        .static_attributes
        .get("tick_spacing")
        .and_then(|v| i32::try_from(BigInt::from_signed_bytes_be(v)).ok())
        .filter(|spacing| *spacing > 0)
        .ok_or_else(|| {
            ExtractionError::Setup(format!("Component {} has no valid tick_spacing", component.id))
        })
}

fn pool_address(component: &ProtocolComponent) -> Result<Address, ExtractionError> {
    component.id.parse().map_err(|e| {
        ExtractionError::Setup(format!("Component id {} is not an address: {e}", component.id))
    })
}

#[async_trait]
impl SnapshotSource for UniswapV3Source {
    async fn components(
        &self,
        from: u64,
        to: u64,
    ) -> Result<Vec<pb::ProtocolComponent>, ExtractionError> {
        let logs = self
            .logs
            .logs(self.config.factory, PoolCreated::SIGNATURE_HASH, from, to)
            .await?;
        logs.into_iter()
            .map(|log| {
                let event = PoolCreated::decode_raw_log(log.topics.iter().copied(), &log.data)
                    .map_err(|e| {
                        ExtractionError::Setup(format!(
                            "Invalid PoolCreated log in tx {}: {e}",
                            log.transaction_hash
                        ))
                    })?;
                let creation = |name: &str, value: Bytes| pb::Attribute {
                    name: name.to_string(),
                    value: value.to_vec(),
                    change: pb::ChangeType::Creation.into(),
                };
                Ok(pb::ProtocolComponent {
                    id: format!("0x{}", hex::encode(event.pool)),
                    tokens: vec![event.token0.to_vec(), event.token1.to_vec()],
                    contracts: vec![],
                    static_att: vec![
                        creation("fee", signed(event.fee.to::<u32>())),
                        creation("tick_spacing", signed(event.tickSpacing.as_i32())),
                        creation("pool_address", event.pool.to_vec().into()),
                    ],
                    change: pb::ChangeType::Creation.into(),
                    protocol_type: Some(pb::ProtocolType {
                        name: self.config.protocol_type_name.clone(),
                        financial_type: pb::FinancialType::Swap.into(),
                        attribute_schema: vec![],
                        implementation_type: pb::ImplementationType::Custom.into(),
                    }),
                })
            })
            .collect()
    }

    async fn state(
        &self,
        components: &[ProtocolComponent],
        block: BlockId,
    ) -> Result<Vec<ProtocolComponentState>, ExtractionError> {
        let pools = components
            .iter()
            .map(|c| Ok((pool_address(c)?, tick_spacing(c)?, c)))
            .collect::<Result<Vec<_>, ExtractionError>>()?;

        let mut calls = Vec::new();
        for (pool, spacing, component) in &pools {
            calls.push((*pool, slot0Call {}.abi_encode().into()));
            calls.push((*pool, liquidityCall {}.abi_encode().into()));
            for token in &component.tokens {
                let token = Address::from_slice(token);
                calls.push((
                    token,
                    balanceOfCall { _owner: *pool }
                        .abi_encode()
                        .into(),
                ));
            }
            for word in word_range(*spacing) {
                calls.push((
                    *pool,
                    tickBitmapCall { wordPosition: word }
                        .abi_encode()
                        .into(),
                ));
            }
        }
        let mut results = self
            .multicall(calls, block)
            .await?
            .into_iter();

        let mut states = Vec::with_capacity(pools.len());
        let mut tick_calls = Vec::new();
        let mut tick_owners = Vec::new();
        for (pool, spacing, component) in &pools {
            let mut next = || {
                results
                    .next()
                    .ok_or_else(|| decode_error("multicall", *pool, "missing result"))
            };
            let slot0 = Slot0::decode(&next()?, *pool)?;
            let liquidity = liquidityCall::abi_decode_returns(&next()?)
                .map_err(|e| decode_error("liquidity", *pool, e))?;
            let mut balances = HashMap::new();
            for token in &component.tokens {
                let balance = balanceOfCall::abi_decode_returns(&next()?)
                    .map_err(|e| decode_error("balanceOf", *pool, e))?;
                balances.insert(token.clone(), Bytes::from(uint(balance).to_bytes_be().1));
            }
            for word in word_range(*spacing) {
                let bitmap = tickBitmapCall::abi_decode_returns(&next()?)
                    .map_err(|e| decode_error("tickBitmap", *pool, e))?;
                for tick in ticks_in_word(word, bitmap, *spacing) {
                    let tick_arg =
                        I24::try_from(tick).map_err(|e| decode_error("tickBitmap", *pool, e))?;
                    tick_calls.push((
                        *pool,
                        ticksCall { tick: tick_arg }
                            .abi_encode()
                            .into(),
                    ));
                    tick_owners.push((states.len(), tick));
                }
            }

            let mut attributes = HashMap::from([
                ("sqrt_price_x96".to_string(), signed(uint(slot0.sqrt_price_x96))),
                ("tick".to_string(), signed(slot0.tick)),
                ("liquidity".to_string(), signed(liquidity)),
            ]);
            attributes.extend(self.protocol_fees(slot0.fee_protocol));
            states.push(ProtocolComponentState::new(&component.id, attributes, balances));
        }

        let tick_results = self
            .multicall(tick_calls, block)
            .await?;
        for ((index, tick), data) in tick_owners
            .into_iter()
            .zip(tick_results)
        {
            let info = ticksCall::abi_decode_returns(&data)
                .map_err(|e| decode_error("ticks", pools[index].0, e))?;
            if info.liquidityNet != 0 {
                states[index]
                    .attributes
                    .insert(format!("ticks/{tick}/net-liquidity"), signed(info.liquidityNet));
            }
        }
        Ok(states)
    }
}

impl UniswapV3Source {
    fn protocol_fees(&self, fee_protocol: u32) -> Vec<(String, Bytes)> {
        match self.config.protocol_fee_layout {
            ProtocolFeeLayout::Uniswap if fee_protocol == 0 => vec![],
            ProtocolFeeLayout::Uniswap => vec![
                ("protocol_fees/token0".to_string(), signed(fee_protocol % 16)),
                ("protocol_fees/token1".to_string(), signed(fee_protocol >> 4)),
            ],
            ProtocolFeeLayout::Pancakeswap => vec![
                ("protocol_fees/zero2one".to_string(), signed(fee_protocol % 65536)),
                ("protocol_fees/one2zero".to_string(), signed(fee_protocol >> 16)),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy::rpc::types::{Filter, Log};

    use super::*;
    use crate::extractor::bootstrap::logs::RpcLogSource;

    #[test]
    fn test_word_range_covers_tick_bounds() {
        assert_eq!(word_range(60), -58..=57);
        assert_eq!(word_range(1), -3466..=3465);
    }

    #[test]
    fn test_ticks_in_word() {
        let bitmap = (U256::from(1) << 0) | (U256::from(1) << 255);
        assert_eq!(ticks_in_word(-1, bitmap, 10), vec![-2560, -10]);
        assert_eq!(ticks_in_word(0, bitmap, 10), vec![0, 2550]);
    }

    #[test]
    fn test_slot0_decodes_both_fee_layouts() {
        let mut data = vec![0u8; 32 * 7];
        data[31] = 7;
        data[32..64].copy_from_slice(
            &I256::try_from(-5)
                .unwrap()
                .to_be_bytes::<32>(),
        );
        data[5 * 32 + 28..6 * 32].copy_from_slice(&0x0003_0002u32.to_be_bytes());
        let slot0 = Slot0::decode(&data, Address::ZERO).unwrap();
        assert_eq!(
            (slot0.sqrt_price_x96, slot0.tick, slot0.fee_protocol),
            (U256::from(7), -5, 0x0003_0002)
        );
    }

    sol! {
        event Swap(
            address indexed sender,
            address indexed recipient,
            int256 amount0,
            int256 amount1,
            uint160 sqrtPriceX96,
            uint128 liquidity,
            int24 tick
        );
        function factory() external view returns (address);
        function token0() external view returns (address);
        function token1() external view returns (address);
        function tickSpacing() external view returns (int24);
    }

    fn env_or(name: &str, default: &str) -> String {
        std::env::var(name).unwrap_or_else(|_| default.to_string())
    }

    fn hex_state(state: &ProtocolComponentState) -> serde_json::Value {
        let map = |m: &HashMap<Bytes, Bytes>| -> serde_json::Map<String, serde_json::Value> {
            m.iter()
                .map(|(k, v)| (k.to_string(), v.to_string().into()))
                .collect()
        };
        serde_json::json!({
            "attributes": state
                .attributes
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::from(v.to_string())))
                .collect::<serde_json::Map<_, _>>(),
            "balances": map(&state.balances),
        })
    }

    async fn logs_in_range(
        rpc: &EthereumRpcClient,
        filter: Filter,
        from: u64,
        to: u64,
        range: u64,
    ) -> Vec<Log> {
        let mut logs = Vec::new();
        let mut start = from;
        while start <= to {
            let end = to.min(start + range - 1);
            logs.extend(
                rpc.get_logs(
                    &filter
                        .clone()
                        .from_block(start)
                        .to_block(end),
                )
                .await
                .unwrap(),
            );
            start = end + 1;
        }
        logs
    }

    fn log_json(log: &Log) -> serde_json::Value {
        serde_json::json!({
            "block_number": log.block_number.unwrap(),
            "transaction_hash": log.transaction_hash.unwrap().to_string(),
            "transaction_index": log.transaction_index.unwrap(),
            "log_index": log.log_index.unwrap(),
            "address": log.address().to_string().to_lowercase(),
            "topics": log.topics().iter().map(|t| t.to_string()).collect::<Vec<_>>(),
            "data": log.data().data.to_string(),
        })
    }

    /// Checks that a pool's net liquidity sums to zero over its ticks, as every position adds
    /// its liquidity at the lower tick and removes it at the upper one.
    fn assert_ticks_balance(state: &ProtocolComponentState) {
        let sum: BigInt = state
            .attributes
            .iter()
            .filter(|(name, _)| name.starts_with("ticks/"))
            .map(|(_, v)| BigInt::from_signed_bytes_be(v))
            .sum();
        assert_eq!(
            sum,
            BigInt::default(),
            "net liquidity of {} does not sum to zero",
            state.component_id
        );
    }

    /// Records pool logs and snapshot reads from a live chain into the replay fixture of the
    /// `ethereum-uniswap-v3-logs-only` package, whose `test_replay_matches_chain` replays them.
    ///
    /// Two cases: `bootstrap` starts from the snapshot of the most active pools at a finalized
    /// block and replays `SPAN` blocks of their logs; `replay` starts from a pool's creation and
    /// replays its whole history. The defaults target Uniswap V3 on Monad.
    #[tokio::test]
    #[ignore = "Requires RPC_URL with historical eth_call; LOGS_RPC_URL, FACTORY, SPAN, POOLS, \
                YOUNG_SPAN and LOGS_BLOCK_RANGE are optional"]
    async fn test_generate_uniswap_v3_replay_fixture() {
        let rpc = EthereumRpcClient::new(&std::env::var("RPC_URL").expect("RPC_URL"))
            .unwrap()
            .with_retry(tycho_ethereum::rpc::config::RPCRetryConfig::new(8, 500, 30_000));
        let logs_rpc = match std::env::var("LOGS_RPC_URL") {
            Ok(url) => EthereumRpcClient::new(&url)
                .unwrap()
                .with_retry(tycho_ethereum::rpc::config::RPCRetryConfig::new(8, 500, 30_000)),
            Err(_) => rpc.clone(),
        };
        let factory: Address = env_or("FACTORY", "0x204FAca1764B154221e35c0d20aBb3c525710498")
            .parse()
            .unwrap();
        let span: u64 = env_or("SPAN", "2000").parse().unwrap();
        let young_span: u64 = env_or("YOUNG_SPAN", "2000000")
            .parse()
            .unwrap();
        let pool_count: usize = env_or("POOLS", "5").parse().unwrap();
        let range: u64 = env_or("LOGS_BLOCK_RANGE", "100")
            .parse()
            .unwrap();

        let end = rpc.get_block_number().await.unwrap() - 100;
        let start = end - span;
        let source = UniswapV3Source::new(
            UniswapV3Config {
                factory,
                protocol_type_name: "uniswap_v3_pool".to_string(),
                protocol_fee_layout: ProtocolFeeLayout::Uniswap,
            },
            rpc.clone(),
            Box::new(RpcLogSource::new(logs_rpc.clone(), range, 4)),
            200,
            4,
        );

        // Bootstrap case: the factory's pools with the most swaps in the span.
        let swaps = logs_in_range(
            &logs_rpc,
            Filter::new().event_signature(Swap::SIGNATURE_HASH),
            start + 1,
            end,
            range,
        )
        .await;
        let mut activity: HashMap<Address, usize> = HashMap::new();
        for log in &swaps {
            *activity
                .entry(log.address())
                .or_default() += 1;
        }
        let mut candidates: Vec<Address> = activity.keys().copied().collect();
        candidates.sort_by_key(|pool| std::cmp::Reverse(activity[pool]));
        let factories = rpc
            .multicall(
                &candidates
                    .iter()
                    .map(|pool| (*pool, factoryCall {}.abi_encode().into()))
                    .collect::<Vec<_>>(),
                BlockId::number(end),
                200,
                4,
            )
            .await
            .unwrap();
        let pools: Vec<Address> = candidates
            .into_iter()
            .zip(factories)
            .filter(|(_, owner)| factoryCall::abi_decode_returns(owner).ok() == Some(factory))
            .map(|(pool, _)| pool)
            .take(pool_count)
            .collect();
        assert!(!pools.is_empty(), "no factory pool swapped in blocks {start}..={end}");

        let mut calls = Vec::new();
        for pool in &pools {
            calls.push((*pool, token0Call {}.abi_encode().into()));
            calls.push((*pool, token1Call {}.abi_encode().into()));
            calls.push((*pool, tickSpacingCall {}.abi_encode().into()));
        }
        let mut info = rpc
            .multicall(&calls, BlockId::number(end), 200, 4)
            .await
            .unwrap()
            .into_iter();
        let components: Vec<ProtocolComponent> = pools
            .iter()
            .map(|pool| {
                let token0 = token0Call::abi_decode_returns(&info.next().unwrap()).unwrap();
                let token1 = token1Call::abi_decode_returns(&info.next().unwrap()).unwrap();
                let spacing = tickSpacingCall::abi_decode_returns(&info.next().unwrap()).unwrap();
                ProtocolComponent {
                    id: format!("0x{}", hex::encode(pool)),
                    tokens: vec![token0.to_vec().into(), token1.to_vec().into()],
                    static_attributes: HashMap::from([(
                        "tick_spacing".to_string(),
                        signed(spacing.as_i32()),
                    )]),
                    ..Default::default()
                }
            })
            .collect();
        let pool_logs =
            logs_in_range(&logs_rpc, Filter::new().address(pools.clone()), start + 1, end, range)
                .await;

        let bootstrap_case = case_json(
            "bootstrap",
            &components,
            &source
                .state(&components, start.into())
                .await
                .unwrap(),
            &pool_logs,
            &source
                .state(&components, end.into())
                .await
                .unwrap(),
        );

        // Replay case: the youngest pool with logs, replayed from its creation with no snapshot.
        let created = source
            .components(end - young_span, end)
            .await
            .unwrap();
        let creations = source
            .logs
            .logs(factory, PoolCreated::SIGNATURE_HASH, end - young_span, end)
            .await
            .unwrap();
        let mut replay_case = None;
        for (component, creation) in created.iter().zip(&creations).rev() {
            let pool: Address = component.id.parse().unwrap();
            let creation = creation.block_number;
            let logs =
                logs_in_range(&logs_rpc, Filter::new().address(pool), creation, end, range).await;
            if logs
                .iter()
                .any(|log| log.topics()[0] == Swap::SIGNATURE_HASH)
            {
                let model = ProtocolComponent {
                    id: component.id.clone(),
                    tokens: component
                        .tokens
                        .iter()
                        .map(|t| t.clone().into())
                        .collect(),
                    static_attributes: component
                        .static_att
                        .iter()
                        .map(|a| (a.name.clone(), a.value.clone().into()))
                        .collect(),
                    ..Default::default()
                };
                let empty = ProtocolComponentState::new(&model.id, HashMap::new(), HashMap::new());
                let models = [model];
                let end_state = source
                    .state(&models, end.into())
                    .await
                    .unwrap();
                replay_case = Some(case_json("replay", &models, &[empty], &logs, &end_state));
                break;
            }
        }

        let replay_case = replay_case
            .unwrap_or_else(|| panic!("no pool created in the last {young_span} blocks swapped"));
        let cases = vec![bootstrap_case, replay_case];
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../protocols/substreams/ethereum-uniswap-v3-logs-only/tests/fixtures/replay.json"
        );
        std::fs::create_dir_all(
            std::path::Path::new(path)
                .parent()
                .unwrap(),
        )
        .unwrap();
        std::fs::write(
            path,
            serde_json::to_string_pretty(&serde_json::json!({ "cases": cases })).unwrap(),
        )
        .unwrap();
    }

    fn case_json(
        name: &str,
        components: &[ProtocolComponent],
        start: &[ProtocolComponentState],
        logs: &[Log],
        end: &[ProtocolComponentState],
    ) -> serde_json::Value {
        for state in end {
            assert_ticks_balance(state);
        }
        serde_json::json!({
            "name": name,
            "pools": components
                .iter()
                .map(|c| serde_json::json!({
                    "address": c.id,
                    "token0": c.tokens[0].to_string(),
                    "token1": c.tokens[1].to_string(),
                }))
                .collect::<Vec<_>>(),
            "start": start
                .iter()
                .map(|s| (s.component_id.clone(), hex_state(s)))
                .collect::<serde_json::Map<_, _>>(),
            "logs": logs.iter().map(log_json).collect::<Vec<_>>(),
            "end": end
                .iter()
                .map(|s| (s.component_id.clone(), hex_state(s)))
                .collect::<serde_json::Map<_, _>>(),
        })
    }
}
