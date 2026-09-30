//! Snapshot source for Uniswap V4 pools without swap hooks, matching the output of the
//! `ethereum-uniswap-v4-no-hooks` package.

use std::collections::HashMap;

use alloy::{
    primitives::{Address, B256, U256, U512},
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
use tycho_ethereum::rpc::EthereumRpcClient;
use tycho_protobuf::pb::tycho::evm::v1 as pb;

use crate::extractor::{
    bootstrap::{
        logs::LogSource,
        ticks::{decode_error, TickReader, TickSource},
        uniswap_v3::{signed, tick_spacing, uint},
        SnapshotSource,
    },
    ExtractionError,
};

sol! {
    event Initialize(
        bytes32 indexed id,
        address indexed currency0,
        address indexed currency1,
        uint24 fee,
        int24 tickSpacing,
        address hooks,
        uint160 sqrtPriceX96,
        int24 tick
    );

    function getSlot0(bytes32 poolId)
        external
        view
        returns (uint160 sqrtPriceX96, int24 tick, uint24 protocolFee, uint24 lpFee);
    function getLiquidity(bytes32 poolId) external view returns (uint128 liquidity);
}

/// The hook permission bits the no-hooks package excludes: `beforeSwap` and `afterSwap`.
const SWAP_HOOK_FLAGS: u32 = (1 << 7) | (1 << 6);

#[derive(Debug, Deserialize, Clone)]
pub struct UniswapV4Config {
    /// The PoolManager whose `Initialize` logs define the components and that holds the pools'
    /// tokens.
    pub pool_manager: Address,
    /// The periphery `StateView` of `pool_manager`, which the state reads go through.
    pub state_view: Address,
    /// The protocol type name the package gives the components.
    pub protocol_type_name: String,
}

/// Reads Uniswap V4 pools through `StateView`: `getSlot0`, `getLiquidity`, and every initialized
/// tick with its net liquidity.
pub struct UniswapV4Source {
    config: UniswapV4Config,
    ticks: TickReader,
    logs: Box<dyn LogSource>,
}

impl UniswapV4Source {
    pub fn new(
        config: UniswapV4Config,
        rpc: EthereumRpcClient,
        logs: Box<dyn LogSource>,
        calls_per_request: usize,
        concurrency: usize,
    ) -> Self {
        let ticks = TickReader::new(rpc, calls_per_request, concurrency, None);
        Self { config, ticks, logs }
    }

    /// Reads the state of `components` at `block`, with balances when `balances` is set.
    async fn read_state(
        &self,
        components: &[ProtocolComponent],
        block: BlockId,
        balances: bool,
    ) -> Result<Vec<ProtocolComponentState>, ExtractionError> {
        let pools = components
            .iter()
            .map(|c| Ok((pool_id(c)?, tick_spacing(c)?, c)))
            .collect::<Result<Vec<_>, ExtractionError>>()?;
        let state_view = self.config.state_view;
        let mut calls = Vec::new();
        for (id, _, _) in &pools {
            calls.push((
                state_view,
                getSlot0Call { poolId: *id }
                    .abi_encode()
                    .into(),
            ));
            calls.push((
                state_view,
                getLiquidityCall { poolId: *id }
                    .abi_encode()
                    .into(),
            ));
        }
        let mut results = self
            .ticks
            .multicall(calls, block)
            .await?
            .into_iter();
        let ticks = self
            .ticks
            .initialized_ticks(
                &pools
                    .iter()
                    .map(|(id, spacing, _)| (TickSource::V4 { state_view, pool_id: *id }, *spacing))
                    .collect::<Vec<_>>(),
                block,
            )
            .await?;

        let mut states = Vec::with_capacity(pools.len());
        for ((id, _, component), pool_ticks) in pools.iter().zip(ticks) {
            let mut next = || {
                results
                    .next()
                    .ok_or_else(|| decode_error("multicall", id, "missing result"))
            };
            let slot0 = getSlot0Call::abi_decode_returns(&next()?)
                .map_err(|e| decode_error("getSlot0", id, e))?;
            let liquidity = getLiquidityCall::abi_decode_returns(&next()?)
                .map_err(|e| decode_error("getLiquidity", id, e))?;
            let sqrt_price = U256::from(slot0.sqrtPriceX96);
            let protocol_fee = slot0.protocolFee.to::<u32>();

            let mut attributes = HashMap::from([
                ("sqrt_price_x96".to_string(), signed(uint(sqrt_price))),
                ("tick".to_string(), signed(slot0.tick.as_i32())),
                ("liquidity".to_string(), signed(liquidity)),
                ("protocol_fees/zero2one".to_string(), signed(protocol_fee & 0xfff)),
                ("protocol_fees/one2zero".to_string(), signed((protocol_fee >> 12) & 0xfff)),
                ("balance_owner".to_string(), Bytes::from(self.config.pool_manager.to_vec())),
            ]);
            for (tick, net) in &pool_ticks {
                if *net != 0 {
                    attributes.insert(format!("ticks/{tick}/net-liquidity"), signed(*net));
                }
            }
            let balances = if balances {
                let (amount0, amount1) = reserves(&pool_ticks, sqrt_price);
                component
                    .tokens
                    .iter()
                    .cloned()
                    .zip([amount0, amount1])
                    .map(|(token, amount)| (token, Bytes::from(uint(amount).to_bytes_be().1)))
                    .collect()
            } else {
                HashMap::new()
            };
            states.push(ProtocolComponentState::new(&component.id, attributes, balances));
        }
        Ok(states)
    }
}

fn pool_id(component: &ProtocolComponent) -> Result<B256, ExtractionError> {
    component.id.parse().map_err(|e| {
        ExtractionError::Setup(format!("Component id {} is not a pool id: {e}", component.id))
    })
}

fn int_attribute(state: &ProtocolComponentState, name: &str) -> Option<BigInt> {
    state
        .attributes
        .get(name)
        .map(|value| BigInt::from_signed_bytes_be(value))
}

#[async_trait]
impl SnapshotSource for UniswapV4Source {
    /// Passes every pool as `<pool id>:<currency0>:<currency1>:<tick>:<sqrtPriceX96>` to the
    /// package's `map_snapshot`, which seeds `store_pools`, `store_pool_current_tick` and
    /// `store_pool_current_sqrt_price`.
    fn snapshot_rows(
        &self,
        block: u64,
        components: &[ProtocolComponent],
        states: &[ProtocolComponentState],
    ) -> Result<Vec<String>, ExtractionError> {
        let states: HashMap<&str, &ProtocolComponentState> = states
            .iter()
            .map(|state| (state.component_id.as_str(), state))
            .collect();
        components
            .iter()
            .map(|component| {
                let missing = |what: &str| {
                    ExtractionError::Setup(format!(
                        "No {what} for component {} at block {block}",
                        component.id
                    ))
                };
                let state = states
                    .get(component.id.as_str())
                    .ok_or_else(|| missing("state"))?;
                let tick = int_attribute(state, "tick").ok_or_else(|| missing("tick"))?;
                let sqrt_price = int_attribute(state, "sqrt_price_x96")
                    .ok_or_else(|| missing("sqrt_price_x96"))?;
                let [currency0, currency1] = &component.tokens[..] else {
                    return Err(ExtractionError::Setup(format!(
                        "Component {} does not have two tokens",
                        component.id
                    )));
                };
                Ok(format!(
                    "{}:{}:{}:{tick}:{sqrt_price}",
                    component.id.trim_start_matches("0x"),
                    hex::encode(currency0),
                    hex::encode(currency1)
                ))
            })
            .collect()
    }

    async fn components(
        &self,
        from: u64,
        to: u64,
    ) -> Result<Vec<pb::ProtocolComponent>, ExtractionError> {
        let logs = self
            .logs
            .logs(self.config.pool_manager, Initialize::SIGNATURE_HASH, from, to)
            .await?;
        let mut components = Vec::new();
        for log in logs {
            let event =
                Initialize::decode_raw_log(log.topics.iter().copied(), &log.data).map_err(|e| {
                    ExtractionError::Setup(format!(
                        "Invalid Initialize log in tx {}: {e}",
                        log.transaction_hash
                    ))
                })?;
            if let Some(component) = self.component(&event) {
                components.push(component);
            }
        }
        Ok(components)
    }

    /// Balances are the pool's token amounts implied by its liquidity distribution at the
    /// current price, see `reserves`.
    async fn state(
        &self,
        components: &[ProtocolComponent],
        block: BlockId,
    ) -> Result<Vec<ProtocolComponentState>, ExtractionError> {
        self.read_state(components, block, true)
            .await
    }

    /// Leaves balances out. The package adds each liquidity change's amounts at the price of the
    /// event and each swap's amounts net of the LP fee, each rounded on its own, while the
    /// snapshot derives amounts from the liquidity distribution at one price. The two agree only
    /// up to rounding that grows with the number of events, and the PoolManager holds every
    /// pool's tokens, so no `balanceOf` bounds them either.
    async fn expected_state(
        &self,
        components: &[ProtocolComponent],
        block: BlockId,
    ) -> Result<Vec<ProtocolComponentState>, ExtractionError> {
        self.read_state(components, block, false)
            .await
    }
}

impl UniswapV4Source {
    /// The component the no-hooks package emits for `event`, or none for a pool it skips.
    fn component(&self, event: &Initialize) -> Option<pb::ProtocolComponent> {
        let flags = u32::from_be_bytes(
            event.hooks[16..20]
                .try_into()
                .expect("4 bytes"),
        );
        if flags & SWAP_HOOK_FLAGS != 0 {
            return None;
        }
        let creation = |name: &str, value: Vec<u8>| pb::Attribute {
            name: name.to_string(),
            value,
            change: pb::ChangeType::Creation.into(),
        };
        Some(pb::ProtocolComponent {
            id: format!("0x{}", hex::encode(event.id)),
            tokens: vec![event.currency0.to_vec(), event.currency1.to_vec()],
            contracts: vec![],
            static_att: vec![
                creation("tick_spacing", signed(event.tickSpacing.as_i32()).to_vec()),
                creation("pool_id", event.id.to_vec()),
                creation("hooks", event.hooks.to_vec()),
                creation("key_lp_fee", signed(event.fee.to::<u32>()).to_vec()),
            ],
            change: pb::ChangeType::Creation.into(),
            protocol_type: Some(pb::ProtocolType {
                name: self.config.protocol_type_name.clone(),
                financial_type: pb::FinancialType::Swap.into(),
                attribute_schema: vec![],
                implementation_type: pb::ImplementationType::Custom.into(),
            }),
        })
    }
}

/// `sqrt(1.0001^tick) * 2^96`, rounded up, as Uniswap's `TickMath.getSqrtPriceAtTick`.
fn sqrt_price_at_tick(tick: i32) -> U256 {
    const FACTORS: [(u32, u128); 19] = [
        (0x2, 0xfff97272373d413259a46990580e213a),
        (0x4, 0xfff2e50f5f656932ef12357cf3c7fdcc),
        (0x8, 0xffe5caca7e10e4e61c3624eaa0941cd0),
        (0x10, 0xffcb9843d60f6159c9db58835c926644),
        (0x20, 0xff973b41fa98c081472e6896dfb254c0),
        (0x40, 0xff2ea16466c96a3843ec78b326b52861),
        (0x80, 0xfe5dee046a99a2a811c461f1969c3053),
        (0x100, 0xfcbe86c7900a88aedcffc83b479aa3a4),
        (0x200, 0xf987a7253ac413176f2b074cf7815e54),
        (0x400, 0xf3392b0822b70005940c7a398e4b70f3),
        (0x800, 0xe7159475a2c29b7443b29c7fa6e889d9),
        (0x1000, 0xd097f3bdfd2022b8845ad8f792aa5825),
        (0x2000, 0xa9f746462d870fdf8a65dc1f90e061e5),
        (0x4000, 0x70d869a156d2a1b890bb3df62baf32f7),
        (0x8000, 0x31be135f97d08fd981231505542fcfa6),
        (0x10000, 0x9aa508b5b7a84e1c677de54f3e99bc9),
        (0x20000, 0x5d6af8dedb81196699c329225ee604),
        (0x40000, 0x2216e584f5fa1ea926041bedfe98),
        (0x80000, 0x48a170391f7dc42444e8fa2),
    ];
    let abs = tick.unsigned_abs();
    let mut ratio = if abs & 1 != 0 {
        U256::from(0xfffcb933bd6fad37aa2d162d1a594001u128)
    } else {
        U256::from(1) << 128
    };
    for (bit, factor) in FACTORS {
        if abs & bit != 0 {
            ratio = (ratio * U256::from(factor)) >> 128;
        }
    }
    if tick > 0 {
        ratio = U256::MAX / ratio;
    }
    let rounding =
        if (ratio % (U256::from(1) << 32)).is_zero() { U256::ZERO } else { U256::from(1) };
    (ratio >> 32) + rounding
}

fn wide(value: U256) -> U512 {
    U512::from_be_slice(&value.to_be_bytes::<32>())
}

/// The low 256 bits of `value`, which the amount formulas keep within.
fn narrow(value: U512) -> U256 {
    U256::from_be_slice(&value.to_be_bytes::<64>()[32..])
}

/// Token0 held by `liquidity` over the price range `lower..upper`, rounded down.
fn amount0(lower: U256, upper: U256, liquidity: u128) -> U256 {
    let numerator = (wide(U256::from(liquidity)) << 96) * wide(upper - lower);
    narrow(numerator / wide(upper) / wide(lower))
}

/// Token1 held by `liquidity` over the price range `lower..upper`, rounded down.
fn amount1(lower: U256, upper: U256, liquidity: u128) -> U256 {
    narrow((wide(U256::from(liquidity)) * wide(upper - lower)) >> 96)
}

/// The token amounts a pool's positions hold at `sqrt_price`, from its initialized ticks in
/// ascending order: every range between two ticks holds the net liquidity summed below it.
/// Collected fees and donations are not part of it, which matches what the package accumulates.
fn reserves(ticks: &[(i32, i128)], sqrt_price: U256) -> (U256, U256) {
    let (mut total0, mut total1) = (U256::ZERO, U256::ZERO);
    let mut liquidity: i128 = 0;
    for pair in ticks.windows(2) {
        let [(lower, net), (upper, _)] = pair else { unreachable!("windows of 2") };
        liquidity += net;
        let Ok(active) = u128::try_from(liquidity) else { continue };
        if active == 0 {
            continue;
        }
        let (low, high) = (sqrt_price_at_tick(*lower), sqrt_price_at_tick(*upper));
        if sqrt_price <= low {
            total0 += amount0(low, high, active);
        } else if sqrt_price >= high {
            total1 += amount1(low, high, active);
        } else {
            total0 += amount0(sqrt_price, high, active);
            total1 += amount1(low, sqrt_price, active);
        }
    }
    (total0, total1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sqrt_price_at_tick_matches_tick_math() {
        assert_eq!(sqrt_price_at_tick(0), U256::from(1) << 96);
        assert_eq!(sqrt_price_at_tick(-887272), U256::from(4295128739u64));
        assert_eq!(
            sqrt_price_at_tick(887272),
            "1461446703485210103287273052203988822378723970342"
                .parse::<U256>()
                .unwrap()
        );
    }

    #[test]
    fn test_reserves_split_the_range_at_the_price() {
        let one = U256::from(1) << 96;
        // Liquidity 1e18 over [-60, 60) at price 1: both tokens, symmetric within rounding.
        let (a0, a1) = reserves(&[(-60, 10i128.pow(18)), (60, -10i128.pow(18))], one);
        let expected = amount1(sqrt_price_at_tick(-60), one, 10u128.pow(18));
        assert_eq!(a1, expected);
        assert!(a0.abs_diff(a1) <= U256::from(1u64 << 20), "{a0} vs {a1}");
        // All token0 above the price, all token1 below.
        let (a0, a1) = reserves(&[(60, 5), (120, -5)], one);
        assert!(a0 > U256::ZERO && a1.is_zero());
        let (a0, a1) = reserves(&[(-120, 5), (-60, -5)], one);
        assert!(a0.is_zero() && a1 > U256::ZERO);
    }

    #[test]
    fn test_component_skips_swap_hooks() {
        let rpc = EthereumRpcClient::new("http://127.0.0.1:1").unwrap();
        let source = UniswapV4Source::new(
            UniswapV4Config {
                pool_manager: Address::ZERO,
                state_view: Address::ZERO,
                protocol_type_name: "uniswap_v4_pool".to_string(),
            },
            rpc.clone(),
            Box::new(crate::extractor::bootstrap::logs::RpcLogSource::new(rpc, 1, 1)),
            1,
            1,
        );
        let event = |hooks: Address, fee: u32| Initialize {
            id: B256::repeat_byte(0xab),
            currency0: Address::repeat_byte(1),
            currency1: Address::repeat_byte(2),
            fee: alloy::primitives::aliases::U24::from(fee),
            tickSpacing: alloy::primitives::aliases::I24::try_from(60).unwrap(),
            hooks,
            sqrtPriceX96: alloy::primitives::aliases::U160::from(1),
            tick: alloy::primitives::aliases::I24::try_from(0).unwrap(),
        };
        let component = source
            .component(&event(Address::ZERO, 0x800000))
            .unwrap();
        assert_eq!(component.id, format!("0x{}", "ab".repeat(32)));
        let fee = &component.static_att[3];
        assert_eq!(
            (fee.name.as_str(), fee.value.clone()),
            ("key_lp_fee", vec![0x00, 0x80, 0x00, 0x00])
        );
        let mut hooked = [0u8; 20];
        hooked[19] = 0x80;
        assert!(source
            .component(&event(Address::from(hooked), 3000))
            .is_none());
        hooked[19] = 0x01;
        assert!(source
            .component(&event(Address::from(hooked), 3000))
            .is_some());
    }

    #[test]
    fn test_snapshot_rows_carry_tick_and_price() {
        let rpc = EthereumRpcClient::new("http://127.0.0.1:1").unwrap();
        let source = UniswapV4Source::new(
            UniswapV4Config {
                pool_manager: Address::ZERO,
                state_view: Address::ZERO,
                protocol_type_name: String::new(),
            },
            rpc.clone(),
            Box::new(crate::extractor::bootstrap::logs::RpcLogSource::new(rpc, 1, 1)),
            1,
            1,
        );
        let component = ProtocolComponent {
            id: "0xaa".to_string(),
            tokens: vec![Bytes::from(vec![0x0a]), Bytes::from(vec![0x0b])],
            ..Default::default()
        };
        let state = ProtocolComponentState::new(
            "0xaa",
            HashMap::from([
                ("tick".to_string(), signed(-3)),
                ("sqrt_price_x96".to_string(), signed(uint(U256::from(1) << 96))),
            ]),
            HashMap::new(),
        );
        assert_eq!(
            source
                .snapshot_rows(9, &[component.clone()], &[state])
                .unwrap(),
            vec!["aa:0a:0b:-3:79228162514264337593543950336"]
        );
        assert!(source
            .snapshot_rows(9, &[component], &[])
            .is_err());
    }

    sol! {
        event Swap(
            bytes32 indexed id,
            address indexed sender,
            int128 amount0,
            int128 amount1,
            uint160 sqrtPriceX96,
            uint128 liquidity,
            int24 tick,
            uint24 fee
        );
        function poolKeys(bytes25 poolId) external view returns (
            address currency0,
            address currency1,
            uint24 fee,
            int24 tickSpacing,
            address hooks
        );
    }

    fn case_json(
        name: &str,
        start_block: u64,
        snapshot_params: &str,
        components: &[ProtocolComponent],
        start: &[ProtocolComponentState],
        logs: &[alloy::rpc::types::Log],
        end: &[ProtocolComponentState],
    ) -> serde_json::Value {
        use crate::extractor::bootstrap::uniswap_v3::tests::{
            assert_ticks_balance, hex_state, log_json,
        };
        for state in end {
            assert_ticks_balance(state);
        }
        serde_json::json!({
            "name": name,
            "start_block": start_block,
            "snapshot_params": snapshot_params,
            "pools": components
                .iter()
                .map(|c| serde_json::json!({
                    "id": c.id,
                    "currency0": c.tokens[0].to_string(),
                    "currency1": c.tokens[1].to_string(),
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

    /// Records PoolManager logs and `StateView` reads from a live chain into the replay fixture of
    /// the `ethereum-uniswap-v4` shared package, whose `test_replay_matches_chain` replays them.
    ///
    /// Two cases: `bootstrap` starts from the snapshot of the most active hookless pools at a
    /// finalized block and replays `SPAN` blocks of their logs; `replay` starts from a pool's
    /// `Initialize` and replays its whole history. Pool keys come from the `PositionManager`, so
    /// only pools initialized through it are candidates. The defaults target Uniswap V4 on Monad.
    #[tokio::test]
    #[ignore = "Requires RPC_URL with historical eth_call; LOGS_RPC_URL, POOL_MANAGER, STATE_VIEW, \
                POSITION_MANAGER, SPAN, POOLS, YOUNG_SPAN and LOGS_BLOCK_RANGE are optional"]
    async fn test_generate_uniswap_v4_replay_fixture() {
        use alloy::rpc::types::Filter;

        use crate::extractor::bootstrap::{
            logs::RpcLogSource,
            snapshot_chunks,
            uniswap_v3::tests::{env_or, logs_in_range},
        };
        let retry = || tycho_ethereum::rpc::config::RPCRetryConfig::new(8, 500, 30_000);
        let rpc = EthereumRpcClient::new(&std::env::var("RPC_URL").expect("RPC_URL"))
            .unwrap()
            .with_retry(retry());
        let logs_rpc = match std::env::var("LOGS_RPC_URL") {
            Ok(url) => EthereumRpcClient::new(&url)
                .unwrap()
                .with_retry(retry()),
            Err(_) => rpc.clone(),
        };
        let address =
            |name: &str, default: &str| -> Address { env_or(name, default).parse().unwrap() };
        let pool_manager = address("POOL_MANAGER", "0x188d586ddcf52439676ca21a244753fa19f9ea8e");
        let state_view = address("STATE_VIEW", "0x77395f3b2e73ae90843717371294fa97cc419d64");
        let position_manager =
            address("POSITION_MANAGER", "0x5b7ec4a94ff9bedb700fb82ab09d5846972f4016");
        let number = |name: &str, default: &str| -> u64 { env_or(name, default).parse().unwrap() };
        let (span, young_span, range) = (
            number("SPAN", "3000"),
            number("YOUNG_SPAN", "300000"),
            number("LOGS_BLOCK_RANGE", "100"),
        );
        let pool_count = number("POOLS", "5") as usize;

        let end = rpc.get_block_number().await.unwrap() - 100;
        let start = end - span;
        let source = UniswapV4Source::new(
            UniswapV4Config {
                pool_manager,
                state_view,
                protocol_type_name: "uniswap_v4_pool".to_string(),
            },
            rpc.clone(),
            Box::new(RpcLogSource::new(logs_rpc.clone(), range, 4)),
            200,
            4,
        );
        let manager_logs = |filter: Filter, from: u64| {
            let filter = filter.address(pool_manager);
            let logs_rpc = logs_rpc.clone();
            async move { logs_in_range(&logs_rpc, filter, from, end, range).await }
        };

        // Bootstrap case: the hookless pools with the most swaps in the span.
        let swaps =
            manager_logs(Filter::new().event_signature(Swap::SIGNATURE_HASH), start + 1).await;
        let mut activity: HashMap<B256, usize> = HashMap::new();
        for log in &swaps {
            *activity
                .entry(log.topics()[1])
                .or_default() += 1;
        }
        let mut candidates: Vec<B256> = activity.keys().copied().collect();
        candidates.sort_by_key(|id| (std::cmp::Reverse(activity[id]), *id));
        let keys = rpc
            .multicall(
                &candidates
                    .iter()
                    .map(|id| {
                        let call = poolKeysCall {
                            poolId: alloy::primitives::FixedBytes::from_slice(&id[..25]),
                        };
                        (position_manager, call.abi_encode().into())
                    })
                    .collect::<Vec<_>>(),
                BlockId::number(end),
                200,
                4,
            )
            .await
            .unwrap();
        let mut components = Vec::new();
        for (id, key) in candidates.iter().zip(keys) {
            let Ok(key) = poolKeysCall::abi_decode_returns(&key) else { continue };
            if key.tickSpacing.as_i32() <= 0 {
                continue;
            }
            let event = Initialize {
                id: *id,
                currency0: key.currency0,
                currency1: key.currency1,
                fee: key.fee,
                tickSpacing: key.tickSpacing,
                hooks: key.hooks,
                sqrtPriceX96: Default::default(),
                tick: Default::default(),
            };
            if let Some(component) = source.component(&event) {
                components.push(model(&component));
            }
            if components.len() == pool_count {
                break;
            }
        }
        assert!(!components.is_empty(), "no hookless pool swapped in blocks {start}..={end}");
        let ids: Vec<B256> = components
            .iter()
            .map(|c| pool_id(c).unwrap())
            .collect();
        let pool_logs = manager_logs(Filter::new().topic1(ids), start + 1).await;
        let start_states = source
            .state(&components, start.into())
            .await
            .unwrap();
        let params = snapshot_chunks(
            start,
            source
                .snapshot_rows(start, &components, &start_states)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(params.len(), 1, "fixture snapshot spans several chunks");
        let bootstrap_case = case_json(
            "bootstrap",
            start,
            &params[0],
            &components,
            &start_states,
            &pool_logs,
            &source
                .expected_state(&components, end.into())
                .await
                .unwrap(),
        );

        // Replay case: the youngest hookless pool that swapped, replayed from its `Initialize`.
        let initializes = manager_logs(
            Filter::new().event_signature(Initialize::SIGNATURE_HASH),
            end - young_span,
        )
        .await;
        let mut replay_case = None;
        for log in initializes.iter().rev() {
            let event =
                Initialize::decode_raw_log(log.topics().iter().copied(), &log.data().data).unwrap();
            let Some(component) = source.component(&event) else { continue };
            let creation = log.block_number.unwrap();
            let logs = manager_logs(Filter::new().topic1(event.id), creation).await;
            if !logs
                .iter()
                .any(|log| log.topics()[0] == Swap::SIGNATURE_HASH)
            {
                continue;
            }
            let models = [model(&component)];
            // What `map_pools_created` emits on creation, which the replay does not run.
            let start_state = ProtocolComponentState::new(
                &models[0].id,
                HashMap::from([
                    ("balance_owner".to_string(), Bytes::from(pool_manager.to_vec())),
                    ("tick".to_string(), signed(event.tick.as_i32())),
                    ("sqrt_price_x96".to_string(), signed(uint(U256::from(event.sqrtPriceX96)))),
                ]),
                HashMap::new(),
            );
            let end_state = source
                .expected_state(&models, end.into())
                .await
                .unwrap();
            replay_case = Some(case_json(
                "replay",
                creation - 1,
                "",
                &models,
                &[start_state],
                &logs,
                &end_state,
            ));
            break;
        }
        let replay_case = replay_case.unwrap_or_else(|| {
            panic!("no hookless pool created in the last {young_span} blocks swapped")
        });

        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../protocols/substreams/ethereum-uniswap-v4/shared/tests/fixtures/replay.json"
        );
        std::fs::create_dir_all(
            std::path::Path::new(path)
                .parent()
                .unwrap(),
        )
        .unwrap();
        std::fs::write(
            path,
            serde_json::to_string_pretty(
                &serde_json::json!({ "cases": [bootstrap_case, replay_case] }),
            )
            .unwrap(),
        )
        .unwrap();
    }

    /// The model form of a component the source emits.
    fn model(component: &pb::ProtocolComponent) -> ProtocolComponent {
        ProtocolComponent {
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
        }
    }
}
